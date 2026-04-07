from __future__ import annotations

import json
from collections.abc import AsyncIterator, Callable, Iterator
from dataclasses import dataclass
from typing import Any, TypeVar, cast
from urllib.parse import quote

import httpx

from ._errors import (
    OpenyakApiError,
    OpenyakProtocolError,
    OpenyakReconnectRequiredError,
    OpenyakResyncRequiredError,
)
from ._models import (
    AcceptedResponse,
    AssistantTextDeltaEvent,
    AssistantUsageEvent,
    AsyncRunStreamedResult,
    AwaitingUserInputRunResult,
    CompletedRunResult,
    CreateThreadOptions,
    FailedRunResult,
    InterruptedRunResult,
    JsonValue,
    ListThreadsResponse,
    PermissionMode,
    RunCompletedEvent,
    RunEvent,
    RunFailedEvent,
    RunResult,
    RunStreamedResult,
    RunWaitingUserInputEvent,
    ThreadEventAny,
    ThreadResyncRequiredEvent,
    ThreadSnapshot,
    ThreadSnapshotEvent,
    TokenUsage,
    TurnAcceptedResponse,
    UserInputAcceptedResponse,
    is_terminal_run_event,
    messages_to_text,
    parse_api_error_envelope,
    parse_list_threads_response,
    parse_thread_event,
    parse_thread_snapshot,
    parse_turn_accepted_response,
    parse_user_input_accepted_response,
    sum_assistant_usage,
)
from ._sse import aiter_sse_frames, iter_sse_frames

ParseResultT = TypeVar("ParseResultT")


@dataclass(slots=True)
class _RunResultParts:
    final_text: str | None
    usage: TokenUsage | None
    snapshot: ThreadSnapshot | None = None


class OpenyakClient:
    def __init__(
        self,
        *,
        base_url: str,
        timeout_s: float = 10.0,
        transport: httpx.BaseTransport | None = None,
    ) -> None:
        trimmed = base_url.rstrip("/")
        if not trimmed:
            raise OpenyakProtocolError("base_url is required")
        self.base_url = trimmed
        self._client = httpx.Client(
            base_url=self.base_url,
            timeout=timeout_s,
            transport=transport,
            headers={"accept": "application/json, text/event-stream"},
        )

    def close(self) -> None:
        self._client.close()

    def __enter__(self) -> OpenyakClient:
        return self

    def __exit__(self, _exc_type: object, _exc: object, _tb: object) -> None:
        self.close()

    def create_thread_snapshot(
        self,
        *,
        cwd: str | None = None,
        model: str | None = None,
        permission_mode: PermissionMode | None = None,
        allowed_tools: list[str] | None = None,
    ) -> ThreadSnapshot:
        return self._request_json(
            "POST",
            "/v1/threads",
            json_body=_create_thread_body(
                CreateThreadOptions(
                    cwd=cwd,
                    model=model,
                    permission_mode=permission_mode,
                    allowed_tools=allowed_tools,
                )
            ),
            parser=lambda data: parse_thread_snapshot(data, "POST /v1/threads"),
        )

    def create_thread(
        self,
        *,
        cwd: str | None = None,
        model: str | None = None,
        permission_mode: PermissionMode | None = None,
        allowed_tools: list[str] | None = None,
    ) -> Thread:
        snapshot = self.create_thread_snapshot(
            cwd=cwd,
            model=model,
            permission_mode=permission_mode,
            allowed_tools=allowed_tools,
        )
        return Thread(self, snapshot.thread_id, snapshot)

    def resume_thread(self, thread_id: str) -> Thread:
        return Thread(self, thread_id)

    def list_threads(self) -> ListThreadsResponse:
        return self._request_json(
            "GET",
            "/v1/threads",
            parser=lambda data: parse_list_threads_response(data, "GET /v1/threads"),
        )

    def get_thread(self, thread_id: str) -> ThreadSnapshot:
        return self._request_json(
            "GET",
            f"/v1/threads/{_quote(thread_id)}",
            parser=lambda data: parse_thread_snapshot(data, f"GET /v1/threads/{thread_id}"),
        )

    def _request_json(
        self,
        method: str,
        path: str,
        *,
        json_body: dict[str, JsonValue] | None = None,
        parser: Callable[[object], ParseResultT],
    ) -> ParseResultT:
        request_kwargs: dict[str, Any] = {}
        if json_body is not None:
            request_kwargs["json"] = json_body
        response = self._client.request(method, path, **request_kwargs)
        try:
            _raise_for_error(response)
            return parser(response.json())
        finally:
            response.close()

    def _open_event_stream(self, path: str) -> httpx.Response:
        request = self._client.build_request("GET", path)
        response = self._client.send(request, stream=True)
        try:
            _raise_for_error(response)
        except Exception:
            response.close()
            raise
        return response


class Thread:
    def __init__(
        self,
        client: OpenyakClient,
        thread_id: str,
        snapshot: ThreadSnapshot | None = None,
    ) -> None:
        self._client = client
        self.thread_id = thread_id
        self._last_snapshot = snapshot

    @property
    def snapshot(self) -> ThreadSnapshot | None:
        return self._last_snapshot

    def read(self) -> ThreadSnapshot:
        snapshot = self._client.get_thread(self.thread_id)
        self._last_snapshot = snapshot
        return snapshot

    def start_turn(self, message: str) -> TurnAcceptedResponse:
        return self._request_json(
            "POST",
            f"/v1/threads/{_quote(self.thread_id)}/turns",
            {"message": message},
            lambda data: parse_turn_accepted_response(
                data, f"POST /v1/threads/{self.thread_id}/turns"
            ),
        )

    def submit_user_input(
        self,
        *,
        request_id: str,
        content: str,
        selected_option: str | None = None,
    ) -> UserInputAcceptedResponse:
        payload = {"request_id": request_id, "content": content}
        if selected_option is not None:
            payload["selected_option"] = selected_option
        return self._request_json(
            "POST",
            f"/v1/threads/{_quote(self.thread_id)}/user-input",
            payload,
            lambda data: parse_user_input_accepted_response(
                data, f"POST /v1/threads/{self.thread_id}/user-input"
            ),
        )

    def stream_events(self) -> Iterator[ThreadEventAny]:
        response = self._client._open_event_stream(f"/v1/threads/{_quote(self.thread_id)}/events")

        def iterator() -> Iterator[ThreadEventAny]:
            try:
                for event in _iter_decoded_thread_events(response):
                    if event.type == "thread.snapshot":
                        self._last_snapshot = cast(ThreadSnapshotEvent, event).payload
                    yield event
            finally:
                response.close()

        return iterator()

    def run_streamed(self, message: str) -> RunStreamedResult:
        return self._prepare_run_stream(lambda: self.start_turn(message))

    def run(self, message: str) -> RunResult:
        streamed = self.run_streamed(message)
        return self._consume_buffered_run(streamed)

    def resume_user_input_streamed(
        self,
        *,
        request_id: str,
        content: str,
        selected_option: str | None = None,
    ) -> RunStreamedResult:
        return self._prepare_run_stream(
            lambda: self.submit_user_input(
                request_id=request_id,
                content=content,
                selected_option=selected_option,
            )
        )

    def resume_user_input(
        self,
        *,
        request_id: str,
        content: str,
        selected_option: str | None = None,
    ) -> RunResult:
        streamed = self.resume_user_input_streamed(
            request_id=request_id,
            content=content,
            selected_option=selected_option,
        )
        return self._consume_buffered_run(streamed)

    def _prepare_run_stream(self, submit: Callable[[], AcceptedResponse]) -> RunStreamedResult:
        response = self._client._open_event_stream(f"/v1/threads/{_quote(self.thread_id)}/events")
        iterator = _iter_decoded_thread_events(response)
        try:
            first = next(iterator)
        except StopIteration as error:
            response.close()
            raise OpenyakProtocolError("event stream closed before thread.snapshot") from error
        if first.type != "thread.snapshot":
            response.close()
            raise OpenyakProtocolError(
                f"expected initial thread.snapshot event, received {first.type}"
            )
        first_snapshot = cast(ThreadSnapshotEvent, first)
        self._last_snapshot = first_snapshot.payload

        try:
            accepted = submit()
        except Exception:
            response.close()
            raise

        return RunStreamedResult(
            snapshot=first_snapshot.payload,
            accepted=accepted,
            events=self._run_events(response, iterator, accepted.run_id),
            _close=response.close,
        )

    def _run_events(
        self,
        response: httpx.Response,
        iterator: Iterator[ThreadEventAny],
        run_id: str,
    ) -> Iterator[RunEvent]:
        try:
            for event in iterator:
                if event.type == "thread.resync_required":
                    raise OpenyakResyncRequiredError(cast(ThreadResyncRequiredEvent, event))
                if event.type == "thread.snapshot":
                    self._last_snapshot = cast(ThreadSnapshotEvent, event).payload
                    continue
                if event.run_id != run_id:
                    continue
                run_event = cast(RunEvent, event)
                yield run_event
                if is_terminal_run_event(run_event):
                    return
        finally:
            response.close()
        raise OpenyakReconnectRequiredError(self.thread_id, run_id)

    def _consume_buffered_run(self, streamed: RunStreamedResult) -> RunResult:
        events: list[RunEvent] = []
        final_text_parts: list[str] = []
        latest_usage: TokenUsage | None = None
        try:
            for event in streamed.events:
                events.append(event)
                if event.type == "assistant.text.delta":
                    final_text_parts.append(cast(AssistantTextDeltaEvent, event).payload.text)
                    continue
                if event.type == "assistant.usage":
                    latest_usage = cast(AssistantUsageEvent, event).payload
                    continue
                if event.type == "run.completed":
                    completed_event = cast(RunCompletedEvent, event)
                    return CompletedRunResult(
                        thread_id=self.thread_id,
                        run_id=streamed.accepted.run_id,
                        events=events,
                        recovered_from_snapshot=False,
                        final_text=_normalize_text("".join(final_text_parts)),
                        usage=completed_event.payload.cumulative_usage,
                        terminal_event=completed_event,
                    )
                if event.type == "run.waiting_user_input":
                    waiting_event = cast(RunWaitingUserInputEvent, event)
                    return AwaitingUserInputRunResult(
                        thread_id=self.thread_id,
                        run_id=streamed.accepted.run_id,
                        events=events,
                        recovered_from_snapshot=False,
                        pending_user_input=waiting_event.payload,
                        final_text=_normalize_text("".join(final_text_parts)),
                        usage=latest_usage,
                        terminal_event=waiting_event,
                    )
                if event.type == "run.failed":
                    failed_event = cast(RunFailedEvent, event)
                    return FailedRunResult(
                        thread_id=self.thread_id,
                        run_id=streamed.accepted.run_id,
                        events=events,
                        recovered_from_snapshot=False,
                        error=failed_event.payload,
                        terminal_event=failed_event,
                        final_text=_normalize_text("".join(final_text_parts)),
                        usage=latest_usage,
                    )
            raise OpenyakReconnectRequiredError(self.thread_id, streamed.accepted.run_id)
        except (OpenyakResyncRequiredError, OpenyakReconnectRequiredError) as error:
            return self._reconcile_buffered_run(
                initial_snapshot=streamed.snapshot,
                run_id=streamed.accepted.run_id,
                events=events,
                partial_text="".join(final_text_parts),
                partial_usage=latest_usage,
                error=error,
            )
        finally:
            streamed.close()

    def _reconcile_buffered_run(
        self,
        *,
        initial_snapshot: ThreadSnapshot,
        run_id: str,
        events: list[RunEvent],
        partial_text: str,
        partial_usage: TokenUsage | None,
        error: OpenyakResyncRequiredError | OpenyakReconnectRequiredError,
    ) -> RunResult:
        try:
            latest_snapshot = self.read()
        except Exception as read_error:
            raise error from read_error
        if latest_snapshot.state.run_id is not None and latest_snapshot.state.run_id != run_id:
            raise OpenyakReconnectRequiredError(self.thread_id, run_id, latest_snapshot)
        if latest_snapshot.state.status == "running":
            raise OpenyakReconnectRequiredError(self.thread_id, run_id, latest_snapshot)

        appended_messages = latest_snapshot.session.messages[
            len(initial_snapshot.session.messages) :
        ]
        parts = _run_result_parts(
            final_text=_normalize_text(partial_text)
            or _normalize_text(messages_to_text(appended_messages)),
            usage=partial_usage or sum_assistant_usage(appended_messages),
            snapshot=latest_snapshot,
        )
        if latest_snapshot.state.status == "idle":
            return CompletedRunResult(
                thread_id=self.thread_id,
                run_id=run_id,
                events=events,
                recovered_from_snapshot=True,
                final_text=parts.final_text,
                usage=parts.usage,
                snapshot=parts.snapshot,
                terminal_event=None,
            )
        if (
            latest_snapshot.state.status == "awaiting_user_input"
            and latest_snapshot.state.pending_user_input is not None
        ):
            return AwaitingUserInputRunResult(
                thread_id=self.thread_id,
                run_id=run_id,
                events=events,
                recovered_from_snapshot=True,
                pending_user_input=latest_snapshot.state.pending_user_input,
                final_text=parts.final_text,
                usage=parts.usage,
                snapshot=parts.snapshot,
                terminal_event=None,
            )
        if latest_snapshot.state.status == "interrupted":
            return InterruptedRunResult(
                thread_id=self.thread_id,
                run_id=run_id,
                events=events,
                recovered_from_snapshot=True,
                recovery_note=latest_snapshot.state.recovery_note,
                final_text=parts.final_text,
                usage=parts.usage,
                snapshot=parts.snapshot,
                terminal_event=None,
            )
        raise OpenyakReconnectRequiredError(self.thread_id, run_id, latest_snapshot)

    def _request_json(
        self,
        method: str,
        path: str,
        payload: dict[str, JsonValue],
        parser: Callable[[object], ParseResultT],
    ) -> ParseResultT:
        return self._client._request_json(method, path, json_body=payload, parser=parser)


class AsyncOpenyakClient:
    def __init__(
        self,
        *,
        base_url: str,
        timeout_s: float = 10.0,
        transport: httpx.AsyncBaseTransport | None = None,
    ) -> None:
        trimmed = base_url.rstrip("/")
        if not trimmed:
            raise OpenyakProtocolError("base_url is required")
        self.base_url = trimmed
        self._client = httpx.AsyncClient(
            base_url=self.base_url,
            timeout=timeout_s,
            transport=transport,
            headers={"accept": "application/json, text/event-stream"},
        )

    async def aclose(self) -> None:
        await self._client.aclose()

    async def __aenter__(self) -> AsyncOpenyakClient:
        return self

    async def __aexit__(self, _exc_type: object, _exc: object, _tb: object) -> None:
        await self.aclose()

    async def create_thread_snapshot(
        self,
        *,
        cwd: str | None = None,
        model: str | None = None,
        permission_mode: PermissionMode | None = None,
        allowed_tools: list[str] | None = None,
    ) -> ThreadSnapshot:
        return await self._request_json(
            "POST",
            "/v1/threads",
            json_body=_create_thread_body(
                CreateThreadOptions(
                    cwd=cwd,
                    model=model,
                    permission_mode=permission_mode,
                    allowed_tools=allowed_tools,
                )
            ),
            parser=lambda data: parse_thread_snapshot(data, "POST /v1/threads"),
        )

    async def create_thread(
        self,
        *,
        cwd: str | None = None,
        model: str | None = None,
        permission_mode: PermissionMode | None = None,
        allowed_tools: list[str] | None = None,
    ) -> AsyncThread:
        snapshot = await self.create_thread_snapshot(
            cwd=cwd,
            model=model,
            permission_mode=permission_mode,
            allowed_tools=allowed_tools,
        )
        return AsyncThread(self, snapshot.thread_id, snapshot)

    def resume_thread(self, thread_id: str) -> AsyncThread:
        return AsyncThread(self, thread_id)

    async def list_threads(self) -> ListThreadsResponse:
        return await self._request_json(
            "GET",
            "/v1/threads",
            parser=lambda data: parse_list_threads_response(data, "GET /v1/threads"),
        )

    async def get_thread(self, thread_id: str) -> ThreadSnapshot:
        return await self._request_json(
            "GET",
            f"/v1/threads/{_quote(thread_id)}",
            parser=lambda data: parse_thread_snapshot(data, f"GET /v1/threads/{thread_id}"),
        )

    async def _request_json(
        self,
        method: str,
        path: str,
        *,
        json_body: dict[str, JsonValue] | None = None,
        parser: Callable[[object], ParseResultT],
    ) -> ParseResultT:
        request_kwargs: dict[str, Any] = {}
        if json_body is not None:
            request_kwargs["json"] = json_body
        response = await self._client.request(method, path, **request_kwargs)
        try:
            await _araise_for_error(response)
            return parser(response.json())
        finally:
            await response.aclose()

    async def _open_event_stream(self, path: str) -> httpx.Response:
        request = self._client.build_request("GET", path)
        response = await self._client.send(request, stream=True)
        try:
            await _araise_for_error(response)
        except Exception:
            await response.aclose()
            raise
        return response


class AsyncThread:
    def __init__(
        self,
        client: AsyncOpenyakClient,
        thread_id: str,
        snapshot: ThreadSnapshot | None = None,
    ) -> None:
        self._client = client
        self.thread_id = thread_id
        self._last_snapshot = snapshot

    @property
    def snapshot(self) -> ThreadSnapshot | None:
        return self._last_snapshot

    async def read(self) -> ThreadSnapshot:
        snapshot = await self._client.get_thread(self.thread_id)
        self._last_snapshot = snapshot
        return snapshot

    async def start_turn(self, message: str) -> TurnAcceptedResponse:
        return await self._request_json(
            "POST",
            f"/v1/threads/{_quote(self.thread_id)}/turns",
            {"message": message},
            lambda data: parse_turn_accepted_response(
                data, f"POST /v1/threads/{self.thread_id}/turns"
            ),
        )

    async def submit_user_input(
        self,
        *,
        request_id: str,
        content: str,
        selected_option: str | None = None,
    ) -> UserInputAcceptedResponse:
        payload = {"request_id": request_id, "content": content}
        if selected_option is not None:
            payload["selected_option"] = selected_option
        return await self._request_json(
            "POST",
            f"/v1/threads/{_quote(self.thread_id)}/user-input",
            payload,
            lambda data: parse_user_input_accepted_response(
                data, f"POST /v1/threads/{self.thread_id}/user-input"
            ),
        )

    def stream_events(self) -> AsyncIterator[ThreadEventAny]:
        async def iterator() -> AsyncIterator[ThreadEventAny]:
            response = await self._client._open_event_stream(
                f"/v1/threads/{_quote(self.thread_id)}/events"
            )
            try:
                async for event in _aiter_decoded_thread_events(response):
                    if event.type == "thread.snapshot":
                        self._last_snapshot = cast(ThreadSnapshotEvent, event).payload
                    yield event
            finally:
                await response.aclose()

        return iterator()

    async def run_streamed(self, message: str) -> AsyncRunStreamedResult:
        return await self._prepare_run_stream(lambda: self.start_turn(message))

    async def run(self, message: str) -> RunResult:
        streamed = await self.run_streamed(message)
        return await self._consume_buffered_run(streamed)

    async def resume_user_input_streamed(
        self,
        *,
        request_id: str,
        content: str,
        selected_option: str | None = None,
    ) -> AsyncRunStreamedResult:
        return await self._prepare_run_stream(
            lambda: self.submit_user_input(
                request_id=request_id,
                content=content,
                selected_option=selected_option,
            )
        )

    async def resume_user_input(
        self,
        *,
        request_id: str,
        content: str,
        selected_option: str | None = None,
    ) -> RunResult:
        streamed = await self.resume_user_input_streamed(
            request_id=request_id,
            content=content,
            selected_option=selected_option,
        )
        return await self._consume_buffered_run(streamed)

    async def _prepare_run_stream(
        self,
        submit: Callable[[], Any],
    ) -> AsyncRunStreamedResult:
        response = await self._client._open_event_stream(
            f"/v1/threads/{_quote(self.thread_id)}/events"
        )
        iterator = _aiter_decoded_thread_events(response).__aiter__()
        try:
            first = await iterator.__anext__()
        except StopAsyncIteration as error:
            await response.aclose()
            raise OpenyakProtocolError("event stream closed before thread.snapshot") from error
        if first.type != "thread.snapshot":
            await response.aclose()
            raise OpenyakProtocolError(
                f"expected initial thread.snapshot event, received {first.type}"
            )
        first_snapshot = cast(ThreadSnapshotEvent, first)
        self._last_snapshot = first_snapshot.payload

        try:
            accepted = await submit()
        except Exception:
            await response.aclose()
            raise

        return AsyncRunStreamedResult(
            snapshot=first_snapshot.payload,
            accepted=accepted,
            events=self._run_events(response, iterator, accepted.run_id),
            _close=response.aclose,
        )

    async def _run_events(
        self,
        response: httpx.Response,
        iterator: AsyncIterator[ThreadEventAny],
        run_id: str,
    ) -> AsyncIterator[RunEvent]:
        try:
            async for event in iterator:
                if event.type == "thread.resync_required":
                    raise OpenyakResyncRequiredError(cast(ThreadResyncRequiredEvent, event))
                if event.type == "thread.snapshot":
                    self._last_snapshot = cast(ThreadSnapshotEvent, event).payload
                    continue
                if event.run_id != run_id:
                    continue
                run_event = cast(RunEvent, event)
                yield run_event
                if is_terminal_run_event(run_event):
                    return
        finally:
            await response.aclose()
        raise OpenyakReconnectRequiredError(self.thread_id, run_id)

    async def _consume_buffered_run(self, streamed: AsyncRunStreamedResult) -> RunResult:
        events: list[RunEvent] = []
        final_text_parts: list[str] = []
        latest_usage: TokenUsage | None = None
        try:
            async for event in streamed.events:
                events.append(event)
                if event.type == "assistant.text.delta":
                    final_text_parts.append(cast(AssistantTextDeltaEvent, event).payload.text)
                    continue
                if event.type == "assistant.usage":
                    latest_usage = cast(AssistantUsageEvent, event).payload
                    continue
                if event.type == "run.completed":
                    completed_event = cast(RunCompletedEvent, event)
                    return CompletedRunResult(
                        thread_id=self.thread_id,
                        run_id=streamed.accepted.run_id,
                        events=events,
                        recovered_from_snapshot=False,
                        final_text=_normalize_text("".join(final_text_parts)),
                        usage=completed_event.payload.cumulative_usage,
                        terminal_event=completed_event,
                    )
                if event.type == "run.waiting_user_input":
                    waiting_event = cast(RunWaitingUserInputEvent, event)
                    return AwaitingUserInputRunResult(
                        thread_id=self.thread_id,
                        run_id=streamed.accepted.run_id,
                        events=events,
                        recovered_from_snapshot=False,
                        pending_user_input=waiting_event.payload,
                        final_text=_normalize_text("".join(final_text_parts)),
                        usage=latest_usage,
                        terminal_event=waiting_event,
                    )
                if event.type == "run.failed":
                    failed_event = cast(RunFailedEvent, event)
                    return FailedRunResult(
                        thread_id=self.thread_id,
                        run_id=streamed.accepted.run_id,
                        events=events,
                        recovered_from_snapshot=False,
                        error=failed_event.payload,
                        terminal_event=failed_event,
                        final_text=_normalize_text("".join(final_text_parts)),
                        usage=latest_usage,
                    )
            raise OpenyakReconnectRequiredError(self.thread_id, streamed.accepted.run_id)
        except (OpenyakResyncRequiredError, OpenyakReconnectRequiredError) as error:
            return await self._reconcile_buffered_run(
                initial_snapshot=streamed.snapshot,
                run_id=streamed.accepted.run_id,
                events=events,
                partial_text="".join(final_text_parts),
                partial_usage=latest_usage,
                error=error,
            )
        finally:
            await streamed.close()

    async def _reconcile_buffered_run(
        self,
        *,
        initial_snapshot: ThreadSnapshot,
        run_id: str,
        events: list[RunEvent],
        partial_text: str,
        partial_usage: TokenUsage | None,
        error: OpenyakResyncRequiredError | OpenyakReconnectRequiredError,
    ) -> RunResult:
        try:
            latest_snapshot = await self.read()
        except Exception as read_error:
            raise error from read_error
        if latest_snapshot.state.run_id is not None and latest_snapshot.state.run_id != run_id:
            raise OpenyakReconnectRequiredError(self.thread_id, run_id, latest_snapshot)
        if latest_snapshot.state.status == "running":
            raise OpenyakReconnectRequiredError(self.thread_id, run_id, latest_snapshot)

        appended_messages = latest_snapshot.session.messages[
            len(initial_snapshot.session.messages) :
        ]
        parts = _run_result_parts(
            final_text=_normalize_text(partial_text)
            or _normalize_text(messages_to_text(appended_messages)),
            usage=partial_usage or sum_assistant_usage(appended_messages),
            snapshot=latest_snapshot,
        )
        if latest_snapshot.state.status == "idle":
            return CompletedRunResult(
                thread_id=self.thread_id,
                run_id=run_id,
                events=events,
                recovered_from_snapshot=True,
                final_text=parts.final_text,
                usage=parts.usage,
                snapshot=parts.snapshot,
                terminal_event=None,
            )
        if (
            latest_snapshot.state.status == "awaiting_user_input"
            and latest_snapshot.state.pending_user_input is not None
        ):
            return AwaitingUserInputRunResult(
                thread_id=self.thread_id,
                run_id=run_id,
                events=events,
                recovered_from_snapshot=True,
                pending_user_input=latest_snapshot.state.pending_user_input,
                final_text=parts.final_text,
                usage=parts.usage,
                snapshot=parts.snapshot,
                terminal_event=None,
            )
        if latest_snapshot.state.status == "interrupted":
            return InterruptedRunResult(
                thread_id=self.thread_id,
                run_id=run_id,
                events=events,
                recovered_from_snapshot=True,
                recovery_note=latest_snapshot.state.recovery_note,
                final_text=parts.final_text,
                usage=parts.usage,
                snapshot=parts.snapshot,
                terminal_event=None,
            )
        raise OpenyakReconnectRequiredError(self.thread_id, run_id, latest_snapshot)

    async def _request_json(
        self,
        method: str,
        path: str,
        payload: dict[str, JsonValue],
        parser: Callable[[object], ParseResultT],
    ) -> ParseResultT:
        return await self._client._request_json(method, path, json_body=payload, parser=parser)


def _create_thread_body(options: CreateThreadOptions) -> dict[str, JsonValue]:
    body: dict[str, JsonValue] = {}
    if options.cwd is not None:
        body["cwd"] = options.cwd
    if options.model is not None:
        body["model"] = options.model
    if options.permission_mode is not None:
        body["permission_mode"] = options.permission_mode
    if options.allowed_tools is not None:
        body["allowed_tools"] = options.allowed_tools
    return body


def _quote(thread_id: str) -> str:
    return quote(thread_id, safe="")


def _iter_decoded_thread_events(response: httpx.Response) -> Iterator[ThreadEventAny]:
    for frame in iter_sse_frames(response.iter_lines()):
        yield _parse_thread_event_json(frame.data)


async def _aiter_decoded_thread_events(
    response: httpx.Response,
) -> AsyncIterator[ThreadEventAny]:
    async for frame in aiter_sse_frames(response.aiter_lines()):
        yield _parse_thread_event_json(frame.data)


def _parse_thread_event_json(data: str) -> ThreadEventAny:
    try:
        payload = json.loads(data)
    except json.JSONDecodeError as error:
        raise OpenyakProtocolError("failed to parse thread event JSON") from error
    return parse_thread_event(payload)


def _raise_for_error(response: httpx.Response) -> None:
    if response.is_error:
        raise _response_error(response)


async def _araise_for_error(response: httpx.Response) -> None:
    if response.is_error:
        raise _response_error(response)


def _response_error(response: httpx.Response) -> OpenyakApiError | OpenyakProtocolError:
    try:
        envelope = parse_api_error_envelope(response.json())
    except Exception:
        return OpenyakProtocolError(
            f"unexpected {response.status_code} response from {response.url}"
        )
    return OpenyakApiError(
        status=response.status_code,
        code=envelope.code,
        message=envelope.message,
        details=envelope.details,
    )


def _normalize_text(value: str) -> str | None:
    return value if value else None


def _run_result_parts(
    *,
    final_text: str | None,
    usage: TokenUsage | None,
    snapshot: ThreadSnapshot | None = None,
) -> _RunResultParts:
    return _RunResultParts(final_text=final_text, usage=usage, snapshot=snapshot)

from __future__ import annotations

from collections.abc import AsyncIterator, Callable, Iterator
from dataclasses import dataclass, field
from typing import Any, Generic, Literal, TypeAlias, TypeVar, cast

from ._errors import OpenyakCompatibilityError, OpenyakProtocolError

SUPPORTED_PROTOCOL_VERSION = "v1"
ProtocolVersion: TypeAlias = Literal["v1"]
PermissionMode: TypeAlias = Literal["read-only", "workspace-write", "danger-full-access"]
JsonValue: TypeAlias = Any


@dataclass(frozen=True, slots=True)
class TokenUsage:
    input_tokens: int
    output_tokens: int
    cache_creation_input_tokens: int
    cache_read_input_tokens: int


@dataclass(frozen=True, slots=True)
class TextBlock:
    type: Literal["text"]
    text: str


@dataclass(frozen=True, slots=True)
class ToolUseBlock:
    type: Literal["tool_use"]
    id: str
    name: str
    input: str


@dataclass(frozen=True, slots=True)
class ToolResultBlock:
    type: Literal["tool_result"]
    tool_use_id: str
    tool_name: str
    output: str
    is_error: bool


@dataclass(frozen=True, slots=True)
class UserInputRequestBlock:
    type: Literal["user_input_request"]
    request_id: str
    prompt: str
    options: list[str]
    allow_freeform: bool


@dataclass(frozen=True, slots=True)
class UserInputResponseBlock:
    type: Literal["user_input_response"]
    request_id: str
    content: str
    selected_option: str | None = None


ContentBlock: TypeAlias = (
    TextBlock
    | ToolUseBlock
    | ToolResultBlock
    | UserInputRequestBlock
    | UserInputResponseBlock
)


@dataclass(frozen=True, slots=True)
class ConversationMessage:
    role: Literal["system", "user", "assistant", "tool"]
    blocks: list[ContentBlock]
    usage: TokenUsage | None = None


@dataclass(frozen=True, slots=True)
class SessionTelemetry:
    compacted_usage: TokenUsage
    compacted_turns: int
    accounting_status: Literal["complete", "partial_legacy_compaction"] | None = None


@dataclass(frozen=True, slots=True)
class SessionSnapshot:
    version: int
    messages: list[ConversationMessage]
    telemetry: SessionTelemetry | None = None


@dataclass(frozen=True, slots=True)
class ThreadConfigSnapshot:
    cwd: str
    model: str
    permission_mode: PermissionMode
    allowed_tools: list[str]


@dataclass(frozen=True, slots=True)
class LifecycleContractSnapshot:
    truth_layer: str
    operator_plane: str
    persistence: str


@dataclass(frozen=True, slots=True)
class ThreadContractSnapshot:
    truth_layer: str
    operator_plane: str
    persistence: str
    attach_api: str


@dataclass(frozen=True, slots=True)
class RecoveryGuidanceSnapshot:
    failure_kind: str
    recovery_kind: str
    recommended_actions: list[str]


@dataclass(frozen=True, slots=True)
class LifecycleStateSnapshot:
    status: str
    failure_kind: str | None = None
    recovery: RecoveryGuidanceSnapshot | None = None


@dataclass(frozen=True, slots=True)
class UserInputRequestPayload:
    request_id: str
    prompt: str
    options: list[str]
    allow_freeform: bool


@dataclass(frozen=True, slots=True)
class ThreadStateSnapshot:
    status: Literal["idle", "running", "awaiting_user_input", "interrupted"]
    lifecycle: LifecycleStateSnapshot | None = None
    run_id: str | None = None
    pending_user_input: UserInputRequestPayload | None = None
    recovery_note: str | None = None
    recovery: RecoveryGuidanceSnapshot | None = None


@dataclass(frozen=True, slots=True)
class ThreadSnapshot:
    protocol_version: ProtocolVersion
    contract: ThreadContractSnapshot | None
    thread_id: str
    created_at: int
    updated_at: int
    state: ThreadStateSnapshot
    config: ThreadConfigSnapshot
    session: SessionSnapshot


@dataclass(frozen=True, slots=True)
class ThreadSummary:
    contract: ThreadContractSnapshot | None
    thread_id: str
    created_at: int
    updated_at: int
    state: ThreadStateSnapshot
    message_count: int


@dataclass(frozen=True, slots=True)
class ListThreadsResponse:
    protocol_version: ProtocolVersion
    threads: list[ThreadSummary]


@dataclass(frozen=True, slots=True)
class TurnAcceptedResponse:
    protocol_version: ProtocolVersion
    contract: ThreadContractSnapshot | None
    thread_id: str
    run_id: str
    lifecycle: LifecycleStateSnapshot | None
    status: Literal["accepted"]


@dataclass(frozen=True, slots=True)
class UserInputAcceptedResponse:
    protocol_version: ProtocolVersion
    contract: ThreadContractSnapshot | None
    thread_id: str
    run_id: str
    request_id: str
    lifecycle: LifecycleStateSnapshot | None
    status: Literal["accepted"]


AcceptedResponse: TypeAlias = TurnAcceptedResponse | UserInputAcceptedResponse


@dataclass(frozen=True, slots=True)
class ApiErrorEnvelope:
    code: str
    message: str
    details: JsonValue | None = None


@dataclass(frozen=True, slots=True)
class CreateThreadOptions:
    cwd: str | None = None
    model: str | None = None
    permission_mode: PermissionMode | None = None
    allowed_tools: list[str] | None = None


@dataclass(frozen=True, slots=True)
class SubmitUserInputOptions:
    request_id: str
    content: str
    selected_option: str | None = None


@dataclass(frozen=True, slots=True)
class RunStartedPayload:
    kind: str
    message: str
    status: Literal["running"]


@dataclass(frozen=True, slots=True)
class AssistantTextDeltaPayload:
    text: str


@dataclass(frozen=True, slots=True)
class AssistantToolUsePayload:
    id: str
    name: str
    input: JsonValue


@dataclass(frozen=True, slots=True)
class AssistantToolResultPayload:
    tool_use_id: str
    tool_name: str
    output: str
    is_error: bool


@dataclass(frozen=True, slots=True)
class AssistantMessageStopPayload:
    values: dict[str, JsonValue] = field(default_factory=dict)


@dataclass(frozen=True, slots=True)
class UserInputSubmittedPayload:
    request_id: str
    content: str
    selected_option: str | None = None


@dataclass(frozen=True, slots=True)
class RunCompletedPayload:
    iterations: int
    assistant_message_count: int
    tool_result_count: int
    cumulative_usage: TokenUsage


@dataclass(frozen=True, slots=True)
class RunFailedPayload:
    code: str
    message: str


@dataclass(frozen=True, slots=True)
class ThreadResyncRequiredPayload:
    skipped: int
    snapshot: ThreadSnapshot


PayloadT = TypeVar("PayloadT")


@dataclass(frozen=True, slots=True)
class ThreadEvent(Generic[PayloadT]):
    protocol_version: ProtocolVersion
    thread_id: str
    sequence: int
    timestamp_ms: int
    type: str
    payload: PayloadT
    run_id: str | None = None


ThreadSnapshotEvent: TypeAlias = ThreadEvent[ThreadSnapshot]
RunStartedEvent: TypeAlias = ThreadEvent[RunStartedPayload]
AssistantTextDeltaEvent: TypeAlias = ThreadEvent[AssistantTextDeltaPayload]
AssistantToolUseEvent: TypeAlias = ThreadEvent[AssistantToolUsePayload]
AssistantToolResultEvent: TypeAlias = ThreadEvent[AssistantToolResultPayload]
AssistantRequestUserInputEvent: TypeAlias = ThreadEvent[UserInputRequestPayload]
AssistantUsageEvent: TypeAlias = ThreadEvent[TokenUsage]
AssistantMessageStopEvent: TypeAlias = ThreadEvent[AssistantMessageStopPayload]
UserInputSubmittedEvent: TypeAlias = ThreadEvent[UserInputSubmittedPayload]
RunCompletedEvent: TypeAlias = ThreadEvent[RunCompletedPayload]
RunWaitingUserInputEvent: TypeAlias = ThreadEvent[UserInputRequestPayload]
RunFailedEvent: TypeAlias = ThreadEvent[RunFailedPayload]
ThreadResyncRequiredEvent: TypeAlias = ThreadEvent[ThreadResyncRequiredPayload]

RunEvent: TypeAlias = (
    RunStartedEvent
    | AssistantTextDeltaEvent
    | AssistantToolUseEvent
    | AssistantToolResultEvent
    | AssistantRequestUserInputEvent
    | AssistantUsageEvent
    | AssistantMessageStopEvent
    | UserInputSubmittedEvent
    | RunCompletedEvent
    | RunWaitingUserInputEvent
    | RunFailedEvent
)
RunTerminalEvent: TypeAlias = RunCompletedEvent | RunWaitingUserInputEvent | RunFailedEvent
ThreadEventAny: TypeAlias = ThreadSnapshotEvent | RunEvent | ThreadResyncRequiredEvent


@dataclass(slots=True)
class RunStreamedResult:
    snapshot: ThreadSnapshot
    accepted: AcceptedResponse
    events: Iterator[RunEvent]
    _close: Callable[[], None] = field(repr=False)

    def close(self) -> None:
        self._close()

    def __enter__(self) -> RunStreamedResult:
        return self

    def __exit__(self, _exc_type: object, _exc: object, _tb: object) -> None:
        self.close()


@dataclass(slots=True)
class AsyncRunStreamedResult:
    snapshot: ThreadSnapshot
    accepted: AcceptedResponse
    events: AsyncIterator[RunEvent]
    _close: Callable[[], Any] = field(repr=False)

    async def close(self) -> None:
        await self._close()

    async def __aenter__(self) -> AsyncRunStreamedResult:
        return self

    async def __aexit__(self, _exc_type: object, _exc: object, _tb: object) -> None:
        await self.close()


@dataclass(slots=True, kw_only=True)
class CompletedRunResult:
    thread_id: str
    run_id: str
    events: list[RunEvent]
    recovered_from_snapshot: bool
    final_text: str | None = None
    usage: TokenUsage | None = None
    snapshot: ThreadSnapshot | None = None
    terminal_event: RunCompletedEvent | None = None
    status: Literal["completed"] = "completed"


@dataclass(slots=True, kw_only=True)
class AwaitingUserInputRunResult:
    thread_id: str
    run_id: str
    events: list[RunEvent]
    recovered_from_snapshot: bool
    pending_user_input: UserInputRequestPayload
    final_text: str | None = None
    usage: TokenUsage | None = None
    snapshot: ThreadSnapshot | None = None
    terminal_event: RunWaitingUserInputEvent | None = None
    status: Literal["awaiting_user_input"] = "awaiting_user_input"


@dataclass(slots=True, kw_only=True)
class FailedRunResult:
    thread_id: str
    run_id: str
    events: list[RunEvent]
    recovered_from_snapshot: bool
    error: RunFailedPayload
    terminal_event: RunFailedEvent
    final_text: str | None = None
    usage: TokenUsage | None = None
    snapshot: ThreadSnapshot | None = None
    status: Literal["failed"] = "failed"


@dataclass(slots=True, kw_only=True)
class InterruptedRunResult:
    thread_id: str
    run_id: str
    events: list[RunEvent]
    recovered_from_snapshot: bool
    recovery_note: str | None = None
    final_text: str | None = None
    usage: TokenUsage | None = None
    snapshot: ThreadSnapshot | None = None
    terminal_event: None = None
    status: Literal["interrupted"] = "interrupted"


RunResult: TypeAlias = (
    CompletedRunResult
    | AwaitingUserInputRunResult
    | FailedRunResult
    | InterruptedRunResult
)


def parse_api_error_envelope(value: object, context: str = "api error") -> ApiErrorEnvelope:
    record = _as_mapping(value, context)
    return ApiErrorEnvelope(
        code=_require_str(record, "code", context),
        message=_require_str(record, "message", context),
        details=record.get("details"),
    )


def parse_thread_snapshot(value: object, context: str = "thread snapshot") -> ThreadSnapshot:
    record = _as_mapping(value, context)
    return ThreadSnapshot(
        protocol_version=_parse_protocol_version(record, context),
        contract=(
            parse_thread_contract_snapshot(record.get("contract"), f"{context}.contract")
            if record.get("contract") is not None
            else None
        ),
        thread_id=_require_str(record, "thread_id", context),
        created_at=_require_int(record, "created_at", context),
        updated_at=_require_int(record, "updated_at", context),
        state=parse_thread_state_snapshot(record.get("state"), f"{context}.state"),
        config=parse_thread_config_snapshot(record.get("config"), f"{context}.config"),
        session=parse_session_snapshot(record.get("session"), f"{context}.session"),
    )


def parse_list_threads_response(
    value: object, context: str = "list threads response"
) -> ListThreadsResponse:
    record = _as_mapping(value, context)
    threads = _require_list(record, "threads", context)
    return ListThreadsResponse(
        protocol_version=_parse_protocol_version(record, context),
        threads=[
            parse_thread_summary(item, f"{context}.threads[{index}]")
            for index, item in enumerate(threads)
        ],
    )


def parse_turn_accepted_response(
    value: object, context: str = "turn accepted response"
) -> TurnAcceptedResponse:
    record = _as_mapping(value, context)
    return TurnAcceptedResponse(
        protocol_version=_parse_protocol_version(record, context),
        contract=(
            parse_thread_contract_snapshot(record.get("contract"), f"{context}.contract")
            if record.get("contract") is not None
            else None
        ),
        thread_id=_require_str(record, "thread_id", context),
        run_id=_require_str(record, "run_id", context),
        lifecycle=(
            parse_lifecycle_state_snapshot(record.get("lifecycle"), f"{context}.lifecycle")
            if record.get("lifecycle") is not None
            else None
        ),
        status=_require_accepted(record, context),
    )


def parse_user_input_accepted_response(
    value: object, context: str = "user-input accepted response"
) -> UserInputAcceptedResponse:
    record = _as_mapping(value, context)
    return UserInputAcceptedResponse(
        protocol_version=_parse_protocol_version(record, context),
        contract=(
            parse_thread_contract_snapshot(record.get("contract"), f"{context}.contract")
            if record.get("contract") is not None
            else None
        ),
        thread_id=_require_str(record, "thread_id", context),
        run_id=_require_str(record, "run_id", context),
        request_id=_require_str(record, "request_id", context),
        lifecycle=(
            parse_lifecycle_state_snapshot(record.get("lifecycle"), f"{context}.lifecycle")
            if record.get("lifecycle") is not None
            else None
        ),
        status=_require_accepted(record, context),
    )


def parse_thread_event(value: object, context: str = "thread event") -> ThreadEventAny:
    record = _as_mapping(value, context)
    event_type = _require_str(record, "type", context)
    protocol_version = _parse_protocol_version(record, context)
    thread_id = _require_str(record, "thread_id", context)
    run_id = _optional_str(record, "run_id", context)
    sequence = _require_int(record, "sequence", context)
    timestamp_ms = _require_int(record, "timestamp_ms", context)
    payload = record.get("payload")
    if event_type == "thread.snapshot":
        return _thread_event(
            protocol_version=protocol_version,
            thread_id=thread_id,
            run_id=run_id,
            sequence=sequence,
            timestamp_ms=timestamp_ms,
            event_type=event_type,
            payload=parse_thread_snapshot(payload, f"{context}.payload"),
        )
    if event_type == "run.started":
        payload_record = _as_mapping(payload, f"{context}.payload")
        return _thread_event(
            protocol_version=protocol_version,
            thread_id=thread_id,
            run_id=run_id,
            sequence=sequence,
            timestamp_ms=timestamp_ms,
            event_type=event_type,
            payload=RunStartedPayload(
                kind=_require_str(payload_record, "kind", f"{context}.payload"),
                message=_require_str(payload_record, "message", f"{context}.payload"),
                status=_require_running(payload_record, f"{context}.payload"),
            ),
        )
    if event_type == "assistant.text.delta":
        payload_record = _as_mapping(payload, f"{context}.payload")
        return _thread_event(
            protocol_version=protocol_version,
            thread_id=thread_id,
            run_id=run_id,
            sequence=sequence,
            timestamp_ms=timestamp_ms,
            event_type=event_type,
            payload=AssistantTextDeltaPayload(
                text=_require_str(payload_record, "text", f"{context}.payload")
            ),
        )
    if event_type == "assistant.tool_use":
        payload_record = _as_mapping(payload, f"{context}.payload")
        return _thread_event(
            protocol_version=protocol_version,
            thread_id=thread_id,
            run_id=run_id,
            sequence=sequence,
            timestamp_ms=timestamp_ms,
            event_type=event_type,
            payload=AssistantToolUsePayload(
                id=_require_str(payload_record, "id", f"{context}.payload"),
                name=_require_str(payload_record, "name", f"{context}.payload"),
                input=payload_record.get("input"),
            ),
        )
    if event_type == "assistant.tool_result":
        payload_record = _as_mapping(payload, f"{context}.payload")
        return _thread_event(
            protocol_version=protocol_version,
            thread_id=thread_id,
            run_id=run_id,
            sequence=sequence,
            timestamp_ms=timestamp_ms,
            event_type=event_type,
            payload=AssistantToolResultPayload(
                tool_use_id=_require_str(payload_record, "tool_use_id", f"{context}.payload"),
                tool_name=_require_str(payload_record, "tool_name", f"{context}.payload"),
                output=_require_str(payload_record, "output", f"{context}.payload"),
                is_error=_require_bool(payload_record, "is_error", f"{context}.payload"),
            ),
        )
    if event_type == "assistant.request_user_input":
        return _thread_event(
            protocol_version=protocol_version,
            thread_id=thread_id,
            run_id=run_id,
            sequence=sequence,
            timestamp_ms=timestamp_ms,
            event_type=event_type,
            payload=parse_user_input_request_payload(payload, f"{context}.payload"),
        )
    if event_type == "assistant.usage":
        return _thread_event(
            protocol_version=protocol_version,
            thread_id=thread_id,
            run_id=run_id,
            sequence=sequence,
            timestamp_ms=timestamp_ms,
            event_type=event_type,
            payload=parse_token_usage(payload, f"{context}.payload"),
        )
    if event_type == "assistant.message_stop":
        payload_record = _as_mapping(payload, f"{context}.payload")
        return _thread_event(
            protocol_version=protocol_version,
            thread_id=thread_id,
            run_id=run_id,
            sequence=sequence,
            timestamp_ms=timestamp_ms,
            event_type=event_type,
            payload=AssistantMessageStopPayload(values=dict(payload_record)),
        )
    if event_type == "user_input.submitted":
        payload_record = _as_mapping(payload, f"{context}.payload")
        return _thread_event(
            protocol_version=protocol_version,
            thread_id=thread_id,
            run_id=run_id,
            sequence=sequence,
            timestamp_ms=timestamp_ms,
            event_type=event_type,
            payload=UserInputSubmittedPayload(
                request_id=_require_str(payload_record, "request_id", f"{context}.payload"),
                content=_require_str(payload_record, "content", f"{context}.payload"),
                selected_option=_optional_str(
                    payload_record,
                    "selected_option",
                    f"{context}.payload",
                ),
            ),
        )
    if event_type == "run.completed":
        payload_record = _as_mapping(payload, f"{context}.payload")
        return _thread_event(
            protocol_version=protocol_version,
            thread_id=thread_id,
            run_id=run_id,
            sequence=sequence,
            timestamp_ms=timestamp_ms,
            event_type=event_type,
            payload=RunCompletedPayload(
                iterations=_require_int(payload_record, "iterations", f"{context}.payload"),
                assistant_message_count=_require_int(
                    payload_record,
                    "assistant_message_count",
                    f"{context}.payload",
                ),
                tool_result_count=_require_int(
                    payload_record,
                    "tool_result_count",
                    f"{context}.payload",
                ),
                cumulative_usage=parse_token_usage(
                    payload_record.get("cumulative_usage"),
                    f"{context}.payload.cumulative_usage",
                ),
            ),
        )
    if event_type == "run.waiting_user_input":
        return _thread_event(
            protocol_version=protocol_version,
            thread_id=thread_id,
            run_id=run_id,
            sequence=sequence,
            timestamp_ms=timestamp_ms,
            event_type=event_type,
            payload=parse_user_input_request_payload(payload, f"{context}.payload"),
        )
    if event_type == "run.failed":
        payload_record = _as_mapping(payload, f"{context}.payload")
        return _thread_event(
            protocol_version=protocol_version,
            thread_id=thread_id,
            run_id=run_id,
            sequence=sequence,
            timestamp_ms=timestamp_ms,
            event_type=event_type,
            payload=RunFailedPayload(
                code=_require_str(payload_record, "code", f"{context}.payload"),
                message=_require_str(payload_record, "message", f"{context}.payload"),
            ),
        )
    if event_type == "thread.resync_required":
        payload_record = _as_mapping(payload, f"{context}.payload")
        return _thread_event(
            protocol_version=protocol_version,
            thread_id=thread_id,
            run_id=run_id,
            sequence=sequence,
            timestamp_ms=timestamp_ms,
            event_type=event_type,
            payload=ThreadResyncRequiredPayload(
                skipped=_require_int(payload_record, "skipped", f"{context}.payload"),
                snapshot=parse_thread_snapshot(
                    payload_record.get("snapshot"),
                    f"{context}.payload.snapshot",
                ),
            ),
        )
    raise OpenyakCompatibilityError(
        f"unsupported event type {event_type!r} for the current Python alpha",
        expected=SUPPORTED_PROTOCOL_VERSION,
        received=_protocol_version_from_mapping(record),
    )


def parse_thread_summary(value: object, context: str = "thread summary") -> ThreadSummary:
    record = _as_mapping(value, context)
    return ThreadSummary(
        contract=(
            parse_thread_contract_snapshot(record.get("contract"), f"{context}.contract")
            if record.get("contract") is not None
            else None
        ),
        thread_id=_require_str(record, "thread_id", context),
        created_at=_require_int(record, "created_at", context),
        updated_at=_require_int(record, "updated_at", context),
        state=parse_thread_state_snapshot(record.get("state"), f"{context}.state"),
        message_count=_require_int(record, "message_count", context),
    )


def parse_thread_state_snapshot(
    value: object, context: str = "thread state snapshot"
) -> ThreadStateSnapshot:
    record = _as_mapping(value, context)
    status = _require_str(record, "status", context)
    if status not in {"idle", "running", "awaiting_user_input", "interrupted"}:
        raise OpenyakProtocolError(f"{context} has unsupported status {status!r}")
    typed_status = cast(
        Literal["idle", "running", "awaiting_user_input", "interrupted"],
        status,
    )
    pending_user_input = record.get("pending_user_input")
    lifecycle = record.get("lifecycle")
    recovery = record.get("recovery")
    return ThreadStateSnapshot(
        status=typed_status,
        lifecycle=(
            parse_lifecycle_state_snapshot(lifecycle, f"{context}.lifecycle")
            if lifecycle is not None
            else None
        ),
        run_id=_optional_str(record, "run_id", context),
        pending_user_input=(
            parse_user_input_request_payload(
                pending_user_input,
                f"{context}.pending_user_input",
            )
            if pending_user_input is not None
            else None
        ),
        recovery_note=_optional_str(record, "recovery_note", context),
        recovery=(
            parse_recovery_guidance_snapshot(recovery, f"{context}.recovery")
            if recovery is not None
            else None
        ),
    )


def parse_thread_contract_snapshot(
    value: object, context: str = "thread contract snapshot"
) -> ThreadContractSnapshot:
    record = _as_mapping(value, context)
    return ThreadContractSnapshot(
        truth_layer=_require_str(record, "truth_layer", context),
        operator_plane=_require_str(record, "operator_plane", context),
        persistence=_require_str(record, "persistence", context),
        attach_api=_require_str(record, "attach_api", context),
    )


def parse_recovery_guidance_snapshot(
    value: object, context: str = "recovery guidance snapshot"
) -> RecoveryGuidanceSnapshot:
    record = _as_mapping(value, context)
    return RecoveryGuidanceSnapshot(
        failure_kind=_require_str(record, "failure_kind", context),
        recovery_kind=_require_str(record, "recovery_kind", context),
        recommended_actions=_require_list_of_str(record, "recommended_actions", context),
    )


def parse_lifecycle_state_snapshot(
    value: object, context: str = "lifecycle state snapshot"
) -> LifecycleStateSnapshot:
    record = _as_mapping(value, context)
    recovery = record.get("recovery")
    return LifecycleStateSnapshot(
        status=_require_str(record, "status", context),
        failure_kind=_optional_str(record, "failure_kind", context),
        recovery=(
            parse_recovery_guidance_snapshot(recovery, f"{context}.recovery")
            if recovery is not None
            else None
        ),
    )


def parse_thread_config_snapshot(
    value: object, context: str = "thread config snapshot"
) -> ThreadConfigSnapshot:
    record = _as_mapping(value, context)
    permission_mode = _require_str(record, "permission_mode", context)
    if permission_mode not in {"read-only", "workspace-write", "danger-full-access"}:
        raise OpenyakProtocolError(f"{context} has unsupported permission_mode {permission_mode!r}")
    typed_permission_mode = cast(PermissionMode, permission_mode)
    return ThreadConfigSnapshot(
        cwd=_require_str(record, "cwd", context),
        model=_require_str(record, "model", context),
        permission_mode=typed_permission_mode,
        allowed_tools=_require_list_of_str(record, "allowed_tools", context),
    )


def parse_session_snapshot(value: object, context: str = "session snapshot") -> SessionSnapshot:
    record = _as_mapping(value, context)
    messages = _require_list(record, "messages", context)
    telemetry = record.get("telemetry")
    return SessionSnapshot(
        version=_require_int(record, "version", context),
        messages=[
            parse_conversation_message(item, f"{context}.messages[{index}]")
            for index, item in enumerate(messages)
        ],
        telemetry=(
            parse_session_telemetry(telemetry, f"{context}.telemetry")
            if telemetry is not None
            else None
        ),
    )


def parse_conversation_message(
    value: object, context: str = "conversation message"
) -> ConversationMessage:
    record = _as_mapping(value, context)
    blocks = _require_list(record, "blocks", context)
    usage = record.get("usage")
    return ConversationMessage(
        role=_require_message_role(record, context),
        blocks=[
            parse_content_block(item, f"{context}.blocks[{index}]")
            for index, item in enumerate(blocks)
        ],
        usage=parse_token_usage(usage, f"{context}.usage") if usage is not None else None,
    )


def parse_content_block(value: object, context: str = "content block") -> ContentBlock:
    record = _as_mapping(value, context)
    block_type = _require_str(record, "type", context)
    if block_type == "text":
        return TextBlock(type="text", text=_require_str(record, "text", context))
    if block_type == "tool_use":
        return ToolUseBlock(
            type="tool_use",
            id=_require_str(record, "id", context),
            name=_require_str(record, "name", context),
            input=_require_str(record, "input", context),
        )
    if block_type == "tool_result":
        return ToolResultBlock(
            type="tool_result",
            tool_use_id=_require_str(record, "tool_use_id", context),
            tool_name=_require_str(record, "tool_name", context),
            output=_require_str(record, "output", context),
            is_error=_require_bool(record, "is_error", context),
        )
    if block_type == "user_input_request":
        return UserInputRequestBlock(
            type="user_input_request",
            request_id=_require_str(record, "request_id", context),
            prompt=_require_str(record, "prompt", context),
            options=_require_list_of_str(record, "options", context),
            allow_freeform=_require_bool(record, "allow_freeform", context),
        )
    if block_type == "user_input_response":
        return UserInputResponseBlock(
            type="user_input_response",
            request_id=_require_str(record, "request_id", context),
            content=_require_str(record, "content", context),
            selected_option=_optional_str(record, "selected_option", context),
        )
    raise OpenyakProtocolError(f"{context} has unsupported content block type {block_type!r}")


def parse_session_telemetry(value: object, context: str = "session telemetry") -> SessionTelemetry:
    record = _as_mapping(value, context)
    accounting_status = _optional_str(record, "accounting_status", context)
    if accounting_status is not None and accounting_status not in {
        "complete",
        "partial_legacy_compaction",
    }:
        raise OpenyakProtocolError(
            f"{context} has unsupported accounting_status {accounting_status!r}"
        )
    typed_accounting_status = cast(
        Literal["complete", "partial_legacy_compaction"] | None,
        accounting_status,
    )
    return SessionTelemetry(
        compacted_usage=parse_token_usage(
            record.get("compacted_usage"),
            f"{context}.compacted_usage",
        ),
        compacted_turns=_require_int(record, "compacted_turns", context),
        accounting_status=typed_accounting_status,
    )


def parse_user_input_request_payload(
    value: object, context: str = "user-input request payload"
) -> UserInputRequestPayload:
    record = _as_mapping(value, context)
    return UserInputRequestPayload(
        request_id=_require_str(record, "request_id", context),
        prompt=_require_str(record, "prompt", context),
        options=_require_list_of_str(record, "options", context),
        allow_freeform=_require_bool(record, "allow_freeform", context),
    )


def parse_token_usage(value: object, context: str = "token usage") -> TokenUsage:
    record = _as_mapping(value, context)
    return TokenUsage(
        input_tokens=_require_int(record, "input_tokens", context),
        output_tokens=_require_int(record, "output_tokens", context),
        cache_creation_input_tokens=_require_int(
            record,
            "cache_creation_input_tokens",
            context,
        ),
        cache_read_input_tokens=_require_int(record, "cache_read_input_tokens", context),
    )


def messages_to_text(messages: list[ConversationMessage]) -> str:
    return "".join(
        block.text
        for message in messages
        if message.role == "assistant"
        for block in message.blocks
        if isinstance(block, TextBlock)
    )


def sum_assistant_usage(messages: list[ConversationMessage]) -> TokenUsage | None:
    usages = [
        message.usage
        for message in messages
        if message.role == "assistant" and message.usage
    ]
    if not usages:
        return None
    total = TokenUsage(
        input_tokens=0,
        output_tokens=0,
        cache_creation_input_tokens=0,
        cache_read_input_tokens=0,
    )
    for usage in usages:
        total = TokenUsage(
            input_tokens=total.input_tokens + usage.input_tokens,
            output_tokens=total.output_tokens + usage.output_tokens,
            cache_creation_input_tokens=(
                total.cache_creation_input_tokens + usage.cache_creation_input_tokens
            ),
            cache_read_input_tokens=(
                total.cache_read_input_tokens + usage.cache_read_input_tokens
            ),
        )
    return total


def is_terminal_run_event(event: RunEvent) -> bool:
    return event.type in {"run.completed", "run.waiting_user_input", "run.failed"}


def assert_supported_protocol_version(received: str | None, context: str) -> ProtocolVersion:
    if received != SUPPORTED_PROTOCOL_VERSION:
        raise OpenyakCompatibilityError(
            (
                f"{context} uses unsupported protocol_version "
                f"{received!r}; expected {SUPPORTED_PROTOCOL_VERSION}"
            ),
            expected=SUPPORTED_PROTOCOL_VERSION,
            received=received,
        )
    return cast(ProtocolVersion, received)


def _thread_event(
    *,
    protocol_version: ProtocolVersion,
    thread_id: str,
    run_id: str | None,
    sequence: int,
    timestamp_ms: int,
    event_type: str,
    payload: PayloadT,
) -> ThreadEvent[PayloadT]:
    return ThreadEvent(
        protocol_version=protocol_version,
        thread_id=thread_id,
        run_id=run_id,
        sequence=sequence,
        timestamp_ms=timestamp_ms,
        type=event_type,
        payload=payload,
    )


def _parse_protocol_version(record: dict[str, object], context: str) -> ProtocolVersion:
    return assert_supported_protocol_version(_protocol_version_from_mapping(record), context)


def _protocol_version_from_mapping(record: dict[str, object]) -> str | None:
    value = record.get("protocol_version")
    return value if isinstance(value, str) else None


def _as_mapping(value: object, context: str) -> dict[str, object]:
    if not isinstance(value, dict):
        raise OpenyakProtocolError(f"{context} must be a JSON object")
    return value


def _require_list(record: dict[str, object], field: str, context: str) -> list[object]:
    value = record.get(field)
    if not isinstance(value, list):
        raise OpenyakProtocolError(f"{context} is missing list field {field}")
    return value


def _require_list_of_str(record: dict[str, object], field: str, context: str) -> list[str]:
    values = _require_list(record, field, context)
    if not all(isinstance(value, str) for value in values):
        raise OpenyakProtocolError(f"{context}.{field} must be a list of strings")
    return [value for value in values if isinstance(value, str)]


def _require_str(record: dict[str, object], field: str, context: str) -> str:
    value = record.get(field)
    if not isinstance(value, str):
        raise OpenyakProtocolError(f"{context} is missing string field {field}")
    return value


def _optional_str(record: dict[str, object], field: str, context: str) -> str | None:
    value = record.get(field)
    if value is None:
        return None
    if not isinstance(value, str):
        raise OpenyakProtocolError(f"{context}.{field} must be a string when present")
    return value


def _require_bool(record: dict[str, object], field: str, context: str) -> bool:
    value = record.get(field)
    if not isinstance(value, bool):
        raise OpenyakProtocolError(f"{context} is missing boolean field {field}")
    return value


def _require_int(record: dict[str, object], field: str, context: str) -> int:
    value = record.get(field)
    if not isinstance(value, int) or isinstance(value, bool):
        raise OpenyakProtocolError(f"{context} is missing integer field {field}")
    return value


def _require_message_role(
    record: dict[str, object],
    context: str,
) -> Literal["system", "user", "assistant", "tool"]:
    value = _require_str(record, "role", context)
    if value not in {"system", "user", "assistant", "tool"}:
        raise OpenyakProtocolError(f"{context}.role has unsupported value {value!r}")
    return cast(Literal["system", "user", "assistant", "tool"], value)


def _require_accepted(record: dict[str, object], context: str) -> Literal["accepted"]:
    value = _require_str(record, "status", context)
    if value != "accepted":
        raise OpenyakProtocolError(f"{context}.status expected 'accepted', got {value!r}")
    return cast(Literal["accepted"], value)


def _require_running(record: dict[str, object], context: str) -> Literal["running"]:
    value = _require_str(record, "status", context)
    if value != "running":
        raise OpenyakProtocolError(f"{context}.status expected 'running', got {value!r}")
    return cast(Literal["running"], value)

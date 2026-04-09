from __future__ import annotations

import asyncio
import os
import shutil
import tempfile
import threading
import time
from urllib.parse import urlparse
from collections.abc import Iterator
from contextlib import contextmanager
from typing import cast

from openyak_sdk import AsyncOpenyakClient, OpenyakClient
from openyak_sdk._models import RunFailedEvent

from .helpers.harness import (
    ServerHarness,
    server_harness,
    start_mock_anthropic_service,
    start_openyak_server,
    start_openyak_server_in,
)

PROVIDER_ENV_KEYS = (
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_BASE_URL",
    "OPENAI_API_KEY",
    "OPENAI_BASE_URL",
    "XAI_API_KEY",
    "XAI_BASE_URL",
)


@contextmanager
def runtime_failure_server_harness() -> Iterator[ServerHarness]:
    mock = start_mock_anthropic_service()
    env = {key: value for key, value in os.environ.items() if key not in PROVIDER_ENV_KEYS}
    env["ANTHROPIC_BASE_URL"] = mock.base_url
    server = start_openyak_server(env)
    harness = ServerHarness(mock=mock, server=server)
    try:
        yield harness
    finally:
        harness.close()


def wait_for_thread_status(thread: object, expected: str, timeout_s: float = 5.0) -> None:
    deadline = time.monotonic() + timeout_s
    last_status = None
    while time.monotonic() < deadline:
        snapshot = thread.read()
        last_status = snapshot.state.status
        if last_status == expected:
            return
        time.sleep(0.05)
    raise AssertionError(f"thread did not reach {expected!r}; last status was {last_status!r}")


async def wait_for_thread_status_async(
    thread: object, expected: str, timeout_s: float = 5.0
) -> None:
    deadline = time.monotonic() + timeout_s
    last_status = None
    while time.monotonic() < deadline:
        snapshot = await thread.read()
        last_status = snapshot.state.status
        if last_status == expected:
            return
        await asyncio.sleep(0.05)
    raise AssertionError(f"thread did not reach {expected!r}; last status was {last_status!r}")


def test_attach_first_sync_client_can_create_list_get_and_stream_a_bash_run() -> None:
    with server_harness() as harness:
        with OpenyakClient(base_url=harness.server.base_url, timeout_s=30.0) as client:
            thread = client.create_thread(
                model="claude-sonnet-4-6",
                allowed_tools=["bash"],
            )

            assert thread.thread_id == "thread-1"
            assert thread.snapshot is not None
            assert thread.snapshot.state.status == "idle"

            listed = client.list_threads()
            assert len(listed.threads) == 1
            assert listed.threads[0].thread_id == thread.thread_id

            fetched = thread.read()
            assert fetched.thread_id == thread.thread_id
            assert fetched.config.allowed_tools == ["bash"]

            with thread.run_streamed("PARITY_SCENARIO:bash_stdout_roundtrip") as streamed:
                assert streamed.snapshot.thread_id == thread.thread_id
                assert streamed.accepted.run_id == "run-1"

                event_types: list[str] = []
                final_text_parts: list[str] = []
                saw_usage = False
                for event in streamed.events:
                    event_types.append(event.type)
                    if event.type == "assistant.text.delta":
                        final_text_parts.append(event.payload.text)
                    if event.type == "assistant.usage":
                        saw_usage = True

            assert event_types == [
                "run.started",
                "assistant.tool_use",
                "assistant.usage",
                "assistant.message_stop",
                "assistant.tool_result",
                "assistant.text.delta",
                "assistant.usage",
                "assistant.message_stop",
                "run.completed",
            ]
            assert "".join(final_text_parts).startswith("bash completed:")
            assert saw_usage is True

            completed = thread.read()
            assert completed.state.status == "idle"


def test_buffered_sync_run_preserves_runtime_failure_recovery_metadata() -> None:
    with runtime_failure_server_harness() as harness:
        with OpenyakClient(base_url=harness.server.base_url, timeout_s=30.0) as client:
            thread = client.create_thread(
                model="opus",
                allowed_tools=[],
            )

            failed = thread.run("provider bootstrap should fail")

            assert failed.status == "failed"
            assert failed.run_id == "run-1"
            assert failed.error.code == "runtime_error"
            assert failed.error.status == "failed"
            assert failed.error.lifecycle is not None
            assert failed.error.lifecycle.failure_kind == "daemon_thread_runtime_error"
            assert failed.error.lifecycle.recovery is not None
            assert failed.error.lifecycle.recovery.recovery_kind == "inspect_error_and_retry"
            assert failed.terminal_event is not None
            assert failed.terminal_event.payload.lifecycle is not None
            assert (
                failed.terminal_event.payload.lifecycle.recovery.recommended_actions[0]
                == "inspect the error message and latest /v1/threads snapshot"
            )

            completed = thread.read()
            assert completed.state.status == "idle"


def test_streamed_sync_run_surfaces_runtime_failure_recovery_metadata() -> None:
    with runtime_failure_server_harness() as harness:
        with OpenyakClient(base_url=harness.server.base_url, timeout_s=30.0) as client:
            thread = client.create_thread(
                model="opus",
                allowed_tools=[],
            )

            with thread.run_streamed("provider bootstrap should fail") as streamed:
                event_types: list[str] = []
                failed_event = None
                for event in streamed.events:
                    event_types.append(event.type)
                    if event.type == "run.failed":
                        failed_event = cast(RunFailedEvent, event)
                        break

            assert event_types == ["run.started", "run.failed"]
            assert failed_event is not None
            assert failed_event.payload.lifecycle is not None
            assert failed_event.payload.lifecycle.failure_kind == "daemon_thread_runtime_error"
            assert (
                failed_event.payload.lifecycle.recovery is not None
                and failed_event.payload.lifecycle.recovery.recovery_kind
                == "inspect_error_and_retry"
            )


def test_buffered_sync_run_recovers_interrupted_snapshot_truth_after_server_restart() -> None:
    mock = start_mock_anthropic_service()
    workspace = tempfile.mkdtemp(prefix="openyak-python-sdk-restart-")
    env = {
        **os.environ,
        "ANTHROPIC_API_KEY": "test-sdk-key",
        "ANTHROPIC_BASE_URL": mock.base_url,
    }
    server = start_openyak_server_in(workspace, env, cleanup_workspace=False)
    base_url = server.base_url
    bind = urlparse(base_url).netloc

    try:
        with OpenyakClient(base_url=base_url, timeout_s=30.0) as client:
            thread = client.create_thread(
                model="claude-sonnet-4-6",
                allowed_tools=["read_file"],
            )
            watcher = client.resume_thread(thread.thread_id)
            result_holder: dict[str, object] = {}

            def run_buffered() -> None:
                try:
                    result_holder["result"] = thread.run(
                        "PARITY_SCENARIO:delayed_request_user_input_roundtrip"
                    )
                except Exception as error:  # pragma: no cover - assertion surfaces below
                    result_holder["error"] = error

            runner = threading.Thread(target=run_buffered, daemon=True)
            runner.start()
            wait_for_thread_status(watcher, "running")

            server.close()
            server = start_openyak_server_in(
                workspace,
                env,
                bind=bind,
                cleanup_workspace=False,
            )

            runner.join(timeout=30)
            assert "error" not in result_holder, result_holder.get("error")
            result = result_holder.get("result")
            assert result is not None
            assert result.status == "interrupted"
            assert result.recovered_from_snapshot is True
            assert result.recovery_note is not None
            assert "restart or shutdown" in result.recovery_note
            assert result.snapshot is not None
            assert result.snapshot.state.lifecycle is not None
            assert (
                result.snapshot.state.lifecycle.failure_kind
                == "daemon_restart_interrupted_run"
            )
            assert result.snapshot.state.recovery is not None
            assert (
                result.snapshot.state.recovery.recovery_kind == "reattach_or_retry"
            )
    finally:
        server.close()
        mock.close()
        shutil.rmtree(workspace, ignore_errors=True)


def test_buffered_sync_run_returns_awaiting_user_input_and_resume_continues_the_same_run() -> None:
    with server_harness() as harness:
        with OpenyakClient(base_url=harness.server.base_url, timeout_s=30.0) as client:
            thread = client.create_thread(
                model="claude-sonnet-4-6",
                allowed_tools=["read_file"],
            )

            paused = thread.run("PARITY_SCENARIO:request_user_input_roundtrip")
            assert paused.status == "awaiting_user_input"
            assert paused.run_id == "run-1"
            assert paused.pending_user_input.request_id == "req-user-input-roundtrip"
            assert paused.recovered_from_snapshot is False

            resumed = thread.resume_user_input(
                request_id=paused.pending_user_input.request_id,
                content="feature",
                selected_option="feature",
            )

            assert resumed.status == "completed"
            assert resumed.run_id == paused.run_id
            assert resumed.final_text is not None
            assert "request-user-input completed: feature" in resumed.final_text
            assert resumed.usage is not None
            assert resumed.usage.input_tokens == 26
            assert resumed.recovered_from_snapshot is False

            completed = thread.read()
            assert completed.state.status == "idle"
            assert completed.state.pending_user_input is None


def test_attach_first_async_client_can_create_list_get_and_stream_a_bash_run() -> None:
    async def case() -> None:
        with server_harness() as harness:
            async with AsyncOpenyakClient(
                base_url=harness.server.base_url,
                timeout_s=30.0,
            ) as client:
                created = await client.create_thread(
                    model="claude-sonnet-4-6",
                    allowed_tools=["bash"],
                )
                thread = client.resume_thread(created.thread_id)

                listed = await client.list_threads()
                assert len(listed.threads) == 1
                assert listed.threads[0].thread_id == thread.thread_id

                fetched = await thread.read()
                assert fetched.thread_id == thread.thread_id
                assert fetched.config.allowed_tools == ["bash"]

                async with await thread.run_streamed(
                    "PARITY_SCENARIO:bash_stdout_roundtrip"
                ) as streamed:
                    event_types: list[str] = []
                    final_text_parts: list[str] = []
                    saw_usage = False
                    async for event in streamed.events:
                        event_types.append(event.type)
                        if event.type == "assistant.text.delta":
                            final_text_parts.append(event.payload.text)
                        if event.type == "assistant.usage":
                            saw_usage = True

                assert event_types == [
                    "run.started",
                    "assistant.tool_use",
                    "assistant.usage",
                    "assistant.message_stop",
                    "assistant.tool_result",
                    "assistant.text.delta",
                    "assistant.usage",
                    "assistant.message_stop",
                    "run.completed",
                ]
                assert "".join(final_text_parts).startswith("bash completed:")
                assert saw_usage is True

                completed = await thread.read()
                assert completed.state.status == "idle"

    asyncio.run(case())


def test_buffered_async_run_returns_awaiting_user_input_and_resume_continues_the_same_run() -> None:
    async def case() -> None:
        with server_harness() as harness:
            async with AsyncOpenyakClient(
                base_url=harness.server.base_url,
                timeout_s=30.0,
            ) as client:
                thread = await client.create_thread(
                    model="claude-sonnet-4-6",
                    allowed_tools=["read_file"],
                )

                paused = await thread.run("PARITY_SCENARIO:request_user_input_roundtrip")
                assert paused.status == "awaiting_user_input"
                assert paused.run_id == "run-1"
                assert paused.pending_user_input.request_id == "req-user-input-roundtrip"
                assert paused.recovered_from_snapshot is False

                resumed = await thread.resume_user_input(
                    request_id=paused.pending_user_input.request_id,
                    content="feature",
                    selected_option="feature",
                )

                assert resumed.status == "completed"
                assert resumed.run_id == paused.run_id
                assert resumed.final_text is not None
                assert "request-user-input completed: feature" in resumed.final_text
                assert resumed.usage is not None
                assert resumed.usage.input_tokens == 26

                completed = await thread.read()
                assert completed.state.status == "idle"
                assert completed.state.pending_user_input is None

    asyncio.run(case())


def test_buffered_async_run_preserves_runtime_failure_recovery_metadata() -> None:
    async def case() -> None:
        with runtime_failure_server_harness() as harness:
            async with AsyncOpenyakClient(
                base_url=harness.server.base_url,
                timeout_s=30.0,
            ) as client:
                thread = await client.create_thread(
                    model="opus",
                    allowed_tools=[],
                )

                failed = await thread.run("provider bootstrap should fail")

                assert failed.status == "failed"
                assert failed.run_id == "run-1"
                assert failed.error.code == "runtime_error"
                assert failed.error.status == "failed"
                assert failed.error.lifecycle is not None
                assert failed.error.lifecycle.failure_kind == "daemon_thread_runtime_error"
                assert failed.error.lifecycle.recovery is not None
                assert failed.error.lifecycle.recovery.recovery_kind == "inspect_error_and_retry"
                assert failed.terminal_event is not None
                assert failed.terminal_event.payload.lifecycle is not None

                completed = await thread.read()
                assert completed.state.status == "idle"

    asyncio.run(case())


def test_streamed_async_run_surfaces_runtime_failure_recovery_metadata() -> None:
    async def case() -> None:
        with runtime_failure_server_harness() as harness:
            async with AsyncOpenyakClient(
                base_url=harness.server.base_url,
                timeout_s=30.0,
            ) as client:
                thread = await client.create_thread(
                    model="opus",
                    allowed_tools=[],
                )

                async with await thread.run_streamed(
                    "provider bootstrap should fail"
                ) as streamed:
                    event_types: list[str] = []
                    failed_event = None
                    async for event in streamed.events:
                        event_types.append(event.type)
                        if event.type == "run.failed":
                            failed_event = cast(RunFailedEvent, event)
                            break

                assert event_types == ["run.started", "run.failed"]
                assert failed_event is not None
                assert failed_event.payload.lifecycle is not None
                assert (
                    failed_event.payload.lifecycle.failure_kind
                    == "daemon_thread_runtime_error"
                )
                assert failed_event.payload.lifecycle.recovery is not None
                assert (
                    failed_event.payload.lifecycle.recovery.recovery_kind
                    == "inspect_error_and_retry"
                )

    asyncio.run(case())


def test_buffered_async_run_recovers_interrupted_snapshot_truth_after_server_restart() -> None:
    async def case() -> None:
        mock = start_mock_anthropic_service()
        workspace = tempfile.mkdtemp(prefix="openyak-python-sdk-async-restart-")
        env = {
            **os.environ,
            "ANTHROPIC_API_KEY": "test-sdk-key",
            "ANTHROPIC_BASE_URL": mock.base_url,
        }
        server = start_openyak_server_in(workspace, env, cleanup_workspace=False)
        base_url = server.base_url
        bind = urlparse(base_url).netloc

        try:
            async with AsyncOpenyakClient(base_url=base_url, timeout_s=30.0) as client:
                thread = await client.create_thread(
                    model="claude-sonnet-4-6",
                    allowed_tools=["read_file"],
                )
                watcher = client.resume_thread(thread.thread_id)

                run_task = asyncio.create_task(
                    thread.run("PARITY_SCENARIO:delayed_request_user_input_roundtrip")
                )
                await wait_for_thread_status_async(watcher, "running")

                server.close()
                server = start_openyak_server_in(
                    workspace,
                    env,
                    bind=bind,
                    cleanup_workspace=False,
                )

                result = await asyncio.wait_for(run_task, timeout=30.0)
                assert result.status == "interrupted"
                assert result.recovered_from_snapshot is True
                assert result.recovery_note is not None
                assert "restart or shutdown" in result.recovery_note
                assert result.snapshot is not None
                assert result.snapshot.state.lifecycle is not None
                assert (
                    result.snapshot.state.lifecycle.failure_kind
                    == "daemon_restart_interrupted_run"
                )
                assert result.snapshot.state.recovery is not None
                assert (
                    result.snapshot.state.recovery.recovery_kind == "reattach_or_retry"
                )
        finally:
            server.close()
            mock.close()
            shutil.rmtree(workspace, ignore_errors=True)

    asyncio.run(case())

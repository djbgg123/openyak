from __future__ import annotations

import asyncio

from openyak_sdk import AsyncOpenyakClient, OpenyakClient

from .helpers.harness import server_harness


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

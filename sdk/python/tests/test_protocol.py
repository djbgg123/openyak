from __future__ import annotations

import asyncio
import json
from pathlib import Path

import pytest

from openyak_sdk import (
    AsyncOpenyakClient,
    OpenyakClient,
    OpenyakCompatibilityError,
    OpenyakReconnectRequiredError,
    OpenyakResyncRequiredError,
)
from openyak_sdk._models import (
    parse_api_error_envelope,
    parse_list_threads_response,
    parse_thread_event,
    parse_thread_snapshot,
    parse_turn_accepted_response,
    parse_user_input_accepted_response,
)

from .helpers.httpx_mocks import (
    create_async_queued_transport,
    create_queued_transport,
    json_response,
    sse_response,
)

REPO_ROOT = Path(__file__).resolve().parents[3]
FIXTURE_PATH = (
    REPO_ROOT / "rust" / "crates" / "server" / "tests" / "fixtures" / "threads_protocol_v1.json"
)


def _thread_snapshot() -> dict[str, object]:
    return {
        "protocol_version": "v1",
        "thread_id": "thread-1",
        "created_at": 0,
        "updated_at": 0,
        "contract": {
            "truth_layer": "daemon_local_v1",
            "operator_plane": "local_loopback_operator_v1",
            "persistence": "workspace_sqlite_v1",
            "attach_api": "/v1/threads",
        },
        "state": {
            "status": "idle",
            "lifecycle": {
                "status": "idle",
            },
        },
        "config": {
            "cwd": "/tmp/workspace",
            "model": "claude-sonnet-4-6",
            "permission_mode": "danger-full-access",
            "allowed_tools": ["bash"],
        },
        "session": {
            "version": 1,
            "messages": [],
        },
    }


def test_fixture_matrix_decodes_locked_v1_contract() -> None:
    fixture = json.loads(FIXTURE_PATH.read_text(encoding="utf-8"))

    created = parse_thread_snapshot(fixture["responses"]["create_thread"])
    listed = parse_list_threads_response(fixture["responses"]["list_threads"])
    turn_accepted = parse_turn_accepted_response(fixture["responses"]["turn_accepted"])
    user_input_accepted = parse_user_input_accepted_response(
        fixture["responses"]["user_input_accepted"]
    )

    bash_events = [parse_thread_event(event) for event in fixture["events"]["bash_turn"]]
    user_input_events = [
        parse_thread_event(event) for event in fixture["events"]["user_input_roundtrip"]
    ]
    resync_required = parse_thread_event(fixture["events"]["thread_resync_required"])

    assert created.protocol_version == "v1"
    assert created.contract is not None
    assert created.contract.truth_layer == "daemon_local_v1"
    assert listed.threads[0].thread_id == "thread-1"
    assert listed.threads[0].contract is not None
    assert turn_accepted.run_id == "run-1"
    assert turn_accepted.contract is not None
    assert turn_accepted.lifecycle is not None
    assert turn_accepted.lifecycle.status == "accepted"
    assert user_input_accepted.request_id == "req-user-input-roundtrip"
    assert user_input_accepted.contract is not None
    assert user_input_accepted.lifecycle is not None
    assert user_input_accepted.lifecycle.status == "accepted"
    assert bash_events[0].type == "thread.snapshot"
    assert bash_events[-1].type == "run.completed"
    assert bash_events[-1].payload.cumulative_usage.input_tokens == 26
    assert [event.type for event in user_input_events].count("run.waiting_user_input") == 1
    assert user_input_events[-1].type == "run.completed"
    assert resync_required.type == "thread.resync_required"
    assert resync_required.payload.skipped == 1

    conflict = parse_api_error_envelope(fixture["errors"]["conflict"]["body"])
    assert conflict.code == "conflict"
    assert conflict.details is not None
    assert (
        conflict.details["status"]["lifecycle"]["status"]  # type: ignore[index]
        == "awaiting_user_input"
    )


def test_list_threads_rejects_unsupported_protocol_versions() -> None:
    client = OpenyakClient(
        base_url="http://local.test",
        transport=create_queued_transport(
            [
                json_response(
                    {
                        "protocol_version": "v2",
                        "threads": [],
                    }
                )
            ]
        ),
    )

    with pytest.raises(OpenyakCompatibilityError) as error:
        client.list_threads()

    assert error.value.expected == "v1"
    assert error.value.received == "v2"


def test_stream_events_preserves_snapshot_first_sse_and_resync_envelopes() -> None:
    client = OpenyakClient(
        base_url="http://local.test",
        transport=create_queued_transport(
            [
                sse_response(
                    [
                        {
                            "protocol_version": "v1",
                            "thread_id": "thread-1",
                            "sequence": 0,
                            "timestamp_ms": 0,
                            "type": "thread.snapshot",
                            "payload": _thread_snapshot(),
                        },
                        {
                            "protocol_version": "v1",
                            "thread_id": "thread-1",
                            "sequence": 5,
                            "timestamp_ms": 0,
                            "type": "thread.resync_required",
                            "payload": {
                                "skipped": 3,
                                "snapshot": _thread_snapshot(),
                            },
                        },
                    ]
                )
            ]
        ),
    )

    thread = client.resume_thread("thread-1")
    events = list(thread.stream_events())

    assert [event.type for event in events] == ["thread.snapshot", "thread.resync_required"]
    assert events[1].payload.skipped == 3


def test_run_streamed_surfaces_thread_resync_required_as_a_dedicated_error() -> None:
    client = OpenyakClient(
        base_url="http://local.test",
        transport=create_queued_transport(
            [
                sse_response(
                    [
                        {
                            "protocol_version": "v1",
                            "thread_id": "thread-1",
                            "sequence": 0,
                            "timestamp_ms": 0,
                            "type": "thread.snapshot",
                            "payload": _thread_snapshot(),
                        },
                        {
                            "protocol_version": "v1",
                            "thread_id": "thread-1",
                            "run_id": "run-1",
                            "sequence": 1,
                            "timestamp_ms": 0,
                            "type": "thread.resync_required",
                            "payload": {
                                "skipped": 2,
                                "snapshot": {
                                    **_thread_snapshot(),
                                    "state": {
                                        "status": "running",
                                        "run_id": "run-1",
                                    },
                                },
                            },
                        },
                    ]
                ),
                json_response(
                    {
                        "protocol_version": "v1",
                        "thread_id": "thread-1",
                        "run_id": "run-1",
                        "status": "accepted",
                    }
                ),
            ]
        ),
    )

    thread = client.resume_thread("thread-1")
    streamed = thread.run_streamed("hello")

    with pytest.raises(OpenyakResyncRequiredError) as error:
        list(streamed.events)

    assert error.value.event.thread_id == "thread-1"
    assert error.value.event.payload.skipped == 2


def test_run_recovers_awaiting_user_input_from_the_latest_snapshot_after_a_dropped_stream() -> None:
    client = OpenyakClient(
        base_url="http://local.test",
        transport=create_queued_transport(
            [
                sse_response(
                    [
                        {
                            "protocol_version": "v1",
                            "thread_id": "thread-1",
                            "sequence": 0,
                            "timestamp_ms": 0,
                            "type": "thread.snapshot",
                            "payload": _thread_snapshot(),
                        },
                        {
                            "protocol_version": "v1",
                            "thread_id": "thread-1",
                            "run_id": "run-1",
                            "sequence": 1,
                            "timestamp_ms": 0,
                            "type": "run.started",
                            "payload": {
                                "kind": "turn",
                                "message": "hello",
                                "status": "running",
                            },
                        },
                        {
                            "protocol_version": "v1",
                            "thread_id": "thread-1",
                            "run_id": "run-1",
                            "sequence": 2,
                            "timestamp_ms": 0,
                            "type": "assistant.request_user_input",
                            "payload": {
                                "request_id": "req-1",
                                "prompt": "Continue?",
                                "options": ["yes"],
                                "allow_freeform": True,
                            },
                        },
                    ]
                ),
                json_response(
                    {
                        "protocol_version": "v1",
                        "thread_id": "thread-1",
                        "run_id": "run-1",
                        "status": "accepted",
                    }
                ),
                json_response(
                    {
                        **_thread_snapshot(),
                        "updated_at": 1,
                        "state": {
                            "status": "awaiting_user_input",
                            "run_id": "run-1",
                            "pending_user_input": {
                                "request_id": "req-1",
                                "prompt": "Continue?",
                                "options": ["yes"],
                                "allow_freeform": True,
                            },
                        },
                    }
                ),
            ]
        ),
    )

    thread = client.resume_thread("thread-1")
    result = thread.run("hello")

    assert result.status == "awaiting_user_input"
    assert result.recovered_from_snapshot is True
    assert result.terminal_event is None
    assert result.pending_user_input.request_id == "req-1"


def test_run_fails_predictably_when_reconciliation_sees_a_different_active_run() -> None:
    client = OpenyakClient(
        base_url="http://local.test",
        transport=create_queued_transport(
            [
                sse_response(
                    [
                        {
                            "protocol_version": "v1",
                            "thread_id": "thread-1",
                            "sequence": 0,
                            "timestamp_ms": 0,
                            "type": "thread.snapshot",
                            "payload": _thread_snapshot(),
                        }
                    ]
                ),
                json_response(
                    {
                        "protocol_version": "v1",
                        "thread_id": "thread-1",
                        "run_id": "run-1",
                        "status": "accepted",
                    }
                ),
                json_response(
                    {
                        **_thread_snapshot(),
                        "updated_at": 2,
                        "state": {
                            "status": "awaiting_user_input",
                            "run_id": "run-2",
                            "pending_user_input": {
                                "request_id": "req-2",
                                "prompt": "Different run",
                                "options": ["ok"],
                                "allow_freeform": True,
                            },
                        },
                    }
                ),
            ]
        ),
    )

    thread = client.resume_thread("thread-1")
    with pytest.raises(OpenyakReconnectRequiredError) as error:
        thread.run("hello")

    assert error.value.thread_id == "thread-1"
    assert error.value.run_id == "run-1"
    assert error.value.latest_snapshot is not None
    assert error.value.latest_snapshot.state.run_id == "run-2"


def test_async_run_streamed_surfaces_thread_resync_required_as_a_dedicated_error() -> None:
    async def case() -> None:
        client = AsyncOpenyakClient(
            base_url="http://local.test",
            transport=create_async_queued_transport(
                [
                    sse_response(
                        [
                            {
                                "protocol_version": "v1",
                                "thread_id": "thread-1",
                                "sequence": 0,
                                "timestamp_ms": 0,
                                "type": "thread.snapshot",
                                "payload": _thread_snapshot(),
                            },
                            {
                                "protocol_version": "v1",
                                "thread_id": "thread-1",
                                "run_id": "run-1",
                                "sequence": 1,
                                "timestamp_ms": 0,
                                "type": "thread.resync_required",
                                "payload": {
                                    "skipped": 2,
                                    "snapshot": {
                                        **_thread_snapshot(),
                                        "state": {
                                            "status": "running",
                                            "run_id": "run-1",
                                        },
                                    },
                                },
                            },
                        ]
                    ),
                    json_response(
                        {
                            "protocol_version": "v1",
                            "thread_id": "thread-1",
                            "run_id": "run-1",
                            "status": "accepted",
                        }
                    ),
                ]
            ),
        )
        thread = client.resume_thread("thread-1")
        streamed = await thread.run_streamed("hello")
        with pytest.raises(OpenyakResyncRequiredError):
            async for _event in streamed.events:
                pass
        await client.aclose()

    asyncio.run(case())


def test_async_run_recovers_interrupted_from_the_latest_snapshot() -> None:
    async def case() -> None:
        client = AsyncOpenyakClient(
            base_url="http://local.test",
            transport=create_async_queued_transport(
                [
                    sse_response(
                        [
                            {
                                "protocol_version": "v1",
                                "thread_id": "thread-1",
                                "sequence": 0,
                                "timestamp_ms": 0,
                                "type": "thread.snapshot",
                                "payload": _thread_snapshot(),
                            },
                            {
                                "protocol_version": "v1",
                                "thread_id": "thread-1",
                                "run_id": "run-1",
                                "sequence": 1,
                                "timestamp_ms": 0,
                                "type": "run.started",
                                "payload": {
                                    "kind": "turn",
                                    "message": "hello",
                                    "status": "running",
                                },
                            },
                        ]
                    ),
                    json_response(
                        {
                            "protocol_version": "v1",
                            "thread_id": "thread-1",
                            "run_id": "run-1",
                            "status": "accepted",
                        }
                    ),
                    json_response(
                        {
                            **_thread_snapshot(),
                            "updated_at": 3,
                            "state": {
                                "status": "interrupted",
                                "lifecycle": {
                                    "status": "interrupted",
                                    "failure_kind": "daemon_restart_interrupted_run",
                                    "recovery": {
                                        "failure_kind": "daemon_restart_interrupted_run",
                                        "recovery_kind": "reattach_or_retry",
                                        "recommended_actions": [
                                            "reattach to the thread and inspect the latest snapshot"
                                        ],
                                    },
                                },
                                "run_id": "run-1",
                                "recovery_note": "server restarted mid-run",
                                "recovery": {
                                    "failure_kind": "daemon_restart_interrupted_run",
                                    "recovery_kind": "reattach_or_retry",
                                    "recommended_actions": [
                                        "reattach to the thread and inspect the latest snapshot"
                                    ],
                                },
                            },
                        }
                    ),
                ]
            ),
        )
        thread = client.resume_thread("thread-1")
        result = await thread.run("hello")
        assert result.status == "interrupted"
        assert result.recovered_from_snapshot is True
        assert result.recovery_note == "server restarted mid-run"
        assert result.snapshot is not None
        assert result.snapshot.state.recovery is not None
        assert (
            result.snapshot.state.recovery.failure_kind
            == "daemon_restart_interrupted_run"
        )
        await client.aclose()

    asyncio.run(case())

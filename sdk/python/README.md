# openyak Python SDK alpha

Attach-first Python client for the current local `openyak server` `/v1/threads` protocol.

Current package metadata:

- version: `0.0.0a1`
- requires Python `>=3.10`
- CLI/operator boundary rechecked against local `openyak` CLI `v0.1.0` on `2026-04-10`
- full SDK verification commands below remain last repo-wide rerun on `2026-04-09`

## Scope

- local-only
- protocol version `v1` only
- sync and async clients on the same contract
- no server launcher
- no runtime bundling
- no SDK support for legacy `/sessions`

The local server still exposes legacy `/sessions` compatibility routes for existing callers, but this SDK's public contract remains `/v1/threads` only.

Operator-facing truth labels for this SDK boundary:

- thread snapshots expose `truth_layer = daemon_local_v1`
- thread snapshots expose `attach_api = /v1/threads`
- this does **not** upgrade Task / Team / Cron foundations; those remain `process_local_v1`

## Package name and import path

- Distribution package name: `openyak-python-sdk-alpha`
- Python import path: `openyak_sdk`

Example:

```python
from openyak_sdk import OpenyakClient
```

## Install

Inside this repo:

```bash
python -m pip install -e sdk/python
```

For local development and verification:

```bash
python -m pip install -e "sdk/python[dev]"
```

The editable install path is the recommended setup for local verification inside this repo.

## Start a local server first

In another terminal, you can start a foreground local server:

```bash
cd rust
cargo run --bin openyak -- server --bind 127.0.0.1:0
```

The server prints a listening URL such as:

```text
Local thread server listening on http://127.0.0.1:PORT
```

Use that URL as `OPENYAK_BASE_URL`.

For a longer-lived workspace-local daemon, the current CLI also ships a bounded local operator surface:

```bash
cd rust
cargo run --bin openyak -- server start --detach --bind 127.0.0.1:0
cargo run --bin openyak -- server status
```

If you use detached mode, read the reported `base_url` / operator token from `openyak server status` or the workspace `.openyak/thread-server.json` discovery file. Use `openyak server stop` to shut it down and `openyak server recover` only when you want to reattach persisted local thread truth in that same workspace.

The backing CLI help currently defines `openyak server` as a local HTTP/SSE thread server that:

- exposes `/v1/threads`
- keeps legacy `/sessions` compatibility routes
- persists thread state in workspace `.openyak/state.sqlite3`
- only supports loopback binds such as `127.0.0.1:0`

## Quickstart

```python
import os

from openyak_sdk import OpenyakClient

with OpenyakClient(
    base_url=os.environ["OPENYAK_BASE_URL"],
    operator_token=os.environ.get("OPENYAK_OPERATOR_TOKEN"),
) as client:
    thread = client.create_thread(
        model="claude-sonnet-4-6",
        allowed_tools=["bash"],
    )

    result = thread.run("PARITY_SCENARIO:bash_stdout_roundtrip")
    print(result.status, result.final_text, result.usage)
```

## Async quickstart

```python
import asyncio
import os

from openyak_sdk import AsyncOpenyakClient


async def main() -> None:
    async with AsyncOpenyakClient(
        base_url=os.environ["OPENYAK_BASE_URL"],
        operator_token=os.environ.get("OPENYAK_OPERATOR_TOKEN"),
    ) as client:
        thread = await client.create_thread(
            model="claude-sonnet-4-6",
            allowed_tools=["bash"],
        )
        result = await thread.run("PARITY_SCENARIO:bash_stdout_roundtrip")
        print(result.status, result.final_text, result.usage)


asyncio.run(main())
```

## Streaming

`stream_events()` exposes raw `/v1/threads/{id}/events` envelopes, including the initial `thread.snapshot`.
Current `run.*` SSE payloads are additive-metadata aware: `run.started`, `run.completed`, `run.waiting_user_input`, and `run.failed` now carry `status` plus shared `lifecycle` metadata without widening the attach-first `/v1/threads` boundary.
That now includes locked runtime/storage `run.failed` recovery taxonomy coverage, so `failure_kind`, `recovery_kind`, and `recommended_actions` stay stable across both fixture decoding and live local-server regressions.

```python
for event in thread.stream_events():
    print(event.type)
```

`run_streamed()` is higher-level:

- it opens the event stream first
- consumes the initial `thread.snapshot`
- submits the turn
- then yields only events for the accepted `run_id`
- a fresh `stream_events()` attach also starts from the latest persisted `thread.snapshot`, including restart-recovered `awaiting_user_input` or `interrupted` truth

```python
with thread.run_streamed("PARITY_SCENARIO:bash_stdout_roundtrip") as streamed:
    print(streamed.snapshot.state.status)
    print(streamed.accepted.run_id)

    for event in streamed.events:
        print(event.type, event.payload)
```

The async client exposes the same contract through `async with` and `async for`.

## User-input pause/resume

Buffered `run()` does **not** treat `run.waiting_user_input` as an error.

```python
paused = thread.run("PARITY_SCENARIO:request_user_input_roundtrip")
if paused.status != "awaiting_user_input":
    raise RuntimeError("expected pause")

resumed = thread.resume_user_input(
    request_id=paused.pending_user_input.request_id,
    content="feature",
    selected_option="feature",
)
```

## Existing threads

```python
thread = client.resume_thread("thread-123")
snapshot = thread.read()
```

## Compatibility and reconnect limits

- The SDK hard-fails on unsupported `protocol_version`.
- `thread.resync_required` becomes `OpenyakResyncRequiredError` in `run_streamed()` / `resume_user_input_streamed()`.
- That resync path is now also locked in live local-server integration coverage for deliberately lagged attach-first streams; the SDK still treats it as resync-required, not replay.
- `run()` may reconcile from `thread.read()` after a dropped stream and marks the result with `recovered_from_snapshot=True`.
- If the local server fails before runtime/provider bootstrap completes, the latest thread snapshot still preserves the submitted turn or user-input response instead of silently dropping it.
- If the server restarts mid-run, the latest snapshot may come back as `status="interrupted"` with a `recovery_note`; the SDK surfaces that snapshot truth, but it does not invent daemon-side replay or recovery actions.
- Because local operator auth is per-daemon, an in-flight buffered client from before the restart may no longer be able to read the restarted server; that path now surfaces `OpenyakReconnectRequiredError` instead of pretending buffered replay still exists.
- Fresh attach-first reattachment is now also locked live: after a local server restart, `resume_thread()`, `read()`, and `list_threads()` expose the latest persisted `awaiting_user_input` or `interrupted` snapshot truth without pretending `run()` replay exists.
- The same snapshot contract also exposes `operator_plane`, `persistence`, and structured recovery fields (`failure_kind`, `recovery_kind`, `recommended_actions`) so attach-first clients can render operator guidance without implying broader daemon controls.
- That reconciliation is intentionally best-effort for local attach-first, single-writer usage; if the latest snapshot shows a different active `run_id`, `run()` raises `OpenyakReconnectRequiredError` instead of pretending replay exists.
- `run_streamed()` does **not** pretend replay exists; if live streaming fidelity is lost, it raises.
- 真实本地 server 重启导致的 streamed 断流现在也有 live integration 锁定，并会按当前 attach-first 合约上抛 reconnect-required 语义，而不是假装 replay。

This means the current Python SDK remains compatible with the local-first daemon/control-plane roadmap only at the `/v1/threads` attach-first layer: it can observe persisted interruption state plus the `daemon_local_v1` thread truth label, but it is not yet a client for daemon start/stop/status/recover operator APIs. More specifically, it is not itself a client for the CLI's local-only `server start --detach` / `status` / `stop` / `recover` operator actions.

## Minimal package layout

- `src/openyak_sdk`: runtime models, sync client, async client, SSE handling, errors
- `examples/`: attach-first local examples
- `tests/`: protocol and live integration coverage against a real local `openyak server`

## Verification commands

The commands below assume your active Python environment already has the SDK installed with dev extras:

```bash
python -m pip install -e "sdk/python[dev]"
```

Then run the verification suite from `sdk/python`:

```bash
cd sdk/python
python -m pytest
python -m ruff check .
python -m mypy
python -m build
```

`python -m build` in this list was rerun successfully on `2026-04-10` during the latest documentation refresh. The full SDK verification set above remains the repo-wide baseline last rerun on `2026-04-09`.

# openyak TypeScript SDK alpha

Attach-first TypeScript client for the current local `openyak server` `/v1/threads` protocol.

Current package metadata:

- version: `0.0.0-alpha.1`
- requires Node.js `>=18`
- package manager in-repo: `pnpm`
- verified against local `openyak` CLI `v0.1.0` on `2026-04-09`

## Scope

- local-only
- protocol version `v1` only
- no server launcher
- no runtime bundling
- no SDK support for legacy `/sessions`

The local server still exposes legacy `/sessions` compatibility routes for existing callers, but this SDK's public contract remains `/v1/threads` only.

Operator-facing truth labels for this SDK boundary:

- thread snapshots expose `truth_layer = daemon_local_v1`
- thread snapshots expose `attach_api = /v1/threads`
- this does **not** upgrade Task / Team / Cron foundations; those remain `process_local_v1`

## Package name and import path

- npm package name: `@openyak/typescript-sdk-alpha`
- consumer import path: `@openyak/typescript-sdk-alpha`

Example:

```ts
import { OpenyakClient } from "@openyak/typescript-sdk-alpha";
```

If you are hacking inside this repo rather than consuming the package, build first and import from the built package surface instead of reaching into `src/`.

## Install

Inside this repo:

```bash
cd sdk/typescript
pnpm install --frozen-lockfile
pnpm build
```

For local verification inside this repo, keep using the workspace package root instead of importing files from `src/` directly.

`pnpm-lock.yaml` is the canonical repo lockfile for this SDK and is the file mirrored by CI/fresh-clone verification.

## Start a local server first

In another terminal:

```bash
cd rust
cargo run --bin openyak -- server --bind 127.0.0.1:0
```

The server prints a listening URL such as:

```text
Local thread server listening on http://127.0.0.1:PORT
```

Use that URL as `OPENYAK_BASE_URL`.

The backing CLI help currently defines `openyak server` as a local HTTP/SSE thread server that:

- exposes `/v1/threads`
- keeps legacy `/sessions` compatibility routes
- persists thread state in workspace `.openyak/state.sqlite3`
- only supports loopback binds such as `127.0.0.1:0`

## Quickstart

```ts
import { OpenyakClient } from "@openyak/typescript-sdk-alpha";

const client = new OpenyakClient({
  baseUrl: process.env.OPENYAK_BASE_URL!,
});

const thread = await client.createThread({
  model: "claude-sonnet-4-6",
  allowedTools: ["bash"],
});

const result = await thread.run("PARITY_SCENARIO:bash_stdout_roundtrip");
console.log(result.status, result.finalText, result.usage);
```

## Streaming

`streamEvents()` exposes raw `/v1/threads/{id}/events` envelopes, including the initial `thread.snapshot`.

```ts
for await (const event of thread.streamEvents()) {
  console.log(event.type);
}
```

`runStreamed()` is higher-level:

- it opens the event stream first
- consumes the initial `thread.snapshot`
- submits the turn
- then yields only events for the accepted `run_id`

```ts
const streamed = await thread.runStreamed("PARITY_SCENARIO:bash_stdout_roundtrip");

console.log(streamed.snapshot.state.status);
console.log(streamed.accepted.run_id);

for await (const event of streamed.events) {
  console.log(event.type, event.payload);
}
```

## User-input pause/resume

Buffered `run()` does **not** treat `run.waiting_user_input` as an error.

```ts
const paused = await thread.run("PARITY_SCENARIO:request_user_input_roundtrip");
if (paused.status !== "awaiting_user_input") throw new Error("expected pause");

const resumed = await thread.resumeUserInput({
  requestId: paused.pendingUserInput.request_id,
  content: "feature",
  selectedOption: "feature",
});
```

## Existing threads

```ts
const thread = client.resumeThread("thread-123");
const snapshot = await thread.read();
```

## Compatibility and reconnect limits

- The SDK hard-fails on unsupported `protocol_version`.
- `thread.resync_required` becomes `OpenyakResyncRequiredError` in `runStreamed()`.
- `run()` may reconcile from `thread.read()` after a dropped stream and marks the result with `recoveredFromSnapshot: true`.
- If the local server fails before runtime/provider bootstrap completes, the latest thread snapshot still preserves the submitted turn or user-input response instead of silently dropping it.
- If the server restarts mid-run, the latest snapshot may come back as `status="interrupted"` with a `recovery_note`; the SDK exposes that persisted truth, but it does not invent daemon-side replay or recovery actions.
- That reconciliation is intentionally best-effort for local attach-first, single-writer usage; if the latest snapshot shows a different active `run_id`, `run()` throws `OpenyakReconnectRequiredError` instead of pretending replay exists.
- `runStreamed()` does **not** pretend replay exists; if live streaming fidelity is lost, it throws.

So the current TypeScript SDK is compatible with the daemon/control-plane roadmap only at the `/v1/threads` attach-first boundary: it can observe persisted interruption state plus the `daemon_local_v1` thread truth label, but it is not yet a client for daemon start/stop/status/recover operator APIs.

## Scripts

```bash
cd sdk/typescript
pnpm test
pnpm lint
pnpm build
```

These commands were rerun successfully on `2026-04-09` during the latest repo-wide documentation refresh and full command-by-command CLI verification.

import assert from "node:assert/strict";
import test from "node:test";

import {
  OpenyakApiError,
  OpenyakClient,
  OpenyakCompatibilityError,
  OpenyakReconnectRequiredError,
  OpenyakResyncRequiredError,
} from "../src/index.js";
import type { ThreadEvent } from "../src/index.js";
import { createQueuedFetch, jsonResponse, sseResponse } from "./helpers/fetch.js";

const threadSnapshot = {
  protocol_version: "v1",
  thread_id: "thread-1",
  created_at: 0,
  updated_at: 0,
  contract: {
    truth_layer: "daemon_local_v1",
    operator_plane: "local_loopback_operator_v1",
    persistence: "workspace_sqlite_v1",
    attach_api: "/v1/threads",
  },
  state: {
    status: "idle",
    lifecycle: {
      status: "idle",
    },
  },
  config: {
    cwd: "/tmp/workspace",
    model: "claude-sonnet-4-6",
    permission_mode: "danger-full-access",
    allowed_tools: ["bash"],
  },
  session: {
    version: 1,
    messages: [],
  },
} as const;

test("listThreads rejects unsupported protocol versions", async () => {
  const client = new OpenyakClient({
    baseUrl: "http://local.test",
    fetch: createQueuedFetch([
      jsonResponse({
        protocol_version: "v2",
        threads: [],
      }),
    ]),
  });

  await assert.rejects(
    client.listThreads(),
    (error: unknown) =>
      error instanceof OpenyakCompatibilityError &&
      error.expected === "v1" &&
      error.received === "v2",
  );
});

test("streamEvents preserves snapshot-first SSE ordering and resync envelopes", async () => {
  const client = new OpenyakClient({
    baseUrl: "http://local.test",
    fetch: createQueuedFetch([
      sseResponse([
        {
          protocol_version: "v1",
          thread_id: "thread-1",
          sequence: 0,
          timestamp_ms: 0,
          type: "thread.snapshot",
          payload: threadSnapshot,
        },
        {
          protocol_version: "v1",
          thread_id: "thread-1",
          sequence: 5,
          timestamp_ms: 0,
          type: "thread.resync_required",
          payload: {
            skipped: 3,
            snapshot: threadSnapshot,
          },
        },
      ]),
    ]),
  });

  const thread = client.resumeThread("thread-1");
  const events: ThreadEvent[] = [];
  for await (const event of thread.streamEvents()) {
    events.push(event);
  }

  assert.equal(events[0]?.type, "thread.snapshot");
  assert.equal(events[1]?.type, "thread.resync_required");
  assert.equal(events[1]?.payload.skipped, 3);
});

test("streamEvents preserves run lifecycle metadata on additive terminal payloads", async () => {
  const client = new OpenyakClient({
    baseUrl: "http://local.test",
    fetch: createQueuedFetch([
      sseResponse([
        {
          protocol_version: "v1",
          thread_id: "thread-1",
          sequence: 0,
          timestamp_ms: 0,
          type: "thread.snapshot",
          payload: threadSnapshot,
        },
        {
          protocol_version: "v1",
          thread_id: "thread-1",
          run_id: "run-1",
          sequence: 1,
          timestamp_ms: 0,
          type: "run.started",
          payload: {
            kind: "turn",
            message: "hello",
            status: "running",
            lifecycle: {
              status: "running",
            },
          },
        },
        {
          protocol_version: "v1",
          thread_id: "thread-1",
          run_id: "run-1",
          sequence: 2,
          timestamp_ms: 0,
          type: "run.completed",
          payload: {
            iterations: 1,
            assistant_message_count: 1,
            tool_result_count: 0,
            cumulative_usage: {
              input_tokens: 1,
              output_tokens: 1,
              cache_creation_input_tokens: 0,
              cache_read_input_tokens: 0,
            },
            status: "completed",
            lifecycle: {
              status: "completed",
            },
          },
        },
      ]),
    ]),
  });

  const events: ThreadEvent[] = [];
  for await (const event of client.resumeThread("thread-1").streamEvents()) {
    events.push(event);
  }

  assert.equal(events[1]?.type, "run.started");
  assert.equal(events[1]?.payload.lifecycle?.status, "running");
  assert.equal(events[2]?.type, "run.completed");
  assert.equal(events[2]?.payload.status, "completed");
  assert.equal(events[2]?.payload.lifecycle?.status, "completed");
});

test("streamEvents preserves run.failed recovery metadata on additive failure payloads", async () => {
  const client = new OpenyakClient({
    baseUrl: "http://local.test",
    fetch: createQueuedFetch([
      sseResponse([
        {
          protocol_version: "v1",
          thread_id: "thread-1",
          sequence: 0,
          timestamp_ms: 0,
          type: "thread.snapshot",
          payload: threadSnapshot,
        },
        {
          protocol_version: "v1",
          thread_id: "thread-1",
          run_id: "run-1",
          sequence: 1,
          timestamp_ms: 0,
          type: "run.started",
          payload: {
            kind: "turn",
            message: "hello",
            status: "running",
            lifecycle: {
              status: "running",
            },
          },
        },
        {
          protocol_version: "v1",
          thread_id: "thread-1",
          run_id: "run-1",
          sequence: 2,
          timestamp_ms: 0,
          type: "run.failed",
          payload: {
            code: "runtime_error",
            message: "boom",
            status: "failed",
            lifecycle: {
              status: "failed",
              failure_kind: "daemon_thread_runtime_error",
              recovery: {
                failure_kind: "daemon_thread_runtime_error",
                recovery_kind: "inspect_error_and_retry",
                recommended_actions: [
                  "inspect the error message and latest /v1/threads snapshot",
                ],
              },
            },
          },
        },
      ]),
    ]),
  });

  const events: ThreadEvent[] = [];
  for await (const event of client.resumeThread("thread-1").streamEvents()) {
    events.push(event);
  }

  assert.equal(events[2]?.type, "run.failed");
  assert.equal(events[2]?.payload.status, "failed");
  assert.equal(events[2]?.payload.lifecycle?.failure_kind, "daemon_thread_runtime_error");
  assert.equal(
    events[2]?.payload.lifecycle?.recovery?.recovery_kind,
    "inspect_error_and_retry",
  );
});

test("runStreamed surfaces thread.resync_required as a dedicated error", async () => {
  const client = new OpenyakClient({
    baseUrl: "http://local.test",
    fetch: createQueuedFetch([
      sseResponse([
        {
          protocol_version: "v1",
          thread_id: "thread-1",
          sequence: 0,
          timestamp_ms: 0,
          type: "thread.snapshot",
          payload: threadSnapshot,
        },
        {
          protocol_version: "v1",
          thread_id: "thread-1",
          run_id: "run-1",
          sequence: 1,
          timestamp_ms: 0,
          type: "thread.resync_required",
          payload: {
            skipped: 2,
            snapshot: {
              ...threadSnapshot,
              state: {
                status: "running",
                run_id: "run-1",
              },
            },
          },
        },
      ]),
      jsonResponse({
        protocol_version: "v1",
        contract: {
          truth_layer: "daemon_local_v1",
          operator_plane: "local_loopback_operator_v1",
          persistence: "workspace_sqlite_v1",
          attach_api: "/v1/threads",
        },
        thread_id: "thread-1",
        run_id: "run-1",
        lifecycle: {
          status: "accepted",
        },
        status: "accepted",
      }),
    ]),
  });

  const thread = client.resumeThread("thread-1");
  const streamed = await thread.runStreamed("hello");

  await assert.rejects(
    (async () => {
      for await (const _event of streamed.events) {
        // no-op
      }
    })(),
    (error: unknown) =>
      error instanceof OpenyakResyncRequiredError &&
      error.event.thread_id === "thread-1" &&
      error.event.payload.skipped === 2,
  );
});

test("run recovers awaiting_user_input from the latest snapshot after a dropped stream", async () => {
  const client = new OpenyakClient({
    baseUrl: "http://local.test",
    fetch: createQueuedFetch([
      sseResponse([
        {
          protocol_version: "v1",
          thread_id: "thread-1",
          sequence: 0,
          timestamp_ms: 0,
          type: "thread.snapshot",
          payload: threadSnapshot,
        },
        {
          protocol_version: "v1",
          thread_id: "thread-1",
          run_id: "run-1",
          sequence: 1,
          timestamp_ms: 0,
          type: "run.started",
          payload: {
            kind: "turn",
            message: "hello",
            status: "running",
          },
        },
        {
          protocol_version: "v1",
          thread_id: "thread-1",
          run_id: "run-1",
          sequence: 2,
          timestamp_ms: 0,
          type: "assistant.request_user_input",
          payload: {
            request_id: "req-1",
            prompt: "Continue?",
            options: ["yes"],
            allow_freeform: true,
          },
        },
      ]),
      jsonResponse({
        protocol_version: "v1",
        contract: {
          truth_layer: "daemon_local_v1",
          operator_plane: "local_loopback_operator_v1",
          persistence: "workspace_sqlite_v1",
          attach_api: "/v1/threads",
        },
        thread_id: "thread-1",
        run_id: "run-1",
        lifecycle: {
          status: "accepted",
        },
        status: "accepted",
      }),
      jsonResponse({
        ...threadSnapshot,
        updated_at: 1,
        state: {
          status: "awaiting_user_input",
          lifecycle: {
            status: "awaiting_user_input",
          },
          run_id: "run-1",
          pending_user_input: {
            request_id: "req-1",
            prompt: "Continue?",
            options: ["yes"],
            allow_freeform: true,
          },
        },
      }),
    ]),
  });

  const thread = client.resumeThread("thread-1");
  const result = await thread.run("hello");

  assert.equal(result.status, "awaiting_user_input");
  assert.equal(result.recoveredFromSnapshot, true);
  assert.equal(result.terminalEvent, null);
  assert.equal(result.pendingUserInput.request_id, "req-1");
});

test("run fails predictably when reconciliation sees a different active run", async () => {
  const client = new OpenyakClient({
    baseUrl: "http://local.test",
    fetch: createQueuedFetch([
      sseResponse([
        {
          protocol_version: "v1",
          thread_id: "thread-1",
          sequence: 0,
          timestamp_ms: 0,
          type: "thread.snapshot",
          payload: threadSnapshot,
        },
      ]),
      jsonResponse({
        protocol_version: "v1",
        contract: {
          truth_layer: "daemon_local_v1",
          operator_plane: "local_loopback_operator_v1",
          persistence: "workspace_sqlite_v1",
          attach_api: "/v1/threads",
        },
        thread_id: "thread-1",
        run_id: "run-1",
        lifecycle: {
          status: "accepted",
        },
        status: "accepted",
      }),
      jsonResponse({
        ...threadSnapshot,
        updated_at: 2,
        state: {
          status: "awaiting_user_input",
          lifecycle: {
            status: "awaiting_user_input",
          },
          run_id: "run-2",
          pending_user_input: {
            request_id: "req-2",
            prompt: "Different run",
            options: ["ok"],
            allow_freeform: true,
          },
        },
      }),
    ]),
  });

  const thread = client.resumeThread("thread-1");
  await assert.rejects(
    thread.run("hello"),
    (error: unknown) =>
      error instanceof OpenyakReconnectRequiredError &&
      error.threadId === "thread-1" &&
      error.runId === "run-1" &&
      error.latestSnapshot?.state.run_id === "run-2",
  );
});

test("thread conflict responses preserve lifecycle metadata in error details", async () => {
  const client = new OpenyakClient({
    baseUrl: "http://local.test",
    fetch: createQueuedFetch([
      jsonResponse(
        {
          code: "conflict",
          message: "thread already has an active or blocked run",
          details: {
            status: {
              status: "awaiting_user_input",
              lifecycle: {
                status: "awaiting_user_input",
              },
              run_id: "run-2",
              pending_user_input: {
                request_id: "req-1",
                prompt: "Continue?",
                options: ["yes"],
                allow_freeform: true,
              },
            },
          },
        },
        { status: 409 },
      ),
    ]),
  });

  const thread = client.resumeThread("thread-1");
  await assert.rejects(
    thread.startTurn("hello"),
    (error: unknown) =>
      error instanceof OpenyakApiError &&
      (error.details as Record<string, unknown>).status !== undefined &&
      ((error.details as Record<string, unknown>).status as Record<string, unknown>)
        .lifecycle !== undefined,
  );
});

test("run recovers interrupted lifecycle metadata from latest snapshot", async () => {
  const client = new OpenyakClient({
    baseUrl: "http://local.test",
    fetch: createQueuedFetch([
      sseResponse([
        {
          protocol_version: "v1",
          thread_id: "thread-1",
          sequence: 0,
          timestamp_ms: 0,
          type: "thread.snapshot",
          payload: threadSnapshot,
        },
      ]),
      jsonResponse({
        protocol_version: "v1",
        contract: {
          truth_layer: "daemon_local_v1",
          operator_plane: "local_loopback_operator_v1",
          persistence: "workspace_sqlite_v1",
          attach_api: "/v1/threads",
        },
        thread_id: "thread-1",
        run_id: "run-1",
        lifecycle: {
          status: "accepted",
        },
        status: "accepted",
      }),
      jsonResponse({
        ...threadSnapshot,
        updated_at: 3,
        state: {
          status: "interrupted",
          lifecycle: {
            status: "interrupted",
            failure_kind: "daemon_restart_interrupted_run",
            recovery: {
              failure_kind: "daemon_restart_interrupted_run",
              recovery_kind: "reattach_or_retry",
              recommended_actions: [
                "reattach to the thread and inspect the latest snapshot",
              ],
            },
          },
          run_id: "run-1",
          recovery_note: "server restarted mid-run",
          recovery: {
            failure_kind: "daemon_restart_interrupted_run",
            recovery_kind: "reattach_or_retry",
            recommended_actions: [
              "reattach to the thread and inspect the latest snapshot",
            ],
          },
        },
      }),
    ]),
  });

  const thread = client.resumeThread("thread-1");
  const result = await thread.run("hello");

  assert.equal(result.status, "interrupted");
  assert.equal(result.recoveryNote, "server restarted mid-run");
  assert.equal(
    result.snapshot?.state.lifecycle?.failure_kind,
    "daemon_restart_interrupted_run",
  );
});

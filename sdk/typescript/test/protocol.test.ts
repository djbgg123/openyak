import assert from "node:assert/strict";
import test from "node:test";

import {
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
  state: {
    status: "idle",
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
        thread_id: "thread-1",
        run_id: "run-1",
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
        thread_id: "thread-1",
        run_id: "run-1",
        status: "accepted",
      }),
      jsonResponse({
        ...threadSnapshot,
        updated_at: 1,
        state: {
          status: "awaiting_user_input",
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
        thread_id: "thread-1",
        run_id: "run-1",
        status: "accepted",
      }),
      jsonResponse({
        ...threadSnapshot,
        updated_at: 2,
        state: {
          status: "awaiting_user_input",
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

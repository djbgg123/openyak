import assert from "node:assert/strict";
import { mkdtemp, rm } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import test from "node:test";

import { OpenyakClient, OpenyakReconnectRequiredError } from "../src/index.js";
import {
  startMockAnthropicService,
  startOpenyakServer,
  startOpenyakServerIn,
  withServerHarness,
} from "./helpers/harness.js";

const providerEnvKeys = [
  "ANTHROPIC_API_KEY",
  "ANTHROPIC_AUTH_TOKEN",
  "ANTHROPIC_BASE_URL",
  "OPENAI_API_KEY",
  "OPENAI_BASE_URL",
  "XAI_API_KEY",
  "XAI_BASE_URL",
] as const;

async function withRuntimeFailureHarness<T>(
  fn: (harness: {
    mock: Awaited<ReturnType<typeof startMockAnthropicService>>;
    server: Awaited<ReturnType<typeof startOpenyakServer>>;
  }) => Promise<T>,
): Promise<T> {
  const mock = await startMockAnthropicService();
  const env = { ...process.env };
  for (const key of providerEnvKeys) {
    delete env[key];
  }
  env.ANTHROPIC_BASE_URL = mock.baseUrl;
  const server = await startOpenyakServer(env);

  try {
    return await fn({ mock, server });
  } finally {
    await server.close();
    await mock.close();
  }
}

async function waitForThreadStatus(
  thread: { read(): Promise<{ state: { status: string } }> },
  expected: string,
  timeoutMs = 5_000,
): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  let lastStatus = "unknown";
  while (Date.now() < deadline) {
    const snapshot = await thread.read();
    lastStatus = snapshot.state.status;
    if (lastStatus === expected) {
      return;
    }
    await new Promise((resolve) => setTimeout(resolve, 50));
  }
  throw new Error(`thread did not reach ${expected}; last status was ${lastStatus}`);
}

test("attach-first client can create/list/get threads and stream a bash run", async () => {
  await withServerHarness(async ({ server }) => {
    const client = new OpenyakClient({
      baseUrl: server.baseUrl,
      timeoutMs: 30_000,
    });

    const thread = await client.createThread({
      model: "claude-sonnet-4-6",
      allowedTools: ["bash"],
    });

    assert.equal(thread.threadId, "thread-1");
    assert.equal(thread.snapshot?.state.status, "idle");

    const listed = await client.listThreads();
    assert.equal(listed.threads.length, 1);
    assert.equal(listed.threads[0]?.thread_id, thread.threadId);

    const fetched = await thread.read();
    assert.equal(fetched.thread_id, thread.threadId);
    assert.equal(fetched.config.allowed_tools[0], "bash");

    const streamed = await thread.runStreamed("PARITY_SCENARIO:bash_stdout_roundtrip");
    assert.equal(streamed.snapshot.thread_id, thread.threadId);
    assert.equal(streamed.accepted.run_id, "run-1");

    const eventTypes: string[] = [];
    let finalText = "";
    let sawUsage = false;
    for await (const event of streamed.events) {
      eventTypes.push(event.type);
      if (event.type === "assistant.text.delta") {
        finalText += event.payload.text;
      }
      if (event.type === "assistant.usage") {
        sawUsage = true;
      }
    }

    assert.deepEqual(eventTypes, [
      "run.started",
      "assistant.tool_use",
      "assistant.usage",
      "assistant.message_stop",
      "assistant.tool_result",
      "assistant.text.delta",
      "assistant.usage",
      "assistant.message_stop",
      "run.completed",
    ]);
    assert.match(finalText, /^bash completed:/);
    assert.equal(sawUsage, true);

    const completed = await thread.read();
    assert.equal(completed.state.status, "idle");
  });
});

test("buffered run returns awaiting_user_input and resumeUserInput continues the same run", async () => {
  await withServerHarness(async ({ server }) => {
    const client = new OpenyakClient({
      baseUrl: server.baseUrl,
      timeoutMs: 30_000,
    });

    const thread = await client.createThread({
      model: "claude-sonnet-4-6",
      allowedTools: ["read_file"],
    });

    const paused = await thread.run("PARITY_SCENARIO:request_user_input_roundtrip");
    assert.equal(paused.status, "awaiting_user_input");
    assert.equal(paused.runId, "run-1");
    assert.equal(paused.pendingUserInput.request_id, "req-user-input-roundtrip");
    assert.equal(paused.recoveredFromSnapshot, false);

    const resumed = await thread.resumeUserInput({
      requestId: paused.pendingUserInput.request_id,
      content: "feature",
      selectedOption: "feature",
    });

    assert.equal(resumed.status, "completed");
    assert.equal(resumed.runId, paused.runId);
    assert.match(resumed.finalText ?? "", /request-user-input completed: feature/);
    assert.equal(resumed.usage?.input_tokens, 26);
    assert.equal(resumed.recoveredFromSnapshot, false);

    const completed = await thread.read();
    assert.equal(completed.state.status, "idle");
    assert.equal(completed.state.pending_user_input, undefined);
  });
});

test("buffered run preserves runtime failure recovery metadata against a real local server", async () => {
  await withRuntimeFailureHarness(async ({ server }) => {
    const client = new OpenyakClient({
      baseUrl: server.baseUrl,
      timeoutMs: 30_000,
    });

    const thread = await client.createThread({
      model: "opus",
      allowedTools: [],
    });

    const failed = await thread.run("provider bootstrap should fail");

    assert.equal(failed.status, "failed");
    assert.equal(failed.runId, "run-1");
    assert.equal(failed.error.code, "runtime_error");
    assert.equal(failed.error.status, "failed");
    assert.equal(
      failed.error.lifecycle?.failure_kind,
      "daemon_thread_runtime_error",
    );
    assert.equal(
      failed.error.lifecycle?.recovery?.recovery_kind,
      "inspect_error_and_retry",
    );

    const completed = await thread.read();
    assert.equal(completed.state.status, "idle");
  });
});

test("runStreamed surfaces runtime failure recovery metadata against a real local server", async () => {
  await withRuntimeFailureHarness(async ({ server }) => {
    const client = new OpenyakClient({
      baseUrl: server.baseUrl,
      timeoutMs: 30_000,
    });

    const thread = await client.createThread({
      model: "opus",
      allowedTools: [],
    });

    const streamed = await thread.runStreamed("provider bootstrap should fail");
    const eventTypes: string[] = [];
    let failedEvent: { payload: { lifecycle?: { failure_kind?: string; recovery?: { recovery_kind?: string } } } } | undefined;

    for await (const event of streamed.events) {
      eventTypes.push(event.type);
      if (event.type === "run.failed") {
        failedEvent = event;
        break;
      }
    }

    assert.deepEqual(eventTypes, ["run.started", "run.failed"]);
    assert.equal(
      failedEvent?.payload.lifecycle?.failure_kind,
      "daemon_thread_runtime_error",
    );
    assert.equal(
      failedEvent?.payload.lifecycle?.recovery?.recovery_kind,
      "inspect_error_and_retry",
    );
  });
});

test("buffered run recovers interrupted snapshot truth after local server restart", async () => {
  const mock = await startMockAnthropicService();
  const workspace = await mkdtemp(path.join(os.tmpdir(), "openyak-sdk-restart-"));
  const env = {
    ...process.env,
    ANTHROPIC_API_KEY: "test-sdk-key",
    ANTHROPIC_BASE_URL: mock.baseUrl,
  };
  let server = await startOpenyakServerIn(workspace, env, { cleanupWorkspace: false });
  const bind = new URL(server.baseUrl).host;

  try {
    const client = new OpenyakClient({
      baseUrl: server.baseUrl,
      timeoutMs: 30_000,
    });

    const thread = await client.createThread({
      model: "claude-sonnet-4-6",
      allowedTools: ["read_file"],
    });
    const watcher = client.resumeThread(thread.threadId);

    const runPromise = thread.run("PARITY_SCENARIO:delayed_request_user_input_roundtrip");
    await waitForThreadStatus(watcher, "running");

    await server.close();
    server = await startOpenyakServerIn(workspace, env, {
      bind,
      cleanupWorkspace: false,
    });

    const interrupted = await runPromise;
    assert.equal(interrupted.status, "interrupted");
    assert.equal(interrupted.recoveredFromSnapshot, true);
    assert.match(interrupted.recoveryNote ?? "", /restart or shutdown/);
    assert.equal(
      interrupted.snapshot?.state.lifecycle?.failure_kind,
      "daemon_restart_interrupted_run",
    );
    assert.equal(
      interrupted.snapshot?.state.recovery?.recovery_kind,
      "reattach_or_retry",
    );
  } finally {
    await server.close();
    await mock.close();
    await rm(workspace, { recursive: true, force: true });
  }
});

test("runStreamed surfaces reconnect-required truth after local server restart", async () => {
  const mock = await startMockAnthropicService();
  const workspace = await mkdtemp(path.join(os.tmpdir(), "openyak-sdk-stream-restart-"));
  const env = {
    ...process.env,
    ANTHROPIC_API_KEY: "test-sdk-key",
    ANTHROPIC_BASE_URL: mock.baseUrl,
  };
  let server = await startOpenyakServerIn(workspace, env, { cleanupWorkspace: false });
  const bind = new URL(server.baseUrl).host;

  try {
    const client = new OpenyakClient({
      baseUrl: server.baseUrl,
      timeoutMs: 30_000,
    });

    const thread = await client.createThread({
      model: "claude-sonnet-4-6",
      allowedTools: ["read_file"],
    });

    const streamed = await thread.runStreamed(
      "PARITY_SCENARIO:delayed_request_user_input_roundtrip",
    );
    const iterator = streamed.events[Symbol.asyncIterator]();
    const first = await iterator.next();
    assert.equal(first.value?.type, "run.started");

    await server.close();
    server = await startOpenyakServerIn(workspace, env, {
      bind,
      cleanupWorkspace: false,
    });

    await assert.rejects(
      (async () => {
        while (true) {
          const next = await iterator.next();
          if (next.done) {
            break;
          }
        }
      })(),
      (error: unknown) =>
        error instanceof OpenyakReconnectRequiredError &&
        error.threadId === thread.threadId &&
        error.runId === "run-1",
    );
  } finally {
    await server.close();
    await mock.close();
    await rm(workspace, { recursive: true, force: true });
  }
});

test("attach-first resumeThread/read/listThreads preserve awaiting_user_input truth after local server restart", async () => {
  const mock = await startMockAnthropicService();
  const workspace = await mkdtemp(path.join(os.tmpdir(), "openyak-sdk-reattach-awaiting-"));
  const env = {
    ...process.env,
    ANTHROPIC_API_KEY: "test-sdk-key",
    ANTHROPIC_BASE_URL: mock.baseUrl,
  };
  let server = await startOpenyakServerIn(workspace, env, { cleanupWorkspace: false });
  const bind = new URL(server.baseUrl).host;

  try {
    const client = new OpenyakClient({
      baseUrl: server.baseUrl,
      timeoutMs: 30_000,
    });

    const thread = await client.createThread({
      model: "claude-sonnet-4-6",
      allowedTools: ["read_file"],
    });
    const accepted = await thread.startTurn("PARITY_SCENARIO:request_user_input_roundtrip");
    let waiting = await thread.read();
    if (waiting.state.status !== "awaiting_user_input") {
      await waitForThreadStatus(thread, "awaiting_user_input");
      waiting = await thread.read();
    }
    const pending = waiting.state.pending_user_input;
    assert.ok(pending);
    const threadId = thread.threadId;

    await server.close();
    server = await startOpenyakServerIn(workspace, env, {
      bind,
      cleanupWorkspace: false,
    });

    const restartedClient = new OpenyakClient({
      baseUrl: server.baseUrl,
      timeoutMs: 30_000,
    });
    const reattached = restartedClient.resumeThread(threadId);
    const recovered = await reattached.read();
    assert.equal(recovered.state.status, "awaiting_user_input");
    assert.equal(recovered.state.run_id, accepted.run_id);
    assert.ok(recovered.state.pending_user_input);
    assert.equal(recovered.state.pending_user_input.request_id, pending.request_id);

    const listed = await restartedClient.listThreads();
    const listedThread = listed.threads.find((value) => value.thread_id === threadId);
    assert.ok(listedThread);
    assert.equal(listedThread.state.status, "awaiting_user_input");
    assert.equal(listedThread.state.run_id, accepted.run_id);
    assert.equal(listedThread.state.pending_user_input?.request_id, pending.request_id);

    const resumed = await reattached.resumeUserInput({
      requestId: pending.request_id,
      content: "feature",
      selectedOption: "feature",
    });
    assert.equal(resumed.status, "completed");
    assert.equal(resumed.runId, accepted.run_id);
  } finally {
    await server.close();
    await mock.close();
    await rm(workspace, { recursive: true, force: true });
  }
});

test("attach-first resumeThread/read/listThreads preserve interrupted recovery truth after local server restart", async () => {
  const mock = await startMockAnthropicService();
  const workspace = await mkdtemp(path.join(os.tmpdir(), "openyak-sdk-reattach-interrupted-"));
  const env = {
    ...process.env,
    ANTHROPIC_API_KEY: "test-sdk-key",
    ANTHROPIC_BASE_URL: mock.baseUrl,
  };
  let server = await startOpenyakServerIn(workspace, env, { cleanupWorkspace: false });
  const bind = new URL(server.baseUrl).host;

  try {
    const client = new OpenyakClient({
      baseUrl: server.baseUrl,
      timeoutMs: 30_000,
    });

    const thread = await client.createThread({
      model: "claude-sonnet-4-6",
      allowedTools: ["read_file"],
    });
    const accepted = await thread.startTurn("PARITY_SCENARIO:delayed_request_user_input_roundtrip");
    await waitForThreadStatus(thread, "running");
    const threadId = thread.threadId;

    await server.close();
    server = await startOpenyakServerIn(workspace, env, {
      bind,
      cleanupWorkspace: false,
    });

    const restartedClient = new OpenyakClient({
      baseUrl: server.baseUrl,
      timeoutMs: 30_000,
    });
    const reattached = restartedClient.resumeThread(threadId);
    await waitForThreadStatus(reattached, "interrupted");
    const recovered = await reattached.read();
    assert.equal(recovered.state.status, "interrupted");
    assert.equal(recovered.state.run_id, accepted.run_id);
    assert.match(recovered.state.recovery_note ?? "", /restart or shutdown/);
    assert.equal(
      recovered.state.lifecycle?.failure_kind,
      "daemon_restart_interrupted_run",
    );
    assert.equal(
      recovered.state.recovery?.recovery_kind,
      "reattach_or_retry",
    );

    const listed = await restartedClient.listThreads();
    const listedThread = listed.threads.find((value) => value.thread_id === threadId);
    assert.ok(listedThread);
    assert.equal(listedThread.state.status, "interrupted");
    assert.equal(listedThread.state.run_id, accepted.run_id);
    assert.match(listedThread.state.recovery_note ?? "", /restart or shutdown/);
    assert.equal(
      listedThread.state.lifecycle?.failure_kind,
      "daemon_restart_interrupted_run",
    );
    assert.equal(
      listedThread.state.recovery?.recovery_kind,
      "reattach_or_retry",
    );
  } finally {
    await server.close();
    await mock.close();
    await rm(workspace, { recursive: true, force: true });
  }
});

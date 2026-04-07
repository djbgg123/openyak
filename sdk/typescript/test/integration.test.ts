import assert from "node:assert/strict";
import test from "node:test";

import { OpenyakClient } from "../src/index.js";
import { withServerHarness } from "./helpers/harness.js";

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

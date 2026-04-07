import { OpenyakClient } from "../src/index.js";

const baseUrl = process.env.OPENYAK_BASE_URL;
if (!baseUrl) {
  throw new Error("Set OPENYAK_BASE_URL to a running local `openyak server` address.");
}

const client = new OpenyakClient({ baseUrl });
const thread = await client.createThread({
  model: "claude-sonnet-4-6",
  allowedTools: ["bash"],
});

const result = await thread.run("PARITY_SCENARIO:bash_stdout_roundtrip");

console.log({
  threadId: result.threadId,
  runId: result.runId,
  status: result.status,
  finalText: result.finalText,
  usage: result.usage,
});

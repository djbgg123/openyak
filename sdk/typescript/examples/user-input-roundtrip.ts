import { OpenyakClient } from "../src/index.js";

const baseUrl = process.env.OPENYAK_BASE_URL;
if (!baseUrl) {
  throw new Error("Set OPENYAK_BASE_URL to a running local `openyak server` address.");
}
const operatorToken = process.env.OPENYAK_OPERATOR_TOKEN;

const client = new OpenyakClient({
  baseUrl,
  ...(operatorToken === undefined ? {} : { operatorToken }),
});
const thread = await client.createThread({
  model: "claude-sonnet-4-6",
  allowedTools: ["read_file"],
});

const paused = await thread.run("PARITY_SCENARIO:request_user_input_roundtrip");
if (paused.status !== "awaiting_user_input") {
  throw new Error(`Expected awaiting_user_input, received ${paused.status}`);
}

const resumed = await thread.resumeUserInput({
  requestId: paused.pendingUserInput.request_id,
  content: "feature",
  selectedOption: "feature",
});

console.log({
  paused,
  resumed,
});

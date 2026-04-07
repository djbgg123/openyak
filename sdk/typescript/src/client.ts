import {
  OpenyakApiError,
  OpenyakCompatibilityError,
  OpenyakProtocolError,
  OpenyakReconnectRequiredError,
  OpenyakResyncRequiredError,
} from "./errors.js";
import {
  SUPPORTED_PROTOCOL_VERSION,
  type ApiErrorEnvelope,
  type ConversationMessage,
  type CreateThreadOptions,
  type ListThreadsResponse,
  type RunCompletedEvent,
  type RunEvent,
  type RunFailedEvent,
  type RunOptions,
  type RunResult,
  type RunWaitingUserInputEvent,
  type RunStreamedResult,
  type SubmitUserInputOptions,
  type ThreadEvent,
  type ThreadSnapshot,
  type TokenUsage,
  type TurnAcceptedResponse,
  type UserInputAcceptedResponse,
  type UserInputRequestPayload,
} from "./types.js";

type FetchLike = typeof fetch;

interface RetryPolicy {
  attempts: number;
  delayMs: number;
}

interface JsonRequestOptions {
  body: unknown | undefined;
  signal: AbortSignal | undefined;
  expectProtocolVersion: boolean;
  disableRetry?: boolean | undefined;
}

interface RunResultContext {
  threadId: string;
  runId: string;
  events: RunEvent[];
  recoveredFromSnapshot: boolean;
  finalText?: string;
  usage?: TokenUsage;
  snapshot?: ThreadSnapshot;
}

type CompletedRunResult = Extract<RunResult, { status: "completed" }>;
type AwaitingUserInputRunResult = Extract<RunResult, { status: "awaiting_user_input" }>;
type FailedRunResult = Extract<RunResult, { status: "failed" }>;
type InterruptedRunResult = Extract<RunResult, { status: "interrupted" }>;

export interface OpenyakClientOptions {
  baseUrl: string;
  timeoutMs?: number;
  retry?: number | Partial<RetryPolicy>;
  fetch?: FetchLike;
}

export class OpenyakClient {
  readonly #baseUrl: string;
  readonly #timeoutMs: number;
  readonly #retry: RetryPolicy;
  readonly #fetch: FetchLike;

  constructor(options: OpenyakClientOptions) {
    this.#baseUrl = options.baseUrl.replace(/\/+$/, "");
    this.#timeoutMs = options.timeoutMs ?? 10_000;
    this.#retry = normalizeRetryPolicy(options.retry);
    this.#fetch = options.fetch ?? globalThis.fetch.bind(globalThis);
    if (!this.#baseUrl) {
      throw new OpenyakProtocolError("baseUrl is required");
    }
  }

  async createThreadSnapshot(
    options: CreateThreadOptions = {},
    runOptions: RunOptions = {},
  ): Promise<ThreadSnapshot> {
    return this.requestJson<ThreadSnapshot>("POST", "/v1/threads", {
      body: createThreadRequestBody(options),
      signal: runOptions.signal,
      expectProtocolVersion: true,
    });
  }

  async createThread(
    options: CreateThreadOptions = {},
    runOptions: RunOptions = {},
  ): Promise<Thread> {
    const snapshot = await this.createThreadSnapshot(options, runOptions);
    return new Thread(this, snapshot.thread_id, snapshot);
  }

  resumeThread(threadId: string): Thread {
    return new Thread(this, threadId);
  }

  async listThreads(runOptions: RunOptions = {}): Promise<ListThreadsResponse> {
    return this.requestJson<ListThreadsResponse>("GET", "/v1/threads", {
      body: undefined,
      signal: runOptions.signal,
      expectProtocolVersion: true,
    });
  }

  async getThread(threadId: string, runOptions: RunOptions = {}): Promise<ThreadSnapshot> {
    return this.requestJson<ThreadSnapshot>(
      "GET",
      `/v1/threads/${encodeURIComponent(threadId)}`,
      {
        body: undefined,
        signal: runOptions.signal,
        expectProtocolVersion: true,
      },
    );
  }

  async requestJson<T>(
    method: string,
    path: string,
    options: JsonRequestOptions,
  ): Promise<T> {
    const attempts = options.disableRetry ? 1 : this.#retry.attempts;
    let attempt = 0;
    let lastError: unknown;
    while (attempt < attempts) {
      attempt += 1;
      try {
        const response = await this.#fetchResponse(method, path, options.body, options.signal);
        if (!response.ok) {
          throw await toApiError(response);
        }
        const data = (await response.json()) as T;
        if (options.expectProtocolVersion) {
          assertProtocolVersion(readProtocolVersion(data), `${method} ${path}`);
        }
        return data;
      } catch (error) {
        lastError = error;
        if (!shouldRetry(error) || attempt >= attempts) {
          throw error;
        }
        await sleep(this.#retry.delayMs);
      }
    }
    throw lastError instanceof Error ? lastError : new Error(String(lastError));
  }

  async openEventStream(path: string, signal?: AbortSignal): Promise<Response> {
    const response = await this.#fetchResponse("GET", path, undefined, signal);
    if (!response.ok) {
      throw await toApiError(response);
    }
    return response;
  }

  async #fetchResponse(
    method: string,
    path: string,
    body: unknown,
    signal?: AbortSignal,
  ): Promise<Response> {
    const { signal: effectiveSignal, cleanup } = withTimeout(signal, this.#timeoutMs);
    try {
      const requestInit: RequestInit = {
        method,
        headers: {
          Accept: "application/json, text/event-stream",
          ...(body === undefined ? {} : { "Content-Type": "application/json" }),
        },
        signal: effectiveSignal,
      };
      if (body !== undefined) {
        requestInit.body = JSON.stringify(body);
      }
      return await this.#fetch(`${this.#baseUrl}${path}`, requestInit);
    } catch (error) {
      if (isAbortError(error) && signal?.aborted) {
        throw error;
      }
      if (isAbortError(error)) {
        throw new OpenyakProtocolError(`request timed out after ${this.#timeoutMs}ms`);
      }
      throw error;
    } finally {
      cleanup();
    }
  }
}

export class Thread {
  readonly #client: OpenyakClient;
  readonly threadId: string;
  #lastSnapshot: ThreadSnapshot | undefined;

  constructor(client: OpenyakClient, threadId: string, snapshot?: ThreadSnapshot) {
    this.#client = client;
    this.threadId = threadId;
    this.#lastSnapshot = snapshot;
  }

  get snapshot(): ThreadSnapshot | undefined {
    return this.#lastSnapshot;
  }

  async read(runOptions: RunOptions = {}): Promise<ThreadSnapshot> {
    const snapshot = await this.#client.getThread(this.threadId, runOptions);
    this.#lastSnapshot = snapshot;
    return snapshot;
  }

  async startTurn(
    message: string,
    runOptions: RunOptions = {},
  ): Promise<TurnAcceptedResponse> {
    return this.#requestJson<TurnAcceptedResponse>(
      "POST",
      `/v1/threads/${encodeURIComponent(this.threadId)}/turns`,
      { message },
      runOptions.signal,
    );
  }

  async submitUserInput(
    input: SubmitUserInputOptions,
    runOptions: RunOptions = {},
  ): Promise<UserInputAcceptedResponse> {
    return this.#requestJson<UserInputAcceptedResponse>(
      "POST",
      `/v1/threads/${encodeURIComponent(this.threadId)}/user-input`,
      {
        request_id: input.requestId,
        content: input.content,
        ...(input.selectedOption === undefined
          ? {}
          : { selected_option: input.selectedOption }),
      },
      runOptions.signal,
    );
  }

  async *streamEvents(runOptions: RunOptions = {}): AsyncGenerator<ThreadEvent> {
    const response = await this.#client.openEventStream(
      `/v1/threads/${encodeURIComponent(this.threadId)}/events`,
      runOptions.signal,
    );
    for await (const event of decodeThreadEvents(response)) {
      if (event.type === "thread.snapshot") {
        this.#lastSnapshot = event.payload;
      }
      yield event;
    }
  }

  async runStreamed(
    message: string,
    runOptions: RunOptions = {},
  ): Promise<RunStreamedResult> {
    return this.#prepareRunStream(
      () =>
        this.#requestJson<TurnAcceptedResponse>(
          "POST",
          `/v1/threads/${encodeURIComponent(this.threadId)}/turns`,
          { message },
          runOptions.signal,
        ),
      runOptions.signal,
    );
  }

  async run(message: string, runOptions: RunOptions = {}): Promise<RunResult> {
    const streamed = await this.runStreamed(message, runOptions);
    return this.#consumeBufferedRun(streamed);
  }

  async resumeUserInputStreamed(
    input: SubmitUserInputOptions,
    runOptions: RunOptions = {},
  ): Promise<RunStreamedResult> {
    return this.#prepareRunStream(
      () => this.submitUserInput(input, runOptions),
      runOptions.signal,
    );
  }

  async resumeUserInput(
    input: SubmitUserInputOptions,
    runOptions: RunOptions = {},
  ): Promise<RunResult> {
    const streamed = await this.resumeUserInputStreamed(input, runOptions);
    return this.#consumeBufferedRun(streamed);
  }

  async #prepareRunStream(
    submit:
      | (() => Promise<TurnAcceptedResponse>)
      | (() => Promise<UserInputAcceptedResponse>),
    signal?: AbortSignal,
  ): Promise<RunStreamedResult> {
    const controller = new AbortController();
    const combinedSignal = anySignal([signal, controller.signal]);
    const response = await this.#client.openEventStream(
      `/v1/threads/${encodeURIComponent(this.threadId)}/events`,
      combinedSignal,
    );
    const iterator = decodeThreadEvents(response)[Symbol.asyncIterator]();
    const first = await iterator.next();
    if (first.done) {
      controller.abort();
      throw new OpenyakProtocolError("event stream closed before thread.snapshot");
    }
    if (first.value.type !== "thread.snapshot") {
      controller.abort();
      throw new OpenyakProtocolError(
        `expected initial thread.snapshot event, received ${first.value.type}`,
      );
    }
    this.#lastSnapshot = first.value.payload;

    let accepted: TurnAcceptedResponse | UserInputAcceptedResponse;
    try {
      accepted = await submit();
    } catch (error) {
      controller.abort();
      throw error;
    }

    return {
      snapshot: first.value.payload,
      accepted,
      events: this.#runEvents(iterator, accepted.run_id),
      close: () => controller.abort(),
    };
  }

  async *#runEvents(
    iterator: AsyncIterator<ThreadEvent>,
    runId: string,
  ): AsyncGenerator<RunEvent> {
    while (true) {
      const next = await iterator.next();
      if (next.done) {
        break;
      }
      const event = next.value;
      if (event.type === "thread.resync_required") {
        throw new OpenyakResyncRequiredError(event);
      }
      if (event.type === "thread.snapshot") {
        this.#lastSnapshot = event.payload;
        continue;
      }
      if (event.run_id !== runId) {
        continue;
      }
      yield event;
      if (isTerminalRunEvent(event)) {
        return;
      }
    }
    throw new OpenyakReconnectRequiredError(this.threadId, runId);
  }

  async #consumeBufferedRun(streamed: RunStreamedResult): Promise<RunResult> {
    const events: RunEvent[] = [];
    let finalText = "";
    let latestUsage: TokenUsage | undefined;
    try {
      for await (const event of streamed.events) {
        events.push(event);
        if (event.type === "assistant.text.delta") {
          finalText += event.payload.text;
          continue;
        }
        if (event.type === "assistant.usage") {
          latestUsage = event.payload;
          continue;
        }
        if (event.type === "run.completed") {
          return completedRunResult(
            {
              threadId: this.threadId,
              runId: streamed.accepted.run_id,
              events,
              recoveredFromSnapshot: false,
              ...runResultContext(
                normalizeText(finalText),
                event.payload.cumulative_usage,
              ),
            },
            event,
          );
        }
        if (event.type === "run.waiting_user_input") {
          return awaitingUserInputRunResult(
            {
              threadId: this.threadId,
              runId: streamed.accepted.run_id,
              events,
              recoveredFromSnapshot: false,
              ...runResultContext(normalizeText(finalText), latestUsage),
            },
            event.payload,
            event,
          );
        }
        if (event.type === "run.failed") {
          return failedRunResult(
            {
              threadId: this.threadId,
              runId: streamed.accepted.run_id,
              events,
              recoveredFromSnapshot: false,
              ...runResultContext(normalizeText(finalText), latestUsage),
            },
            event.payload,
            event,
          );
        }
      }
      throw new OpenyakReconnectRequiredError(this.threadId, streamed.accepted.run_id);
    } catch (error) {
      if (
        error instanceof OpenyakResyncRequiredError ||
        error instanceof OpenyakReconnectRequiredError
      ) {
        return this.#reconcileBufferedRun(
          streamed.snapshot,
          streamed.accepted.run_id,
          events,
          finalText,
          latestUsage,
          error,
        );
      }
      throw error;
    } finally {
      streamed.close();
    }
  }

  async #reconcileBufferedRun(
    initialSnapshot: ThreadSnapshot,
    runId: string,
    events: RunEvent[],
    partialText: string,
    partialUsage: TokenUsage | undefined,
    error: OpenyakResyncRequiredError | OpenyakReconnectRequiredError,
  ): Promise<RunResult> {
    const latestSnapshot = await this.read().catch(() => undefined);
    if (!latestSnapshot) {
      throw error;
    }
    if (
      latestSnapshot.state.run_id !== undefined &&
      latestSnapshot.state.run_id !== runId
    ) {
      throw new OpenyakReconnectRequiredError(this.threadId, runId, latestSnapshot);
    }
    if (latestSnapshot.state.status === "running") {
      throw new OpenyakReconnectRequiredError(this.threadId, runId, latestSnapshot);
    }

    const appendedMessages = latestSnapshot.session.messages.slice(
      initialSnapshot.session.messages.length,
    );
    const derivedText = normalizeText(partialText) ?? normalizeText(messagesToText(appendedMessages));
    const derivedUsage = partialUsage ?? sumAssistantUsage(appendedMessages);

    if (latestSnapshot.state.status === "idle") {
      return completedRunResult(
        {
          threadId: this.threadId,
          runId,
          events,
          recoveredFromSnapshot: true,
          ...runResultContext(derivedText, derivedUsage, latestSnapshot),
        },
        null,
      );
    }

    if (
      latestSnapshot.state.status === "awaiting_user_input" &&
      latestSnapshot.state.pending_user_input
    ) {
      return awaitingUserInputRunResult(
        {
          threadId: this.threadId,
          runId,
          events,
          recoveredFromSnapshot: true,
          ...runResultContext(derivedText, derivedUsage, latestSnapshot),
        },
        latestSnapshot.state.pending_user_input,
        null,
      );
    }

    if (latestSnapshot.state.status === "interrupted") {
      return interruptedRunResult(
        {
          threadId: this.threadId,
          runId,
          events,
          recoveredFromSnapshot: true,
          ...runResultContext(derivedText, derivedUsage, latestSnapshot),
        },
        latestSnapshot.state.recovery_note,
      );
    }

    throw new OpenyakReconnectRequiredError(this.threadId, runId, latestSnapshot);
  }

  async #requestJson<T>(
    method: string,
    path: string,
    body: unknown,
    signal?: AbortSignal,
  ): Promise<T> {
    const response = await this.#client.requestJson<T>(method, path, {
      body,
      signal,
      expectProtocolVersion: true,
    });
    return response;
  }
}

function normalizeRetryPolicy(retry?: number | Partial<RetryPolicy>): RetryPolicy {
  if (typeof retry === "number") {
    return {
      attempts: Math.max(1, Math.trunc(retry)),
      delayMs: 150,
    };
  }
  return {
    attempts: Math.max(1, Math.trunc(retry?.attempts ?? 1)),
    delayMs: Math.max(0, Math.trunc(retry?.delayMs ?? 150)),
  };
}

function createThreadRequestBody(options: CreateThreadOptions): Record<string, unknown> {
  return {
    ...(options.cwd === undefined ? {} : { cwd: options.cwd }),
    ...(options.model === undefined ? {} : { model: options.model }),
    ...(options.permissionMode === undefined
      ? {}
      : { permission_mode: options.permissionMode }),
    ...(options.allowedTools === undefined ? {} : { allowed_tools: options.allowedTools }),
  };
}

function shouldRetry(error: unknown): boolean {
  return !(
    error instanceof OpenyakApiError ||
    error instanceof OpenyakCompatibilityError ||
    error instanceof OpenyakProtocolError ||
    error instanceof OpenyakResyncRequiredError ||
    error instanceof OpenyakReconnectRequiredError
  );
}

function withTimeout(signal: AbortSignal | undefined, timeoutMs: number) {
  const controller = new AbortController();
  const timeout = setTimeout(() => {
    controller.abort();
  }, timeoutMs);
  const onAbort = () => controller.abort(signal?.reason);
  if (signal) {
    if (signal.aborted) {
      controller.abort(signal.reason);
    } else {
      signal.addEventListener("abort", onAbort, { once: true });
    }
  }
  return {
    signal: controller.signal,
    cleanup: () => {
      clearTimeout(timeout);
      if (signal) {
        signal.removeEventListener("abort", onAbort);
      }
    },
  };
}

function anySignal(signals: Array<AbortSignal | undefined>): AbortSignal | undefined {
  const active = signals.filter((signal): signal is AbortSignal => signal !== undefined);
  if (active.length === 0) {
    return undefined;
  }
  if (active.length === 1) {
    return active[0];
  }
  const controller = new AbortController();
  const forwardAbort = (event: Event) => {
    const source = event.target as AbortSignal | null;
    controller.abort(source?.reason);
  };
  for (const signal of active) {
    if (signal.aborted) {
      controller.abort(signal.reason);
      break;
    }
    signal.addEventListener("abort", forwardAbort, { once: true });
  }
  return controller.signal;
}

function isAbortError(error: unknown): boolean {
  return error instanceof DOMException
    ? error.name === "AbortError"
    : error instanceof Error && error.name === "AbortError";
}

async function toApiError(response: Response): Promise<OpenyakApiError | OpenyakProtocolError> {
  let envelope: ApiErrorEnvelope | undefined;
  try {
    envelope = (await response.json()) as ApiErrorEnvelope;
  } catch {
    // Ignore parse failures and surface a protocol error below.
  }
  if (envelope && typeof envelope.code === "string" && typeof envelope.message === "string") {
    return new OpenyakApiError(response.status, envelope);
  }
  return new OpenyakProtocolError(
    `unexpected ${response.status} response from ${response.url || "openyak server"}`,
  );
}

async function* decodeThreadEvents(response: Response): AsyncGenerator<ThreadEvent> {
  if (!response.body) {
    throw new OpenyakProtocolError("event stream response did not include a body");
  }
  const reader = response.body.getReader();
  const decoder = new TextDecoder();
  let buffer = "";
  try {
    while (true) {
      const { done, value } = await reader.read();
      if (done) {
        buffer += decoder.decode();
        break;
      }
      buffer += decoder.decode(value, { stream: true }).replace(/\r\n/g, "\n").replace(/\r/g, "\n");
      for (const frame of drainFrames(() => buffer, (next) => {
        buffer = next;
      })) {
        const event = parseSseFrame(frame);
        if (!event) {
          continue;
        }
        yield parseThreadEvent(parseJson(event.data, "thread event"));
      }
    }
    for (const frame of drainFrames(() => buffer, (next) => {
      buffer = next;
    })) {
      const event = parseSseFrame(frame);
      if (!event) {
        continue;
      }
      yield parseThreadEvent(parseJson(event.data, "thread event"));
    }
  } finally {
    reader.releaseLock();
  }
}

function drainFrames(read: () => string, write: (next: string) => void): string[] {
  const frames: string[] = [];
  let buffer = read();
  while (true) {
    const boundary = buffer.indexOf("\n\n");
    if (boundary < 0) {
      write(buffer);
      return frames;
    }
    frames.push(buffer.slice(0, boundary));
    buffer = buffer.slice(boundary + 2);
  }
}

function parseSseFrame(frame: string): { event?: string; data: string } | null {
  const dataLines: string[] = [];
  let eventName: string | undefined;
  for (const line of frame.split("\n")) {
    if (!line || line.startsWith(":")) {
      continue;
    }
    if (line.startsWith("event:")) {
      eventName = line.slice(6).trim();
      continue;
    }
    if (line.startsWith("data:")) {
      dataLines.push(line.slice(5).trimStart());
    }
  }
  if (dataLines.length === 0) {
    return null;
  }
  if (eventName === undefined) {
    return {
      data: dataLines.join("\n"),
    };
  }
  return {
    event: eventName,
    data: dataLines.join("\n"),
  };
}

function parseThreadEvent(value: unknown): ThreadEvent {
  const record = asRecord(value, "thread event");
  const type = stringField(record, "type", "thread event");
  assertProtocolVersion(readProtocolVersion(record), `event ${type}`);
  switch (type) {
    case "thread.snapshot":
    case "run.started":
    case "assistant.text.delta":
    case "assistant.tool_use":
    case "assistant.tool_result":
    case "assistant.request_user_input":
    case "assistant.usage":
    case "assistant.message_stop":
    case "user_input.submitted":
    case "run.completed":
    case "run.waiting_user_input":
    case "run.failed":
    case "thread.resync_required":
      return record as unknown as ThreadEvent;
    default:
      throw new OpenyakCompatibilityError(
        `unsupported event type ${JSON.stringify(type)} for the current TypeScript alpha`,
        SUPPORTED_PROTOCOL_VERSION,
        String(readProtocolVersion(record) ?? "unknown"),
      );
  }
}

function asRecord(value: unknown, context: string): Record<string, unknown> {
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    throw new OpenyakProtocolError(`${context} must be a JSON object`);
  }
  return value as Record<string, unknown>;
}

function stringField(
  record: Record<string, unknown>,
  field: string,
  context: string,
): string {
  const value = record[field];
  if (typeof value !== "string") {
    throw new OpenyakProtocolError(`${context} is missing string field ${field}`);
  }
  return value;
}

function readProtocolVersion(value: unknown): string | undefined {
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    return undefined;
  }
  const protocolVersion = (value as Record<string, unknown>).protocol_version;
  return typeof protocolVersion === "string" ? protocolVersion : undefined;
}

function assertProtocolVersion(
  received: string | undefined,
  context: string,
): asserts received is typeof SUPPORTED_PROTOCOL_VERSION {
  if (received !== SUPPORTED_PROTOCOL_VERSION) {
    throw new OpenyakCompatibilityError(
      `${context} uses unsupported protocol_version ${JSON.stringify(received ?? "missing")}; expected ${SUPPORTED_PROTOCOL_VERSION}`,
      SUPPORTED_PROTOCOL_VERSION,
      received,
    );
  }
}

function isTerminalRunEvent(event: RunEvent): boolean {
  return (
    event.type === "run.completed" ||
    event.type === "run.failed" ||
    event.type === "run.waiting_user_input"
  );
}

function normalizeText(value: string): string | undefined {
  return value.length === 0 ? undefined : value;
}

function messagesToText(messages: ConversationMessage[]): string {
  return messages
    .filter((message) => message.role === "assistant")
    .flatMap((message) =>
      message.blocks.flatMap((block) =>
        block.type === "text" ? [block.text] : [],
      ),
    )
    .join("");
}

function sumAssistantUsage(messages: ConversationMessage[]): TokenUsage | undefined {
  const assistantUsages = messages
    .filter((message) => message.role === "assistant" && message.usage)
    .map((message) => message.usage as TokenUsage);
  if (assistantUsages.length === 0) {
    return undefined;
  }
  return assistantUsages.reduce<TokenUsage>(
    (total, usage) => ({
      input_tokens: total.input_tokens + usage.input_tokens,
      output_tokens: total.output_tokens + usage.output_tokens,
      cache_creation_input_tokens:
        total.cache_creation_input_tokens + usage.cache_creation_input_tokens,
      cache_read_input_tokens:
        total.cache_read_input_tokens + usage.cache_read_input_tokens,
    }),
    {
      input_tokens: 0,
      output_tokens: 0,
      cache_creation_input_tokens: 0,
      cache_read_input_tokens: 0,
    },
  );
}

function runResultContext(
  finalText: string | undefined,
  usage: TokenUsage | undefined,
  snapshot?: ThreadSnapshot,
): Pick<RunResultContext, "finalText" | "usage" | "snapshot"> {
  const context: Pick<RunResultContext, "finalText" | "usage" | "snapshot"> = {};
  if (finalText !== undefined) {
    context.finalText = finalText;
  }
  if (usage !== undefined) {
    context.usage = usage;
  }
  if (snapshot !== undefined) {
    context.snapshot = snapshot;
  }
  return context;
}

function parseJson(text: string, context: string): unknown {
  try {
    return JSON.parse(text) as unknown;
  } catch (error) {
    throw new OpenyakProtocolError(`failed to parse ${context} JSON`, { cause: error });
  }
}

function sleep(delayMs: number): Promise<void> {
  return new Promise((resolve) => {
    setTimeout(resolve, delayMs);
  });
}

function completedRunResult(
  context: RunResultContext,
  terminalEvent: RunCompletedEvent | null,
): CompletedRunResult {
  return {
    status: "completed",
    ...context,
    terminalEvent,
  };
}

function awaitingUserInputRunResult(
  context: RunResultContext,
  pendingUserInput: UserInputRequestPayload,
  terminalEvent: RunWaitingUserInputEvent | null,
): AwaitingUserInputRunResult {
  return {
    status: "awaiting_user_input",
    ...context,
    pendingUserInput,
    terminalEvent,
  };
}

function failedRunResult(
  context: RunResultContext,
  error: RunFailedEvent["payload"],
  terminalEvent: RunFailedEvent,
): FailedRunResult {
  return {
    status: "failed",
    ...context,
    error,
    terminalEvent,
  };
}

function interruptedRunResult(
  context: RunResultContext,
  recoveryNote?: string,
): InterruptedRunResult {
  return {
    status: "interrupted",
    ...context,
    ...(recoveryNote === undefined ? {} : { recoveryNote }),
    terminalEvent: null,
  };
}

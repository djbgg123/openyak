import type {
  ApiErrorEnvelope,
  JsonValue,
  ThreadResyncRequiredEvent,
  ThreadSnapshot,
} from "./types.js";

export class OpenyakError extends Error {
  constructor(message: string, options?: ErrorOptions) {
    super(message, options);
    this.name = new.target.name;
  }
}

export class OpenyakApiError extends OpenyakError {
  readonly status: number;
  readonly code: string;
  readonly details: JsonValue | undefined;

  constructor(status: number, envelope: ApiErrorEnvelope) {
    super(`${envelope.code}: ${envelope.message}`);
    this.status = status;
    this.code = envelope.code;
    this.details = envelope.details;
  }
}

export class OpenyakCompatibilityError extends OpenyakError {
  readonly received: string | undefined;
  readonly expected: string;

  constructor(message: string, expected: string, received?: string) {
    super(message);
    this.expected = expected;
    this.received = received;
  }
}

export class OpenyakProtocolError extends OpenyakError {}

export class OpenyakResyncRequiredError extends OpenyakError {
  readonly event: ThreadResyncRequiredEvent;

  constructor(event: ThreadResyncRequiredEvent) {
    super(
      `thread.resync_required for ${event.thread_id} after skipping ${event.payload.skipped} events`,
    );
    this.event = event;
  }
}

export class OpenyakReconnectRequiredError extends OpenyakError {
  readonly threadId: string;
  readonly runId: string;
  readonly latestSnapshot: ThreadSnapshot | undefined;

  constructor(threadId: string, runId: string, latestSnapshot?: ThreadSnapshot) {
    super(
      `stream disconnected before ${runId} reached a terminal event; replay is unavailable on the current local server contract`,
    );
    this.threadId = threadId;
    this.runId = runId;
    this.latestSnapshot = latestSnapshot;
  }
}

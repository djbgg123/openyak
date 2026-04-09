export const SUPPORTED_PROTOCOL_VERSION = "v1" as const;

export type ProtocolVersion = typeof SUPPORTED_PROTOCOL_VERSION;

export type PermissionMode =
  | "read-only"
  | "workspace-write"
  | "danger-full-access";

export type JsonPrimitive = string | number | boolean | null;
export type JsonValue =
  | JsonPrimitive
  | JsonValue[]
  | { [key: string]: JsonValue };

export interface TokenUsage {
  input_tokens: number;
  output_tokens: number;
  cache_creation_input_tokens: number;
  cache_read_input_tokens: number;
}

export type MessageRole = "system" | "user" | "assistant" | "tool";

export type ContentBlock =
  | { type: "text"; text: string }
  | { type: "tool_use"; id: string; name: string; input: string }
  | {
      type: "tool_result";
      tool_use_id: string;
      tool_name: string;
      output: string;
      is_error: boolean;
    }
  | {
      type: "user_input_request";
      request_id: string;
      prompt: string;
      options: string[];
      allow_freeform: boolean;
    }
  | {
      type: "user_input_response";
      request_id: string;
      content: string;
      selected_option?: string | null;
    };

export interface ConversationMessage {
  role: MessageRole;
  blocks: ContentBlock[];
  usage?: TokenUsage | null;
}

export interface SessionTelemetry {
  compacted_usage: TokenUsage;
  compacted_turns: number;
  accounting_status?: "complete" | "partial_legacy_compaction";
}

export interface SessionSnapshot {
  version: number;
  messages: ConversationMessage[];
  telemetry?: SessionTelemetry | null;
}

export interface ThreadConfigSnapshot {
  cwd: string;
  model: string;
  permission_mode: PermissionMode;
  allowed_tools: string[];
}

export interface ThreadContractSnapshot {
  truth_layer: string;
  operator_plane: string;
  persistence: string;
  attach_api: string;
}

export interface RecoveryGuidanceSnapshot {
  failure_kind: string;
  recovery_kind: string;
  recommended_actions: string[];
}

export interface LifecycleStateSnapshot {
  status: string;
  failure_kind?: string;
  recovery?: RecoveryGuidanceSnapshot;
}

export interface UserInputRequestPayload {
  request_id: string;
  prompt: string;
  options: string[];
  allow_freeform: boolean;
}

export interface ThreadStateSnapshot {
  status: "idle" | "running" | "awaiting_user_input" | "interrupted";
  lifecycle?: LifecycleStateSnapshot;
  run_id?: string;
  pending_user_input?: UserInputRequestPayload;
  recovery_note?: string;
  recovery?: RecoveryGuidanceSnapshot;
}

export interface ThreadSnapshot {
  protocol_version: ProtocolVersion;
  contract?: ThreadContractSnapshot;
  thread_id: string;
  created_at: number;
  updated_at: number;
  state: ThreadStateSnapshot;
  config: ThreadConfigSnapshot;
  session: SessionSnapshot;
}

export interface ThreadSummary {
  contract?: ThreadContractSnapshot;
  thread_id: string;
  created_at: number;
  updated_at: number;
  state: ThreadStateSnapshot;
  message_count: number;
}

export interface ListThreadsResponse {
  protocol_version: ProtocolVersion;
  threads: ThreadSummary[];
}

export interface TurnAcceptedResponse {
  protocol_version: ProtocolVersion;
  contract?: ThreadContractSnapshot;
  thread_id: string;
  run_id: string;
  lifecycle?: LifecycleStateSnapshot;
  status: "accepted";
}

export interface UserInputAcceptedResponse {
  protocol_version: ProtocolVersion;
  contract?: ThreadContractSnapshot;
  thread_id: string;
  run_id: string;
  request_id: string;
  lifecycle?: LifecycleStateSnapshot;
  status: "accepted";
}

export interface ApiErrorEnvelope {
  code: string;
  message: string;
  details?: JsonValue;
}

export interface CreateThreadOptions {
  cwd?: string;
  model?: string;
  permissionMode?: PermissionMode;
  allowedTools?: string[];
}

export interface SubmitUserInputOptions {
  requestId: string;
  content: string;
  selectedOption?: string;
}

export interface RunOptions {
  signal?: AbortSignal;
}

export interface ThreadEventBase<TType extends string, TPayload> {
  protocol_version: ProtocolVersion;
  thread_id: string;
  run_id?: string;
  sequence: number;
  timestamp_ms: number;
  type: TType;
  payload: TPayload;
}

export type ThreadSnapshotEvent = ThreadEventBase<"thread.snapshot", ThreadSnapshot>;

export type RunStartedEvent = ThreadEventBase<
  "run.started",
  {
    kind: "turn";
    message: string;
    status: "running";
  }
>;

export type AssistantTextDeltaEvent = ThreadEventBase<
  "assistant.text.delta",
  { text: string }
>;

export type AssistantToolUseEvent = ThreadEventBase<
  "assistant.tool_use",
  { id: string; name: string; input: JsonValue }
>;

export type AssistantToolResultEvent = ThreadEventBase<
  "assistant.tool_result",
  {
    tool_use_id: string;
    tool_name: string;
    output: string;
    is_error: boolean;
  }
>;

export type AssistantRequestUserInputEvent = ThreadEventBase<
  "assistant.request_user_input",
  UserInputRequestPayload
>;

export type AssistantUsageEvent = ThreadEventBase<"assistant.usage", TokenUsage>;

export type AssistantMessageStopEvent = ThreadEventBase<
  "assistant.message_stop",
  Record<string, never>
>;

export type UserInputSubmittedEvent = ThreadEventBase<
  "user_input.submitted",
  {
    request_id: string;
    content: string;
    selected_option?: string | null;
  }
>;

export type RunCompletedEvent = ThreadEventBase<
  "run.completed",
  {
    iterations: number;
    assistant_message_count: number;
    tool_result_count: number;
    cumulative_usage: TokenUsage;
  }
>;

export type RunWaitingUserInputEvent = ThreadEventBase<
  "run.waiting_user_input",
  UserInputRequestPayload
>;

export type RunFailedEvent = ThreadEventBase<
  "run.failed",
  {
    code: string;
    message: string;
  }
>;

export type ThreadResyncRequiredEvent = ThreadEventBase<
  "thread.resync_required",
  {
    skipped: number;
    snapshot: ThreadSnapshot;
  }
>;

export type ThreadEvent =
  | ThreadSnapshotEvent
  | RunStartedEvent
  | AssistantTextDeltaEvent
  | AssistantToolUseEvent
  | AssistantToolResultEvent
  | AssistantRequestUserInputEvent
  | AssistantUsageEvent
  | AssistantMessageStopEvent
  | UserInputSubmittedEvent
  | RunCompletedEvent
  | RunWaitingUserInputEvent
  | RunFailedEvent
  | ThreadResyncRequiredEvent;

export type RunEvent = Exclude<ThreadEvent, ThreadSnapshotEvent>;

export type RunTerminalEvent =
  | RunCompletedEvent
  | RunWaitingUserInputEvent
  | RunFailedEvent;

export interface RunStreamedResult {
  snapshot: ThreadSnapshot;
  accepted: TurnAcceptedResponse | UserInputAcceptedResponse;
  events: AsyncIterable<RunEvent>;
  close(): void;
}

interface RunResultBase {
  threadId: string;
  runId: string;
  events: RunEvent[];
  finalText?: string;
  usage?: TokenUsage;
  recoveredFromSnapshot: boolean;
  snapshot?: ThreadSnapshot;
}

export type RunResult =
  | (RunResultBase & {
      status: "completed";
      terminalEvent: RunCompletedEvent | null;
    })
  | (RunResultBase & {
      status: "awaiting_user_input";
      pendingUserInput: UserInputRequestPayload;
      terminalEvent: RunWaitingUserInputEvent | null;
    })
  | (RunResultBase & {
      status: "failed";
      error: RunFailedEvent["payload"];
      terminalEvent: RunFailedEvent;
    })
  | (RunResultBase & {
      status: "interrupted";
      recoveryNote?: string;
      terminalEvent: null;
    });

use std::collections::{BTreeSet, HashMap};
use std::convert::Infallible;
use std::env;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, RwLock};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_stream::stream;
use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use runtime::{
    ApiClient, ApiRequest, AssistantEvent, ConfigLoader, ContentBlock, ConversationMessage,
    ConversationRuntime, MessageRole, PendingUserInputRequest, PermissionEnforcer, PermissionMode,
    PermissionPolicy, ResolvedPermissionMode, RuntimeError, Session as RuntimeSession, ToolError,
    ToolExecutor, TurnSummary, UserInputOutcome, UserInputPrompter, UserInputRequest,
    UserInputResponse,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
pub use state_store::StateStoreError;
use state_store::{PersistedThreadRecord, SqliteThreadStore};
use tokio::sync::broadcast;
use tools::{GlobalToolRegistry, ProviderRuntimeClient};

mod state_store;

pub type SessionId = String;
pub type ThreadId = String;
pub type RunId = String;
type ThreadStore = Arc<RwLock<HashMap<ThreadId, Arc<ThreadRecord>>>>;
type RecordedAssistantBatches = Arc<Mutex<Vec<Vec<AssistantEvent>>>>;

const BROADCAST_CAPACITY: usize = 128;
const DEFAULT_MODEL_ALIAS: &str = "opus";
const PROTOCOL_VERSION: &str = "v1";

#[derive(Clone)]
pub struct AppState {
    cwd: PathBuf,
    store: Arc<SqliteThreadStore>,
    threads: ThreadStore,
    next_thread_id: Arc<AtomicU64>,
    next_run_id: Arc<AtomicU64>,
}

impl AppState {
    pub fn load_for_current_dir() -> Result<Self, StateStoreError> {
        let cwd = env::current_dir().map_err(|error| {
            StateStoreError::new(format!("failed to read current directory: {error}"))
        })?;
        Self::load_for_cwd(cwd)
    }

    pub fn load_for_cwd(cwd: impl AsRef<Path>) -> Result<Self, StateStoreError> {
        let cwd = cwd.as_ref().canonicalize().map_err(|error| {
            StateStoreError::new(format!(
                "failed to resolve server workspace `{}`: {error}",
                cwd.as_ref().display()
            ))
        })?;
        let store = Arc::new(SqliteThreadStore::open(&cwd)?);
        let persisted_threads = store.load_threads()?;
        let mut threads = HashMap::new();
        let mut max_thread_id = 0_u64;
        let mut max_run_id = 0_u64;

        for persisted in persisted_threads {
            max_thread_id = max_thread_id.max(id_counter(&persisted.thread_id, "thread-"));
            if let Some(run_id) = persisted.last_run_id.as_deref() {
                max_run_id = max_run_id.max(id_counter(run_id, "run-"));
            }

            let (inner, needs_rewrite) = thread_inner_from_persisted(persisted)?;
            if let Some(run_id) = inner.status.run_id() {
                max_run_id = max_run_id.max(id_counter(run_id, "run-"));
            }
            let record = Arc::new(ThreadRecord::from_inner(inner, Arc::clone(&store)));
            if needs_rewrite {
                record.persist_snapshot()?;
            }
            threads.insert(record.thread_id(), record);
        }

        Ok(Self {
            cwd,
            store,
            threads: Arc::new(RwLock::new(threads)),
            next_thread_id: Arc::new(AtomicU64::new(max_thread_id.saturating_add(1))),
            next_run_id: Arc::new(AtomicU64::new(max_run_id.saturating_add(1))),
        })
    }

    fn allocate_thread_id(&self) -> ThreadId {
        let id = self.next_thread_id.fetch_add(1, Ordering::Relaxed);
        format!("thread-{id}")
    }

    fn allocate_run_id(&self) -> RunId {
        let id = self.next_run_id.fetch_add(1, Ordering::Relaxed);
        format!("run-{id}")
    }

    fn insert_thread(&self, record: Arc<ThreadRecord>) {
        write_lock(&self.threads).insert(record.thread_id(), record);
    }

    fn thread(&self, id: &str) -> Result<Arc<ThreadRecord>, ApiError> {
        read_lock(&self.threads)
            .get(id)
            .cloned()
            .ok_or_else(|| not_found(format!("thread `{id}` not found")))
    }

    fn resolve_thread_config(
        &self,
        payload: CreateThreadRequest,
    ) -> ApiResult<ThreadExecutionConfig> {
        let cwd = resolve_thread_cwd(&self.cwd, payload.cwd.as_deref())?;
        let runtime_config = ConfigLoader::default_for(&cwd)
            .load()
            .map_err(|error| internal_error(error.to_string()))?;
        let tool_registry = GlobalToolRegistry::builtin();
        let allowed_tools = tool_registry
            .normalize_allowed_tools(&payload.allowed_tools)
            .map_err(|error| bad_request(error, None))?
            .unwrap_or_else(|| {
                tool_registry
                    .definitions(None)
                    .into_iter()
                    .map(|definition| definition.name)
                    .collect()
            });

        let model = payload
            .model
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| {
                runtime_config
                    .model()
                    .unwrap_or(DEFAULT_MODEL_ALIAS)
                    .to_string()
            });
        let permission_mode = match payload.permission_mode {
            Some(mode) => parse_server_permission_mode(&mode)?,
            None => runtime_config.permission_mode().map_or(
                PermissionMode::DangerFullAccess,
                permission_mode_from_resolved,
            ),
        };

        Ok(ThreadExecutionConfig {
            cwd,
            model,
            permission_mode,
            allowed_tools,
        })
    }
}

#[derive(Debug, Clone)]
struct ThreadExecutionConfig {
    cwd: PathBuf,
    model: String,
    permission_mode: PermissionMode,
    allowed_tools: BTreeSet<String>,
}

impl ThreadExecutionConfig {
    fn snapshot(&self) -> ThreadConfigSnapshot {
        ThreadConfigSnapshot {
            cwd: self.cwd.display().to_string(),
            model: self.model.clone(),
            permission_mode: self.permission_mode.as_str().to_string(),
            allowed_tools: self.allowed_tools.iter().cloned().collect(),
        }
    }
}

#[derive(Debug, Clone)]
enum ThreadStatus {
    Idle,
    Running {
        run_id: RunId,
    },
    AwaitingUserInput {
        run_id: RunId,
        request: PendingUserInputRequest,
    },
    Interrupted {
        run_id: Option<RunId>,
        recovery_note: String,
    },
}

impl ThreadStatus {
    fn run_id(&self) -> Option<&str> {
        match self {
            Self::Idle => None,
            Self::Running { run_id } | Self::AwaitingUserInput { run_id, .. } => Some(run_id),
            Self::Interrupted { run_id, .. } => run_id.as_deref(),
        }
    }

    fn snapshot(&self) -> ThreadStateSnapshot {
        match self {
            Self::Idle => ThreadStateSnapshot {
                status: "idle".to_string(),
                run_id: None,
                pending_user_input: None,
                recovery_note: None,
            },
            Self::Running { run_id } => ThreadStateSnapshot {
                status: "running".to_string(),
                run_id: Some(run_id.clone()),
                pending_user_input: None,
                recovery_note: None,
            },
            Self::AwaitingUserInput { run_id, request } => ThreadStateSnapshot {
                status: "awaiting_user_input".to_string(),
                run_id: Some(run_id.clone()),
                pending_user_input: Some(UserInputRequestPayload::from_request(request.clone())),
                recovery_note: None,
            },
            Self::Interrupted {
                run_id,
                recovery_note,
            } => ThreadStateSnapshot {
                status: "interrupted".to_string(),
                run_id: run_id.clone(),
                pending_user_input: None,
                recovery_note: Some(recovery_note.clone()),
            },
        }
    }
}

#[derive(Debug, Clone)]
struct ThreadInner {
    id: ThreadId,
    created_at: u64,
    updated_at: u64,
    session: RuntimeSession,
    config: ThreadExecutionConfig,
    status: ThreadStatus,
    last_sequence: u64,
}

#[derive(Clone)]
struct ThreadRecord {
    inner: Arc<Mutex<ThreadInner>>,
    store: Arc<SqliteThreadStore>,
    protocol_events: broadcast::Sender<ThreadEventEnvelope>,
    legacy_events: broadcast::Sender<SessionEvent>,
}

impl ThreadRecord {
    fn new(
        id: ThreadId,
        config: ThreadExecutionConfig,
        store: Arc<SqliteThreadStore>,
    ) -> Result<Self, StateStoreError> {
        let now = unix_timestamp_millis();
        let record = Self::from_inner(
            ThreadInner {
                id,
                created_at: now,
                updated_at: now,
                session: RuntimeSession::new(),
                config,
                status: ThreadStatus::Idle,
                last_sequence: 0,
            },
            store,
        );
        record.persist_snapshot()?;
        Ok(record)
    }

    fn from_inner(inner: ThreadInner, store: Arc<SqliteThreadStore>) -> Self {
        let (protocol_events, _) = broadcast::channel(BROADCAST_CAPACITY);
        let (legacy_events, _) = broadcast::channel(BROADCAST_CAPACITY);
        Self {
            inner: Arc::new(Mutex::new(inner)),
            store,
            protocol_events,
            legacy_events,
        }
    }

    fn thread_id(&self) -> ThreadId {
        lock(&self.inner).id.clone()
    }

    fn subscribe_protocol(&self) -> broadcast::Receiver<ThreadEventEnvelope> {
        self.protocol_events.subscribe()
    }

    fn subscribe_legacy(&self) -> broadcast::Receiver<SessionEvent> {
        self.legacy_events.subscribe()
    }

    fn snapshot(&self) -> ThreadSnapshot {
        let inner = lock(&self.inner);
        snapshot_from_inner(&inner)
    }

    fn snapshot_event(&self) -> ThreadEventEnvelope {
        let inner = lock(&self.inner);
        ThreadEventEnvelope {
            protocol_version: PROTOCOL_VERSION.to_string(),
            thread_id: inner.id.clone(),
            run_id: inner.status.run_id().map(ToOwned::to_owned),
            sequence: inner.last_sequence,
            timestamp_ms: unix_timestamp_millis(),
            event_type: "thread.snapshot".to_string(),
            payload: to_value(snapshot_from_inner(&inner)),
        }
    }

    fn current_session_snapshot(&self) -> SessionEvent {
        let inner = lock(&self.inner);
        SessionEvent::Snapshot {
            session_id: inner.id.clone(),
            session: inner.session.clone(),
        }
    }

    fn worker_base_session(&self) -> RuntimeSession {
        lock(&self.inner).session.clone()
    }

    fn session_summary(&self) -> SessionSummary {
        let inner = lock(&self.inner);
        SessionSummary {
            id: inner.id.clone(),
            created_at: inner.created_at,
            message_count: inner.session.messages.len(),
        }
    }

    fn session_details(&self) -> SessionDetailsResponse {
        let inner = lock(&self.inner);
        SessionDetailsResponse {
            id: inner.id.clone(),
            created_at: inner.created_at,
            session: inner.session.clone(),
        }
    }

    fn persist_snapshot(&self) -> Result<(), StateStoreError> {
        let inner = lock(&self.inner);
        self.persist_inner(&inner)
    }

    fn persist_inner(&self, inner: &ThreadInner) -> Result<(), StateStoreError> {
        self.store
            .upsert_thread(&persisted_thread_from_inner(inner))
    }

    fn start_turn(&self, run_id: &str, message: &str) -> Result<usize, ApiError> {
        let optimistic_message = ConversationMessage::user_text(message.to_string());
        let optimistic_len = {
            let mut inner = lock(&self.inner);
            let previous = inner.clone();
            match inner.status {
                ThreadStatus::Idle | ThreadStatus::Interrupted { .. } => {}
                _ => {
                    return Err(conflict(
                        "thread already has an active or blocked run".to_string(),
                        Some(json!({ "status": inner.status.snapshot() })),
                    ))
                }
            }
            inner.session.messages.push(optimistic_message.clone());
            inner.updated_at = unix_timestamp_millis();
            inner.status = ThreadStatus::Running {
                run_id: run_id.to_string(),
            };
            if let Err(error) = self.persist_inner(&inner) {
                *inner = previous;
                return Err(internal_error(error.to_string()));
            }
            inner.session.messages.len()
        };

        self.broadcast_legacy_message(optimistic_message);
        self.emit_protocol_event(
            "run.started",
            Some(run_id),
            json!({
                "kind": "turn",
                "message": message,
                "status": "running",
            }),
        );
        Ok(optimistic_len)
    }

    fn submit_user_input(&self, response: &UserInputResponse) -> Result<(RunId, usize), ApiError> {
        let optimistic_message = ConversationMessage::user_input_response(
            response.request_id.clone(),
            response.content.clone(),
            response.selected_option.clone(),
        );

        let (run_id, optimistic_len) = {
            let mut inner = lock(&self.inner);
            let previous = inner.clone();
            let ThreadStatus::AwaitingUserInput { run_id, request } = &inner.status else {
                return Err(conflict(
                    "thread has no pending request-user-input item".to_string(),
                    Some(json!({ "status": inner.status.snapshot() })),
                ));
            };
            if request.request_id != response.request_id {
                return Err(conflict(
                    format!(
                        "submitted request_id `{}` does not match pending request `{}`",
                        response.request_id, request.request_id
                    ),
                    Some(json!({
                        "expected_request_id": request.request_id,
                        "submitted_request_id": response.request_id,
                    })),
                ));
            }

            let run_id = run_id.clone();
            inner.session.messages.push(optimistic_message.clone());
            inner.updated_at = unix_timestamp_millis();
            inner.status = ThreadStatus::Running {
                run_id: run_id.clone(),
            };
            if let Err(error) = self.persist_inner(&inner) {
                *inner = previous;
                return Err(internal_error(error.to_string()));
            }
            (run_id, inner.session.messages.len())
        };

        self.broadcast_legacy_message(optimistic_message);
        self.emit_protocol_event(
            "user_input.submitted",
            Some(&run_id),
            json!({
                "request_id": response.request_id,
                "content": response.content,
                "selected_option": response.selected_option,
            }),
        );
        Ok((run_id, optimistic_len))
    }

    fn append_session_message(&self, message: ConversationMessage) -> Result<(), ApiError> {
        {
            let mut inner = lock(&self.inner);
            let previous = inner.clone();
            match inner.status {
                ThreadStatus::Idle | ThreadStatus::Interrupted { .. } => {
                    inner.session.messages.push(message.clone());
                    inner.updated_at = unix_timestamp_millis();
                    if let Err(error) = self.persist_inner(&inner) {
                        *inner = previous;
                        return Err(internal_error(error.to_string()));
                    }
                }
                _ => return Err(conflict(
                    "legacy /sessions message append is unavailable while a thread run is active"
                        .to_string(),
                    Some(json!({ "status": inner.status.snapshot() })),
                )),
            }
        }
        self.broadcast_legacy_message(message);
        Ok(())
    }

    fn complete_run(
        &self,
        run_id: &str,
        final_session: &RuntimeSession,
        optimistic_len: usize,
        recorded_batches: &[Vec<AssistantEvent>],
        result: Result<TurnSummary, RuntimeError>,
    ) {
        let pending_request = final_session.pending_user_input_request();
        let persistence_error = {
            let mut inner = lock(&self.inner);
            {
                inner.session = final_session.clone();
                inner.updated_at = unix_timestamp_millis();
                inner.status = if let Some(request) = pending_request.clone() {
                    ThreadStatus::AwaitingUserInput {
                        run_id: run_id.to_string(),
                        request,
                    }
                } else {
                    ThreadStatus::Idle
                };
            }
            self.persist_inner(&inner).err()
        };

        if let Some(error) = persistence_error {
            self.emit_protocol_event(
                "run.failed",
                Some(run_id),
                json!({
                    "code": "storage_error",
                    "message": error.to_string(),
                }),
            );
            return;
        }

        for message in final_session.messages.iter().skip(optimistic_len).cloned() {
            self.broadcast_legacy_message(message);
        }

        emit_protocol_replay(
            self,
            run_id,
            recorded_batches,
            final_session.messages.iter().skip(optimistic_len),
        );

        match result {
            Ok(summary) => {
                self.emit_protocol_event(
                    "run.completed",
                    Some(run_id),
                    json!({
                        "iterations": summary.iterations,
                        "assistant_message_count": summary.assistant_messages.len(),
                        "tool_result_count": summary.tool_results.len(),
                        "cumulative_usage": summary.usage,
                    }),
                );
            }
            Err(error) => {
                if let Some(request) = pending_request {
                    self.emit_protocol_event(
                        "run.waiting_user_input",
                        Some(run_id),
                        json!({
                            "request_id": request.request_id,
                            "prompt": request.prompt,
                            "options": request.options,
                            "allow_freeform": request.allow_freeform,
                        }),
                    );
                } else {
                    self.emit_protocol_event(
                        "run.failed",
                        Some(run_id),
                        json!({
                            "code": "runtime_error",
                            "message": error.to_string(),
                        }),
                    );
                }
            }
        }
    }

    fn emit_protocol_event(&self, event_type: &str, run_id: Option<&str>, payload: Value) {
        let envelope = {
            let mut inner = lock(&self.inner);
            inner.last_sequence = inner.last_sequence.saturating_add(1);
            ThreadEventEnvelope {
                protocol_version: PROTOCOL_VERSION.to_string(),
                thread_id: inner.id.clone(),
                run_id: run_id.map(ToOwned::to_owned),
                sequence: inner.last_sequence,
                timestamp_ms: unix_timestamp_millis(),
                event_type: event_type.to_string(),
                payload,
            }
        };
        let _ = self.protocol_events.send(envelope);
    }

    fn broadcast_legacy_message(&self, message: ConversationMessage) {
        let session_id = self.thread_id();
        let _ = self.legacy_events.send(SessionEvent::Message {
            session_id,
            message,
        });
    }
}

#[derive(Debug, Serialize)]
struct ErrorEnvelope {
    code: String,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    details: Option<Value>,
}

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    code: String,
    message: String,
    details: Option<Value>,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(ErrorEnvelope {
                code: self.code,
                message: self.message,
                details: self.details,
            }),
        )
            .into_response()
    }
}

type ApiResult<T> = Result<T, ApiError>;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
enum SessionEvent {
    Snapshot {
        session_id: SessionId,
        session: RuntimeSession,
    },
    Message {
        session_id: SessionId,
        message: ConversationMessage,
    },
}

impl SessionEvent {
    fn event_name(&self) -> &'static str {
        match self {
            Self::Snapshot { .. } => "snapshot",
            Self::Message { .. } => "message",
        }
    }

    fn to_sse_event(&self) -> Event {
        Event::default()
            .event(self.event_name())
            .data(serde_json::to_string(self).expect("legacy session event should serialize"))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CreateSessionResponse {
    pub session_id: SessionId,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionSummary {
    pub id: SessionId,
    pub created_at: u64,
    pub message_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ListSessionsResponse {
    pub sessions: Vec<SessionSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionDetailsResponse {
    pub id: SessionId,
    pub created_at: u64,
    pub session: RuntimeSession,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SendMessageRequest {
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ThreadConfigSnapshot {
    pub cwd: String,
    pub model: String,
    pub permission_mode: String,
    pub allowed_tools: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UserInputRequestPayload {
    pub request_id: String,
    pub prompt: String,
    pub options: Vec<String>,
    pub allow_freeform: bool,
}

impl UserInputRequestPayload {
    fn from_request(request: PendingUserInputRequest) -> Self {
        Self {
            request_id: request.request_id,
            prompt: request.prompt,
            options: request.options,
            allow_freeform: request.allow_freeform,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ThreadStateSnapshot {
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<RunId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pending_user_input: Option<UserInputRequestPayload>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recovery_note: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ThreadSnapshot {
    pub protocol_version: String,
    pub thread_id: ThreadId,
    pub created_at: u64,
    pub updated_at: u64,
    pub state: ThreadStateSnapshot,
    pub config: ThreadConfigSnapshot,
    pub session: RuntimeSession,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ThreadSummary {
    pub thread_id: ThreadId,
    pub created_at: u64,
    pub updated_at: u64,
    pub state: ThreadStateSnapshot,
    pub message_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ListThreadsResponse {
    pub protocol_version: String,
    pub threads: Vec<ThreadSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TurnAcceptedResponse {
    pub protocol_version: String,
    pub thread_id: ThreadId,
    pub run_id: RunId,
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UserInputAcceptedResponse {
    pub protocol_version: String,
    pub thread_id: ThreadId,
    pub run_id: RunId,
    pub request_id: String,
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ThreadEventEnvelope {
    pub protocol_version: String,
    pub thread_id: ThreadId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<RunId>,
    pub sequence: u64,
    pub timestamp_ms: u64,
    #[serde(rename = "type")]
    pub event_type: String,
    pub payload: Value,
}

impl ThreadEventEnvelope {
    fn to_sse_event(&self) -> Event {
        Event::default()
            .event(&self.event_type)
            .data(serde_json::to_string(self).expect("thread event should serialize"))
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct CreateThreadRequest {
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default, alias = "permissionMode")]
    pub permission_mode: Option<String>,
    #[serde(default, alias = "allowedTools")]
    pub allowed_tools: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StartTurnRequest {
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubmitUserInputRequest {
    pub request_id: String,
    pub content: String,
    #[serde(default)]
    pub selected_option: Option<String>,
}

pub fn app(state: AppState) -> Router {
    Router::new()
        .route("/v1/threads", post(create_thread).get(list_threads))
        .route("/v1/threads/{id}", get(get_thread))
        .route("/v1/threads/{id}/events", get(stream_thread_events))
        .route("/v1/threads/{id}/turns", post(start_thread_turn))
        .route(
            "/v1/threads/{id}/user-input",
            post(submit_thread_user_input),
        )
        .route("/sessions", post(create_session).get(list_sessions))
        .route("/sessions/{id}", get(get_session))
        .route("/sessions/{id}/events", get(stream_session_events))
        .route("/sessions/{id}/message", post(send_message))
        .with_state(state)
}

pub async fn serve(listener: tokio::net::TcpListener, state: AppState) -> std::io::Result<()> {
    axum::serve(listener, app(state)).await
}

async fn create_thread(
    State(state): State<AppState>,
    Json(payload): Json<CreateThreadRequest>,
) -> ApiResult<(StatusCode, Json<ThreadSnapshot>)> {
    let thread_id = state.allocate_thread_id();
    let config = state.resolve_thread_config(payload)?;
    let record = Arc::new(
        ThreadRecord::new(thread_id, config, Arc::clone(&state.store))
            .map_err(|error| internal_error(error.to_string()))?,
    );
    let snapshot = record.snapshot();
    state.insert_thread(record);
    Ok((StatusCode::CREATED, Json(snapshot)))
}

async fn list_threads(State(state): State<AppState>) -> Json<ListThreadsResponse> {
    let mut threads = read_lock(&state.threads)
        .values()
        .map(|record| {
            let snapshot = record.snapshot();
            ThreadSummary {
                thread_id: snapshot.thread_id,
                created_at: snapshot.created_at,
                updated_at: snapshot.updated_at,
                state: snapshot.state,
                message_count: snapshot.session.messages.len(),
            }
        })
        .collect::<Vec<_>>();
    threads.sort_by(|left, right| left.thread_id.cmp(&right.thread_id));

    Json(ListThreadsResponse {
        protocol_version: PROTOCOL_VERSION.to_string(),
        threads,
    })
}

async fn get_thread(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<ThreadId>,
) -> ApiResult<Json<ThreadSnapshot>> {
    Ok(Json(state.thread(&id)?.snapshot()))
}

async fn start_thread_turn(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<ThreadId>,
    Json(payload): Json<StartTurnRequest>,
) -> ApiResult<(StatusCode, Json<TurnAcceptedResponse>)> {
    if payload.message.trim().is_empty() {
        return Err(bad_request(
            "turn message must not be empty".to_string(),
            None,
        ));
    }

    let record = state.thread(&id)?;
    let worker_base_session = record.worker_base_session();
    let run_id = state.allocate_run_id();
    let optimistic_len = record.start_turn(&run_id, &payload.message)?;
    spawn_thread_run(
        record,
        run_id.clone(),
        worker_base_session,
        optimistic_len,
        RunInvocation::Turn {
            message: payload.message,
        },
    );

    Ok((
        StatusCode::ACCEPTED,
        Json(TurnAcceptedResponse {
            protocol_version: PROTOCOL_VERSION.to_string(),
            thread_id: id,
            run_id,
            status: "accepted".to_string(),
        }),
    ))
}

async fn submit_thread_user_input(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<ThreadId>,
    Json(payload): Json<SubmitUserInputRequest>,
) -> ApiResult<(StatusCode, Json<UserInputAcceptedResponse>)> {
    if payload.content.trim().is_empty() {
        return Err(bad_request(
            "user-input content must not be empty".to_string(),
            None,
        ));
    }

    let record = state.thread(&id)?;
    let worker_base_session = record.worker_base_session();
    let response = UserInputResponse {
        request_id: payload.request_id.clone(),
        content: payload.content,
        selected_option: payload.selected_option,
    };
    let (run_id, optimistic_len) = record.submit_user_input(&response)?;
    spawn_thread_run(
        record,
        run_id.clone(),
        worker_base_session,
        optimistic_len,
        RunInvocation::Resume { response },
    );

    Ok((
        StatusCode::ACCEPTED,
        Json(UserInputAcceptedResponse {
            protocol_version: PROTOCOL_VERSION.to_string(),
            thread_id: id,
            run_id,
            request_id: payload.request_id,
            status: "accepted".to_string(),
        }),
    ))
}

async fn stream_thread_events(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<ThreadId>,
) -> ApiResult<impl IntoResponse> {
    let record = state.thread(&id)?;
    let (mut receiver, snapshot) = subscribe_thread_stream(&record);
    let mut snapshot_sequence = snapshot.sequence;
    let stream = stream! {
        yield Ok::<Event, Infallible>(snapshot.to_sse_event());
        loop {
            let Some(event) = recv_thread_stream_event(&record, &mut receiver, &mut snapshot_sequence).await else {
                break;
            };
            let should_break = event.event_type == "thread.resync_required";
            yield Ok::<Event, Infallible>(event.to_sse_event());
            if should_break {
                break;
            }
        }
    };

    Ok(Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15))))
}

async fn create_session(
    State(state): State<AppState>,
) -> ApiResult<(StatusCode, Json<CreateSessionResponse>)> {
    let thread_id = state.allocate_thread_id();
    let config = state.resolve_thread_config(CreateThreadRequest::default())?;
    let record = Arc::new(
        ThreadRecord::new(thread_id.clone(), config, Arc::clone(&state.store))
            .map_err(|error| internal_error(error.to_string()))?,
    );
    state.insert_thread(record);
    Ok((
        StatusCode::CREATED,
        Json(CreateSessionResponse {
            session_id: thread_id,
        }),
    ))
}

async fn list_sessions(State(state): State<AppState>) -> Json<ListSessionsResponse> {
    let mut sessions = read_lock(&state.threads)
        .values()
        .map(|record| record.session_summary())
        .collect::<Vec<_>>();
    sessions.sort_by(|left, right| left.id.cmp(&right.id));
    Json(ListSessionsResponse { sessions })
}

async fn get_session(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<SessionId>,
) -> ApiResult<Json<SessionDetailsResponse>> {
    Ok(Json(state.thread(&id)?.session_details()))
}

async fn send_message(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<SessionId>,
    Json(payload): Json<SendMessageRequest>,
) -> ApiResult<StatusCode> {
    if payload.message.trim().is_empty() {
        return Err(bad_request("message must not be empty".to_string(), None));
    }

    state
        .thread(&id)?
        .append_session_message(ConversationMessage::user_text(payload.message))?;
    Ok(StatusCode::NO_CONTENT)
}

async fn stream_session_events(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<SessionId>,
) -> ApiResult<impl IntoResponse> {
    let record = state.thread(&id)?;
    let (mut receiver, snapshot) = subscribe_session_stream(&record);
    let stream = stream! {
        yield Ok::<Event, Infallible>(snapshot.to_sse_event());
        loop {
            let Some(event) = recv_legacy_session_event(&mut receiver).await else {
                break;
            };
            yield Ok::<Event, Infallible>(event.to_sse_event());
            if receiver.is_closed() {
                break;
            }
        }
    };

    Ok(Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15))))
}

fn subscribe_thread_stream(
    record: &ThreadRecord,
) -> (
    broadcast::Receiver<ThreadEventEnvelope>,
    ThreadEventEnvelope,
) {
    let receiver = record.subscribe_protocol();
    let snapshot = record.snapshot_event();
    (receiver, snapshot)
}

fn subscribe_session_stream(
    record: &ThreadRecord,
) -> (broadcast::Receiver<SessionEvent>, SessionEvent) {
    let receiver = record.subscribe_legacy();
    let snapshot = record.current_session_snapshot();
    (receiver, snapshot)
}

async fn recv_thread_stream_event(
    record: &ThreadRecord,
    receiver: &mut broadcast::Receiver<ThreadEventEnvelope>,
    snapshot_sequence: &mut u64,
) -> Option<ThreadEventEnvelope> {
    loop {
        match receiver.recv().await {
            Ok(event) => {
                if event.sequence <= *snapshot_sequence {
                    continue;
                }
                *snapshot_sequence = event.sequence;
                return Some(event);
            }
            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                return Some(thread_resync_required_event(record, skipped))
            }
            Err(broadcast::error::RecvError::Closed) => return None,
        }
    }
}

async fn recv_legacy_session_event(
    receiver: &mut broadcast::Receiver<SessionEvent>,
) -> Option<SessionEvent> {
    receiver.recv().await.ok()
}

fn spawn_thread_run(
    record: Arc<ThreadRecord>,
    run_id: RunId,
    base_session: RuntimeSession,
    optimistic_len: usize,
    invocation: RunInvocation,
) {
    thread::spawn(move || {
        let config = {
            let inner = lock(&record.inner);
            inner.config.clone()
        };

        let execution = execute_run(base_session, &config, &invocation);
        record.complete_run(
            &run_id,
            &execution.final_session,
            optimistic_len,
            &execution.recorded_batches,
            execution.result,
        );
    });
}

fn execute_run(
    session: RuntimeSession,
    config: &ThreadExecutionConfig,
    invocation: &RunInvocation,
) -> RunExecution {
    let (provider, recorded_batches) = match build_runtime_client(config) {
        Ok(value) => value,
        Err(error) => {
            return RunExecution {
                final_session: apply_optimistic_invocation(&session, invocation),
                recorded_batches: Vec::new(),
                result: Err(RuntimeError::new(error)),
            }
        }
    };

    let policy = permission_policy(config.permission_mode, &config.allowed_tools);
    let tool_registry =
        GlobalToolRegistry::builtin().with_enforcer(PermissionEnforcer::new(policy.clone()));
    let tool_executor = ServerToolExecutor::new(tool_registry, config.allowed_tools.clone());
    let mut runtime =
        ConversationRuntime::new(session, provider, tool_executor, policy, Vec::new());

    let result = match invocation {
        RunInvocation::Turn { message } => runtime.run_turn(message.clone(), None, None),
        RunInvocation::Resume { response } => {
            let mut user_input_prompter = SubmittedUserInputPrompter::new(response.clone());
            runtime.resume_pending_turn(None, Some(&mut user_input_prompter))
        }
    };

    RunExecution {
        final_session: runtime.session().clone(),
        recorded_batches: take_recorded_batches(&recorded_batches),
        result,
    }
}

fn apply_optimistic_invocation(
    session: &RuntimeSession,
    invocation: &RunInvocation,
) -> RuntimeSession {
    let mut next = session.clone();
    match invocation {
        RunInvocation::Turn { message } => {
            next.messages
                .push(ConversationMessage::user_text(message.clone()));
        }
        RunInvocation::Resume { response } => {
            next.messages.push(ConversationMessage::user_input_response(
                response.request_id.clone(),
                response.content.clone(),
                response.selected_option.clone(),
            ));
        }
    }
    next
}

fn build_runtime_client(
    config: &ThreadExecutionConfig,
) -> Result<
    (
        RecordingApiClient<ProviderRuntimeClient>,
        RecordedAssistantBatches,
    ),
    String,
> {
    let provider = ProviderRuntimeClient::new(&config.model, config.allowed_tools.clone())?;
    Ok(RecordingApiClient::new(provider))
}

fn emit_protocol_replay<'a>(
    record: &ThreadRecord,
    run_id: &str,
    recorded_batches: &[Vec<AssistantEvent>],
    new_messages: impl Iterator<Item = &'a ConversationMessage>,
) {
    let messages = new_messages.cloned().collect::<Vec<_>>();
    let mut cursor = 0usize;

    for batch in recorded_batches {
        for event in batch {
            match event {
                AssistantEvent::TextDelta(text) => {
                    record.emit_protocol_event(
                        "assistant.text.delta",
                        Some(run_id),
                        json!({ "text": text }),
                    );
                }
                AssistantEvent::ToolUse { id, name, input } => {
                    record.emit_protocol_event(
                        "assistant.tool_use",
                        Some(run_id),
                        json!({
                            "id": id,
                            "name": name,
                            "input": parse_tool_input(input),
                        }),
                    );
                }
                AssistantEvent::RequestUserInput(request) => {
                    record.emit_protocol_event(
                        "assistant.request_user_input",
                        Some(run_id),
                        json!({
                            "request_id": request.request_id,
                            "prompt": request.prompt,
                            "options": request.options,
                            "allow_freeform": request.allow_freeform,
                        }),
                    );
                }
                AssistantEvent::Usage(usage) => {
                    record.emit_protocol_event("assistant.usage", Some(run_id), json!(usage));
                }
                AssistantEvent::MessageStop => {
                    record.emit_protocol_event("assistant.message_stop", Some(run_id), json!({}));
                }
            }
        }

        if messages
            .get(cursor)
            .is_some_and(|message| message.role == MessageRole::Assistant)
        {
            cursor += 1;
        }

        while messages
            .get(cursor)
            .is_some_and(|message| message.role != MessageRole::Assistant)
        {
            emit_protocol_message_blocks(record, run_id, &messages[cursor]);
            cursor += 1;
        }
    }

    while let Some(message) = messages.get(cursor) {
        emit_protocol_message_blocks(record, run_id, message);
        cursor += 1;
    }
}

fn emit_protocol_message_blocks(
    record: &ThreadRecord,
    run_id: &str,
    message: &ConversationMessage,
) {
    for block in &message.blocks {
        match block {
            ContentBlock::ToolResult {
                tool_use_id,
                tool_name,
                output,
                is_error,
            } => {
                record.emit_protocol_event(
                    "assistant.tool_result",
                    Some(run_id),
                    json!({
                        "tool_use_id": tool_use_id,
                        "tool_name": tool_name,
                        "output": output,
                        "is_error": is_error,
                    }),
                );
            }
            ContentBlock::UserInputResponse {
                request_id,
                content,
                selected_option,
            } => {
                record.emit_protocol_event(
                    "user_input.submitted",
                    Some(run_id),
                    json!({
                        "request_id": request_id,
                        "content": content,
                        "selected_option": selected_option,
                    }),
                );
            }
            ContentBlock::Text { .. }
            | ContentBlock::ToolUse { .. }
            | ContentBlock::UserInputRequest { .. } => {}
        }
    }
}

#[derive(Debug, Clone)]
enum RunInvocation {
    Turn { message: String },
    Resume { response: UserInputResponse },
}

struct RunExecution {
    final_session: RuntimeSession,
    recorded_batches: Vec<Vec<AssistantEvent>>,
    result: Result<TurnSummary, RuntimeError>,
}

#[derive(Clone)]
struct RecordingApiClient<C> {
    inner: C,
    recorded_batches: Arc<Mutex<Vec<Vec<AssistantEvent>>>>,
}

impl<C> RecordingApiClient<C> {
    fn new(inner: C) -> (Self, Arc<Mutex<Vec<Vec<AssistantEvent>>>>) {
        let recorded_batches = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                inner,
                recorded_batches: Arc::clone(&recorded_batches),
            },
            recorded_batches,
        )
    }
}

impl<C> ApiClient for RecordingApiClient<C>
where
    C: ApiClient,
{
    fn stream(&mut self, request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
        let events = self.inner.stream(request)?;
        lock(&self.recorded_batches).push(events.clone());
        Ok(events)
    }
}

struct ServerToolExecutor {
    registry: GlobalToolRegistry,
    allowed_tools: BTreeSet<String>,
}

impl ServerToolExecutor {
    fn new(registry: GlobalToolRegistry, allowed_tools: BTreeSet<String>) -> Self {
        Self {
            registry,
            allowed_tools,
        }
    }
}

impl ToolExecutor for ServerToolExecutor {
    fn execute(&mut self, tool_name: &str, input: &str) -> Result<String, ToolError> {
        if !self.allowed_tools.contains(tool_name) {
            return Err(ToolError::new(format!(
                "tool `{tool_name}` is not enabled for this thread"
            )));
        }
        let value = serde_json::from_str(input)
            .map_err(|error| ToolError::new(format!("invalid tool input JSON: {error}")))?;
        self.registry
            .execute(tool_name, &value)
            .map_err(ToolError::new)
    }
}

struct SubmittedUserInputPrompter {
    response: Option<UserInputResponse>,
}

impl SubmittedUserInputPrompter {
    fn new(response: UserInputResponse) -> Self {
        Self {
            response: Some(response),
        }
    }
}

impl UserInputPrompter for SubmittedUserInputPrompter {
    fn prompt(&mut self, request: &UserInputRequest) -> UserInputOutcome {
        let Some(response) = self.response.take() else {
            return UserInputOutcome::Unavailable {
                reason: format!(
                    "no submitted reply is available for request-user-input `{}`",
                    request.request_id
                ),
            };
        };

        if response.request_id != request.request_id {
            return UserInputOutcome::Unavailable {
                reason: format!(
                    "submitted reply targets `{}` but the runtime is waiting for `{}`",
                    response.request_id, request.request_id
                ),
            };
        }

        UserInputOutcome::Submitted(response)
    }
}

fn resolve_thread_cwd(server_cwd: &Path, value: Option<&str>) -> ApiResult<PathBuf> {
    let current_dir = server_cwd.to_path_buf();
    let Some(requested) = value.map(str::trim).filter(|path| !path.is_empty()) else {
        return Ok(current_dir);
    };
    let requested_path = if Path::new(requested).is_absolute() {
        PathBuf::from(requested)
    } else {
        current_dir.join(requested)
    };
    let canonical_current = current_dir
        .canonicalize()
        .map_err(|error| internal_error(format!("failed to resolve current directory: {error}")))?;
    let canonical_requested = requested_path.canonicalize().map_err(|error| {
        bad_request(
            format!("thread cwd `{requested}` could not be resolved: {error}"),
            None,
        )
    })?;
    if canonical_requested != canonical_current {
        return Err(bad_request(
            format!(
                "phase-1 server-backed threads only support the server process cwd `{}`",
                current_dir.display()
            ),
            Some(json!({
                "requested_cwd": canonical_requested.display().to_string(),
                "server_cwd": canonical_current.display().to_string(),
            })),
        ));
    }
    Ok(canonical_current)
}

fn persisted_thread_from_inner(inner: &ThreadInner) -> PersistedThreadRecord {
    PersistedThreadRecord {
        thread_id: inner.id.clone(),
        created_at_ms: inner.created_at,
        updated_at_ms: inner.updated_at,
        cwd: inner.config.cwd.display().to_string(),
        model: inner.config.model.clone(),
        permission_mode: inner.config.permission_mode.as_str().to_string(),
        allowed_tools_json: serde_json::to_string(&inner.config.allowed_tools)
            .expect("allowed tools should serialize"),
        status: inner.status.snapshot().status,
        last_run_id: inner.status.run_id().map(ToOwned::to_owned),
        last_sequence: inner.last_sequence,
        session_json: serde_json::to_string(&inner.session).expect("session should serialize"),
        pending_request_json: match &inner.status {
            ThreadStatus::AwaitingUserInput { request, .. } => Some(
                serde_json::to_string(&UserInputRequestPayload::from_request(request.clone()))
                    .expect("pending request should serialize"),
            ),
            _ => None,
        },
        recovery_note: match &inner.status {
            ThreadStatus::Interrupted { recovery_note, .. } => Some(recovery_note.clone()),
            _ => None,
        },
    }
}

fn decode_allowed_tools(
    persisted: &PersistedThreadRecord,
) -> Result<BTreeSet<String>, StateStoreError> {
    serde_json::from_str::<Vec<String>>(&persisted.allowed_tools_json)
        .map_err(|error| {
            StateStoreError::new(format!(
                "failed to decode allowed tools for thread `{}`: {error}",
                persisted.thread_id
            ))
        })
        .map(|values| values.into_iter().collect())
}

fn decode_recovered_session(
    persisted: &PersistedThreadRecord,
) -> Result<RuntimeSession, StateStoreError> {
    serde_json::from_str::<RuntimeSession>(&persisted.session_json).map_err(|error| {
        StateStoreError::new(format!(
            "failed to decode session snapshot for thread `{}`: {error}",
            persisted.thread_id
        ))
    })
}

fn decode_pending_request_payload(
    persisted: &PersistedThreadRecord,
) -> Result<Option<UserInputRequestPayload>, StateStoreError> {
    persisted
        .pending_request_json
        .as_deref()
        .map(serde_json::from_str::<UserInputRequestPayload>)
        .transpose()
        .map_err(|error| {
            StateStoreError::new(format!(
                "failed to decode pending request for thread `{}`: {error}",
                persisted.thread_id
            ))
        })
}

fn recovered_status_from_persisted(
    persisted: &PersistedThreadRecord,
    pending_request: Option<UserInputRequestPayload>,
    session_pending: Option<UserInputRequestPayload>,
) -> Result<(ThreadStatus, bool), StateStoreError> {
    let recovered = match persisted.status.as_str() {
        "idle" if session_pending.is_none() => (ThreadStatus::Idle, false),
        "idle" => (
            ThreadStatus::Interrupted {
                run_id: persisted.last_run_id.clone(),
                recovery_note: "recovered idle thread snapshot still contained pending user input"
                    .to_string(),
            },
            true,
        ),
        "running" => (
            ThreadStatus::Interrupted {
                run_id: persisted.last_run_id.clone(),
                recovery_note: "previous run ended during server restart or shutdown".to_string(),
            },
            true,
        ),
        "awaiting_user_input" => match (
            persisted.last_run_id.clone(),
            pending_request,
            session_pending,
        ) {
            (Some(run_id), Some(expected), Some(actual)) if expected == actual => (
                ThreadStatus::AwaitingUserInput {
                    run_id,
                    request: PendingUserInputRequest {
                        request_id: expected.request_id,
                        prompt: expected.prompt,
                        options: expected.options,
                        allow_freeform: expected.allow_freeform,
                    },
                },
                false,
            ),
            _ => (
                ThreadStatus::Interrupted {
                    run_id: persisted.last_run_id.clone(),
                    recovery_note:
                        "recovered pending user input did not match the durable session snapshot"
                            .to_string(),
                },
                true,
            ),
        },
        "interrupted" => (
            ThreadStatus::Interrupted {
                run_id: persisted.last_run_id.clone(),
                recovery_note: persisted.recovery_note.clone().unwrap_or_else(|| {
                    "thread requires recovery after prior interruption".to_string()
                }),
            },
            false,
        ),
        other => {
            return Err(StateStoreError::new(format!(
                "unsupported persisted thread status `{other}` for thread `{}`",
                persisted.thread_id
            )))
        }
    };

    Ok(recovered)
}

fn thread_inner_from_persisted(
    persisted: PersistedThreadRecord,
) -> Result<(ThreadInner, bool), StateStoreError> {
    let allowed_tools = decode_allowed_tools(&persisted)?;
    let session = decode_recovered_session(&persisted)?;
    let pending_request = decode_pending_request_payload(&persisted)?;
    let session_pending = session
        .pending_user_input_request()
        .map(UserInputRequestPayload::from_request);
    let (status, needs_rewrite) =
        recovered_status_from_persisted(&persisted, pending_request, session_pending)?;

    Ok((
        ThreadInner {
            id: persisted.thread_id,
            created_at: persisted.created_at_ms,
            updated_at: persisted.updated_at_ms,
            session,
            config: ThreadExecutionConfig {
                cwd: PathBuf::from(persisted.cwd),
                model: persisted.model,
                permission_mode: parse_server_permission_mode(&persisted.permission_mode).map_err(
                    |error| {
                        StateStoreError::new(format!(
                            "failed to decode permission mode for recovered thread: {}",
                            error.message
                        ))
                    },
                )?,
                allowed_tools,
            },
            status,
            last_sequence: persisted.last_sequence,
        },
        needs_rewrite,
    ))
}

fn id_counter(id: &str, prefix: &str) -> u64 {
    id.strip_prefix(prefix)
        .and_then(|suffix| suffix.parse::<u64>().ok())
        .unwrap_or_default()
}

fn parse_server_permission_mode(value: &str) -> ApiResult<PermissionMode> {
    match value.trim() {
        "read-only" => Ok(PermissionMode::ReadOnly),
        "workspace-write" => Ok(PermissionMode::WorkspaceWrite),
        "danger-full-access" => Ok(PermissionMode::DangerFullAccess),
        "prompt" | "allow" => Err(bad_request(
            format!(
                "permission mode `{}` is not supported on the server thread protocol surface",
                value.trim()
            ),
            Some(json!({
                "supported_permission_modes": [
                    "read-only",
                    "workspace-write",
                    "danger-full-access"
                ],
            })),
        )),
        other => Err(bad_request(
            format!("unsupported permission mode `{other}`"),
            Some(json!({
                "supported_permission_modes": [
                    "read-only",
                    "workspace-write",
                    "danger-full-access"
                ],
            })),
        )),
    }
}

fn permission_mode_from_resolved(mode: ResolvedPermissionMode) -> PermissionMode {
    match mode {
        ResolvedPermissionMode::ReadOnly => PermissionMode::ReadOnly,
        ResolvedPermissionMode::WorkspaceWrite => PermissionMode::WorkspaceWrite,
        ResolvedPermissionMode::DangerFullAccess => PermissionMode::DangerFullAccess,
    }
}

fn permission_policy(
    permission_mode: PermissionMode,
    allowed_tools: &BTreeSet<String>,
) -> PermissionPolicy {
    let registry = GlobalToolRegistry::builtin();
    registry
        .permission_specs(Some(allowed_tools))
        .into_iter()
        .fold(
            PermissionPolicy::new(permission_mode),
            |policy, (name, required_mode)| policy.with_tool_requirement(name, required_mode),
        )
}

fn parse_tool_input(input: &str) -> Value {
    serde_json::from_str(input).unwrap_or_else(|_| json!({ "raw_input": input }))
}

fn take_recorded_batches(recorded_batches: &RecordedAssistantBatches) -> Vec<Vec<AssistantEvent>> {
    std::mem::take(&mut *lock(recorded_batches))
}

fn snapshot_from_inner(inner: &ThreadInner) -> ThreadSnapshot {
    ThreadSnapshot {
        protocol_version: PROTOCOL_VERSION.to_string(),
        thread_id: inner.id.clone(),
        created_at: inner.created_at,
        updated_at: inner.updated_at,
        state: inner.status.snapshot(),
        config: inner.config.snapshot(),
        session: inner.session.clone(),
    }
}

fn thread_resync_required_event(record: &ThreadRecord, skipped: u64) -> ThreadEventEnvelope {
    let snapshot = record.snapshot();
    let snapshot_event = record.snapshot_event();
    ThreadEventEnvelope {
        protocol_version: PROTOCOL_VERSION.to_string(),
        thread_id: snapshot.thread_id.clone(),
        run_id: snapshot.state.run_id.clone(),
        sequence: snapshot_event.sequence,
        timestamp_ms: unix_timestamp_millis(),
        event_type: "thread.resync_required".to_string(),
        payload: json!({
            "skipped": skipped,
            "snapshot": snapshot,
        }),
    }
}

fn unix_timestamp_millis() -> u64 {
    u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be after epoch")
            .as_millis(),
    )
    .unwrap_or(u64::MAX)
}

fn to_value<T: Serialize>(value: T) -> Value {
    serde_json::to_value(value).expect("value should serialize")
}

fn bad_request(message: String, details: Option<Value>) -> ApiError {
    ApiError {
        status: StatusCode::BAD_REQUEST,
        code: "invalid_request".to_string(),
        message,
        details,
    }
}

fn conflict(message: String, details: Option<Value>) -> ApiError {
    ApiError {
        status: StatusCode::CONFLICT,
        code: "conflict".to_string(),
        message,
        details,
    }
}

fn not_found(message: String) -> ApiError {
    ApiError {
        status: StatusCode::NOT_FOUND,
        code: "not_found".to_string(),
        message,
        details: None,
    }
}

fn internal_error(message: String) -> ApiError {
    ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        code: "internal_error".to_string(),
        message,
        details: None,
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn read_lock<T>(lockable: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    lockable
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn write_lock<T>(lockable: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    lockable
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::{
        app, execute_run, recv_thread_stream_event, subscribe_thread_stream, AppState,
        CreateSessionResponse, ListSessionsResponse, ListThreadsResponse, RunInvocation,
        SessionDetailsResponse, SqliteThreadStore, StartTurnRequest, SubmitUserInputRequest,
        ThreadEventEnvelope, ThreadExecutionConfig, ThreadRecord, ThreadSnapshot,
        TurnAcceptedResponse, UserInputAcceptedResponse,
    };
    use mock_anthropic_service::MockAnthropicService;
    use reqwest::Client;
    use runtime::{ContentBlock, ConversationMessage, MessageRole, Session as RuntimeSession};
    use serde_json::{json, Value};
    use std::collections::BTreeSet;
    use std::ffi::OsString;
    use std::net::SocketAddr;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;
    use std::sync::OnceLock;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
    use tokio::net::TcpListener;
    use tokio::sync::Mutex;
    use tokio::task::JoinHandle;
    use tokio::time::{sleep, timeout};

    struct TestServer {
        address: SocketAddr,
        handle: Option<JoinHandle<()>>,
        workspace: PathBuf,
    }

    impl TestServer {
        async fn spawn() -> Self {
            let workspace = unique_temp_dir("server-state");
            std::fs::create_dir_all(&workspace).expect("workspace should create");
            Self::spawn_in(workspace).await
        }

        async fn spawn_in(workspace: PathBuf) -> Self {
            std::fs::create_dir_all(&workspace).expect("workspace should create");
            let listener = TcpListener::bind("127.0.0.1:0")
                .await
                .expect("test listener should bind");
            let address = listener
                .local_addr()
                .expect("listener should report local address");
            let state = AppState::load_for_cwd(&workspace).expect("state should load");
            let handle = tokio::spawn(async move {
                axum::serve(listener, app(state))
                    .await
                    .expect("server should run");
            });

            Self {
                address,
                handle: Some(handle),
                workspace,
            }
        }

        fn url(&self, path: &str) -> String {
            format!("http://{}{}", self.address, path)
        }

        fn state_db_path(&self) -> PathBuf {
            self.workspace.join(".openyak").join("state.sqlite3")
        }

        async fn shutdown(mut self) {
            if let Some(handle) = self.handle.take() {
                handle.abort();
                let _ = handle.await;
            }
        }
    }

    impl Drop for TestServer {
        fn drop(&mut self) {
            if let Some(handle) = self.handle.take() {
                handle.abort();
            }
        }
    }

    struct EnvGuard {
        anthropic_api_key: Option<String>,
        anthropic_base_url: Option<String>,
    }

    impl EnvGuard {
        fn set(base_url: &str) -> Self {
            let anthropic_api_key = std::env::var("ANTHROPIC_API_KEY").ok();
            let anthropic_base_url = std::env::var("ANTHROPIC_BASE_URL").ok();
            std::env::set_var("ANTHROPIC_API_KEY", "test-server-key");
            std::env::set_var("ANTHROPIC_BASE_URL", base_url);
            Self {
                anthropic_api_key,
                anthropic_base_url,
            }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.anthropic_api_key {
                Some(value) => std::env::set_var("ANTHROPIC_API_KEY", value),
                None => std::env::remove_var("ANTHROPIC_API_KEY"),
            }
            match &self.anthropic_base_url {
                Some(value) => std::env::set_var("ANTHROPIC_BASE_URL", value),
                None => std::env::remove_var("ANTHROPIC_BASE_URL"),
            }
        }
    }

    struct RemovedEnvGuard {
        entries: Vec<(&'static str, Option<OsString>)>,
    }

    impl RemovedEnvGuard {
        fn remove(keys: &[&'static str]) -> Self {
            let entries = keys
                .iter()
                .map(|key| {
                    let previous = std::env::var_os(key);
                    std::env::remove_var(key);
                    (*key, previous)
                })
                .collect();
            Self { entries }
        }
    }

    impl Drop for RemovedEnvGuard {
        fn drop(&mut self) {
            for (key, previous) in &self.entries {
                match previous {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
    }

    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    fn unique_temp_dir(prefix: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(1);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be after epoch")
            .as_nanos();
        let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("{prefix}-{nanos}-{counter}"))
    }

    async fn create_thread(client: &Client, server: &TestServer, payload: Value) -> ThreadSnapshot {
        client
            .post(server.url("/v1/threads"))
            .json(&payload)
            .send()
            .await
            .expect("create thread request should succeed")
            .error_for_status()
            .expect("create thread should return success")
            .json::<ThreadSnapshot>()
            .await
            .expect("thread snapshot should parse")
    }

    async fn create_session(client: &Client, server: &TestServer) -> CreateSessionResponse {
        client
            .post(server.url("/sessions"))
            .send()
            .await
            .expect("create request should succeed")
            .error_for_status()
            .expect("create request should return success")
            .json::<CreateSessionResponse>()
            .await
            .expect("create response should parse")
    }

    async fn wait_for_thread_status(
        client: &Client,
        server: &TestServer,
        thread_id: &str,
        expected: &str,
    ) -> ThreadSnapshot {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut last_status = None;

        while Instant::now() < deadline {
            let snapshot = client
                .get(server.url(&format!("/v1/threads/{thread_id}")))
                .send()
                .await
                .expect("thread get should succeed")
                .error_for_status()
                .expect("thread get should return success")
                .json::<ThreadSnapshot>()
                .await
                .expect("thread snapshot should parse");
            if snapshot.state.status == expected {
                return snapshot;
            }
            last_status = Some(snapshot.state.status);
            sleep(Duration::from_millis(25)).await;
        }

        panic!(
            "thread `{thread_id}` never reached status `{expected}` (last observed: {})",
            last_status.unwrap_or_else(|| "<unknown>".to_string())
        );
    }

    async fn next_sse_frame(response: &mut reqwest::Response, buffer: &mut String) -> String {
        loop {
            if let Some(index) = buffer.find("\n\n") {
                let frame = buffer[..index].to_string();
                let remainder = buffer[index + 2..].to_string();
                *buffer = remainder;
                return frame;
            }

            let next_chunk = timeout(Duration::from_secs(5), response.chunk())
                .await
                .expect("SSE stream should yield within timeout")
                .expect("SSE stream should remain readable")
                .expect("SSE stream should stay open");
            buffer.push_str(&String::from_utf8_lossy(&next_chunk));
        }
    }

    fn sse_json<T: serde::de::DeserializeOwned>(frame: &str) -> T {
        let data = frame
            .lines()
            .find_map(|line| line.strip_prefix("data: "))
            .expect("SSE frame should contain data");
        serde_json::from_str(data).expect("SSE JSON should parse")
    }

    fn normalized_protocol_fixture_value(mut value: Value) -> Value {
        fn normalize(value: &mut Value) {
            match value {
                Value::Object(object) => {
                    if let Some(timestamp_ms) = object.get_mut("timestamp_ms") {
                        *timestamp_ms = json!(0);
                    }
                    if let Some(created_at) = object.get_mut("created_at") {
                        *created_at = json!(0);
                    }
                    if let Some(updated_at) = object.get_mut("updated_at") {
                        *updated_at = json!(0);
                    }
                    if let Some(cwd) = object.get_mut("cwd") {
                        *cwd = json!("<cwd>");
                    }
                    if let Some(text) = object.get_mut("text") {
                        if text
                            .as_str()
                            .is_some_and(|value| value.starts_with("bash completed:"))
                        {
                            *text = json!("<bash-completed-text>");
                        }
                    }
                    if let Some(skipped) = object.get_mut("skipped") {
                        *skipped = json!(1);
                    }
                    if object.contains_key("tool_use_id")
                        && object.contains_key("tool_name")
                        && object.contains_key("output")
                        && object.contains_key("is_error")
                    {
                        object.insert("output".to_string(), json!("<tool-output>"));
                    }
                    for child in object.values_mut() {
                        normalize(child);
                    }
                }
                Value::Array(items) => {
                    for item in items {
                        normalize(item);
                    }
                }
                _ => {}
            }
        }

        normalize(&mut value);
        value
    }

    fn assert_threads_protocol_fixture(actual: Value) {
        let fixture_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join("threads_protocol_v1.json");
        let expected = serde_json::from_str::<Value>(
            &std::fs::read_to_string(&fixture_path)
                .expect("thread protocol fixture should be readable"),
        )
        .expect("thread protocol fixture should parse");
        assert_eq!(
            normalized_protocol_fixture_value(actual),
            expected,
            "thread protocol fixture drifted; update {} only after explicit SDK contract review",
            fixture_path.display()
        );
    }

    #[tokio::test]
    async fn creates_lists_and_gets_threads() {
        let server = TestServer::spawn().await;
        let client = Client::new();

        let created = create_thread(&client, &server, serde_json::json!({})).await;

        let listed = client
            .get(server.url("/v1/threads"))
            .send()
            .await
            .expect("list request should succeed")
            .error_for_status()
            .expect("list request should return success")
            .json::<ListThreadsResponse>()
            .await
            .expect("list response should parse");
        let fetched = client
            .get(server.url(&format!("/v1/threads/{}", created.thread_id)))
            .send()
            .await
            .expect("get request should succeed")
            .error_for_status()
            .expect("get request should return success")
            .json::<ThreadSnapshot>()
            .await
            .expect("get response should parse");

        assert_eq!(created.thread_id, "thread-1");
        assert_eq!(created.protocol_version, "v1");
        assert_eq!(created.state.status, "idle");
        assert!(created.session.messages.is_empty());
        assert_eq!(listed.threads.len(), 1);
        assert_eq!(listed.threads[0].thread_id, created.thread_id);
        assert_eq!(fetched.thread_id, created.thread_id);
        assert_eq!(fetched.config.permission_mode, "danger-full-access");
        assert!(!fetched.config.allowed_tools.is_empty());
    }

    #[allow(clippy::too_many_lines)]
    #[tokio::test]
    async fn threads_protocol_fixture_matches_current_v1_contract() {
        let _env_guard = env_lock().lock().await;
        let mock_service = MockAnthropicService::spawn()
            .await
            .expect("mock service should start");
        let _env = EnvGuard::set(&mock_service.base_url());
        let server = TestServer::spawn().await;
        let client = Client::new();

        let create_thread_request = json!({
            "model": "claude-sonnet-4-6",
            "permission_mode": "danger-full-access",
            "allowed_tools": ["bash"],
        });
        let start_turn_request = json!({
            "message": "PARITY_SCENARIO:bash_stdout_roundtrip",
        });
        let created = create_thread(&client, &server, create_thread_request.clone()).await;
        let listed = client
            .get(server.url("/v1/threads"))
            .send()
            .await
            .expect("list request should succeed")
            .error_for_status()
            .expect("list request should return success")
            .json::<ListThreadsResponse>()
            .await
            .expect("list response should parse");
        let fetched = client
            .get(server.url(&format!("/v1/threads/{}", created.thread_id)))
            .send()
            .await
            .expect("get request should succeed")
            .error_for_status()
            .expect("get request should return success")
            .json::<ThreadSnapshot>()
            .await
            .expect("get response should parse");

        let invalid_request_response = client
            .post(server.url(&format!("/v1/threads/{}/turns", created.thread_id)))
            .json(&StartTurnRequest {
                message: "   ".to_string(),
            })
            .send()
            .await
            .expect("invalid request should return a response");
        let invalid_request_status = invalid_request_response.status().as_u16();
        let invalid_request_body = invalid_request_response
            .json::<Value>()
            .await
            .expect("invalid request body should parse");

        let not_found_response = client
            .get(server.url("/v1/threads/thread-missing"))
            .send()
            .await
            .expect("not found request should return a response");
        let not_found_status = not_found_response.status().as_u16();
        let not_found_body = not_found_response
            .json::<Value>()
            .await
            .expect("not found body should parse");

        let mut bash_events_response = client
            .get(server.url(&format!("/v1/threads/{}/events", created.thread_id)))
            .send()
            .await
            .expect("bash events request should succeed")
            .error_for_status()
            .expect("bash events request should return success");
        let mut bash_buffer = String::new();
        let bash_snapshot: ThreadEventEnvelope =
            sse_json(&next_sse_frame(&mut bash_events_response, &mut bash_buffer).await);
        let bash_turn_accepted = client
            .post(server.url(&format!("/v1/threads/{}/turns", created.thread_id)))
            .json(&StartTurnRequest {
                message: "PARITY_SCENARIO:bash_stdout_roundtrip".to_string(),
            })
            .send()
            .await
            .expect("bash turn request should succeed")
            .error_for_status()
            .expect("bash turn request should return success")
            .json::<TurnAcceptedResponse>()
            .await
            .expect("bash turn response should parse");
        let mut bash_events =
            vec![serde_json::to_value(bash_snapshot).expect("bash snapshot should serialize")];
        loop {
            let frame = next_sse_frame(&mut bash_events_response, &mut bash_buffer).await;
            let event: ThreadEventEnvelope = sse_json(&frame);
            let event_type = event.event_type.clone();
            if event.run_id.as_deref() == Some(bash_turn_accepted.run_id.as_str()) {
                bash_events.push(serde_json::to_value(event).expect("bash event should serialize"));
            }
            if event_type == "run.completed" {
                break;
            }
        }

        let user_input_created = create_thread(
            &client,
            &server,
            json!({
                "model": "claude-sonnet-4-6",
                "allowed_tools": ["read_file"],
            }),
        )
        .await;
        let mut user_input_events_response = client
            .get(server.url(&format!(
                "/v1/threads/{}/events",
                user_input_created.thread_id
            )))
            .send()
            .await
            .expect("user-input events request should succeed")
            .error_for_status()
            .expect("user-input events request should return success");
        let mut user_input_buffer = String::new();
        let user_input_snapshot: ThreadEventEnvelope = sse_json(
            &next_sse_frame(&mut user_input_events_response, &mut user_input_buffer).await,
        );
        let user_input_turn_accepted = client
            .post(server.url(&format!(
                "/v1/threads/{}/turns",
                user_input_created.thread_id
            )))
            .json(&StartTurnRequest {
                message: "PARITY_SCENARIO:request_user_input_roundtrip".to_string(),
            })
            .send()
            .await
            .expect("request-user-input turn should succeed")
            .error_for_status()
            .expect("request-user-input turn should return success")
            .json::<TurnAcceptedResponse>()
            .await
            .expect("request-user-input turn response should parse");
        let mut user_input_events = vec![serde_json::to_value(user_input_snapshot)
            .expect("user-input snapshot should serialize")];
        loop {
            let frame =
                next_sse_frame(&mut user_input_events_response, &mut user_input_buffer).await;
            let event: ThreadEventEnvelope = sse_json(&frame);
            let event_type = event.event_type.clone();
            if event.run_id.as_deref() == Some(user_input_turn_accepted.run_id.as_str()) {
                user_input_events
                    .push(serde_json::to_value(event).expect("user-input event should serialize"));
            }
            if event_type == "run.waiting_user_input" {
                break;
            }
        }

        let waiting = wait_for_thread_status(
            &client,
            &server,
            &user_input_created.thread_id,
            "awaiting_user_input",
        )
        .await;
        let pending = waiting
            .state
            .pending_user_input
            .clone()
            .expect("pending user-input request should exist");

        let conflict_response = client
            .post(server.url(&format!(
                "/v1/threads/{}/turns",
                user_input_created.thread_id
            )))
            .json(&StartTurnRequest {
                message: "turn should conflict while waiting".to_string(),
            })
            .send()
            .await
            .expect("conflict request should return a response");
        let conflict_status = conflict_response.status().as_u16();
        let conflict_body = conflict_response
            .json::<Value>()
            .await
            .expect("conflict body should parse");

        let submit_user_input_request = json!({
            "request_id": pending.request_id.clone(),
            "content": "feature",
            "selected_option": "feature",
        });
        let user_input_accepted = client
            .post(server.url(&format!(
                "/v1/threads/{}/user-input",
                user_input_created.thread_id
            )))
            .json(&SubmitUserInputRequest {
                request_id: pending.request_id.clone(),
                content: "feature".to_string(),
                selected_option: Some("feature".to_string()),
            })
            .send()
            .await
            .expect("submit user-input request should succeed")
            .error_for_status()
            .expect("submit user-input request should return success")
            .json::<UserInputAcceptedResponse>()
            .await
            .expect("submit user-input response should parse");
        loop {
            let frame =
                next_sse_frame(&mut user_input_events_response, &mut user_input_buffer).await;
            let event: ThreadEventEnvelope = sse_json(&frame);
            let event_type = event.event_type.clone();
            if event.run_id.as_deref() == Some(user_input_accepted.run_id.as_str()) {
                user_input_events.push(
                    serde_json::to_value(event).expect("resumed user-input event should serialize"),
                );
            }
            if event_type == "run.completed" {
                break;
            }
        }

        let workspace = unique_temp_dir("server-lagged-fixture");
        std::fs::create_dir_all(&workspace).expect("workspace should create");
        let record = ThreadRecord::new(
            "thread-lag".to_string(),
            ThreadExecutionConfig {
                cwd: std::env::current_dir().expect("current directory should resolve"),
                model: "opus".to_string(),
                permission_mode: runtime::PermissionMode::DangerFullAccess,
                allowed_tools: BTreeSet::new(),
            },
            Arc::new(SqliteThreadStore::open(&workspace).expect("store should open")),
        )
        .expect("record should persist");
        let (mut receiver, snapshot) = subscribe_thread_stream(&record);
        let mut snapshot_sequence = snapshot.sequence;
        for index in 0..160 {
            record.emit_protocol_event(
                "assistant.text.delta",
                Some("run-1"),
                json!({ "text": format!("delta-{index}") }),
            );
        }
        let resync_required =
            recv_thread_stream_event(&record, &mut receiver, &mut snapshot_sequence)
                .await
                .expect("lagged receiver should yield resync_required");
        let _ = std::fs::remove_dir_all(workspace);

        let internal_error_fixture = json!({
            "status": 500,
            "body": {
                "code": "internal_error",
                "message": "fixture storage failure",
            }
        });

        let protocol_fixture = json!({
            "requests": {
                "create_thread": create_thread_request,
                "start_turn": start_turn_request,
                "submit_user_input": submit_user_input_request,
            },
            "responses": {
                "create_thread": serde_json::to_value(created).expect("created thread should serialize"),
                "list_threads": serde_json::to_value(listed).expect("listed threads should serialize"),
                "get_thread": serde_json::to_value(fetched).expect("fetched thread should serialize"),
                "turn_accepted": serde_json::to_value(bash_turn_accepted)
                    .expect("turn accepted should serialize"),
                "user_input_accepted": serde_json::to_value(user_input_accepted)
                    .expect("user-input accepted should serialize"),
            },
            "errors": {
                "invalid_request": {
                    "status": invalid_request_status,
                    "body": invalid_request_body,
                },
                "conflict": {
                    "status": conflict_status,
                    "body": conflict_body,
                },
                "not_found": {
                    "status": not_found_status,
                    "body": not_found_body,
                },
                "internal_error": internal_error_fixture,
            },
            "events": {
                "bash_turn": bash_events,
                "user_input_roundtrip": user_input_events,
                "thread_resync_required": serde_json::to_value(resync_required)
                    .expect("resync event should serialize"),
            }
        });

        assert_threads_protocol_fixture(protocol_fixture);

        mock_service
            .shutdown()
            .await
            .expect("mock service should shut down cleanly");
    }

    #[tokio::test]
    async fn startup_creates_state_db_and_recovers_idle_threads_after_restart() {
        let workspace = unique_temp_dir("server-restart-idle");
        let server = TestServer::spawn_in(workspace.clone()).await;
        let client = Client::new();
        assert!(server.state_db_path().exists());

        let created = create_thread(&client, &server, serde_json::json!({})).await;
        server.shutdown().await;

        let restarted = TestServer::spawn_in(workspace.clone()).await;
        let listed = client
            .get(restarted.url("/v1/threads"))
            .send()
            .await
            .expect("restarted list request should succeed")
            .error_for_status()
            .expect("restarted list request should return success")
            .json::<ListThreadsResponse>()
            .await
            .expect("restarted list response should parse");
        let recovered = client
            .get(restarted.url(&format!("/v1/threads/{}", created.thread_id)))
            .send()
            .await
            .expect("restarted get request should succeed")
            .error_for_status()
            .expect("restarted get request should return success")
            .json::<ThreadSnapshot>()
            .await
            .expect("restarted snapshot should parse");
        let legacy = client
            .get(restarted.url(&format!("/sessions/{}", created.thread_id)))
            .send()
            .await
            .expect("restarted legacy get should succeed")
            .error_for_status()
            .expect("restarted legacy get should return success")
            .json::<SessionDetailsResponse>()
            .await
            .expect("restarted legacy snapshot should parse");

        assert_eq!(listed.threads.len(), 1);
        assert_eq!(listed.threads[0].thread_id, created.thread_id);
        assert_eq!(listed.threads[0].state.status, "idle");
        assert_eq!(recovered.thread_id, created.thread_id);
        assert_eq!(recovered.state.status, "idle");
        assert!(recovered.session.messages.is_empty());
        assert_eq!(legacy.id, created.thread_id);
        assert!(legacy.session.messages.is_empty());

        restarted.shutdown().await;
        let _ = std::fs::remove_dir_all(workspace);
    }

    #[tokio::test]
    async fn recovers_pending_user_input_threads_after_restart() {
        let _env_guard = env_lock().lock().await;
        let mock_service = MockAnthropicService::spawn()
            .await
            .expect("mock service should start");
        let _env = EnvGuard::set(&mock_service.base_url());
        let workspace = unique_temp_dir("server-restart-pending");
        let server = TestServer::spawn_in(workspace.clone()).await;
        let client = Client::new();

        let created = create_thread(&client, &server, serde_json::json!({})).await;
        let accepted = client
            .post(server.url(&format!("/v1/threads/{}/turns", created.thread_id)))
            .json(&StartTurnRequest {
                message: "PARITY_SCENARIO:request_user_input_roundtrip".to_string(),
            })
            .send()
            .await
            .expect("turn request should succeed")
            .error_for_status()
            .expect("turn request should return success")
            .json::<TurnAcceptedResponse>()
            .await
            .expect("turn response should parse");
        let waiting =
            wait_for_thread_status(&client, &server, &created.thread_id, "awaiting_user_input")
                .await;
        let pending = waiting
            .state
            .pending_user_input
            .clone()
            .expect("pending request should be present");

        server.shutdown().await;
        let restarted = TestServer::spawn_in(workspace.clone()).await;
        let recovered = wait_for_thread_status(
            &client,
            &restarted,
            &created.thread_id,
            "awaiting_user_input",
        )
        .await;
        let recovered_pending = recovered
            .state
            .pending_user_input
            .expect("recovered pending request should be present");
        assert_eq!(recovered_pending.request_id, pending.request_id);
        assert_eq!(
            recovered.state.run_id.as_deref(),
            Some(accepted.run_id.as_str())
        );

        let resumed = client
            .post(restarted.url(&format!("/v1/threads/{}/user-input", created.thread_id)))
            .json(&SubmitUserInputRequest {
                request_id: pending.request_id,
                content: "feature".to_string(),
                selected_option: Some("feature".to_string()),
            })
            .send()
            .await
            .expect("recovered user-input request should succeed")
            .error_for_status()
            .expect("recovered user-input request should return success")
            .json::<UserInputAcceptedResponse>()
            .await
            .expect("recovered user-input response should parse");
        assert_eq!(resumed.run_id, accepted.run_id);
        let completed =
            wait_for_thread_status(&client, &restarted, &created.thread_id, "idle").await;
        let transcript =
            serde_json::to_string(&completed.session).expect("session should serialize");
        assert!(transcript.contains("\"feature\""));

        restarted.shutdown().await;
        let _ = std::fs::remove_dir_all(workspace);
        mock_service
            .shutdown()
            .await
            .expect("mock service should shut down cleanly");
    }

    #[test]
    fn load_for_cwd_recovers_running_threads_as_interrupted() {
        let workspace = unique_temp_dir("server-interrupted");
        std::fs::create_dir_all(&workspace).expect("workspace should create");
        let canonical_workspace = workspace
            .canonicalize()
            .expect("workspace should canonicalize");
        let store = Arc::new(SqliteThreadStore::open(&workspace).expect("store should open"));
        let record = ThreadRecord::new(
            "thread-9".to_string(),
            ThreadExecutionConfig {
                cwd: canonical_workspace,
                model: "opus".to_string(),
                permission_mode: runtime::PermissionMode::DangerFullAccess,
                allowed_tools: BTreeSet::new(),
            },
            store,
        )
        .expect("record should persist");
        {
            let mut inner = super::lock(&record.inner);
            inner.status = super::ThreadStatus::Running {
                run_id: "run-5".to_string(),
            };
            inner.updated_at = super::unix_timestamp_millis();
        }
        record
            .persist_snapshot()
            .expect("running snapshot should persist");

        let state = AppState::load_for_cwd(&workspace).expect("state should reload");
        let recovered = state
            .thread("thread-9")
            .expect("recovered thread should exist")
            .snapshot();
        assert_eq!(recovered.state.status, "interrupted");
        assert_eq!(recovered.state.run_id.as_deref(), Some("run-5"));
        assert!(recovered
            .state
            .recovery_note
            .expect("recovery note should be present")
            .contains("restart or shutdown"));

        let _ = std::fs::remove_dir_all(workspace);
    }

    #[test]
    fn load_for_cwd_fails_when_state_db_path_is_unusable() {
        let workspace = unique_temp_dir("server-state-error");
        let broken_path = workspace.join(".openyak").join("state.sqlite3");
        std::fs::create_dir_all(&broken_path).expect("broken path should create");

        let Err(error) = AppState::load_for_cwd(&workspace) else {
            panic!("state load should fail when db path is a directory");
        };
        assert!(error
            .to_string()
            .contains("failed to open durable state database"));

        let _ = std::fs::remove_dir_all(workspace);
    }

    #[test]
    fn execute_run_preserves_optimistic_turn_message_when_runtime_bootstrap_fails() {
        let _env_guard = env_lock().blocking_lock();
        let _removed = RemovedEnvGuard::remove(&[
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_AUTH_TOKEN",
            "ANTHROPIC_BASE_URL",
            "OPENAI_API_KEY",
            "OPENAI_BASE_URL",
            "XAI_API_KEY",
            "XAI_BASE_URL",
        ]);
        let workspace = unique_temp_dir("server-run-bootstrap-failure-turn");
        std::fs::create_dir_all(&workspace).expect("workspace should create");

        let execution = execute_run(
            RuntimeSession::new(),
            &ThreadExecutionConfig {
                cwd: workspace.clone(),
                model: "opus".to_string(),
                permission_mode: runtime::PermissionMode::DangerFullAccess,
                allowed_tools: BTreeSet::new(),
            },
            &RunInvocation::Turn {
                message: "preserve me".to_string(),
            },
        );

        assert!(execution.result.is_err());
        assert!(matches!(
            execution.final_session.messages.as_slice(),
            [ConversationMessage {
                role: MessageRole::User,
                blocks,
                ..
            }] if matches!(
                blocks.as_slice(),
                [ContentBlock::Text { text }] if text == "preserve me"
            )
        ));

        let _ = std::fs::remove_dir_all(workspace);
    }

    #[test]
    fn execute_run_preserves_optimistic_user_input_response_when_runtime_bootstrap_fails() {
        let _env_guard = env_lock().blocking_lock();
        let _removed = RemovedEnvGuard::remove(&[
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_AUTH_TOKEN",
            "ANTHROPIC_BASE_URL",
            "OPENAI_API_KEY",
            "OPENAI_BASE_URL",
            "XAI_API_KEY",
            "XAI_BASE_URL",
        ]);
        let workspace = unique_temp_dir("server-run-bootstrap-failure-user-input");
        std::fs::create_dir_all(&workspace).expect("workspace should create");

        let mut session = RuntimeSession::new();
        session.messages.push(ConversationMessage::assistant(vec![
            ContentBlock::UserInputRequest {
                request_id: "req-1".to_string(),
                prompt: "Pick one".to_string(),
                options: vec!["alpha".to_string(), "beta".to_string()],
                allow_freeform: false,
            },
        ]));

        let execution = execute_run(
            session,
            &ThreadExecutionConfig {
                cwd: workspace.clone(),
                model: "opus".to_string(),
                permission_mode: runtime::PermissionMode::DangerFullAccess,
                allowed_tools: BTreeSet::new(),
            },
            &RunInvocation::Resume {
                response: runtime::UserInputResponse {
                    request_id: "req-1".to_string(),
                    content: "alpha".to_string(),
                    selected_option: Some("alpha".to_string()),
                },
            },
        );

        assert!(execution.result.is_err());
        assert!(matches!(
            execution.final_session.messages.last(),
            Some(ConversationMessage {
                role: MessageRole::User,
                blocks,
                ..
            }) if matches!(
                blocks.as_slice(),
                [ContentBlock::UserInputResponse {
                    request_id,
                    content,
                    selected_option
                }] if request_id == "req-1"
                    && content == "alpha"
                    && selected_option.as_deref() == Some("alpha")
            )
        ));
        assert!(execution
            .final_session
            .pending_user_input_request()
            .is_none());

        let _ = std::fs::remove_dir_all(workspace);
    }

    #[tokio::test]
    async fn thread_event_stream_starts_with_snapshot_and_replays_bash_turn() {
        let _env_guard = env_lock().lock().await;
        let mock_service = MockAnthropicService::spawn()
            .await
            .expect("mock service should start");
        let _env = EnvGuard::set(&mock_service.base_url());
        let server = TestServer::spawn().await;
        let client = Client::new();

        let created = create_thread(
            &client,
            &server,
            serde_json::json!({ "allowed_tools": ["bash"] }),
        )
        .await;
        let mut response = client
            .get(server.url(&format!("/v1/threads/{}/events", created.thread_id)))
            .send()
            .await
            .expect("events request should succeed")
            .error_for_status()
            .expect("events request should return success");
        let mut buffer = String::new();
        let snapshot_frame = next_sse_frame(&mut response, &mut buffer).await;
        let snapshot_event: ThreadEventEnvelope = sse_json(&snapshot_frame);
        assert_eq!(snapshot_event.event_type, "thread.snapshot");
        assert_eq!(snapshot_event.thread_id, created.thread_id);

        let accepted = client
            .post(server.url(&format!("/v1/threads/{}/turns", created.thread_id)))
            .json(&StartTurnRequest {
                message: "PARITY_SCENARIO:bash_stdout_roundtrip".to_string(),
            })
            .send()
            .await
            .expect("turn request should succeed")
            .error_for_status()
            .expect("turn request should return success")
            .json::<TurnAcceptedResponse>()
            .await
            .expect("turn response should parse");
        assert_eq!(accepted.status, "accepted");

        let mut seen_types = Vec::new();
        let mut saw_tool_result = false;
        let mut saw_final_text = false;
        for _ in 0..16 {
            let frame = next_sse_frame(&mut response, &mut buffer).await;
            let event: ThreadEventEnvelope = sse_json(&frame);
            seen_types.push(event.event_type.clone());
            if event.event_type == "assistant.tool_result" {
                saw_tool_result = true;
            }
            if event.event_type == "assistant.text.delta" {
                saw_final_text = event.payload["text"]
                    .as_str()
                    .expect("text delta should be a string")
                    .contains("bash completed");
            }
            if event.event_type == "run.completed" {
                break;
            }
        }

        assert!(seen_types.iter().any(|value| value == "run.started"));
        assert!(seen_types.iter().any(|value| value == "assistant.tool_use"));
        assert!(seen_types
            .iter()
            .any(|value| value == "assistant.message_stop"));
        assert!(seen_types.iter().any(|value| value == "run.completed"));
        assert!(saw_tool_result);
        assert!(saw_final_text);

        let completed = wait_for_thread_status(&client, &server, &created.thread_id, "idle").await;
        assert_eq!(completed.session.messages.len(), 4);

        mock_service
            .shutdown()
            .await
            .expect("mock service should shut down cleanly");
    }

    #[tokio::test]
    async fn request_user_input_roundtrip_resumes_same_run_and_conflicts_while_waiting() {
        let _env_guard = env_lock().lock().await;
        let mock_service = MockAnthropicService::spawn()
            .await
            .expect("mock service should start");
        let _env = EnvGuard::set(&mock_service.base_url());
        let server = TestServer::spawn().await;
        let client = Client::new();

        let created = create_thread(&client, &server, serde_json::json!({})).await;
        let accepted = client
            .post(server.url(&format!("/v1/threads/{}/turns", created.thread_id)))
            .json(&StartTurnRequest {
                message: "PARITY_SCENARIO:request_user_input_roundtrip".to_string(),
            })
            .send()
            .await
            .expect("turn request should succeed")
            .error_for_status()
            .expect("turn request should return success")
            .json::<TurnAcceptedResponse>()
            .await
            .expect("turn response should parse");
        let waiting =
            wait_for_thread_status(&client, &server, &created.thread_id, "awaiting_user_input")
                .await;
        let pending = waiting
            .state
            .pending_user_input
            .expect("pending request should be present");
        assert_eq!(
            accepted.run_id,
            waiting.state.run_id.expect("run id should be present")
        );

        let conflict_response = client
            .post(server.url(&format!("/v1/threads/{}/turns", created.thread_id)))
            .json(&StartTurnRequest {
                message: "second turn should conflict".to_string(),
            })
            .send()
            .await
            .expect("conflict request should return a response");
        assert_eq!(conflict_response.status(), reqwest::StatusCode::CONFLICT);
        let conflict_body: Value = conflict_response
            .json()
            .await
            .expect("conflict body should parse");
        assert_eq!(conflict_body["code"], "conflict");

        let resumed = client
            .post(server.url(&format!("/v1/threads/{}/user-input", created.thread_id)))
            .json(&SubmitUserInputRequest {
                request_id: pending.request_id.clone(),
                content: "feature".to_string(),
                selected_option: Some("feature".to_string()),
            })
            .send()
            .await
            .expect("user-input request should succeed")
            .error_for_status()
            .expect("user-input request should return success")
            .json::<UserInputAcceptedResponse>()
            .await
            .expect("user-input response should parse");
        assert_eq!(resumed.run_id, accepted.run_id);

        let completed = wait_for_thread_status(&client, &server, &created.thread_id, "idle").await;
        let transcript = serde_json::to_string(&completed.session)
            .expect("session should serialize for transcript assertions");
        assert!(transcript.contains("request-user-input"));
        assert!(transcript.contains("\"feature\""));

        mock_service
            .shutdown()
            .await
            .expect("mock service should shut down cleanly");
    }

    #[tokio::test]
    async fn connecting_to_active_thread_receives_snapshot_then_live_resume_events() {
        let _env_guard = env_lock().lock().await;
        let mock_service = MockAnthropicService::spawn()
            .await
            .expect("mock service should start");
        let _env = EnvGuard::set(&mock_service.base_url());
        let server = TestServer::spawn().await;
        let client = Client::new();

        let created = create_thread(&client, &server, serde_json::json!({})).await;
        client
            .post(server.url(&format!("/v1/threads/{}/turns", created.thread_id)))
            .json(&StartTurnRequest {
                message: "PARITY_SCENARIO:request_user_input_roundtrip".to_string(),
            })
            .send()
            .await
            .expect("turn request should succeed")
            .error_for_status()
            .expect("turn request should return success");
        let waiting =
            wait_for_thread_status(&client, &server, &created.thread_id, "awaiting_user_input")
                .await;
        let pending = waiting
            .state
            .pending_user_input
            .expect("pending request should be present");

        let mut response = client
            .get(server.url(&format!("/v1/threads/{}/events", created.thread_id)))
            .send()
            .await
            .expect("events request should succeed")
            .error_for_status()
            .expect("events request should return success");
        let mut buffer = String::new();
        let snapshot_frame = next_sse_frame(&mut response, &mut buffer).await;
        let snapshot_event: ThreadEventEnvelope = sse_json(&snapshot_frame);
        assert_eq!(snapshot_event.event_type, "thread.snapshot");
        assert_eq!(
            snapshot_event.payload["state"]["status"],
            "awaiting_user_input"
        );
        assert_eq!(
            snapshot_event.payload["state"]["pending_user_input"]["request_id"],
            pending.request_id
        );

        client
            .post(server.url(&format!("/v1/threads/{}/user-input", created.thread_id)))
            .json(&SubmitUserInputRequest {
                request_id: pending.request_id,
                content: "feature".to_string(),
                selected_option: Some("feature".to_string()),
            })
            .send()
            .await
            .expect("user-input request should succeed")
            .error_for_status()
            .expect("user-input request should return success");

        let mut saw_resubmitted = false;
        let mut saw_completed = false;
        for _ in 0..16 {
            let frame = next_sse_frame(&mut response, &mut buffer).await;
            let event: ThreadEventEnvelope = sse_json(&frame);
            if event.event_type == "user_input.submitted" {
                saw_resubmitted = true;
            }
            if event.event_type == "run.completed" {
                saw_completed = true;
                break;
            }
        }

        assert!(saw_resubmitted);
        assert!(saw_completed);

        mock_service
            .shutdown()
            .await
            .expect("mock service should shut down cleanly");
    }

    #[tokio::test]
    async fn lagged_thread_receivers_emit_resync_required_event() {
        let workspace = unique_temp_dir("server-lagged");
        std::fs::create_dir_all(&workspace).expect("workspace should create");
        let store = Arc::new(SqliteThreadStore::open(&workspace).expect("store should open"));
        let record = ThreadRecord::new(
            "thread-lag".to_string(),
            ThreadExecutionConfig {
                cwd: std::env::current_dir().expect("current directory should resolve"),
                model: "opus".to_string(),
                permission_mode: runtime::PermissionMode::DangerFullAccess,
                allowed_tools: BTreeSet::new(),
            },
            store,
        )
        .expect("record should persist");
        let (mut receiver, snapshot) = subscribe_thread_stream(&record);
        let mut snapshot_sequence = snapshot.sequence;

        for index in 0..160 {
            record.emit_protocol_event(
                "assistant.text.delta",
                Some("run-1"),
                serde_json::json!({ "text": format!("delta-{index}") }),
            );
        }

        let event = recv_thread_stream_event(&record, &mut receiver, &mut snapshot_sequence)
            .await
            .expect("lagged receiver should produce a resync event");

        assert_eq!(event.event_type, "thread.resync_required");
        assert!(event.payload["skipped"].as_u64().unwrap_or_default() > 0);
        assert_eq!(event.payload["snapshot"]["thread_id"], "thread-lag");

        let _ = std::fs::remove_dir_all(workspace);
    }

    #[tokio::test]
    async fn reconnecting_thread_events_starts_with_fresh_snapshot_after_prior_activity() {
        let _env_guard = env_lock().lock().await;
        let mock_service = MockAnthropicService::spawn()
            .await
            .expect("mock service should start");
        let _env = EnvGuard::set(&mock_service.base_url());
        let server = TestServer::spawn().await;
        let client = Client::new();

        let created = create_thread(
            &client,
            &server,
            serde_json::json!({ "allowed_tools": ["bash"] }),
        )
        .await;
        client
            .post(server.url(&format!("/v1/threads/{}/turns", created.thread_id)))
            .json(&StartTurnRequest {
                message: "PARITY_SCENARIO:bash_stdout_roundtrip".to_string(),
            })
            .send()
            .await
            .expect("turn request should succeed")
            .error_for_status()
            .expect("turn request should return success");
        let completed = wait_for_thread_status(&client, &server, &created.thread_id, "idle").await;
        assert!(completed.session.messages.len() >= 4);

        let mut response = client
            .get(server.url(&format!("/v1/threads/{}/events", created.thread_id)))
            .send()
            .await
            .expect("events request should succeed")
            .error_for_status()
            .expect("events request should return success");
        let mut buffer = String::new();
        let snapshot_frame = next_sse_frame(&mut response, &mut buffer).await;
        let snapshot_event: ThreadEventEnvelope = sse_json(&snapshot_frame);
        assert_eq!(snapshot_event.event_type, "thread.snapshot");
        assert_eq!(snapshot_event.payload["state"]["status"], "idle");
        assert!(
            snapshot_event.payload["session"]["messages"]
                .as_array()
                .expect("snapshot messages should be an array")
                .len()
                >= 4
        );

        mock_service
            .shutdown()
            .await
            .expect("mock service should shut down cleanly");
    }

    #[tokio::test]
    async fn creates_and_lists_sessions() {
        let server = TestServer::spawn().await;
        let client = Client::new();

        let created = create_session(&client, &server).await;

        let sessions = client
            .get(server.url("/sessions"))
            .send()
            .await
            .expect("list request should succeed")
            .error_for_status()
            .expect("list request should return success")
            .json::<ListSessionsResponse>()
            .await
            .expect("list response should parse");
        let details = client
            .get(server.url(&format!("/sessions/{}", created.session_id)))
            .send()
            .await
            .expect("details request should succeed")
            .error_for_status()
            .expect("details request should return success")
            .json::<SessionDetailsResponse>()
            .await
            .expect("details response should parse");

        assert_eq!(created.session_id, "thread-1");
        assert_eq!(sessions.sessions.len(), 1);
        assert_eq!(sessions.sessions[0].id, created.session_id);
        assert_eq!(sessions.sessions[0].message_count, 0);
        assert_eq!(details.id, "thread-1");
        assert!(details.session.messages.is_empty());
    }

    #[tokio::test]
    async fn streams_message_events_and_persists_message_flow() {
        let server = TestServer::spawn().await;
        let client = Client::new();

        let created = create_session(&client, &server).await;
        let mut response = client
            .get(server.url(&format!("/sessions/{}/events", created.session_id)))
            .send()
            .await
            .expect("events request should succeed")
            .error_for_status()
            .expect("events request should return success");
        let mut buffer = String::new();
        let snapshot_frame = next_sse_frame(&mut response, &mut buffer).await;

        let send_status = client
            .post(server.url(&format!("/sessions/{}/message", created.session_id)))
            .json(&super::SendMessageRequest {
                message: "hello from test".to_string(),
            })
            .send()
            .await
            .expect("message request should succeed")
            .status();
        let message_frame = next_sse_frame(&mut response, &mut buffer).await;
        let details = client
            .get(server.url(&format!("/sessions/{}", created.session_id)))
            .send()
            .await
            .expect("details request should succeed")
            .error_for_status()
            .expect("details request should return success")
            .json::<SessionDetailsResponse>()
            .await
            .expect("details response should parse");

        assert_eq!(send_status, reqwest::StatusCode::NO_CONTENT);
        assert!(snapshot_frame.contains("event: snapshot"));
        assert!(snapshot_frame.contains("\"session_id\":\"thread-1\""));
        assert!(message_frame.contains("event: message"));
        assert!(message_frame.contains("hello from test"));
        assert_eq!(details.session.messages.len(), 1);
        assert_eq!(
            details.session.messages[0],
            runtime::ConversationMessage::user_text("hello from test")
        );
    }
}

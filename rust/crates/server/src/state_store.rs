use std::fmt::{Display, Formatter};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rusqlite::{params, Connection};

const STATE_DB_FILENAME: &str = "state.sqlite3";
const SCHEMA_VERSION: i64 = 1;

#[derive(Debug)]
pub struct StateStoreError {
    message: String,
}

impl StateStoreError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl Display for StateStoreError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for StateStoreError {}

pub fn resolve_workspace_state_root(workspace_root: &Path) -> Result<PathBuf, StateStoreError> {
    let canonical_workspace = workspace_root.canonicalize().map_err(|error| {
        StateStoreError::new(format!(
            "failed to resolve server workspace `{}`: {error}",
            workspace_root.display()
        ))
    })?;
    let state_root = canonical_workspace.join(".openyak");
    if state_root.exists() {
        let metadata = fs::symlink_metadata(&state_root).map_err(|error| {
            StateStoreError::new(format!(
                "failed to inspect durable state directory `{}`: {error}",
                state_root.display()
            ))
        })?;
        if metadata.file_type().is_symlink() {
            return Err(StateStoreError::new(format!(
                "durable state directory `{}` must not be a symlink",
                state_root.display()
            )));
        }
    } else {
        fs::create_dir_all(&state_root).map_err(|error| {
            StateStoreError::new(format!(
                "failed to create durable state directory `{}`: {error}",
                state_root.display()
            ))
        })?;
    }

    let canonical_state_root = state_root.canonicalize().map_err(|error| {
        StateStoreError::new(format!(
            "failed to resolve durable state directory `{}`: {error}",
            state_root.display()
        ))
    })?;
    if !canonical_state_root.starts_with(&canonical_workspace) {
        return Err(StateStoreError::new(format!(
            "durable state directory `{}` escapes workspace `{}`",
            canonical_state_root.display(),
            canonical_workspace.display()
        )));
    }

    Ok(canonical_state_root)
}

#[derive(Debug, Clone)]
pub struct PersistedThreadRecord {
    pub thread_id: String,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    pub cwd: String,
    pub model: String,
    pub permission_mode: String,
    pub allowed_tools_json: String,
    pub status: String,
    pub last_run_id: Option<String>,
    pub last_sequence: u64,
    pub session_json: String,
    pub pending_request_json: Option<String>,
    pub recovery_note: Option<String>,
}

pub struct SqliteThreadStore {
    path: PathBuf,
    connection: Mutex<Connection>,
}

impl SqliteThreadStore {
    pub fn open(workspace_root: &Path) -> Result<Self, StateStoreError> {
        let openyak_dir = resolve_workspace_state_root(workspace_root)?;
        let path = openyak_dir.join(STATE_DB_FILENAME);
        let connection = Connection::open(&path).map_err(|error| {
            StateStoreError::new(format!(
                "failed to open durable state database `{}`: {error}",
                path.display()
            ))
        })?;

        let store = Self {
            path,
            connection: Mutex::new(connection),
        };
        store.initialize_schema()?;
        Ok(store)
    }

    pub fn load_threads(&self) -> Result<Vec<PersistedThreadRecord>, StateStoreError> {
        let connection = self
            .connection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut statement = connection
            .prepare(
                "SELECT
                    thread_id,
                    created_at_ms,
                    updated_at_ms,
                    cwd,
                    model,
                    permission_mode,
                    allowed_tools_json,
                    status,
                    last_run_id,
                    last_sequence,
                    session_json,
                    pending_request_json,
                    recovery_note
                 FROM threads
                 ORDER BY thread_id",
            )
            .map_err(|error| {
                StateStoreError::new(format!("failed to prepare thread load query: {error}"))
            })?;

        let rows = statement
            .query_map([], |row| {
                Ok(PersistedThreadRecord {
                    thread_id: row.get(0)?,
                    created_at_ms: row.get(1)?,
                    updated_at_ms: row.get(2)?,
                    cwd: row.get(3)?,
                    model: row.get(4)?,
                    permission_mode: row.get(5)?,
                    allowed_tools_json: row.get(6)?,
                    status: row.get(7)?,
                    last_run_id: row.get(8)?,
                    last_sequence: row.get(9)?,
                    session_json: row.get(10)?,
                    pending_request_json: row.get(11)?,
                    recovery_note: row.get(12)?,
                })
            })
            .map_err(|error| StateStoreError::new(format!("failed to query threads: {error}")))?;

        rows.collect::<Result<Vec<_>, _>>().map_err(|error| {
            StateStoreError::new(format!("failed to decode persisted thread rows: {error}"))
        })
    }

    pub fn upsert_thread(&self, record: &PersistedThreadRecord) -> Result<(), StateStoreError> {
        let connection = self
            .connection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        connection
            .execute(
                "INSERT INTO threads (
                    thread_id,
                    created_at_ms,
                    updated_at_ms,
                    cwd,
                    model,
                    permission_mode,
                    allowed_tools_json,
                    status,
                    last_run_id,
                    last_sequence,
                    session_json,
                    pending_request_json,
                    recovery_note
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
                 ON CONFLICT(thread_id) DO UPDATE SET
                    created_at_ms = excluded.created_at_ms,
                    updated_at_ms = excluded.updated_at_ms,
                    cwd = excluded.cwd,
                    model = excluded.model,
                    permission_mode = excluded.permission_mode,
                    allowed_tools_json = excluded.allowed_tools_json,
                    status = excluded.status,
                    last_run_id = excluded.last_run_id,
                    last_sequence = excluded.last_sequence,
                    session_json = excluded.session_json,
                    pending_request_json = excluded.pending_request_json,
                    recovery_note = excluded.recovery_note",
                params![
                    &record.thread_id,
                    record.created_at_ms,
                    record.updated_at_ms,
                    &record.cwd,
                    &record.model,
                    &record.permission_mode,
                    &record.allowed_tools_json,
                    &record.status,
                    record.last_run_id.as_deref(),
                    record.last_sequence,
                    &record.session_json,
                    record.pending_request_json.as_deref(),
                    record.recovery_note.as_deref(),
                ],
            )
            .map_err(|error| {
                StateStoreError::new(format!(
                    "failed to persist thread `{}`: {error}",
                    record.thread_id
                ))
            })?;
        Ok(())
    }

    fn initialize_schema(&self) -> Result<(), StateStoreError> {
        let connection = self
            .connection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let version = connection
            .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            .map_err(|error| {
                StateStoreError::new(format!(
                    "failed to read SQLite schema version from `{}`: {error}",
                    self.path.display()
                ))
            })?;

        match version {
            0 => {
                connection
                    .execute_batch(
                        "BEGIN;
                         CREATE TABLE IF NOT EXISTS threads (
                             thread_id TEXT PRIMARY KEY,
                             created_at_ms INTEGER NOT NULL,
                             updated_at_ms INTEGER NOT NULL,
                             cwd TEXT NOT NULL,
                             model TEXT NOT NULL,
                             permission_mode TEXT NOT NULL,
                             allowed_tools_json TEXT NOT NULL,
                             status TEXT NOT NULL,
                             last_run_id TEXT,
                             last_sequence INTEGER NOT NULL,
                             session_json TEXT NOT NULL,
                             pending_request_json TEXT,
                             recovery_note TEXT
                         );
                         PRAGMA user_version = 1;
                         COMMIT;",
                    )
                    .map_err(|error| {
                        StateStoreError::new(format!(
                            "failed to initialize SQLite durable state schema at `{}`: {error}",
                            self.path.display()
                        ))
                    })?;
                Ok(())
            }
            SCHEMA_VERSION => Ok(()),
            other => Err(StateStoreError::new(format!(
                "unsupported SQLite durable state schema version {other} in `{}`; expected {SCHEMA_VERSION}",
                self.path.display()
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::resolve_workspace_state_root;
    use std::fs;
    use std::io;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_temp_dir(prefix: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(1);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be after epoch")
            .as_nanos();
        let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("{prefix}-{nanos}-{counter}"))
    }

    fn create_test_dir_symlink(link: &Path, target: &Path) -> io::Result<()> {
        #[cfg(windows)]
        {
            std::os::windows::fs::symlink_dir(target, link)
        }
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(target, link)
        }
    }

    #[test]
    fn resolve_workspace_state_root_creates_local_directory() {
        let root = unique_temp_dir("server-state-root-local");
        fs::create_dir_all(&root).expect("workspace should exist");

        let resolved = resolve_workspace_state_root(&root).expect("state root should resolve");
        assert_eq!(
            resolved,
            root.join(".openyak").canonicalize().expect("canonical")
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn resolve_workspace_state_root_rejects_symlink_escape() {
        let root = unique_temp_dir("server-state-root-symlink");
        let outside = unique_temp_dir("server-state-root-outside");
        fs::create_dir_all(&root).expect("workspace should exist");
        fs::create_dir_all(&outside).expect("outside dir should exist");

        let symlink_path = root.join(".openyak");
        match create_test_dir_symlink(&symlink_path, &outside) {
            Ok(()) => {
                let error = resolve_workspace_state_root(&root)
                    .expect_err("symlinked state root should be rejected");
                assert!(error.to_string().contains("must not be a symlink"));
            }
            Err(error)
                if error.kind() == io::ErrorKind::PermissionDenied
                    || error.raw_os_error() == Some(1314) =>
            {
                // Some Windows environments disallow symlink creation without developer mode.
            }
            Err(error) => panic!("symlink creation should not fail unexpectedly: {error}"),
        }

        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(outside);
    }
}

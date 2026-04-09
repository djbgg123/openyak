use serde::{Deserialize, Serialize};

pub const PROCESS_LOCAL_TRUTH_LAYER: &str = "process_local_v1";
pub const DAEMON_LOCAL_TRUTH_LAYER: &str = "daemon_local_v1";
pub const LOCAL_RUNTIME_FOUNDATION_OPERATOR_PLANE: &str = "local_runtime_foundation_v1";
pub const LOCAL_LOOPBACK_OPERATOR_PLANE: &str = "local_loopback_operator_v1";
pub const PROCESS_MEMORY_PERSISTENCE_LAYER: &str = "process_memory_only_v1";
pub const WORKSPACE_SQLITE_PERSISTENCE_LAYER: &str = "workspace_sqlite_v1";
pub const THREAD_ATTACH_API: &str = "/v1/threads";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LifecycleContractSnapshot {
    pub truth_layer: String,
    pub operator_plane: String,
    pub persistence: String,
}

impl LifecycleContractSnapshot {
    #[must_use]
    pub fn process_local_foundation() -> Self {
        Self {
            truth_layer: PROCESS_LOCAL_TRUTH_LAYER.to_string(),
            operator_plane: LOCAL_RUNTIME_FOUNDATION_OPERATOR_PLANE.to_string(),
            persistence: PROCESS_MEMORY_PERSISTENCE_LAYER.to_string(),
        }
    }

    #[must_use]
    pub fn daemon_local_thread() -> Self {
        Self {
            truth_layer: DAEMON_LOCAL_TRUTH_LAYER.to_string(),
            operator_plane: LOCAL_LOOPBACK_OPERATOR_PLANE.to_string(),
            persistence: WORKSPACE_SQLITE_PERSISTENCE_LAYER.to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ThreadContractSnapshot {
    #[serde(flatten)]
    pub lifecycle: LifecycleContractSnapshot,
    pub attach_api: String,
}

impl ThreadContractSnapshot {
    #[must_use]
    pub fn current() -> Self {
        Self {
            lifecycle: LifecycleContractSnapshot::daemon_local_thread(),
            attach_api: THREAD_ATTACH_API.to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RecoveryGuidanceSnapshot {
    pub failure_kind: String,
    pub recovery_kind: String,
    pub recommended_actions: Vec<String>,
}

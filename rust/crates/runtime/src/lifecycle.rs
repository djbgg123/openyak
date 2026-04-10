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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LifecycleStateSnapshot {
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recovery: Option<RecoveryGuidanceSnapshot>,
}

impl LifecycleStateSnapshot {
    #[must_use]
    pub fn status(status: impl Into<String>) -> Self {
        Self {
            status: status.into(),
            failure_kind: None,
            recovery: None,
        }
    }

    #[must_use]
    pub fn with_recovery(status: impl Into<String>, recovery: RecoveryGuidanceSnapshot) -> Self {
        Self {
            status: status.into(),
            failure_kind: Some(recovery.failure_kind.clone()),
            recovery: Some(recovery),
        }
    }

    #[must_use]
    pub fn process_local_task(status: &str) -> Self {
        Self::status(status)
    }

    #[must_use]
    pub fn process_local_task_failure() -> Self {
        Self::with_recovery(
            "failed",
            RecoveryGuidanceSnapshot {
                failure_kind: "process_local_task_failed".to_string(),
                recovery_kind: "inspect_output_and_retry".to_string(),
                recommended_actions: vec![
                    "inspect task output and last_error in the current runtime".to_string(),
                    "retry the task from the same process-local operator surface when safe"
                        .to_string(),
                ],
            },
        )
    }

    #[must_use]
    pub fn process_local_task_error(status: &str) -> Self {
        Self::with_recovery(
            status,
            RecoveryGuidanceSnapshot {
                failure_kind: "process_local_task_conflict".to_string(),
                recovery_kind: "resolve_task_state_and_retry".to_string(),
                recommended_actions: vec![
                    "inspect the task state and last_error in the current runtime".to_string(),
                    "resolve the conflicting operator action before retrying".to_string(),
                ],
            },
        )
    }

    #[must_use]
    pub fn process_local_team(status: &str) -> Self {
        Self::status(status)
    }

    #[must_use]
    pub fn process_local_team_error(status: &str) -> Self {
        Self::with_recovery(
            status,
            RecoveryGuidanceSnapshot {
                failure_kind: "process_local_team_conflict".to_string(),
                recovery_kind: "resolve_team_state_and_retry".to_string(),
                recommended_actions: vec![
                    "inspect the current team state in the active runtime".to_string(),
                    "resolve the conflicting team action before retrying".to_string(),
                ],
            },
        )
    }

    #[must_use]
    pub fn process_local_cron_enabled() -> Self {
        Self::status("enabled")
    }

    #[must_use]
    pub fn process_local_cron_disabled() -> Self {
        Self::status("disabled")
    }

    #[must_use]
    pub fn process_local_cron_deleted() -> Self {
        Self::status("deleted")
    }

    #[must_use]
    pub fn process_local_cron_error(status: &str) -> Self {
        Self::with_recovery(
            status,
            RecoveryGuidanceSnapshot {
                failure_kind: "process_local_cron_conflict".to_string(),
                recovery_kind: "adjust_cron_state_and_retry".to_string(),
                recommended_actions: vec![
                    "inspect the cron entry state in the current runtime".to_string(),
                    "re-enable or update the cron entry before retrying".to_string(),
                ],
            },
        )
    }

    #[must_use]
    pub fn daemon_thread(status: &str, recovery: Option<RecoveryGuidanceSnapshot>) -> Self {
        match recovery {
            Some(recovery) => Self::with_recovery("interrupted", recovery),
            None => Self::status(status),
        }
    }
}

#![allow(clippy::must_use_candidate, clippy::unnecessary_map_or)]

//! In-memory task registry for sub-agent task lifecycle management.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::{LifecycleContractSnapshot, LifecycleStateSnapshot};
use serde::{Deserialize, Serialize};

const TASK_REGISTRY_ORIGIN: &str = crate::PROCESS_LOCAL_TRUTH_LAYER;

fn task_capabilities() -> Vec<String> {
    [
        "get",
        "list",
        "update",
        "stop",
        "output",
        "assign_team",
        "append_output",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Created,
    Running,
    Completed,
    Failed,
    Stopped,
}

impl std::fmt::Display for TaskStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Created => write!(f, "created"),
            Self::Running => write!(f, "running"),
            Self::Completed => write!(f, "completed"),
            Self::Failed => write!(f, "failed"),
            Self::Stopped => write!(f, "stopped"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub task_id: String,
    pub prompt: String,
    pub description: Option<String>,
    pub status: TaskStatus,
    pub lifecycle: LifecycleStateSnapshot,
    pub created_at: u64,
    pub updated_at: u64,
    pub last_error: Option<String>,
    pub origin: String,
    pub contract: LifecycleContractSnapshot,
    pub capabilities: Vec<String>,
    pub messages: Vec<TaskMessage>,
    pub output: String,
    pub team_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskMessage {
    pub role: String,
    pub content: String,
    pub timestamp: u64,
}

#[derive(Debug, Clone, Default)]
pub struct TaskRegistry {
    inner: Arc<Mutex<RegistryInner>>,
}

#[derive(Debug, Default)]
struct RegistryInner {
    tasks: HashMap<String, Task>,
    counter: u64,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn id_counter(id: &str) -> u64 {
    id.rsplit('_')
        .next()
        .and_then(|suffix| suffix.parse::<u64>().ok())
        .unwrap_or_default()
}

impl TaskRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn create(&self, prompt: &str, description: Option<&str>) -> Task {
        let mut inner = self.inner.lock().expect("registry lock poisoned");
        inner.counter += 1;
        let ts = now_secs();
        let task_id = format!("task_{:08x}_{}", ts, inner.counter);
        let task = Task {
            task_id: task_id.clone(),
            prompt: prompt.to_owned(),
            description: description.map(str::to_owned),
            status: TaskStatus::Created,
            lifecycle: LifecycleStateSnapshot::process_local_task("created"),
            created_at: ts,
            updated_at: ts,
            last_error: None,
            origin: TASK_REGISTRY_ORIGIN.to_owned(),
            contract: LifecycleContractSnapshot::process_local_foundation(),
            capabilities: task_capabilities(),
            messages: Vec::new(),
            output: String::new(),
            team_id: None,
        };
        inner.tasks.insert(task_id, task.clone());
        task
    }

    pub fn get(&self, task_id: &str) -> Option<Task> {
        let inner = self.inner.lock().expect("registry lock poisoned");
        inner.tasks.get(task_id).cloned()
    }

    pub fn list(&self, status_filter: Option<TaskStatus>) -> Vec<Task> {
        let inner = self.inner.lock().expect("registry lock poisoned");
        let mut tasks = inner
            .tasks
            .values()
            .filter(|t| status_filter.map_or(true, |s| t.status == s))
            .cloned()
            .collect::<Vec<_>>();
        tasks.sort_by_key(|task| (task.created_at, id_counter(&task.task_id)));
        tasks
    }

    pub fn stop(&self, task_id: &str) -> Result<Task, String> {
        let mut inner = self.inner.lock().expect("registry lock poisoned");
        let task = inner
            .tasks
            .get_mut(task_id)
            .ok_or_else(|| format!("task not found: {task_id}"))?;

        match task.status {
            TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Stopped => {
                let error = format!(
                    "task {task_id} is already in terminal state: {}",
                    task.status
                );
                task.last_error = Some(error.clone());
                task.lifecycle =
                    LifecycleStateSnapshot::process_local_task_error(&task.status.to_string());
                return Err(error);
            }
            _ => {}
        }

        task.status = TaskStatus::Stopped;
        task.lifecycle = LifecycleStateSnapshot::process_local_task("stopped");
        task.updated_at = now_secs();
        task.last_error = None;
        Ok(task.clone())
    }

    pub fn update(&self, task_id: &str, message: &str) -> Result<Task, String> {
        let mut inner = self.inner.lock().expect("registry lock poisoned");
        let task = inner
            .tasks
            .get_mut(task_id)
            .ok_or_else(|| format!("task not found: {task_id}"))?;

        task.messages.push(TaskMessage {
            role: String::from("user"),
            content: message.to_owned(),
            timestamp: now_secs(),
        });
        task.lifecycle = LifecycleStateSnapshot::process_local_task(&task.status.to_string());
        task.updated_at = now_secs();
        task.last_error = None;
        Ok(task.clone())
    }

    pub fn output(&self, task_id: &str) -> Result<String, String> {
        let inner = self.inner.lock().expect("registry lock poisoned");
        let task = inner
            .tasks
            .get(task_id)
            .ok_or_else(|| format!("task not found: {task_id}"))?;
        Ok(task.output.clone())
    }

    pub fn append_output(&self, task_id: &str, output: &str) -> Result<(), String> {
        let mut inner = self.inner.lock().expect("registry lock poisoned");
        let task = inner
            .tasks
            .get_mut(task_id)
            .ok_or_else(|| format!("task not found: {task_id}"))?;
        task.output.push_str(output);
        task.lifecycle = LifecycleStateSnapshot::process_local_task(&task.status.to_string());
        task.updated_at = now_secs();
        task.last_error = None;
        Ok(())
    }

    pub fn set_status(&self, task_id: &str, status: TaskStatus) -> Result<(), String> {
        let mut inner = self.inner.lock().expect("registry lock poisoned");
        let task = inner
            .tasks
            .get_mut(task_id)
            .ok_or_else(|| format!("task not found: {task_id}"))?;
        task.status = status;
        task.lifecycle = if status == TaskStatus::Failed {
            LifecycleStateSnapshot::process_local_task_failure()
        } else {
            LifecycleStateSnapshot::process_local_task(&status.to_string())
        };
        task.updated_at = now_secs();
        if status != TaskStatus::Failed {
            task.last_error = None;
        }
        Ok(())
    }

    pub fn record_failure(&self, task_id: &str, error: &str) -> Result<Task, String> {
        let mut inner = self.inner.lock().expect("registry lock poisoned");
        let task = inner
            .tasks
            .get_mut(task_id)
            .ok_or_else(|| format!("task not found: {task_id}"))?;
        task.status = TaskStatus::Failed;
        task.lifecycle = LifecycleStateSnapshot::process_local_task_failure();
        task.updated_at = now_secs();
        task.last_error = Some(error.to_owned());
        Ok(task.clone())
    }

    pub fn assign_team(&self, task_id: &str, team_id: &str) -> Result<(), String> {
        let mut inner = self.inner.lock().expect("registry lock poisoned");
        let task = inner
            .tasks
            .get_mut(task_id)
            .ok_or_else(|| format!("task not found: {task_id}"))?;
        if let Some(existing_team_id) = task.team_id.as_deref() {
            if existing_team_id == team_id {
                task.last_error = None;
                task.lifecycle = LifecycleStateSnapshot::process_local_task(&task.status.to_string());
                return Ok(());
            }
            let error = format!("task {task_id} is already assigned to team {existing_team_id}");
            task.last_error = Some(error.clone());
            task.lifecycle =
                LifecycleStateSnapshot::process_local_task_error(&task.status.to_string());
            return Err(error);
        }
        task.team_id = Some(team_id.to_owned());
        task.lifecycle = LifecycleStateSnapshot::process_local_task(&task.status.to_string());
        task.updated_at = now_secs();
        task.last_error = None;
        Ok(())
    }

    pub fn unassign_team(&self, task_id: &str, team_id: &str) -> Result<(), String> {
        let mut inner = self.inner.lock().expect("registry lock poisoned");
        let task = inner
            .tasks
            .get_mut(task_id)
            .ok_or_else(|| format!("task not found: {task_id}"))?;
        match task.team_id.as_deref() {
            Some(existing_team_id) if existing_team_id == team_id => {
                task.team_id = None;
                task.lifecycle = LifecycleStateSnapshot::process_local_task(&task.status.to_string());
                task.updated_at = now_secs();
                task.last_error = None;
                Ok(())
            }
            Some(existing_team_id) => {
                let error =
                    format!("task {task_id} is assigned to team {existing_team_id}, not {team_id}");
                task.last_error = Some(error.clone());
                task.lifecycle =
                    LifecycleStateSnapshot::process_local_task_error(&task.status.to_string());
                Err(error)
            }
            None => {
                task.last_error = None;
                task.lifecycle = LifecycleStateSnapshot::process_local_task(&task.status.to_string());
                Ok(())
            }
        }
    }

    pub fn remove(&self, task_id: &str) -> Option<Task> {
        let mut inner = self.inner.lock().expect("registry lock poisoned");
        inner.tasks.remove(task_id)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        let inner = self.inner.lock().expect("registry lock poisoned");
        inner.tasks.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_and_retrieves_tasks() {
        let registry = TaskRegistry::new();
        let task = registry.create("Do something", Some("A test task"));
        assert_eq!(task.status, TaskStatus::Created);
        assert_eq!(task.lifecycle.status, "created");
        assert_eq!(task.prompt, "Do something");
        assert_eq!(task.description.as_deref(), Some("A test task"));

        let fetched = registry.get(&task.task_id).expect("task should exist");
        assert_eq!(fetched.task_id, task.task_id);
    }

    #[test]
    fn lists_tasks_with_optional_filter() {
        let registry = TaskRegistry::new();
        registry.create("Task A", None);
        let task_b = registry.create("Task B", None);
        registry
            .set_status(&task_b.task_id, TaskStatus::Running)
            .expect("set status should succeed");

        let all = registry.list(None);
        assert_eq!(all.len(), 2);

        let running = registry.list(Some(TaskStatus::Running));
        assert_eq!(running.len(), 1);
        assert_eq!(running[0].task_id, task_b.task_id);

        let created = registry.list(Some(TaskStatus::Created));
        assert_eq!(created.len(), 1);
    }

    #[test]
    fn stops_running_task() {
        let registry = TaskRegistry::new();
        let task = registry.create("Stoppable", None);
        registry
            .set_status(&task.task_id, TaskStatus::Running)
            .unwrap();

        let stopped = registry.stop(&task.task_id).expect("stop should succeed");
        assert_eq!(stopped.status, TaskStatus::Stopped);
        assert_eq!(stopped.lifecycle.status, "stopped");

        // Stopping again should fail
        let result = registry.stop(&task.task_id);
        assert!(result.is_err());
    }

    #[test]
    fn updates_task_with_messages() {
        let registry = TaskRegistry::new();
        let task = registry.create("Messageable", None);
        let updated = registry
            .update(&task.task_id, "Here's more context")
            .expect("update should succeed");
        assert_eq!(updated.messages.len(), 1);
        assert_eq!(updated.messages[0].content, "Here's more context");
        assert_eq!(updated.messages[0].role, "user");
    }

    #[test]
    fn appends_and_retrieves_output() {
        let registry = TaskRegistry::new();
        let task = registry.create("Output task", None);
        registry
            .append_output(&task.task_id, "line 1\n")
            .expect("append should succeed");
        registry
            .append_output(&task.task_id, "line 2\n")
            .expect("append should succeed");

        let output = registry.output(&task.task_id).expect("output should exist");
        assert_eq!(output, "line 1\nline 2\n");
    }

    #[test]
    fn assigns_team_and_removes_task() {
        let registry = TaskRegistry::new();
        let task = registry.create("Team task", None);
        registry
            .assign_team(&task.task_id, "team_abc")
            .expect("assign should succeed");

        let fetched = registry.get(&task.task_id).unwrap();
        assert_eq!(fetched.team_id.as_deref(), Some("team_abc"));

        let removed = registry.remove(&task.task_id);
        assert!(removed.is_some());
        assert!(registry.get(&task.task_id).is_none());
        assert!(registry.is_empty());
    }

    #[test]
    fn rejects_operations_on_missing_task() {
        let registry = TaskRegistry::new();
        assert!(registry.stop("nonexistent").is_err());
        assert!(registry.update("nonexistent", "msg").is_err());
        assert!(registry.output("nonexistent").is_err());
        assert!(registry.append_output("nonexistent", "data").is_err());
        assert!(registry
            .set_status("nonexistent", TaskStatus::Running)
            .is_err());
    }

    #[test]
    fn task_status_display_all_variants() {
        // given
        let cases = [
            (TaskStatus::Created, "created"),
            (TaskStatus::Running, "running"),
            (TaskStatus::Completed, "completed"),
            (TaskStatus::Failed, "failed"),
            (TaskStatus::Stopped, "stopped"),
        ];

        // when
        let rendered: Vec<_> = cases
            .into_iter()
            .map(|(status, expected)| (status.to_string(), expected))
            .collect();

        // then
        assert_eq!(
            rendered,
            vec![
                ("created".to_string(), "created"),
                ("running".to_string(), "running"),
                ("completed".to_string(), "completed"),
                ("failed".to_string(), "failed"),
                ("stopped".to_string(), "stopped"),
            ]
        );
    }

    #[test]
    fn stop_rejects_completed_task() {
        // given
        let registry = TaskRegistry::new();
        let task = registry.create("done", None);
        registry
            .set_status(&task.task_id, TaskStatus::Completed)
            .expect("set status should succeed");

        // when
        let result = registry.stop(&task.task_id);

        // then
        let error = result.expect_err("completed task should be rejected");
        assert!(error.contains("already in terminal state"));
        assert!(error.contains("completed"));
        let fetched = registry.get(&task.task_id).expect("task should exist");
        assert_eq!(fetched.lifecycle.status, "completed");
        assert_eq!(
            fetched.lifecycle.failure_kind.as_deref(),
            Some("process_local_task_conflict")
        );
    }

    #[test]
    fn stop_rejects_failed_task() {
        // given
        let registry = TaskRegistry::new();
        let task = registry.create("failed", None);
        registry
            .set_status(&task.task_id, TaskStatus::Failed)
            .expect("set status should succeed");

        // when
        let result = registry.stop(&task.task_id);

        // then
        let error = result.expect_err("failed task should be rejected");
        assert!(error.contains("already in terminal state"));
        assert!(error.contains("failed"));
        let fetched = registry.get(&task.task_id).expect("task should exist");
        assert_eq!(fetched.lifecycle.status, "failed");
        assert_eq!(
            fetched.lifecycle.failure_kind.as_deref(),
            Some("process_local_task_conflict")
        );
    }

    #[test]
    fn stop_succeeds_from_created_state() {
        // given
        let registry = TaskRegistry::new();
        let task = registry.create("created task", None);

        // when
        let stopped = registry.stop(&task.task_id).expect("stop should succeed");

        // then
        assert_eq!(stopped.status, TaskStatus::Stopped);
        assert!(stopped.updated_at >= task.updated_at);
    }

    #[test]
    fn new_registry_is_empty() {
        // given
        let registry = TaskRegistry::new();

        // when
        let all_tasks = registry.list(None);

        // then
        assert!(registry.is_empty());
        assert_eq!(registry.len(), 0);
        assert!(all_tasks.is_empty());
    }

    #[test]
    fn create_without_description() {
        // given
        let registry = TaskRegistry::new();

        // when
        let task = registry.create("Do the thing", None);

        // then
        assert!(task.task_id.starts_with("task_"));
        assert_eq!(task.description, None);
        assert!(task.messages.is_empty());
        assert!(task.output.is_empty());
        assert_eq!(task.team_id, None);
        assert_eq!(task.origin, TASK_REGISTRY_ORIGIN);
        assert_eq!(task.contract.truth_layer, TASK_REGISTRY_ORIGIN);
        assert_eq!(task.lifecycle.status, "created");
        assert_eq!(task.lifecycle.failure_kind, None);
        assert_eq!(
            task.contract.operator_plane,
            crate::LOCAL_RUNTIME_FOUNDATION_OPERATOR_PLANE
        );
        assert_eq!(
            task.contract.persistence,
            crate::PROCESS_MEMORY_PERSISTENCE_LAYER
        );
        assert_eq!(task.last_error, None);
        assert!(task.capabilities.contains(&"stop".to_string()));
    }

    #[test]
    fn remove_nonexistent_returns_none() {
        // given
        let registry = TaskRegistry::new();

        // when
        let removed = registry.remove("missing");

        // then
        assert!(removed.is_none());
    }

    #[test]
    fn assign_team_rejects_missing_task() {
        // given
        let registry = TaskRegistry::new();

        // when
        let result = registry.assign_team("missing", "team_123");

        // then
        let error = result.expect_err("missing task should be rejected");
        assert_eq!(error, "task not found: missing");
    }

    #[test]
    fn assign_team_rejects_different_existing_team() {
        let registry = TaskRegistry::new();
        let task = registry.create("Do the thing", None);

        registry
            .assign_team(&task.task_id, "team_alpha")
            .expect("first assignment should succeed");

        let error = registry
            .assign_team(&task.task_id, "team_beta")
            .expect_err("reassignment should fail");

        assert_eq!(
            error,
            format!(
                "task {} is already assigned to team team_alpha",
                task.task_id
            )
        );

        let fetched = registry.get(&task.task_id).expect("task should exist");
        assert_eq!(fetched.last_error.as_deref(), Some(error.as_str()));
        assert_eq!(fetched.lifecycle.status, "created");
        assert_eq!(
            fetched.lifecycle.failure_kind.as_deref(),
            Some("process_local_task_conflict")
        );
    }

    #[test]
    fn unassign_team_clears_matching_assignment() {
        let registry = TaskRegistry::new();
        let task = registry.create("Do the thing", None);

        registry
            .assign_team(&task.task_id, "team_alpha")
            .expect("assignment should succeed");
        registry
            .unassign_team(&task.task_id, "team_alpha")
            .expect("unassign should succeed");

        let fetched = registry.get(&task.task_id).expect("task should exist");
        assert_eq!(fetched.team_id, None);
    }

    #[test]
    fn list_orders_tasks_by_creation_counter() {
        let registry = TaskRegistry::new();
        let expected_ids = (0..12)
            .map(|index| registry.create(&format!("Task {index}"), None).task_id)
            .collect::<Vec<_>>();

        let listed_ids = registry
            .list(None)
            .into_iter()
            .map(|task| task.task_id)
            .collect::<Vec<_>>();

        assert_eq!(listed_ids, expected_ids);
    }

    #[test]
    fn records_failure_metadata() {
        let registry = TaskRegistry::new();
        let task = registry.create("Failure task", None);

        let failed = registry
            .record_failure(&task.task_id, "worker crashed")
            .expect("record failure should succeed");

        assert_eq!(failed.status, TaskStatus::Failed);
        assert_eq!(failed.last_error.as_deref(), Some("worker crashed"));
        assert_eq!(failed.lifecycle.status, "failed");
        assert_eq!(
            failed.lifecycle.failure_kind.as_deref(),
            Some("process_local_task_failed")
        );
    }

    #[test]
    fn registries_do_not_share_process_local_state() {
        let first = TaskRegistry::new();
        let second = TaskRegistry::new();

        let task = first.create("Only first", None);

        assert!(first.get(&task.task_id).is_some());
        assert!(second.get(&task.task_id).is_none());
        assert!(second.is_empty());
    }
}

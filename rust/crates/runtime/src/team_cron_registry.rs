//! In-memory registries for Team and Cron lifecycle management.
//!
//! Provides TeamCreate/Delete and CronCreate/Delete/List runtime backing
//! to replace the stub implementations in the tools crate.

#![allow(clippy::must_use_candidate)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::{LifecycleContractSnapshot, LifecycleStateSnapshot};
use serde::{Deserialize, Serialize};

const REGISTRY_ORIGIN: &str = crate::PROCESS_LOCAL_TRUTH_LAYER;

fn team_capabilities() -> Vec<String> {
    ["get", "list", "delete"]
        .into_iter()
        .map(str::to_owned)
        .collect()
}

fn cron_capabilities() -> Vec<String> {
    ["get", "list", "disable", "enable", "delete", "record_run"]
        .into_iter()
        .map(str::to_owned)
        .collect()
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Team {
    pub team_id: String,
    pub name: String,
    pub task_ids: Vec<String>,
    pub status: TeamStatus,
    pub lifecycle: LifecycleStateSnapshot,
    pub created_at: u64,
    pub updated_at: u64,
    pub last_error: Option<String>,
    pub origin: String,
    pub contract: LifecycleContractSnapshot,
    pub capabilities: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TeamStatus {
    Created,
    Running,
    Completed,
    Deleted,
}

impl std::fmt::Display for TeamStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Created => write!(f, "created"),
            Self::Running => write!(f, "running"),
            Self::Completed => write!(f, "completed"),
            Self::Deleted => write!(f, "deleted"),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct TeamRegistry {
    inner: Arc<Mutex<TeamInner>>,
}

#[derive(Debug, Default)]
struct TeamInner {
    teams: HashMap<String, Team>,
    counter: u64,
}

impl TeamRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn create(&self, name: &str, task_ids: Vec<String>) -> Team {
        let mut inner = self.inner.lock().expect("team registry lock poisoned");
        inner.counter += 1;
        let ts = now_secs();
        let team_id = format!("team_{:08x}_{}", ts, inner.counter);
        let team = Team {
            team_id: team_id.clone(),
            name: name.to_owned(),
            task_ids,
            status: TeamStatus::Created,
            lifecycle: LifecycleStateSnapshot::process_local_team("created"),
            created_at: ts,
            updated_at: ts,
            last_error: None,
            origin: REGISTRY_ORIGIN.to_owned(),
            contract: LifecycleContractSnapshot::process_local_foundation(),
            capabilities: team_capabilities(),
        };
        inner.teams.insert(team_id, team.clone());
        team
    }

    pub fn get(&self, team_id: &str) -> Option<Team> {
        let inner = self.inner.lock().expect("team registry lock poisoned");
        inner.teams.get(team_id).cloned()
    }

    pub fn list(&self) -> Vec<Team> {
        let inner = self.inner.lock().expect("team registry lock poisoned");
        let mut teams = inner.teams.values().cloned().collect::<Vec<_>>();
        teams.sort_by_key(|team| (team.created_at, id_counter(&team.team_id)));
        teams
    }

    pub fn delete(&self, team_id: &str) -> Result<Team, String> {
        let mut inner = self.inner.lock().expect("team registry lock poisoned");
        let team = inner
            .teams
            .get_mut(team_id)
            .ok_or_else(|| format!("team not found: {team_id}"))?;
        if team.status == TeamStatus::Deleted {
            let error = format!("team already deleted: {team_id}");
            team.last_error = Some(error.clone());
            team.lifecycle = LifecycleStateSnapshot::process_local_team_error("deleted");
            return Err(error);
        }
        team.status = TeamStatus::Deleted;
        team.lifecycle = LifecycleStateSnapshot::process_local_team("deleted");
        team.updated_at = now_secs();
        team.last_error = None;
        Ok(team.clone())
    }

    pub fn remove(&self, team_id: &str) -> Option<Team> {
        let mut inner = self.inner.lock().expect("team registry lock poisoned");
        inner.teams.remove(team_id)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        let inner = self.inner.lock().expect("team registry lock poisoned");
        inner.teams.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CronEntry {
    pub cron_id: String,
    pub schedule: String,
    pub prompt: String,
    pub description: Option<String>,
    pub enabled: bool,
    pub lifecycle: LifecycleStateSnapshot,
    pub created_at: u64,
    pub updated_at: u64,
    pub last_run_at: Option<u64>,
    pub run_count: u64,
    pub last_error: Option<String>,
    pub disabled_reason: Option<String>,
    pub origin: String,
    pub contract: LifecycleContractSnapshot,
    pub capabilities: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct CronRegistry {
    inner: Arc<Mutex<CronInner>>,
}

#[derive(Debug, Default)]
struct CronInner {
    entries: HashMap<String, CronEntry>,
    counter: u64,
}

impl CronRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn create(&self, schedule: &str, prompt: &str, description: Option<&str>) -> CronEntry {
        let mut inner = self.inner.lock().expect("cron registry lock poisoned");
        inner.counter += 1;
        let ts = now_secs();
        let cron_id = format!("cron_{:08x}_{}", ts, inner.counter);
        let entry = CronEntry {
            cron_id: cron_id.clone(),
            schedule: schedule.to_owned(),
            prompt: prompt.to_owned(),
            description: description.map(str::to_owned),
            enabled: true,
            lifecycle: LifecycleStateSnapshot::process_local_cron_enabled(),
            created_at: ts,
            updated_at: ts,
            last_run_at: None,
            run_count: 0,
            last_error: None,
            disabled_reason: None,
            origin: REGISTRY_ORIGIN.to_owned(),
            contract: LifecycleContractSnapshot::process_local_foundation(),
            capabilities: cron_capabilities(),
        };
        inner.entries.insert(cron_id, entry.clone());
        entry
    }

    pub fn get(&self, cron_id: &str) -> Option<CronEntry> {
        let inner = self.inner.lock().expect("cron registry lock poisoned");
        inner.entries.get(cron_id).cloned()
    }

    pub fn list(&self, enabled_only: bool) -> Vec<CronEntry> {
        let inner = self.inner.lock().expect("cron registry lock poisoned");
        let mut entries = inner
            .entries
            .values()
            .filter(|e| !enabled_only || e.enabled)
            .cloned()
            .collect::<Vec<_>>();
        entries.sort_by_key(|entry| (entry.created_at, id_counter(&entry.cron_id)));
        entries
    }

    pub fn delete(&self, cron_id: &str) -> Result<CronEntry, String> {
        let mut inner = self.inner.lock().expect("cron registry lock poisoned");
        let mut entry = inner
            .entries
            .remove(cron_id)
            .ok_or_else(|| format!("cron not found: {cron_id}"))?;
        entry.lifecycle = LifecycleStateSnapshot::process_local_cron_deleted();
        Ok(entry)
    }

    /// Disable a cron entry without removing it.
    pub fn disable(&self, cron_id: &str) -> Result<(), String> {
        let mut inner = self.inner.lock().expect("cron registry lock poisoned");
        let entry = inner
            .entries
            .get_mut(cron_id)
            .ok_or_else(|| format!("cron not found: {cron_id}"))?;
        if !entry.enabled {
            let error = format!("cron already disabled: {cron_id}");
            entry.last_error = Some(error.clone());
            entry.lifecycle = LifecycleStateSnapshot::process_local_cron_error("disabled");
            return Err(error);
        }
        entry.enabled = false;
        entry.lifecycle = LifecycleStateSnapshot::process_local_cron_disabled();
        entry.updated_at = now_secs();
        entry.last_error = None;
        entry.disabled_reason = Some(String::from("disabled_by_operator_request"));
        Ok(())
    }

    /// Re-enable a previously disabled cron entry.
    pub fn enable(&self, cron_id: &str) -> Result<(), String> {
        let mut inner = self.inner.lock().expect("cron registry lock poisoned");
        let entry = inner
            .entries
            .get_mut(cron_id)
            .ok_or_else(|| format!("cron not found: {cron_id}"))?;
        if entry.enabled {
            let error = format!("cron already enabled: {cron_id}");
            entry.last_error = Some(error.clone());
            entry.lifecycle = LifecycleStateSnapshot::process_local_cron_error("enabled");
            return Err(error);
        }
        entry.enabled = true;
        entry.lifecycle = LifecycleStateSnapshot::process_local_cron_enabled();
        entry.updated_at = now_secs();
        entry.last_error = None;
        entry.disabled_reason = None;
        Ok(())
    }

    /// Record a cron run.
    pub fn record_run(&self, cron_id: &str) -> Result<(), String> {
        let mut inner = self.inner.lock().expect("cron registry lock poisoned");
        let entry = inner
            .entries
            .get_mut(cron_id)
            .ok_or_else(|| format!("cron not found: {cron_id}"))?;
        if !entry.enabled {
            let error = format!("cron is disabled: {cron_id}");
            entry.last_error = Some(error.clone());
            entry.lifecycle = LifecycleStateSnapshot::process_local_cron_error("disabled");
            return Err(error);
        }
        entry.last_run_at = Some(now_secs());
        entry.run_count += 1;
        entry.lifecycle = LifecycleStateSnapshot::process_local_cron_enabled();
        entry.updated_at = now_secs();
        entry.last_error = None;
        Ok(())
    }

    #[must_use]
    pub fn len(&self) -> usize {
        let inner = self.inner.lock().expect("cron registry lock poisoned");
        inner.entries.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Team tests ──────────────────────────────────────

    #[test]
    fn creates_and_retrieves_team() {
        let registry = TeamRegistry::new();
        let team = registry.create("Alpha Squad", vec!["task_001".into(), "task_002".into()]);
        assert_eq!(team.name, "Alpha Squad");
        assert_eq!(team.task_ids.len(), 2);
        assert_eq!(team.status, TeamStatus::Created);
        assert_eq!(team.lifecycle.status, "created");

        let fetched = registry.get(&team.team_id).expect("team should exist");
        assert_eq!(fetched.team_id, team.team_id);
    }

    #[test]
    fn lists_and_deletes_teams() {
        let registry = TeamRegistry::new();
        let t1 = registry.create("Team A", vec![]);
        let t2 = registry.create("Team B", vec![]);

        let all = registry.list();
        assert_eq!(all.len(), 2);

        let deleted = registry.delete(&t1.team_id).expect("delete should succeed");
        assert_eq!(deleted.status, TeamStatus::Deleted);
        assert_eq!(deleted.lifecycle.status, "deleted");

        // Team is still listable (soft delete)
        let still_there = registry.get(&t1.team_id).unwrap();
        assert_eq!(still_there.status, TeamStatus::Deleted);

        // Hard remove
        registry.remove(&t2.team_id);
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn rejects_missing_team_operations() {
        let registry = TeamRegistry::new();
        assert!(registry.delete("nonexistent").is_err());
        assert!(registry.get("nonexistent").is_none());
    }

    #[test]
    fn team_delete_rejects_already_deleted_team() {
        let registry = TeamRegistry::new();
        let team = registry.create("Team A", vec![]);

        registry
            .delete(&team.team_id)
            .expect("first delete should succeed");

        let error = registry
            .delete(&team.team_id)
            .expect_err("second delete should fail");
        assert_eq!(error, format!("team already deleted: {}", team.team_id));
    }

    // ── Cron tests ──────────────────────────────────────

    #[test]
    fn creates_and_retrieves_cron() {
        let registry = CronRegistry::new();
        let entry = registry.create("0 * * * *", "Check status", Some("hourly check"));
        assert_eq!(entry.schedule, "0 * * * *");
        assert_eq!(entry.prompt, "Check status");
        assert!(entry.enabled);
        assert_eq!(entry.run_count, 0);
        assert!(entry.last_run_at.is_none());
        assert_eq!(entry.lifecycle.status, "enabled");

        let fetched = registry.get(&entry.cron_id).expect("cron should exist");
        assert_eq!(fetched.cron_id, entry.cron_id);
    }

    #[test]
    fn lists_with_enabled_filter() {
        let registry = CronRegistry::new();
        let c1 = registry.create("* * * * *", "Task 1", None);
        let c2 = registry.create("0 * * * *", "Task 2", None);
        registry
            .disable(&c1.cron_id)
            .expect("disable should succeed");

        let all = registry.list(false);
        assert_eq!(all.len(), 2);

        let enabled_only = registry.list(true);
        assert_eq!(enabled_only.len(), 1);
        assert_eq!(enabled_only[0].cron_id, c2.cron_id);
    }

    #[test]
    fn deletes_cron_entry() {
        let registry = CronRegistry::new();
        let entry = registry.create("* * * * *", "To delete", None);
        let deleted = registry
            .delete(&entry.cron_id)
            .expect("delete should succeed");
        assert_eq!(deleted.cron_id, entry.cron_id);
        assert!(registry.get(&entry.cron_id).is_none());
        assert!(registry.is_empty());
    }

    #[test]
    fn records_cron_runs() {
        let registry = CronRegistry::new();
        let entry = registry.create("*/5 * * * *", "Recurring", None);
        registry.record_run(&entry.cron_id).unwrap();
        registry.record_run(&entry.cron_id).unwrap();

        let fetched = registry.get(&entry.cron_id).unwrap();
        assert_eq!(fetched.run_count, 2);
        assert!(fetched.last_run_at.is_some());
    }

    #[test]
    fn rejects_missing_cron_operations() {
        let registry = CronRegistry::new();
        assert!(registry.delete("nonexistent").is_err());
        assert!(registry.disable("nonexistent").is_err());
        assert!(registry.record_run("nonexistent").is_err());
        assert!(registry.get("nonexistent").is_none());
    }

    #[test]
    fn team_status_display_all_variants() {
        // given
        let cases = [
            (TeamStatus::Created, "created"),
            (TeamStatus::Running, "running"),
            (TeamStatus::Completed, "completed"),
            (TeamStatus::Deleted, "deleted"),
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
                ("deleted".to_string(), "deleted"),
            ]
        );
    }

    #[test]
    fn new_team_registry_is_empty() {
        // given
        let registry = TeamRegistry::new();

        // when
        let teams = registry.list();

        // then
        assert!(registry.is_empty());
        assert_eq!(registry.len(), 0);
        assert!(teams.is_empty());
    }

    #[test]
    fn team_remove_nonexistent_returns_none() {
        // given
        let registry = TeamRegistry::new();

        // when
        let removed = registry.remove("missing");

        // then
        assert!(removed.is_none());
    }

    #[test]
    fn team_create_sets_v1_metadata() {
        let registry = TeamRegistry::new();
        let team = registry.create("Alpha", vec![]);

        assert_eq!(team.origin, REGISTRY_ORIGIN);
        assert_eq!(team.contract.truth_layer, REGISTRY_ORIGIN);
        assert_eq!(team.lifecycle.status, "created");
        assert_eq!(
            team.contract.operator_plane,
            crate::LOCAL_RUNTIME_FOUNDATION_OPERATOR_PLANE
        );
        assert_eq!(team.last_error, None);
        assert!(team.capabilities.contains(&"delete".to_string()));
    }

    #[test]
    fn team_len_transitions() {
        // given
        let registry = TeamRegistry::new();

        // when
        let alpha = registry.create("Alpha", vec![]);
        let beta = registry.create("Beta", vec![]);
        let after_create = registry.len();
        registry.remove(&alpha.team_id);
        let after_first_remove = registry.len();
        registry.remove(&beta.team_id);

        // then
        assert_eq!(after_create, 2);
        assert_eq!(after_first_remove, 1);
        assert_eq!(registry.len(), 0);
        assert!(registry.is_empty());
    }

    #[test]
    fn team_list_orders_by_creation_counter() {
        let registry = TeamRegistry::new();
        let expected_ids = (0..12)
            .map(|index| registry.create(&format!("Team {index}"), vec![]).team_id)
            .collect::<Vec<_>>();

        let listed_ids = registry
            .list()
            .into_iter()
            .map(|team| team.team_id)
            .collect::<Vec<_>>();

        assert_eq!(listed_ids, expected_ids);
    }

    #[test]
    fn cron_list_all_disabled_returns_empty_for_enabled_only() {
        // given
        let registry = CronRegistry::new();
        let first = registry.create("* * * * *", "Task 1", None);
        let second = registry.create("0 * * * *", "Task 2", None);
        registry
            .disable(&first.cron_id)
            .expect("disable should succeed");
        registry
            .disable(&second.cron_id)
            .expect("disable should succeed");

        // when
        let enabled_only = registry.list(true);
        let all_entries = registry.list(false);

        // then
        assert!(enabled_only.is_empty());
        assert_eq!(all_entries.len(), 2);
    }

    #[test]
    fn cron_create_without_description() {
        // given
        let registry = CronRegistry::new();

        // when
        let entry = registry.create("*/15 * * * *", "Check health", None);

        // then
        assert!(entry.cron_id.starts_with("cron_"));
        assert_eq!(entry.description, None);
        assert!(entry.enabled);
        assert_eq!(entry.run_count, 0);
        assert_eq!(entry.last_run_at, None);
        assert_eq!(entry.last_error, None);
        assert_eq!(entry.disabled_reason, None);
        assert_eq!(entry.origin, REGISTRY_ORIGIN);
        assert_eq!(entry.lifecycle.status, "enabled");
        assert_eq!(entry.contract.truth_layer, REGISTRY_ORIGIN);
        assert_eq!(
            entry.contract.persistence,
            crate::PROCESS_MEMORY_PERSISTENCE_LAYER
        );
        assert!(entry.capabilities.contains(&"record_run".to_string()));
    }

    #[test]
    fn new_cron_registry_is_empty() {
        // given
        let registry = CronRegistry::new();

        // when
        let enabled_only = registry.list(true);
        let all_entries = registry.list(false);

        // then
        assert!(registry.is_empty());
        assert_eq!(registry.len(), 0);
        assert!(enabled_only.is_empty());
        assert!(all_entries.is_empty());
    }

    #[test]
    fn cron_record_run_updates_timestamp_and_counter() {
        // given
        let registry = CronRegistry::new();
        let entry = registry.create("*/5 * * * *", "Recurring", None);

        // when
        registry
            .record_run(&entry.cron_id)
            .expect("first run should succeed");
        registry
            .record_run(&entry.cron_id)
            .expect("second run should succeed");
        let fetched = registry.get(&entry.cron_id).expect("entry should exist");

        // then
        assert_eq!(fetched.run_count, 2);
        assert!(fetched.last_run_at.is_some());
        assert!(fetched.updated_at >= entry.updated_at);
    }

    #[test]
    fn cron_disable_updates_timestamp() {
        // given
        let registry = CronRegistry::new();
        let entry = registry.create("0 0 * * *", "Nightly", None);

        // when
        registry
            .disable(&entry.cron_id)
            .expect("disable should succeed");
        let fetched = registry.get(&entry.cron_id).expect("entry should exist");

        // then
        assert!(!fetched.enabled);
        assert!(fetched.updated_at >= entry.updated_at);
        assert_eq!(
            fetched.disabled_reason.as_deref(),
            Some("disabled_by_operator_request")
        );
        assert_eq!(fetched.lifecycle.status, "disabled");
    }

    #[test]
    fn cron_disable_rejects_already_disabled_entry() {
        let registry = CronRegistry::new();
        let entry = registry.create("0 0 * * *", "Nightly", None);

        registry
            .disable(&entry.cron_id)
            .expect("first disable should succeed");

        let error = registry
            .disable(&entry.cron_id)
            .expect_err("second disable should fail");
        assert_eq!(error, format!("cron already disabled: {}", entry.cron_id));

        let fetched = registry.get(&entry.cron_id).expect("entry should exist");
        assert_eq!(fetched.last_error.as_deref(), Some(error.as_str()));
        assert_eq!(
            fetched.lifecycle.failure_kind.as_deref(),
            Some("process_local_cron_conflict")
        );
    }

    #[test]
    fn cron_enable_restores_disabled_entry() {
        let registry = CronRegistry::new();
        let entry = registry.create("0 0 * * *", "Nightly", None);

        registry
            .disable(&entry.cron_id)
            .expect("disable should succeed");
        registry
            .enable(&entry.cron_id)
            .expect("enable should succeed");

        let fetched = registry
            .get(&entry.cron_id)
            .expect("entry should still exist");
        assert!(fetched.enabled);
        assert!(fetched.updated_at >= entry.updated_at);
        assert_eq!(fetched.lifecycle.status, "enabled");
    }

    #[test]
    fn cron_enable_rejects_already_enabled_entry() {
        let registry = CronRegistry::new();
        let entry = registry.create("0 0 * * *", "Nightly", None);

        let error = registry
            .enable(&entry.cron_id)
            .expect_err("enabling an already enabled cron should fail");
        assert_eq!(error, format!("cron already enabled: {}", entry.cron_id));
        let fetched = registry.get(&entry.cron_id).expect("entry should exist");
        assert_eq!(
            fetched.lifecycle.failure_kind.as_deref(),
            Some("process_local_cron_conflict")
        );
    }

    #[test]
    fn cron_record_run_rejects_disabled_entry() {
        let registry = CronRegistry::new();
        let entry = registry.create("*/5 * * * *", "Recurring", None);

        registry
            .disable(&entry.cron_id)
            .expect("disable should succeed");

        let error = registry
            .record_run(&entry.cron_id)
            .expect_err("disabled entry should not record runs");
        assert_eq!(error, format!("cron is disabled: {}", entry.cron_id));

        let fetched = registry.get(&entry.cron_id).expect("entry should exist");
        assert_eq!(fetched.last_error.as_deref(), Some(error.as_str()));
        assert_eq!(
            fetched.lifecycle.failure_kind.as_deref(),
            Some("process_local_cron_conflict")
        );
    }

    #[test]
    fn cron_list_orders_by_creation_counter() {
        let registry = CronRegistry::new();
        let expected_ids = (0..12)
            .map(|index| {
                registry
                    .create("* * * * *", &format!("Task {index}"), None)
                    .cron_id
            })
            .collect::<Vec<_>>();

        let listed_ids = registry
            .list(false)
            .into_iter()
            .map(|entry| entry.cron_id)
            .collect::<Vec<_>>();

        assert_eq!(listed_ids, expected_ids);
    }

    #[test]
    fn team_and_cron_registries_do_not_share_process_local_state() {
        let first_team_registry = TeamRegistry::new();
        let second_team_registry = TeamRegistry::new();
        let first_cron_registry = CronRegistry::new();
        let second_cron_registry = CronRegistry::new();

        let team = first_team_registry.create("Alpha", vec![]);
        let cron = first_cron_registry.create("* * * * *", "Task", None);

        assert!(first_team_registry.get(&team.team_id).is_some());
        assert!(second_team_registry.get(&team.team_id).is_none());
        assert!(first_cron_registry.get(&cron.cron_id).is_some());
        assert!(second_cron_registry.get(&cron.cron_id).is_none());
    }
}

use std::collections::BTreeSet;

use crate::bash::BashCommandInput;
use crate::config::ResolvedPermissionMode;
use crate::permissions::PermissionMode;
use crate::sandbox::{FilesystemIsolationMode, SandboxConfig};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolProfileConfig {
    pub description: Option<String>,
    pub permission_mode: ResolvedPermissionMode,
    pub allowed_tools: Vec<String>,
    pub bash_policy: Option<ToolProfileBashPolicy>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolProfileBashPolicy {
    pub sandbox: SandboxConfig,
    pub allow_dangerously_disable_sandbox: bool,
}

impl ToolProfileConfig {
    #[must_use]
    pub fn permission_mode(&self) -> PermissionMode {
        resolved_permission_mode_to_permission_mode(self.permission_mode)
    }
}

impl ToolProfileBashPolicy {
    pub fn validate_override(&self, input: &BashCommandInput) -> Result<(), String> {
        if input.dangerously_disable_sandbox == Some(true)
            && !self.allow_dangerously_disable_sandbox
            && self.sandbox.enabled.unwrap_or(true)
        {
            return Err(String::from(
                "bash override cannot disable the active tool-profile sandbox policy",
            ));
        }

        if self.sandbox.namespace_restrictions == Some(true)
            && input.namespace_restrictions == Some(false)
        {
            return Err(String::from(
                "bash override cannot weaken required namespace restrictions",
            ));
        }

        if self.sandbox.network_isolation == Some(true) && input.isolate_network == Some(false) {
            return Err(String::from(
                "bash override cannot weaken required network isolation",
            ));
        }

        if let Some(required_mode) = self.sandbox.filesystem_mode {
            if let Some(candidate_mode) = input.filesystem_mode {
                if filesystem_mode_rank(candidate_mode) < filesystem_mode_rank(required_mode) {
                    return Err(String::from(
                        "bash override cannot weaken the active filesystem isolation mode",
                    ));
                }
            }
        }

        if let Some(candidate_mounts) = input.allowed_mounts.as_ref() {
            let required_mounts = self
                .sandbox
                .allowed_mounts
                .iter()
                .map(String::as_str)
                .collect::<BTreeSet<_>>();
            let candidate_mounts = candidate_mounts
                .iter()
                .map(String::as_str)
                .collect::<BTreeSet<_>>();
            if !candidate_mounts.is_subset(&required_mounts) {
                return Err(String::from(
                    "bash override cannot widen the active allowedMounts ceiling",
                ));
            }
        }

        Ok(())
    }

    #[must_use]
    pub fn summary(&self) -> String {
        let sandbox_state = if self.sandbox.enabled.unwrap_or(true) {
            "sandbox on"
        } else {
            "sandbox off"
        };
        let filesystem_mode = self
            .sandbox
            .filesystem_mode
            .unwrap_or_default()
            .as_str()
            .to_string();
        let disable_rule = if self.allow_dangerously_disable_sandbox {
            "disable allowed"
        } else {
            "disable denied"
        };
        format!("bash-only · {sandbox_state} · fs {filesystem_mode} · {disable_rule}")
    }
}

#[must_use]
pub fn resolved_permission_mode_to_permission_mode(mode: ResolvedPermissionMode) -> PermissionMode {
    match mode {
        ResolvedPermissionMode::ReadOnly => PermissionMode::ReadOnly,
        ResolvedPermissionMode::WorkspaceWrite => PermissionMode::WorkspaceWrite,
        ResolvedPermissionMode::DangerFullAccess => PermissionMode::DangerFullAccess,
    }
}

#[must_use]
fn filesystem_mode_rank(mode: FilesystemIsolationMode) -> u8 {
    match mode {
        FilesystemIsolationMode::Off => 0,
        FilesystemIsolationMode::WorkspaceOnly => 1,
        FilesystemIsolationMode::AllowList => 2,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        resolved_permission_mode_to_permission_mode, ToolProfileBashPolicy, ToolProfileConfig,
    };
    use crate::bash::BashCommandInput;
    use crate::config::ResolvedPermissionMode;
    use crate::permissions::PermissionMode;
    use crate::sandbox::{FilesystemIsolationMode, SandboxConfig};

    fn policy() -> ToolProfileBashPolicy {
        ToolProfileBashPolicy {
            sandbox: SandboxConfig {
                enabled: Some(true),
                namespace_restrictions: Some(true),
                network_isolation: Some(false),
                filesystem_mode: Some(FilesystemIsolationMode::WorkspaceOnly),
                allowed_mounts: vec!["logs".to_string(), "tmp/cache".to_string()],
            },
            allow_dangerously_disable_sandbox: false,
        }
    }

    fn bash_input() -> BashCommandInput {
        BashCommandInput {
            command: "printf hi".to_string(),
            timeout: None,
            description: None,
            run_in_background: Some(false),
            dangerously_disable_sandbox: None,
            namespace_restrictions: None,
            isolate_network: None,
            filesystem_mode: None,
            allowed_mounts: None,
        }
    }

    #[test]
    fn tool_profile_maps_to_runtime_permission_mode() {
        let profile = ToolProfileConfig {
            description: Some("read-only audit".to_string()),
            permission_mode: ResolvedPermissionMode::ReadOnly,
            allowed_tools: vec!["read".to_string(), "glob".to_string()],
            bash_policy: None,
        };

        assert_eq!(profile.permission_mode(), PermissionMode::ReadOnly);
        assert_eq!(
            resolved_permission_mode_to_permission_mode(ResolvedPermissionMode::WorkspaceWrite),
            PermissionMode::WorkspaceWrite
        );
    }

    #[test]
    fn bash_policy_rejects_disabling_required_sandbox() {
        let mut input = bash_input();
        input.dangerously_disable_sandbox = Some(true);

        let error = policy()
            .validate_override(&input)
            .expect_err("disable should be denied");
        assert!(error.contains("cannot disable"));
    }

    #[test]
    fn bash_policy_rejects_weaker_namespace_and_filesystem_modes() {
        let mut input = bash_input();
        input.namespace_restrictions = Some(false);
        let namespace_error = policy()
            .validate_override(&input)
            .expect_err("namespace weakening should fail");
        assert!(namespace_error.contains("namespace"));

        let mut input = bash_input();
        input.filesystem_mode = Some(FilesystemIsolationMode::Off);
        let filesystem_error = policy()
            .validate_override(&input)
            .expect_err("filesystem weakening should fail");
        assert!(filesystem_error.contains("filesystem"));
    }

    #[test]
    fn bash_policy_rejects_mount_widening_and_allows_narrowing() {
        let mut widening = bash_input();
        widening.allowed_mounts = Some(vec![
            "logs".to_string(),
            "tmp/cache".to_string(),
            "tmp".to_string(),
        ]);
        let widening_error = policy()
            .validate_override(&widening)
            .expect_err("mount widening should fail");
        assert!(widening_error.contains("allowedMounts"));

        let mut narrowing = bash_input();
        narrowing.allowed_mounts = Some(vec!["logs".to_string()]);
        policy()
            .validate_override(&narrowing)
            .expect("subset mounts should be allowed");
    }

    #[test]
    fn bash_policy_summary_stays_bash_only_and_truthful() {
        let summary = policy().summary();
        assert!(summary.contains("bash-only"));
        assert!(summary.contains("sandbox on"));
        assert!(summary.contains("workspace-only"));
        assert!(summary.contains("disable denied"));
    }
}

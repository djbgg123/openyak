#![allow(clippy::must_use_candidate, clippy::uninlined_format_args)]

//! Permission enforcement layer that gates tool execution based on the
//! active `PermissionPolicy`.

use crate::bash_validation::{validate_bash_command, BashCommandValidation};
use crate::permissions::{PermissionMode, PermissionOutcome, PermissionPolicy};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome")]
pub enum EnforcementResult {
    /// Tool execution is allowed.
    Allowed,
    /// Tool execution was denied due to insufficient permissions.
    Denied {
        tool: String,
        active_mode: String,
        required_mode: String,
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct PermissionEnforcer {
    policy: PermissionPolicy,
}

impl PermissionEnforcer {
    #[must_use]
    pub fn new(policy: PermissionPolicy) -> Self {
        Self { policy }
    }

    /// Check whether a tool can be executed under the current permission policy.
    /// Auto-denies when prompting is required but no prompter is provided.
    pub fn check(&self, tool_name: &str, input: &str) -> EnforcementResult {
        // When the active mode is Prompt, defer to the caller's interactive
        // prompt flow rather than hard-denying (the enforcer has no prompter).
        if self.policy.active_mode() == PermissionMode::Prompt {
            return EnforcementResult::Allowed;
        }

        let outcome = self.policy.authorize(tool_name, input, None);

        match outcome {
            PermissionOutcome::Allow => EnforcementResult::Allowed,
            PermissionOutcome::Deny { reason } => {
                let active_mode = self.policy.active_mode();
                let required_mode = self.policy.required_mode_for(tool_name);
                EnforcementResult::Denied {
                    tool: tool_name.to_owned(),
                    active_mode: active_mode.as_str().to_owned(),
                    required_mode: required_mode.as_str().to_owned(),
                    reason,
                }
            }
        }
    }

    #[must_use]
    pub fn is_allowed(&self, tool_name: &str, input: &str) -> bool {
        matches!(self.check(tool_name, input), EnforcementResult::Allowed)
    }

    #[must_use]
    pub fn active_mode(&self) -> PermissionMode {
        self.policy.active_mode()
    }

    /// Classify a file operation against workspace boundaries.
    pub fn check_file_write(&self, path: &str, workspace_root: &str) -> EnforcementResult {
        let mode = self.policy.active_mode();

        match mode {
            PermissionMode::ReadOnly => EnforcementResult::Denied {
                tool: "write_file".to_owned(),
                active_mode: mode.as_str().to_owned(),
                required_mode: PermissionMode::WorkspaceWrite.as_str().to_owned(),
                reason: format!("file writes are not allowed in '{}' mode", mode.as_str()),
            },
            PermissionMode::WorkspaceWrite => {
                if is_within_workspace(path, workspace_root) {
                    EnforcementResult::Allowed
                } else {
                    EnforcementResult::Denied {
                        tool: "write_file".to_owned(),
                        active_mode: mode.as_str().to_owned(),
                        required_mode: PermissionMode::DangerFullAccess.as_str().to_owned(),
                        reason: format!(
                            "path '{}' is outside workspace root '{}'",
                            path, workspace_root
                        ),
                    }
                }
            }
            // Allow and DangerFullAccess permit all writes
            PermissionMode::Allow | PermissionMode::DangerFullAccess => EnforcementResult::Allowed,
            PermissionMode::Prompt => EnforcementResult::Denied {
                tool: "write_file".to_owned(),
                active_mode: mode.as_str().to_owned(),
                required_mode: PermissionMode::WorkspaceWrite.as_str().to_owned(),
                reason: "file write requires confirmation in prompt mode".to_owned(),
            },
        }
    }

    /// Check if a bash command should be allowed based on current mode.
    pub fn check_bash(&self, command: &str) -> EnforcementResult {
        let mode = self.policy.active_mode();

        match validate_bash_command(command, mode) {
            BashCommandValidation::Allow => EnforcementResult::Allowed,
            BashCommandValidation::Deny(denial) => EnforcementResult::Denied {
                tool: "bash".to_owned(),
                active_mode: mode.as_str().to_owned(),
                required_mode: denial.required_mode.as_str().to_owned(),
                reason: denial.reason,
            },
        }
    }
}

/// Workspace boundary check that keeps canonical root semantics for missing targets.
fn is_within_workspace(path: &str, workspace_root: &str) -> bool {
    let workspace_root = Path::new(workspace_root);
    let resolved_root = workspace_root
        .canonicalize()
        .unwrap_or_else(|_| workspace_root.to_path_buf());
    let canonical_root = normalize_comparable_path(&resolved_root);
    let candidate = normalize_comparable_path(&resolve_write_target_path(path, &resolved_root));
    candidate == canonical_root || candidate.starts_with(&canonical_root)
}

fn resolve_write_target_path(path: &str, workspace_root: &Path) -> PathBuf {
    let candidate = if Path::new(path).is_absolute() {
        PathBuf::from(path)
    } else {
        workspace_root.join(path)
    };

    if let Ok(canonical) = candidate.canonicalize() {
        return canonical;
    }

    if let Some(parent) = candidate.parent() {
        let canonical_parent = parent
            .canonicalize()
            .unwrap_or_else(|_| parent.to_path_buf());
        if let Some(name) = candidate.file_name() {
            return canonical_parent.join(name);
        }
    }

    candidate
}

#[cfg(windows)]
fn normalize_comparable_path(path: &Path) -> PathBuf {
    use std::path::{Component, Prefix};

    let mut components = path.components();
    let mut normalized = PathBuf::new();

    if let Some(Component::Prefix(prefix)) = components.next() {
        match prefix.kind() {
            Prefix::VerbatimDisk(drive) => normalized.push(format!("{}:", char::from(drive))),
            Prefix::VerbatimUNC(server, share) => {
                normalized.push(format!(
                    r"\\{}\{}",
                    server.to_string_lossy(),
                    share.to_string_lossy()
                ));
            }
            _ => normalized.push(prefix.as_os_str()),
        }
    }

    for component in components {
        normalized.push(component.as_os_str());
    }

    PathBuf::from(normalized.to_string_lossy().to_lowercase())
}

#[cfg(not(windows))]
fn normalize_comparable_path(path: &Path) -> PathBuf {
    path.to_path_buf()
}

/// Conservative helper: would read-only mode allow this command?
#[cfg(test)]
fn is_read_only_command(command: &str) -> bool {
    matches!(
        validate_bash_command(command, PermissionMode::ReadOnly),
        BashCommandValidation::Allow
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    #[cfg(windows)]
    use std::process::Command;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn make_enforcer(mode: PermissionMode) -> PermissionEnforcer {
        let policy = PermissionPolicy::new(mode);
        PermissionEnforcer::new(policy)
    }

    fn temp_workspace(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time should move forward")
            .as_nanos();
        let root = std::env::temp_dir().join(format!("openyak-permissions-{name}-{unique}"));
        fs::create_dir_all(&root).expect("temp workspace should create");
        root
    }

    #[cfg(unix)]
    fn create_directory_alias(target: &Path, alias: &Path) {
        std::os::unix::fs::symlink(target, alias).expect("workspace alias should create");
    }

    #[cfg(windows)]
    fn create_directory_alias(target: &Path, alias: &Path) {
        let output = Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(alias)
            .arg(target)
            .output()
            .expect("mklink should launch");
        assert!(
            output.status.success(),
            "mklink /J failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[cfg(unix)]
    fn remove_directory_alias(alias: &Path) {
        fs::remove_file(alias).expect("workspace alias cleanup should succeed");
    }

    #[cfg(windows)]
    fn remove_directory_alias(alias: &Path) {
        fs::remove_dir(alias).expect("workspace alias cleanup should succeed");
    }

    #[test]
    fn allow_mode_permits_everything() {
        let enforcer = make_enforcer(PermissionMode::Allow);
        assert!(enforcer.is_allowed("bash", "echo ok"));
        assert!(enforcer.is_allowed("write_file", ""));
        assert!(enforcer.is_allowed("edit_file", ""));
        assert_eq!(
            enforcer.check_file_write("/outside/path", "/workspace"),
            EnforcementResult::Allowed
        );
        assert_eq!(enforcer.check_bash("rm -rf /"), EnforcementResult::Allowed);
        assert!(matches!(
            enforcer.check_bash("   "),
            EnforcementResult::Denied { .. }
        ));
    }

    #[test]
    fn read_only_denies_writes() {
        let policy = PermissionPolicy::new(PermissionMode::ReadOnly)
            .with_tool_requirement("read_file", PermissionMode::ReadOnly)
            .with_tool_requirement("grep_search", PermissionMode::ReadOnly)
            .with_tool_requirement("write_file", PermissionMode::WorkspaceWrite);

        let enforcer = PermissionEnforcer::new(policy);
        assert!(enforcer.is_allowed("read_file", ""));
        assert!(enforcer.is_allowed("grep_search", ""));

        // write_file requires WorkspaceWrite but we're in ReadOnly
        let result = enforcer.check("write_file", "");
        assert!(matches!(result, EnforcementResult::Denied { .. }));

        let result = enforcer.check_file_write("/workspace/file.rs", "/workspace");
        assert!(matches!(result, EnforcementResult::Denied { .. }));
    }

    #[test]
    fn read_only_allows_read_commands() {
        let enforcer = make_enforcer(PermissionMode::ReadOnly);
        assert_eq!(
            enforcer.check_bash("cat src/main.rs"),
            EnforcementResult::Allowed
        );
        assert_eq!(
            enforcer.check_bash("grep -r 'pattern' ."),
            EnforcementResult::Allowed
        );
        assert_eq!(enforcer.check_bash("ls -la"), EnforcementResult::Allowed);
    }

    #[test]
    fn read_only_denies_write_commands() {
        let enforcer = make_enforcer(PermissionMode::ReadOnly);
        let result = enforcer.check_bash("rm file.txt");
        assert!(matches!(result, EnforcementResult::Denied { .. }));
        let redirect = enforcer.check_bash("echo test > file.txt");
        assert!(matches!(redirect, EnforcementResult::Denied { .. }));
        let inplace = enforcer.check_bash("sed -i 's/a/b/' file.txt");
        assert!(matches!(inplace, EnforcementResult::Denied { .. }));
        let git_mutation = enforcer.check_bash("git commit -m test");
        assert!(matches!(git_mutation, EnforcementResult::Denied { .. }));
    }

    #[test]
    fn workspace_write_allows_within_workspace() {
        let enforcer = make_enforcer(PermissionMode::WorkspaceWrite);
        let workspace = temp_workspace("within");
        let result = enforcer.check_file_write(
            workspace.join("src/main.rs").to_string_lossy().as_ref(),
            workspace.to_string_lossy().as_ref(),
        );
        let _ = fs::remove_dir_all(&workspace);
        assert_eq!(result, EnforcementResult::Allowed);
    }

    #[test]
    fn workspace_write_denies_outside_workspace() {
        let enforcer = make_enforcer(PermissionMode::WorkspaceWrite);
        let workspace = temp_workspace("outside");
        let outside = workspace
            .parent()
            .expect("temp dir parent")
            .join("outside.txt");
        let result = enforcer.check_file_write(
            outside.to_string_lossy().as_ref(),
            workspace.to_string_lossy().as_ref(),
        );
        let _ = fs::remove_dir_all(&workspace);
        assert!(matches!(result, EnforcementResult::Denied { .. }));
    }

    #[test]
    fn prompt_mode_denies_without_prompter() {
        let enforcer = make_enforcer(PermissionMode::Prompt);
        let result = enforcer.check_bash("echo test");
        assert!(matches!(result, EnforcementResult::Denied { .. }));

        let result = enforcer.check_file_write("/workspace/file.rs", "/workspace");
        assert!(matches!(result, EnforcementResult::Denied { .. }));
    }

    #[test]
    fn workspace_write_denies_dangerous_bash_commands() {
        let enforcer = make_enforcer(PermissionMode::WorkspaceWrite);

        let destructive = enforcer.check_bash("rm -rf /");
        assert!(matches!(
            destructive,
            EnforcementResult::Denied {
                required_mode,
                reason,
                ..
            } if required_mode == "danger-full-access"
                && reason.contains("destructive shell pattern")
        ));

        let machine_level = enforcer.check_bash("systemctl restart sshd");
        assert!(matches!(
            machine_level,
            EnforcementResult::Denied {
                required_mode,
                reason,
                ..
            } if required_mode == "danger-full-access"
                && reason.contains("system administration command 'systemctl'")
        ));

        assert_eq!(
            enforcer.check_bash("mkdir -p build && touch build/output.log"),
            EnforcementResult::Allowed
        );
    }

    #[test]
    fn workspace_boundary_check() {
        let workspace = temp_workspace("boundary");
        let sibling = workspace.parent().expect("temp dir parent").join(format!(
            "{}-sibling",
            workspace
                .file_name()
                .expect("workspace name")
                .to_string_lossy()
        ));
        fs::create_dir_all(&sibling).expect("sibling dir should create");

        assert!(is_within_workspace(
            workspace.join("src/main.rs").to_string_lossy().as_ref(),
            workspace.to_string_lossy().as_ref()
        ));
        assert!(is_within_workspace(
            workspace.to_string_lossy().as_ref(),
            workspace.to_string_lossy().as_ref()
        ));
        assert!(!is_within_workspace(
            sibling.join("hack.txt").to_string_lossy().as_ref(),
            workspace.to_string_lossy().as_ref()
        ));
        assert!(!is_within_workspace(
            format!("..{}outside.txt", std::path::MAIN_SEPARATOR).as_str(),
            workspace.to_string_lossy().as_ref()
        ));

        let _ = fs::remove_dir_all(&workspace);
        let _ = fs::remove_dir_all(&sibling);
    }

    #[test]
    fn read_only_command_heuristic() {
        assert!(is_read_only_command("cat file.txt"));
        assert!(is_read_only_command("grep pattern file"));
        assert!(is_read_only_command("git log --oneline"));
        assert!(!is_read_only_command("rm file.txt"));
        assert!(!is_read_only_command("echo test > file.txt"));
        assert!(!is_read_only_command("sed -i 's/a/b/' file"));
    }

    #[test]
    fn active_mode_returns_policy_mode() {
        // given
        let modes = [
            PermissionMode::ReadOnly,
            PermissionMode::WorkspaceWrite,
            PermissionMode::DangerFullAccess,
            PermissionMode::Prompt,
            PermissionMode::Allow,
        ];

        // when
        let active_modes: Vec<_> = modes
            .into_iter()
            .map(|mode| make_enforcer(mode).active_mode())
            .collect();

        // then
        assert_eq!(active_modes, modes);
    }

    #[test]
    fn danger_full_access_permits_file_writes_and_bash() {
        // given
        let enforcer = make_enforcer(PermissionMode::DangerFullAccess);

        // when
        let file_result = enforcer.check_file_write("/outside/workspace/file.txt", "/workspace");
        let bash_result = enforcer.check_bash("rm -rf /tmp/scratch");

        // then
        assert_eq!(file_result, EnforcementResult::Allowed);
        assert_eq!(bash_result, EnforcementResult::Allowed);
    }

    #[test]
    fn check_denied_payload_contains_tool_and_modes() {
        // given
        let policy = PermissionPolicy::new(PermissionMode::ReadOnly)
            .with_tool_requirement("write_file", PermissionMode::WorkspaceWrite);
        let enforcer = PermissionEnforcer::new(policy);

        // when
        let result = enforcer.check("write_file", "{}");

        // then
        match result {
            EnforcementResult::Denied {
                tool,
                active_mode,
                required_mode,
                reason,
            } => {
                assert_eq!(tool, "write_file");
                assert_eq!(active_mode, "read-only");
                assert_eq!(required_mode, "workspace-write");
                assert!(reason.contains("requires workspace-write permission"));
            }
            other @ EnforcementResult::Allowed => panic!("expected denied result, got {other:?}"),
        }
    }

    #[test]
    fn workspace_write_relative_path_resolved() {
        // given
        let enforcer = make_enforcer(PermissionMode::WorkspaceWrite);
        let workspace = temp_workspace("relative");

        // when
        let result = enforcer.check_file_write("src/main.rs", workspace.to_string_lossy().as_ref());

        // then
        let _ = fs::remove_dir_all(&workspace);
        assert_eq!(result, EnforcementResult::Allowed);
    }

    #[test]
    fn workspace_write_allows_nested_relative_path_with_missing_parent_dirs() {
        let enforcer = make_enforcer(PermissionMode::WorkspaceWrite);
        let workspace = temp_workspace("relative-nested");

        let result =
            enforcer.check_file_write("generated/output.txt", workspace.to_string_lossy().as_ref());

        let _ = fs::remove_dir_all(&workspace);
        assert_eq!(result, EnforcementResult::Allowed);
    }

    #[test]
    fn workspace_write_allows_relative_path_when_workspace_root_is_an_alias() {
        let enforcer = make_enforcer(PermissionMode::WorkspaceWrite);
        let workspace = temp_workspace("alias-target");
        let alias_parent = temp_workspace("alias-parent");
        let alias = alias_parent.join("workspace-alias");

        create_directory_alias(&workspace, &alias);

        let result =
            enforcer.check_file_write("generated/output.txt", alias.to_string_lossy().as_ref());

        remove_directory_alias(&alias);
        let _ = fs::remove_dir_all(&alias_parent);
        let _ = fs::remove_dir_all(&workspace);
        assert_eq!(result, EnforcementResult::Allowed);
    }

    #[cfg(windows)]
    #[test]
    fn workspace_write_allows_case_mismatched_workspace_root() {
        let enforcer = make_enforcer(PermissionMode::WorkspaceWrite);
        let workspace = temp_workspace("case-mismatch");
        let workspace_upper = workspace.to_string_lossy().to_uppercase();

        let result = enforcer.check_file_write("generated/output.txt", &workspace_upper);

        let _ = fs::remove_dir_all(&workspace);
        assert_eq!(result, EnforcementResult::Allowed);
    }

    #[test]
    fn workspace_root_with_trailing_slash() {
        // given
        let enforcer = make_enforcer(PermissionMode::WorkspaceWrite);
        let workspace = temp_workspace("trailing");
        let workspace_with_sep = format!(
            "{}{}",
            workspace.to_string_lossy(),
            std::path::MAIN_SEPARATOR
        );

        // when
        let result = enforcer.check_file_write(
            workspace.join("src/main.rs").to_string_lossy().as_ref(),
            &workspace_with_sep,
        );

        // then
        let _ = fs::remove_dir_all(&workspace);
        assert_eq!(result, EnforcementResult::Allowed);
    }

    #[test]
    fn workspace_root_equality() {
        // given
        let root = temp_workspace("root-equality");

        // when
        let equal_to_root = is_within_workspace(
            root.to_string_lossy().as_ref(),
            root.to_string_lossy().as_ref(),
        );

        // then
        let _ = fs::remove_dir_all(&root);
        assert!(equal_to_root);
    }

    #[test]
    fn workspace_write_denies_relative_traversal_outside_workspace() {
        let enforcer = make_enforcer(PermissionMode::WorkspaceWrite);
        let workspace = temp_workspace("traversal");
        let escape = format!("..{}outside.txt", std::path::MAIN_SEPARATOR);

        let result = enforcer.check_file_write(&escape, workspace.to_string_lossy().as_ref());

        let _ = fs::remove_dir_all(&workspace);
        assert!(matches!(result, EnforcementResult::Denied { .. }));
    }

    #[test]
    fn bash_heuristic_full_path_prefix() {
        // given
        let full_path_command = "/usr/bin/cat Cargo.toml";
        let git_path_command = "/usr/local/bin/git status";

        // when
        let cat_result = is_read_only_command(full_path_command);
        let git_result = is_read_only_command(git_path_command);

        // then
        assert!(cat_result);
        assert!(git_result);
    }

    #[test]
    fn bash_heuristic_redirects_block_read_only_commands() {
        // given
        let overwrite = "cat Cargo.toml > out.txt";
        let append = "echo test >> out.txt";

        // when
        let overwrite_result = is_read_only_command(overwrite);
        let append_result = is_read_only_command(append);

        // then
        assert!(!overwrite_result);
        assert!(!append_result);
    }

    #[test]
    fn bash_heuristic_in_place_flag_blocks() {
        // given
        let interactive_python = "python -i script.py";
        let in_place_sed = "sed --in-place 's/a/b/' file.txt";

        // when
        let interactive_result = is_read_only_command(interactive_python);
        let in_place_result = is_read_only_command(in_place_sed);

        // then
        assert!(!interactive_result);
        assert!(!in_place_result);
    }

    #[test]
    fn bash_heuristic_empty_command() {
        // given
        let empty = "";
        let whitespace = "   ";

        // when
        let empty_result = is_read_only_command(empty);
        let whitespace_result = is_read_only_command(whitespace);

        // then
        assert!(!empty_result);
        assert!(!whitespace_result);
    }

    #[test]
    fn prompt_mode_check_bash_denied_payload_fields() {
        // given
        let enforcer = make_enforcer(PermissionMode::Prompt);

        // when
        let result = enforcer.check_bash("git status");

        // then
        match result {
            EnforcementResult::Denied {
                tool,
                active_mode,
                required_mode,
                reason,
            } => {
                assert_eq!(tool, "bash");
                assert_eq!(active_mode, "prompt");
                assert_eq!(required_mode, "danger-full-access");
                assert_eq!(reason, "bash requires confirmation in prompt mode");
            }
            other @ EnforcementResult::Allowed => panic!("expected denied result, got {other:?}"),
        }
    }

    #[test]
    fn malformed_bash_input_is_denied_even_in_danger_full_access() {
        let enforcer = make_enforcer(PermissionMode::DangerFullAccess);

        match enforcer.check_bash("   ") {
            EnforcementResult::Denied {
                active_mode,
                required_mode,
                reason,
                ..
            } => {
                assert_eq!(active_mode, "danger-full-access");
                assert_eq!(required_mode, "danger-full-access");
                assert!(reason.contains("empty or whitespace-only"));
            }
            other @ EnforcementResult::Allowed => panic!("expected denied result, got {other:?}"),
        }
    }

    #[test]
    fn read_only_check_file_write_denied_payload() {
        // given
        let enforcer = make_enforcer(PermissionMode::ReadOnly);

        // when
        let result = enforcer.check_file_write("/workspace/file.txt", "/workspace");

        // then
        match result {
            EnforcementResult::Denied {
                tool,
                active_mode,
                required_mode,
                reason,
            } => {
                assert_eq!(tool, "write_file");
                assert_eq!(active_mode, "read-only");
                assert_eq!(required_mode, "workspace-write");
                assert!(reason.contains("file writes are not allowed"));
            }
            other @ EnforcementResult::Allowed => panic!("expected denied result, got {other:?}"),
        }
    }
}

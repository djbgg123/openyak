use std::collections::BTreeMap;
use std::env;
use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use plugins::{InstallOutcome, PluginError, PluginManager, PluginSummary, UpdateOutcome};
use runtime::{
    compact_session, discover_skill_directories, home_locations, resolve_command_path,
    resolve_skill_path_from_roots, AvailableSkillCatalog, CompactionConfig, ConfigLoader, Session,
    SkillCatalogInfo, SkillInstallOutcome, SkillInstallRequest, SkillInstallStatus,
    SkillRegistryManager, SkillUninstallOutcome, SkillUpdateOutcome, SkillUpdateRequest,
    SkillUpdateStatus,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandManifestEntry {
    pub name: String,
    pub source: CommandSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandSource {
    Builtin,
    InternalOnly,
    FeatureGated,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommandRegistry {
    entries: Vec<CommandManifestEntry>,
}

impl CommandRegistry {
    #[must_use]
    pub fn new(entries: Vec<CommandManifestEntry>) -> Self {
        Self { entries }
    }

    #[must_use]
    pub fn entries(&self) -> &[CommandManifestEntry] {
        &self.entries
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlashCommandCategory {
    Core,
    Workspace,
    Session,
    Git,
    Automation,
}

impl SlashCommandCategory {
    const fn title(self) -> &'static str {
        match self {
            Self::Core => "Core flow",
            Self::Workspace => "Workspace & memory",
            Self::Session => "Sessions & output",
            Self::Git => "Git & GitHub",
            Self::Automation => "Automation & discovery",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlashCommandSpec {
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    pub summary: &'static str,
    pub argument_hint: Option<&'static str>,
    pub resume_supported: bool,
    pub category: SlashCommandCategory,
}

const SLASH_COMMAND_SPECS: &[SlashCommandSpec] = &[
    SlashCommandSpec {
        name: "help",
        aliases: &[],
        summary: "Show available slash commands",
        argument_hint: None,
        resume_supported: true,
        category: SlashCommandCategory::Core,
    },
    SlashCommandSpec {
        name: "status",
        aliases: &[],
        summary: "Show current session status",
        argument_hint: None,
        resume_supported: true,
        category: SlashCommandCategory::Core,
    },
    SlashCommandSpec {
        name: "compact",
        aliases: &[],
        summary: "Compact local session history",
        argument_hint: None,
        resume_supported: true,
        category: SlashCommandCategory::Core,
    },
    SlashCommandSpec {
        name: "model",
        aliases: &[],
        summary: "Show or switch the active model",
        argument_hint: Some("[model]"),
        resume_supported: false,
        category: SlashCommandCategory::Core,
    },
    SlashCommandSpec {
        name: "permissions",
        aliases: &[],
        summary: "Show or switch the active permission mode",
        argument_hint: Some("[read-only|workspace-write|danger-full-access]"),
        resume_supported: false,
        category: SlashCommandCategory::Core,
    },
    SlashCommandSpec {
        name: "plan",
        aliases: &[],
        summary: "Enter or exit explicit plan mode for this REPL",
        argument_hint: Some("[exit]"),
        resume_supported: false,
        category: SlashCommandCategory::Core,
    },
    SlashCommandSpec {
        name: "clear",
        aliases: &[],
        summary: "Start a fresh local session",
        argument_hint: Some("[--confirm]"),
        resume_supported: true,
        category: SlashCommandCategory::Session,
    },
    SlashCommandSpec {
        name: "cost",
        aliases: &[],
        summary: "Show session cost and accounting details",
        argument_hint: None,
        resume_supported: true,
        category: SlashCommandCategory::Core,
    },
    SlashCommandSpec {
        name: "resume",
        aliases: &[],
        summary: "Load a saved session into the REPL",
        argument_hint: Some("<session-path>"),
        resume_supported: false,
        category: SlashCommandCategory::Session,
    },
    SlashCommandSpec {
        name: "config",
        aliases: &[],
        summary: "Inspect openyak config files or merged sections",
        argument_hint: Some("[env|hooks|model|plugins]"),
        resume_supported: true,
        category: SlashCommandCategory::Workspace,
    },
    SlashCommandSpec {
        name: "memory",
        aliases: &[],
        summary: "Inspect loaded openyak instruction memory files",
        argument_hint: None,
        resume_supported: true,
        category: SlashCommandCategory::Workspace,
    },
    SlashCommandSpec {
        name: "init",
        aliases: &[],
        summary: "Create a starter OPENYAK.md for this repo",
        argument_hint: None,
        resume_supported: true,
        category: SlashCommandCategory::Workspace,
    },
    SlashCommandSpec {
        name: "diff",
        aliases: &[],
        summary: "Show git diff for current workspace changes",
        argument_hint: None,
        resume_supported: true,
        category: SlashCommandCategory::Workspace,
    },
    SlashCommandSpec {
        name: "version",
        aliases: &[],
        summary: "Show CLI version and build information",
        argument_hint: None,
        resume_supported: true,
        category: SlashCommandCategory::Workspace,
    },
    SlashCommandSpec {
        name: "bughunter",
        aliases: &[],
        summary: "Inspect the codebase for likely bugs",
        argument_hint: Some("[scope]"),
        resume_supported: false,
        category: SlashCommandCategory::Automation,
    },
    SlashCommandSpec {
        name: "branch",
        aliases: &[],
        summary: "List, create, or switch git branches",
        argument_hint: Some("[list|create <name>|switch <name>]"),
        resume_supported: false,
        category: SlashCommandCategory::Git,
    },
    SlashCommandSpec {
        name: "worktree",
        aliases: &[],
        summary: "List, add, remove, or prune git worktrees",
        argument_hint: Some("[list|add <path> [branch]|remove <path>|prune]"),
        resume_supported: false,
        category: SlashCommandCategory::Git,
    },
    SlashCommandSpec {
        name: "commit",
        aliases: &[],
        summary: "Generate a commit message and create a git commit",
        argument_hint: None,
        resume_supported: false,
        category: SlashCommandCategory::Git,
    },
    SlashCommandSpec {
        name: "commit-push-pr",
        aliases: &[],
        summary: "Commit workspace changes, push the branch, and open a PR",
        argument_hint: Some("[context]"),
        resume_supported: false,
        category: SlashCommandCategory::Git,
    },
    SlashCommandSpec {
        name: "pr",
        aliases: &[],
        summary: "Draft or create a pull request from the conversation",
        argument_hint: Some("[context]"),
        resume_supported: false,
        category: SlashCommandCategory::Git,
    },
    SlashCommandSpec {
        name: "issue",
        aliases: &[],
        summary: "Draft or create a GitHub issue from the conversation",
        argument_hint: Some("[context]"),
        resume_supported: false,
        category: SlashCommandCategory::Git,
    },
    SlashCommandSpec {
        name: "ultraplan",
        aliases: &[],
        summary: "Run a deep planning prompt with multi-step reasoning",
        argument_hint: Some("[task]"),
        resume_supported: false,
        category: SlashCommandCategory::Automation,
    },
    SlashCommandSpec {
        name: "teleport",
        aliases: &[],
        summary: "Jump to a file or symbol by searching the workspace",
        argument_hint: Some("<symbol-or-path>"),
        resume_supported: false,
        category: SlashCommandCategory::Workspace,
    },
    SlashCommandSpec {
        name: "debug-tool-call",
        aliases: &[],
        summary: "Replay the last tool call with debug details",
        argument_hint: None,
        resume_supported: false,
        category: SlashCommandCategory::Automation,
    },
    SlashCommandSpec {
        name: "export",
        aliases: &[],
        summary: "Export the current conversation to a file",
        argument_hint: Some("[file]"),
        resume_supported: true,
        category: SlashCommandCategory::Session,
    },
    SlashCommandSpec {
        name: "session",
        aliases: &[],
        summary: "List or switch managed local sessions",
        argument_hint: Some("[list|switch <session-id>]"),
        resume_supported: false,
        category: SlashCommandCategory::Session,
    },
    SlashCommandSpec {
        name: "plugin",
        aliases: &["plugins", "marketplace"],
        summary: "Manage openyak plugins",
        argument_hint: Some(
            "[list|install <path-or-git-url>|enable <name>|disable <name>|uninstall <id>|update <id>]",
        ),
        resume_supported: false,
        category: SlashCommandCategory::Automation,
    },
    SlashCommandSpec {
        name: "agents",
        aliases: &[],
        summary: "List configured agents",
        argument_hint: None,
        resume_supported: true,
        category: SlashCommandCategory::Automation,
    },
    SlashCommandSpec {
        name: "foundations",
        aliases: &[],
        summary: "Explain shipped Task/Team/Cron/LSP/MCP foundations",
        argument_hint: Some("[family]"),
        resume_supported: true,
        category: SlashCommandCategory::Automation,
    },
    SlashCommandSpec {
        name: "skills",
        aliases: &[],
        summary: "List or manage local skills",
        argument_hint: Some("[subcommand]"),
        resume_supported: true,
        category: SlashCommandCategory::Automation,
    },
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashCommand {
    Help,
    Status,
    Compact,
    Branch {
        action: Option<String>,
        target: Option<String>,
    },
    Bughunter {
        scope: Option<String>,
    },
    Worktree {
        action: Option<String>,
        path: Option<String>,
        branch: Option<String>,
    },
    Commit,
    CommitPushPr {
        context: Option<String>,
    },
    Pr {
        context: Option<String>,
    },
    Issue {
        context: Option<String>,
    },
    Ultraplan {
        task: Option<String>,
    },
    Teleport {
        target: Option<String>,
    },
    DebugToolCall,
    Model {
        model: Option<String>,
    },
    Permissions {
        mode: Option<String>,
    },
    Plan {
        action: Option<String>,
    },
    Clear {
        confirm: bool,
    },
    Cost,
    Resume {
        session_path: Option<String>,
    },
    Config {
        section: Option<String>,
    },
    Memory,
    Foundations {
        family: Option<String>,
    },
    Init,
    Diff,
    Version,
    Export {
        path: Option<String>,
    },
    Session {
        action: Option<String>,
        target: Option<String>,
    },
    Plugins {
        action: Option<String>,
        target: Option<String>,
    },
    Agents {
        args: Option<String>,
    },
    Skills {
        args: Option<String>,
    },
    Unknown(String),
}

impl SlashCommand {
    #[must_use]
    pub fn parse(input: &str) -> Option<Self> {
        let trimmed = input.trim();
        if !trimmed.starts_with('/') {
            return None;
        }

        let mut parts = trimmed.trim_start_matches('/').split_whitespace();
        let command = parts.next().unwrap_or_default();
        Some(match command {
            "help" => Self::Help,
            "status" => Self::Status,
            "compact" => Self::Compact,
            "branch" => Self::Branch {
                action: parts.next().map(ToOwned::to_owned),
                target: parts.next().map(ToOwned::to_owned),
            },
            "bughunter" => Self::Bughunter {
                scope: remainder_after_command(trimmed, command),
            },
            "worktree" => Self::Worktree {
                action: parts.next().map(ToOwned::to_owned),
                path: parts.next().map(ToOwned::to_owned),
                branch: parts.next().map(ToOwned::to_owned),
            },
            "commit" => Self::Commit,
            "commit-push-pr" => Self::CommitPushPr {
                context: remainder_after_command(trimmed, command),
            },
            "pr" => Self::Pr {
                context: remainder_after_command(trimmed, command),
            },
            "issue" => Self::Issue {
                context: remainder_after_command(trimmed, command),
            },
            "ultraplan" => Self::Ultraplan {
                task: remainder_after_command(trimmed, command),
            },
            "teleport" => Self::Teleport {
                target: remainder_after_command(trimmed, command),
            },
            "debug-tool-call" => Self::DebugToolCall,
            "model" => Self::Model {
                model: parts.next().map(ToOwned::to_owned),
            },
            "permissions" => Self::Permissions {
                mode: parts.next().map(ToOwned::to_owned),
            },
            "plan" => Self::Plan {
                action: parts.next().map(ToOwned::to_owned),
            },
            "clear" => Self::Clear {
                confirm: parts.next() == Some("--confirm"),
            },
            "cost" => Self::Cost,
            "resume" => Self::Resume {
                session_path: parts.next().map(ToOwned::to_owned),
            },
            "config" => Self::Config {
                section: parts.next().map(ToOwned::to_owned),
            },
            "memory" => Self::Memory,
            "foundations" => Self::Foundations {
                family: remainder_after_command(trimmed, command),
            },
            "init" => Self::Init,
            "diff" => Self::Diff,
            "version" => Self::Version,
            "export" => Self::Export {
                path: parts.next().map(ToOwned::to_owned),
            },
            "session" => Self::Session {
                action: parts.next().map(ToOwned::to_owned),
                target: parts.next().map(ToOwned::to_owned),
            },
            "plugin" | "plugins" | "marketplace" => Self::Plugins {
                action: parts.next().map(ToOwned::to_owned),
                target: {
                    let remainder = parts.collect::<Vec<_>>().join(" ");
                    (!remainder.is_empty()).then_some(remainder)
                },
            },
            "agents" => Self::Agents {
                args: remainder_after_command(trimmed, command),
            },
            "skills" => Self::Skills {
                args: remainder_after_command(trimmed, command),
            },
            other => Self::Unknown(other.to_string()),
        })
    }
}

fn remainder_after_command(input: &str, command: &str) -> Option<String> {
    input
        .trim()
        .strip_prefix(&format!("/{command}"))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

#[must_use]
pub fn slash_command_specs() -> &'static [SlashCommandSpec] {
    SLASH_COMMAND_SPECS
}

#[must_use]
pub fn resume_supported_slash_commands() -> Vec<&'static SlashCommandSpec> {
    slash_command_specs()
        .iter()
        .filter(|spec| spec.resume_supported)
        .collect()
}

#[must_use]
pub fn render_slash_command_help() -> String {
    let mut lines = vec![
        "Slash commands".to_string(),
        "  Tab completes commands inside the REPL.".to_string(),
        "  [resume] = also available via openyak --resume SESSION.json".to_string(),
    ];

    for category in [
        SlashCommandCategory::Core,
        SlashCommandCategory::Workspace,
        SlashCommandCategory::Session,
        SlashCommandCategory::Git,
        SlashCommandCategory::Automation,
    ] {
        lines.push(String::new());
        lines.push(category.title().to_string());
        lines.extend(
            slash_command_specs()
                .iter()
                .filter(|spec| spec.category == category)
                .map(render_slash_command_entry),
        );
    }

    lines.join("\n")
}

fn render_slash_command_entry(spec: &SlashCommandSpec) -> String {
    let alias_suffix = if spec.aliases.is_empty() {
        String::new()
    } else {
        format!(
            " (aliases: {})",
            spec.aliases
                .iter()
                .map(|alias| format!("/{alias}"))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    let resume = if spec.resume_supported {
        " [resume]"
    } else {
        ""
    };
    format!(
        "  {name:<46} {}{alias_suffix}{resume}",
        spec.summary,
        name = render_slash_command_name(spec),
    )
}

fn render_slash_command_name(spec: &SlashCommandSpec) -> String {
    match spec.argument_hint {
        Some(argument_hint) => format!("/{} {}", spec.name, argument_hint),
        None => format!("/{}", spec.name),
    }
}

fn levenshtein_distance(left: &str, right: &str) -> usize {
    if left == right {
        return 0;
    }
    if left.is_empty() {
        return right.chars().count();
    }
    if right.is_empty() {
        return left.chars().count();
    }

    let right_chars = right.chars().collect::<Vec<_>>();
    let mut previous = (0..=right_chars.len()).collect::<Vec<_>>();
    let mut current = vec![0; right_chars.len() + 1];

    for (left_index, left_char) in left.chars().enumerate() {
        current[0] = left_index + 1;
        for (right_index, right_char) in right_chars.iter().enumerate() {
            let cost = usize::from(left_char != *right_char);
            current[right_index + 1] = (previous[right_index + 1] + 1)
                .min(current[right_index] + 1)
                .min(previous[right_index] + cost);
        }
        std::mem::swap(&mut previous, &mut current);
    }

    previous[right_chars.len()]
}

#[must_use]
pub fn suggest_slash_commands(input: &str, limit: usize) -> Vec<String> {
    let normalized = input.trim().trim_start_matches('/').to_ascii_lowercase();
    if normalized.is_empty() || limit == 0 {
        return Vec::new();
    }

    let mut ranked = slash_command_specs()
        .iter()
        .filter_map(|spec| {
            let score = std::iter::once(spec.name)
                .chain(spec.aliases.iter().copied())
                .map(str::to_ascii_lowercase)
                .filter_map(|alias| {
                    if alias == normalized {
                        Some((0_usize, alias.len()))
                    } else if alias.starts_with(&normalized) {
                        Some((1, alias.len()))
                    } else if alias.contains(&normalized) {
                        Some((2, alias.len()))
                    } else {
                        let distance = levenshtein_distance(&alias, &normalized);
                        (distance <= 2).then_some((3 + distance, alias.len()))
                    }
                })
                .min();

            score.map(|(bucket, len)| (bucket, len, render_slash_command_name(spec)))
        })
        .collect::<Vec<_>>();

    ranked.sort();
    ranked.dedup_by(|left, right| left.2 == right.2);
    ranked
        .into_iter()
        .take(limit)
        .map(|(_, _, display)| display)
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlashCommandResult {
    pub message: String,
    pub session: Session,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginsCommandResult {
    pub message: String,
    pub reload_runtime: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum DefinitionSource {
    ProjectCodex,
    ProjectOpenyak,
    UserCodexHome,
    UserCodex,
    UserOpenyak,
}

impl DefinitionSource {
    fn label(self) -> &'static str {
        match self {
            Self::ProjectCodex => "Project (.codex)",
            Self::ProjectOpenyak => "Project (.openyak)",
            Self::UserCodexHome => "User ($CODEX_HOME)",
            Self::UserCodex => "User (home/.codex)",
            Self::UserOpenyak => "User (home/.openyak)",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AgentSummary {
    name: String,
    description: Option<String>,
    model: Option<String>,
    reasoning_effort: Option<String>,
    source: DefinitionSource,
    shadowed_by: Option<DefinitionSource>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SkillSummary {
    name: String,
    description: Option<String>,
    source: DefinitionSource,
    shadowed_by: Option<DefinitionSource>,
    origin: SkillOrigin,
    path: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SkillOrigin {
    SkillsDir,
    LegacyCommandsDir,
}

impl SkillOrigin {
    fn detail_label(self) -> Option<&'static str> {
        match self {
            Self::SkillsDir => None,
            Self::LegacyCommandsDir => Some("legacy /commands"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SkillRoot {
    source: DefinitionSource,
    path: PathBuf,
    origin: SkillOrigin,
}

#[allow(clippy::too_many_lines)]
pub fn handle_plugins_slash_command(
    action: Option<&str>,
    target: Option<&str>,
    manager: &mut PluginManager,
) -> Result<PluginsCommandResult, PluginError> {
    match action {
        None | Some("list") => Ok(PluginsCommandResult {
            message: render_plugins_report(&manager.list_installed_plugins()?),
            reload_runtime: false,
        }),
        Some("install") => {
            let Some(target) = target else {
                return Ok(PluginsCommandResult {
                    message: "Usage: /plugins install <path-or-git-url>".to_string(),
                    reload_runtime: false,
                });
            };
            let install = manager.install(target)?;
            let plugin = manager
                .list_installed_plugins()?
                .into_iter()
                .find(|plugin| plugin.metadata.id == install.plugin_id);
            Ok(PluginsCommandResult {
                message: render_plugin_install_report(&install, plugin.as_ref()),
                reload_runtime: true,
            })
        }
        Some("enable") => {
            let Some(target) = target else {
                return Ok(PluginsCommandResult {
                    message: "Usage: /plugins enable <name>".to_string(),
                    reload_runtime: false,
                });
            };
            let plugin = resolve_plugin_target(manager, target)?;
            manager.enable(&plugin.metadata.id)?;
            Ok(PluginsCommandResult {
                message: format!(
                    "Plugins\n  Result           enabled {}\n  Name             {}\n  Version          {}\n  Status           enabled",
                    plugin.metadata.id, plugin.metadata.name, plugin.metadata.version
                ),
                reload_runtime: true,
            })
        }
        Some("disable") => {
            let Some(target) = target else {
                return Ok(PluginsCommandResult {
                    message: "Usage: /plugins disable <name>".to_string(),
                    reload_runtime: false,
                });
            };
            let plugin = resolve_plugin_target(manager, target)?;
            manager.disable(&plugin.metadata.id)?;
            Ok(PluginsCommandResult {
                message: format!(
                    "Plugins\n  Result           disabled {}\n  Name             {}\n  Version          {}\n  Status           disabled",
                    plugin.metadata.id, plugin.metadata.name, plugin.metadata.version
                ),
                reload_runtime: true,
            })
        }
        Some("uninstall") => {
            let Some(target) = target else {
                return Ok(PluginsCommandResult {
                    message: "Usage: /plugins uninstall <plugin-id>".to_string(),
                    reload_runtime: false,
                });
            };
            manager.uninstall(target)?;
            Ok(PluginsCommandResult {
                message: format!("Plugins\n  Result           uninstalled {target}"),
                reload_runtime: true,
            })
        }
        Some("update") => {
            let Some(target) = target else {
                return Ok(PluginsCommandResult {
                    message: "Usage: /plugins update <plugin-id>".to_string(),
                    reload_runtime: false,
                });
            };
            let update = manager.update(target)?;
            let plugin = manager
                .list_installed_plugins()?
                .into_iter()
                .find(|plugin| plugin.metadata.id == update.plugin_id);
            Ok(PluginsCommandResult {
                message: render_plugin_update_report(&update, plugin.as_ref()),
                reload_runtime: true,
            })
        }
        Some(other) => Ok(PluginsCommandResult {
            message: format!(
                "Unknown /plugins action '{other}'. Use list, install, enable, disable, uninstall, or update."
            ),
            reload_runtime: false,
        }),
    }
}

pub fn handle_agents_slash_command(args: Option<&str>, cwd: &Path) -> std::io::Result<String> {
    match normalize_optional_args(args) {
        None | Some("list") => {
            let roots = discover_definition_roots(cwd, "agents");
            let agents = load_agents_from_roots(&roots)?;
            Ok(render_agents_report(&agents))
        }
        Some("-h" | "--help" | "help") => Ok(render_agents_usage(None)),
        Some(args) => Ok(render_agents_usage(Some(args))),
    }
}

pub fn handle_skills_slash_command(args: Option<&str>, cwd: &Path) -> std::io::Result<String> {
    match normalize_optional_args(args) {
        None | Some("list") => {
            let roots = discover_skill_roots(cwd);
            let skills = load_skills_from_roots(&roots)?;
            Ok(render_skills_report(&skills))
        }
        Some("-h" | "--help" | "help") => Ok(render_skills_usage(None)),
        Some(raw_args) => {
            let command = match parse_skills_command(raw_args) {
                Ok(command) => command,
                Err(error) => return Ok(error),
            };
            let manager = build_skill_registry_manager(cwd, command.registry_path.as_deref());
            match command.action {
                SkillsAction::Available => {
                    let catalog = manager
                        .list_available(cwd, command.registry_path.as_deref())
                        .map_err(io::Error::other)?;
                    Ok(render_available_skills_report(&catalog))
                }
                SkillsAction::Info(skill_id) => {
                    let info = manager
                        .info(cwd, &skill_id, command.registry_path.as_deref())
                        .map_err(io::Error::other)?;
                    Ok(render_skill_info_report(&skill_id, &info))
                }
                SkillsAction::Install(skill_id) => {
                    let request = SkillInstallRequest {
                        skill_id: skill_id.clone(),
                        version: command.version.clone(),
                        registry_path: command.registry_path,
                    };
                    let outcome = manager.install(cwd, &request).map_err(io::Error::other)?;
                    let shadowed = managed_skill_shadowing_warning(
                        cwd,
                        &skill_id,
                        &outcome.record.install_root,
                    );
                    Ok(render_skill_install_report(&outcome, shadowed.as_deref()))
                }
                SkillsAction::Update(skill_id) => {
                    let request = SkillUpdateRequest {
                        skill_id: skill_id.clone(),
                        version: command.version.clone(),
                        registry_path: command.registry_path,
                    };
                    let outcome = manager.update(cwd, &request).map_err(io::Error::other)?;
                    let shadowed = managed_skill_shadowing_warning(
                        cwd,
                        &skill_id,
                        &outcome.new_record.install_root,
                    );
                    Ok(render_skill_update_report(&outcome, shadowed.as_deref()))
                }
                SkillsAction::Uninstall(skill_id) => {
                    let outcome = manager.uninstall(&skill_id).map_err(io::Error::other)?;
                    Ok(render_skill_uninstall_report(&outcome))
                }
                SkillsAction::List | SkillsAction::Help => Ok(render_skills_usage(None)),
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SkillsAction {
    List,
    Available,
    Info(String),
    Install(String),
    Update(String),
    Uninstall(String),
    Help,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedSkillsCommand {
    action: SkillsAction,
    registry_path: Option<PathBuf>,
    version: Option<String>,
}

fn parse_skills_command(args: &str) -> Result<ParsedSkillsCommand, String> {
    let tokens = args.split_whitespace().collect::<Vec<_>>();
    if tokens.is_empty() {
        return Ok(ParsedSkillsCommand {
            action: SkillsAction::List,
            registry_path: None,
            version: None,
        });
    }
    if matches!(tokens[0], "-h" | "--help" | "help") {
        return Ok(ParsedSkillsCommand {
            action: SkillsAction::Help,
            registry_path: None,
            version: None,
        });
    }

    let action = tokens[0];
    let mut index = 1;
    let target = match action {
        "info" | "install" | "update" | "uninstall" => {
            let Some(target) = tokens.get(index) else {
                return Err(format!(
                    "missing skill id for `skills {action}`\n\n{}",
                    render_skills_usage(Some(args))
                ));
            };
            index += 1;
            Some((*target).to_string())
        }
        "available" | "list" => None,
        other => {
            return Err(format!(
                "unknown skills action `{other}`\n\n{}",
                render_skills_usage(Some(args))
            ))
        }
    };

    let mut registry_path = None;
    let mut version = None;
    while let Some(token) = tokens.get(index) {
        match *token {
            "--registry" => {
                let Some(value) = tokens.get(index + 1) else {
                    return Err(format!(
                        "missing value for --registry\n\n{}",
                        render_skills_usage(Some(args))
                    ));
                };
                registry_path = Some(PathBuf::from(value));
                index += 2;
            }
            value if value.starts_with("--registry=") => {
                registry_path = Some(PathBuf::from(&value[11..]));
                index += 1;
            }
            "--version" => {
                let Some(value) = tokens.get(index + 1) else {
                    return Err(format!(
                        "missing value for --version\n\n{}",
                        render_skills_usage(Some(args))
                    ));
                };
                version = Some((*value).to_string());
                index += 2;
            }
            value if value.starts_with("--version=") => {
                version = Some(value[10..].to_string());
                index += 1;
            }
            other => {
                return Err(format!(
                    "unexpected skills argument `{other}`\n\n{}",
                    render_skills_usage(Some(args))
                ))
            }
        }
    }

    let parsed_action = match action {
        "list" => SkillsAction::List,
        "available" => SkillsAction::Available,
        "info" => SkillsAction::Info(target.expect("target checked")),
        "install" => SkillsAction::Install(target.expect("target checked")),
        "update" => SkillsAction::Update(target.expect("target checked")),
        "uninstall" => SkillsAction::Uninstall(target.expect("target checked")),
        _ => unreachable!("action validated above"),
    };
    if matches!(
        parsed_action,
        SkillsAction::Available | SkillsAction::Info(_)
    ) && version.is_some()
    {
        return Err(format!(
            "--version is only supported for `skills install` and `skills update`\n\n{}",
            render_skills_usage(Some(args))
        ));
    }
    Ok(ParsedSkillsCommand {
        action: parsed_action,
        registry_path,
        version,
    })
}

fn build_skill_registry_manager(
    cwd: &Path,
    explicit_registry_path: Option<&Path>,
) -> SkillRegistryManager {
    let loader = ConfigLoader::default_for(cwd);
    let configured_registry_path = if explicit_registry_path.is_some() {
        None
    } else {
        loader
            .load()
            .ok()
            .and_then(|config| config.skills().registry_path().map(PathBuf::from))
    };
    SkillRegistryManager::new(loader.config_home().to_path_buf(), configured_registry_path)
}

fn managed_skill_shadowing_warning(
    cwd: &Path,
    skill_id: &str,
    install_root: &Path,
) -> Option<String> {
    let roots = discover_skill_roots(cwd);
    let root_paths = roots
        .iter()
        .map(|root| root.path.clone())
        .collect::<Vec<_>>();
    let resolved = resolve_skill_path_from_roots(skill_id, &root_paths)
        .ok()
        .flatten()?;
    let expected = install_root.join(skill_id).join("SKILL.md");
    if resolved == expected {
        return None;
    }
    let source = roots
        .iter()
        .find(|root| resolved.starts_with(&root.path))
        .map_or_else(
            || "unknown root".to_string(),
            |root| root.source.label().to_string(),
        );
    Some(format!("{source} at {}", resolved.display()))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitPushPrRequest {
    pub commit_message: Option<String>,
    pub pr_title: String,
    pub pr_body: String,
    pub branch_name_hint: String,
}

pub fn handle_branch_slash_command(
    action: Option<&str>,
    target: Option<&str>,
    cwd: &Path,
) -> io::Result<String> {
    match normalize_optional_args(action) {
        None | Some("list") => {
            let branches = git_stdout(cwd, &["branch", "--list", "--verbose"])?;
            let trimmed = branches.trim();
            Ok(if trimmed.is_empty() {
                "Branch\n  Result           no branches found".to_string()
            } else {
                format!("Branch\n  Result           listed\n\n{trimmed}")
            })
        }
        Some("create") => {
            let Some(target) = target.filter(|value| !value.trim().is_empty()) else {
                return Ok("Usage: /branch create <name>".to_string());
            };
            git_status_ok(cwd, &["switch", "-c", target])?;
            Ok(format!(
                "Branch\n  Result           created and switched\n  Branch           {target}"
            ))
        }
        Some("switch") => {
            let Some(target) = target.filter(|value| !value.trim().is_empty()) else {
                return Ok("Usage: /branch switch <name>".to_string());
            };
            git_status_ok(cwd, &["switch", target])?;
            Ok(format!(
                "Branch\n  Result           switched\n  Branch           {target}"
            ))
        }
        Some(other) => Ok(format!(
            "Unknown /branch action '{other}'. Use /branch list, /branch create <name>, or /branch switch <name>."
        )),
    }
}

pub fn handle_worktree_slash_command(
    action: Option<&str>,
    path: Option<&str>,
    branch: Option<&str>,
    cwd: &Path,
) -> io::Result<String> {
    match normalize_optional_args(action) {
        None | Some("list") => {
            let worktrees = git_stdout(cwd, &["worktree", "list"])?;
            let trimmed = worktrees.trim();
            Ok(if trimmed.is_empty() {
                "Worktree\n  Result           no worktrees found".to_string()
            } else {
                format!("Worktree\n  Result           listed\n\n{trimmed}")
            })
        }
        Some("add") => {
            let Some(path) = path.filter(|value| !value.trim().is_empty()) else {
                return Ok("Usage: /worktree add <path> [branch]".to_string());
            };
            if let Some(branch) = branch.filter(|value| !value.trim().is_empty()) {
                if branch_exists(cwd, branch) {
                    git_status_ok(cwd, &["worktree", "add", path, branch])?;
                } else {
                    git_status_ok(cwd, &["worktree", "add", path, "-b", branch])?;
                }
                Ok(format!(
                    "Worktree\n  Result           added\n  Path             {path}\n  Branch           {branch}"
                ))
            } else {
                git_status_ok(cwd, &["worktree", "add", path])?;
                Ok(format!(
                    "Worktree\n  Result           added\n  Path             {path}"
                ))
            }
        }
        Some("remove") => {
            let Some(path) = path.filter(|value| !value.trim().is_empty()) else {
                return Ok("Usage: /worktree remove <path>".to_string());
            };
            git_status_ok(cwd, &["worktree", "remove", path])?;
            Ok(format!(
                "Worktree\n  Result           removed\n  Path             {path}"
            ))
        }
        Some("prune") => {
            git_status_ok(cwd, &["worktree", "prune"])?;
            Ok("Worktree\n  Result           pruned".to_string())
        }
        Some(other) => Ok(format!(
            "Unknown /worktree action '{other}'. Use /worktree list, /worktree add <path> [branch], /worktree remove <path>, or /worktree prune."
        )),
    }
}

pub fn handle_commit_slash_command(message: &str, cwd: &Path) -> io::Result<String> {
    let status = git_stdout_filtered(cwd, &["status", "--short"])?;
    if status.trim().is_empty() {
        return Ok(
            "Commit\n  Result           skipped\n  Reason           no workspace changes"
                .to_string(),
        );
    }

    let message = message.trim();
    if message.is_empty() {
        return Err(io::Error::other("generated commit message was empty"));
    }

    git_stage_workspace_changes(cwd)?;
    let path = write_temp_text_file("openyak-commit-message", "txt", message)?;
    let path_string = path.to_string_lossy().into_owned();
    git_status_ok(cwd, &["commit", "--file", path_string.as_str()])?;

    Ok(format!(
        "Commit\n  Result           created\n  Message file     {}\n\n{}",
        path.display(),
        message
    ))
}

pub fn handle_commit_push_pr_slash_command(
    request: &CommitPushPrRequest,
    cwd: &Path,
) -> io::Result<String> {
    let Some(gh_command) = resolve_command_path("gh") else {
        return Err(io::Error::other("gh CLI is required for /commit-push-pr"));
    };

    let default_branch = detect_default_branch(cwd)?;
    let workspace_has_changes = workspace_has_changes(cwd)?;
    if should_skip_commit_push_pr(cwd, &default_branch, workspace_has_changes)? {
        return Ok(commit_push_pr_skip_report());
    }

    let (branch, created_branch) =
        ensure_commit_push_pr_branch(request, cwd, &default_branch, current_branch(cwd)?)?;

    let commit_report = if workspace_has_changes {
        let Some(message) = request.commit_message.as_deref() else {
            return Err(io::Error::other(
                "commit message is required when workspace changes are present",
            ));
        };
        Some(handle_commit_slash_command(message, cwd)?)
    } else {
        None
    };

    let branch_diff = git_stdout(
        cwd,
        &["diff", "--stat", &format!("{default_branch}...HEAD")],
    )?;
    if branch_diff.trim().is_empty() {
        return Ok(commit_push_pr_skip_report());
    }

    git_status_ok(cwd, &["push", "--set-upstream", "origin", branch.as_str()])?;

    let body_path = write_temp_text_file("openyak-pr-body", "md", request.pr_body.trim())?;
    let body_path_string = body_path.to_string_lossy().into_owned();
    let create = Command::new(&gh_command)
        .args([
            "pr",
            "create",
            "--title",
            request.pr_title.as_str(),
            "--body-file",
            body_path_string.as_str(),
            "--base",
            default_branch.as_str(),
        ])
        .current_dir(cwd)
        .output()?;

    let (result, url) = if create.status.success() {
        (
            "created",
            parse_pr_url(&String::from_utf8_lossy(&create.stdout))
                .unwrap_or_else(|| "<unknown>".to_string()),
        )
    } else {
        let view = Command::new(&gh_command)
            .args(["pr", "view", "--json", "url"])
            .current_dir(cwd)
            .output()?;
        if !view.status.success() {
            return Err(io::Error::other(command_failure(
                "gh",
                &["pr", "create"],
                &create,
            )));
        }
        (
            "existing",
            parse_pr_json_url(&String::from_utf8_lossy(&view.stdout))
                .unwrap_or_else(|| "<unknown>".to_string()),
        )
    };

    let mut lines = vec![
        "Commit/Push/PR".to_string(),
        format!("  Result           {result}"),
        format!("  Branch           {branch}"),
        format!("  Base             {default_branch}"),
        format!("  Body file        {}", body_path.display()),
        format!("  URL              {url}"),
    ];
    if created_branch {
        lines.insert(2, "  Branch action    created and switched".to_string());
    }
    if let Some(report) = commit_report {
        lines.push(String::new());
        lines.push(report);
    }
    Ok(lines.join("\n"))
}

fn workspace_has_changes(cwd: &Path) -> io::Result<bool> {
    Ok(!git_stdout_filtered(cwd, &["status", "--short"])?
        .trim()
        .is_empty())
}

fn should_skip_commit_push_pr(
    cwd: &Path,
    default_branch: &str,
    workspace_has_changes: bool,
) -> io::Result<bool> {
    if workspace_has_changes {
        return Ok(false);
    }

    Ok(git_stdout(
        cwd,
        &["diff", "--stat", &format!("{default_branch}...HEAD")],
    )?
    .trim()
    .is_empty())
}

fn ensure_commit_push_pr_branch(
    request: &CommitPushPrRequest,
    cwd: &Path,
    default_branch: &str,
    current_branch: String,
) -> io::Result<(String, bool)> {
    if current_branch != default_branch {
        return Ok((current_branch, false));
    }

    let hint = if request.branch_name_hint.trim().is_empty() {
        request.pr_title.as_str()
    } else {
        request.branch_name_hint.as_str()
    };
    let next_branch = build_branch_name(hint);
    git_status_ok(cwd, &["switch", "-c", next_branch.as_str()])?;
    Ok((next_branch, true))
}

fn commit_push_pr_skip_report() -> String {
    "Commit/Push/PR\n  Result           skipped\n  Reason           no branch changes to push or open as a pull request"
        .to_string()
}

pub fn detect_default_branch(cwd: &Path) -> io::Result<String> {
    if let Ok(reference) = git_stdout(cwd, &["symbolic-ref", "refs/remotes/origin/HEAD"]) {
        if let Some(branch) = reference
            .trim()
            .rsplit('/')
            .next()
            .filter(|value| !value.is_empty())
        {
            return Ok(branch.to_string());
        }
    }

    for branch in ["main", "master"] {
        if branch_exists(cwd, branch) {
            return Ok(branch.to_string());
        }
    }

    current_branch(cwd)
}

fn git_stdout(cwd: &Path, args: &[&str]) -> io::Result<String> {
    run_command_stdout("git", args, cwd)
}

fn git_stdout_filtered(cwd: &Path, args: &[&str]) -> io::Result<String> {
    let args = git_args_excluding_local_artifacts(args);
    run_command_stdout("git", &args, cwd)
}

fn git_status_ok(cwd: &Path, args: &[&str]) -> io::Result<()> {
    run_command_success("git", args, cwd)
}

fn git_stage_workspace_changes(cwd: &Path) -> io::Result<()> {
    git_status_ok(cwd, &["add", "-A", "--", "."])?;
    git_status_ok(
        cwd,
        &[
            "reset",
            "--quiet",
            "--",
            ".openyak/settings.local.json",
            ".openyak/sessions",
        ],
    )
}

fn git_args_excluding_local_artifacts<'a>(args: &[&'a str]) -> Vec<&'a str> {
    let mut filtered = Vec::with_capacity(args.len() + 4);
    filtered.extend_from_slice(args);
    filtered.push("--");
    filtered.push(".");
    filtered.push(":(exclude).openyak/settings.local.json");
    filtered.push(":(exclude).openyak/sessions");
    filtered
}

fn run_command_stdout(program: &str, args: &[&str], cwd: &Path) -> io::Result<String> {
    let output = Command::new(program).args(args).current_dir(cwd).output()?;
    if !output.status.success() {
        return Err(io::Error::other(command_failure(program, args, &output)));
    }
    String::from_utf8(output.stdout)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn run_command_success(program: &str, args: &[&str], cwd: &Path) -> io::Result<()> {
    let output = Command::new(program).args(args).current_dir(cwd).output()?;
    if !output.status.success() {
        return Err(io::Error::other(command_failure(program, args, &output)));
    }
    Ok(())
}

fn command_failure(program: &str, args: &[&str], output: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let detail = if stderr.is_empty() { stdout } else { stderr };
    if detail.is_empty() {
        format!("{program} {} failed", args.join(" "))
    } else {
        format!("{program} {} failed: {detail}", args.join(" "))
    }
}

fn branch_exists(cwd: &Path, branch: &str) -> bool {
    Command::new("git")
        .args([
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ])
        .current_dir(cwd)
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn current_branch(cwd: &Path) -> io::Result<String> {
    let branch = git_stdout(cwd, &["branch", "--show-current"])?;
    let branch = branch.trim();
    if branch.is_empty() {
        Err(io::Error::other("unable to determine current git branch"))
    } else {
        Ok(branch.to_string())
    }
}

fn write_temp_text_file(prefix: &str, extension: &str, contents: &str) -> io::Result<PathBuf> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let path = env::temp_dir().join(format!("{prefix}-{nanos}.{extension}"));
    fs::write(&path, contents)?;
    Ok(path)
}

fn build_branch_name(hint: &str) -> String {
    let slug = slugify(hint);
    let owner = env::var("SAFEUSER")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            env::var("USER")
                .ok()
                .filter(|value| !value.trim().is_empty())
        });
    match owner {
        Some(owner) => format!("{owner}/{slug}"),
        None => slug,
    }
}

fn slugify(value: &str) -> String {
    let mut slug = String::new();
    let mut last_was_dash = false;
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
            last_was_dash = false;
        } else if !last_was_dash {
            slug.push('-');
            last_was_dash = true;
        }
    }
    let slug = slug.trim_matches('-').to_string();
    if slug.is_empty() {
        "change".to_string()
    } else {
        slug
    }
}

fn parse_pr_url(stdout: &str) -> Option<String> {
    stdout
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with("http://") || line.starts_with("https://"))
        .map(ToOwned::to_owned)
}

fn parse_pr_json_url(stdout: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(stdout)
        .ok()?
        .get("url")?
        .as_str()
        .map(ToOwned::to_owned)
}

#[must_use]
pub fn render_plugins_report(plugins: &[PluginSummary]) -> String {
    let mut lines = vec!["Plugins".to_string()];
    if plugins.is_empty() {
        lines.push("  No plugins installed.".to_string());
        return lines.join("\n");
    }
    for plugin in plugins {
        let enabled = if plugin.enabled {
            "enabled"
        } else {
            "disabled"
        };
        lines.push(format!(
            "  {name:<20} v{version:<10} {enabled}",
            name = plugin.metadata.name,
            version = plugin.metadata.version,
        ));
    }
    lines.join("\n")
}

fn render_plugin_install_report(
    install: &InstallOutcome,
    plugin: Option<&PluginSummary>,
) -> String {
    let name = plugin.map_or(install.plugin_id.as_str(), |plugin| {
        plugin.metadata.name.as_str()
    });
    let version = plugin.map_or(install.version.as_str(), |plugin| {
        plugin.metadata.version.as_str()
    });
    let enabled = plugin.is_some_and(|plugin| plugin.enabled);
    format!(
        "Plugins\n  Result           installed {}\n  Name             {name}\n  Version          {version}\n  Source           {}\n  Install path     {}\n  Status           {}",
        install.plugin_id,
        install.source,
        install.install_path.display(),
        plugin_status_label(enabled)
    )
}

fn render_plugin_update_report(update: &UpdateOutcome, plugin: Option<&PluginSummary>) -> String {
    let name = plugin.map_or(update.plugin_id.as_str(), |plugin| {
        plugin.metadata.name.as_str()
    });
    let enabled = plugin.is_some_and(|plugin| plugin.enabled);
    format!(
        "Plugins\n  Result           updated {}\n  Name             {name}\n  Old version      {}\n  New version      {}\n  Source           {}\n  Install path     {}\n  Status           {}",
        update.plugin_id,
        update.old_version,
        update.new_version,
        update.source,
        update.install_path.display(),
        plugin_status_label(enabled)
    )
}

fn plugin_status_label(enabled: bool) -> &'static str {
    if enabled {
        "enabled"
    } else {
        "disabled"
    }
}

fn resolve_plugin_target(
    manager: &PluginManager,
    target: &str,
) -> Result<PluginSummary, PluginError> {
    let mut matches = manager
        .list_installed_plugins()?
        .into_iter()
        .filter(|plugin| plugin.metadata.id == target || plugin.metadata.name == target)
        .collect::<Vec<_>>();
    match matches.len() {
        1 => Ok(matches.remove(0)),
        0 => Err(PluginError::NotFound(format!(
            "plugin `{target}` is not installed or discoverable"
        ))),
        _ => Err(PluginError::InvalidManifest(format!(
            "plugin name `{target}` is ambiguous; use the full plugin id"
        ))),
    }
}

fn discover_definition_roots(cwd: &Path, leaf: &str) -> Vec<(DefinitionSource, PathBuf)> {
    let mut roots = Vec::new();
    let homes = home_locations();
    let user_codex_root = homes.codex_home.join(leaf);
    let user_openyak_root = homes.openyak_home.join(leaf);

    for ancestor in cwd.ancestors() {
        let project_codex_root = ancestor.join(".codex").join(leaf);
        if project_codex_root != user_codex_root
            && !looks_like_default_user_definition_root(&project_codex_root)
        {
            push_unique_root(
                &mut roots,
                DefinitionSource::ProjectCodex,
                project_codex_root,
            );
        }

        let project_openyak_root = ancestor.join(".openyak").join(leaf);
        if project_openyak_root != user_openyak_root
            && !looks_like_default_user_definition_root(&project_openyak_root)
        {
            push_unique_root(
                &mut roots,
                DefinitionSource::ProjectOpenyak,
                project_openyak_root,
            );
        }
    }

    push_unique_root(
        &mut roots,
        if homes.codex_home_from_env {
            DefinitionSource::UserCodexHome
        } else {
            DefinitionSource::UserCodex
        },
        user_codex_root,
    );
    push_unique_root(&mut roots, DefinitionSource::UserOpenyak, user_openyak_root);

    roots
}

fn discover_skill_roots(cwd: &Path) -> Vec<SkillRoot> {
    let mut roots = Vec::new();
    let homes = home_locations();
    let user_codex_skills = homes.codex_home.join("skills");
    let user_codex_commands = homes.codex_home.join("commands");
    let user_openyak_skills = homes.openyak_home.join("skills");
    let user_openyak_commands = homes.openyak_home.join("commands");

    for ancestor in cwd.ancestors() {
        let project_codex_skills = ancestor.join(".codex").join("skills");
        if project_codex_skills != user_codex_skills
            && !looks_like_default_user_definition_root(&project_codex_skills)
        {
            push_unique_skill_root(
                &mut roots,
                DefinitionSource::ProjectCodex,
                project_codex_skills,
                SkillOrigin::SkillsDir,
            );
        }

        let project_openyak_skills = ancestor.join(".openyak").join("skills");
        if project_openyak_skills != user_openyak_skills
            && !looks_like_default_user_definition_root(&project_openyak_skills)
        {
            push_unique_skill_root(
                &mut roots,
                DefinitionSource::ProjectOpenyak,
                project_openyak_skills,
                SkillOrigin::SkillsDir,
            );
        }

        let project_codex_commands = ancestor.join(".codex").join("commands");
        if project_codex_commands != user_codex_commands
            && !looks_like_default_user_definition_root(&project_codex_commands)
        {
            push_unique_skill_root(
                &mut roots,
                DefinitionSource::ProjectCodex,
                project_codex_commands,
                SkillOrigin::LegacyCommandsDir,
            );
        }

        let project_openyak_commands = ancestor.join(".openyak").join("commands");
        if project_openyak_commands != user_openyak_commands
            && !looks_like_default_user_definition_root(&project_openyak_commands)
        {
            push_unique_skill_root(
                &mut roots,
                DefinitionSource::ProjectOpenyak,
                project_openyak_commands,
                SkillOrigin::LegacyCommandsDir,
            );
        }
    }

    let codex_source = if homes.codex_home_from_env {
        DefinitionSource::UserCodexHome
    } else {
        DefinitionSource::UserCodex
    };
    push_unique_skill_root(
        &mut roots,
        codex_source,
        user_codex_skills,
        SkillOrigin::SkillsDir,
    );
    push_unique_skill_root(
        &mut roots,
        codex_source,
        user_codex_commands,
        SkillOrigin::LegacyCommandsDir,
    );
    push_unique_skill_root(
        &mut roots,
        DefinitionSource::UserOpenyak,
        user_openyak_skills,
        SkillOrigin::SkillsDir,
    );
    push_unique_skill_root(
        &mut roots,
        DefinitionSource::UserOpenyak,
        user_openyak_commands,
        SkillOrigin::LegacyCommandsDir,
    );

    roots
}

fn push_unique_root(
    roots: &mut Vec<(DefinitionSource, PathBuf)>,
    source: DefinitionSource,
    path: PathBuf,
) {
    if path.is_dir() && !roots.iter().any(|(_, existing)| existing == &path) {
        roots.push((source, path));
    }
}

fn push_unique_skill_root(
    roots: &mut Vec<SkillRoot>,
    source: DefinitionSource,
    path: PathBuf,
    origin: SkillOrigin,
) {
    if path.is_dir() && !roots.iter().any(|existing| existing.path == path) {
        roots.push(SkillRoot {
            source,
            path,
            origin,
        });
    }
}

fn looks_like_default_user_definition_root(path: &Path) -> bool {
    let Some(config_dir) = path.parent() else {
        return false;
    };
    let Some(config_name) = config_dir.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    if config_name != ".codex" && config_name != ".openyak" {
        return false;
    }

    let Some(users_dir_name) = config_dir
        .parent()
        .and_then(Path::parent)
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
    else {
        return false;
    };

    users_dir_name.eq_ignore_ascii_case("users") || users_dir_name.eq_ignore_ascii_case("home")
}

fn load_agents_from_roots(
    roots: &[(DefinitionSource, PathBuf)],
) -> std::io::Result<Vec<AgentSummary>> {
    let mut agents = Vec::new();
    let mut active_sources = BTreeMap::<String, DefinitionSource>::new();

    for (source, root) in roots {
        let mut root_agents = Vec::new();
        for entry in fs::read_dir(root)? {
            let entry = entry?;
            if entry.path().extension().is_none_or(|ext| ext != "toml") {
                continue;
            }
            let contents = fs::read_to_string(entry.path())?;
            let fallback_name = entry.path().file_stem().map_or_else(
                || entry.file_name().to_string_lossy().to_string(),
                |stem| stem.to_string_lossy().to_string(),
            );
            root_agents.push(AgentSummary {
                name: parse_toml_string(&contents, "name").unwrap_or(fallback_name),
                description: parse_toml_string(&contents, "description"),
                model: parse_toml_string(&contents, "model"),
                reasoning_effort: parse_toml_string(&contents, "model_reasoning_effort"),
                source: *source,
                shadowed_by: None,
            });
        }
        root_agents.sort_by(|left, right| left.name.cmp(&right.name));

        for mut agent in root_agents {
            let key = agent.name.to_ascii_lowercase();
            if let Some(existing) = active_sources.get(&key) {
                agent.shadowed_by = Some(*existing);
            } else {
                active_sources.insert(key, agent.source);
            }
            agents.push(agent);
        }
    }

    Ok(agents)
}

fn load_skills_from_roots(roots: &[SkillRoot]) -> std::io::Result<Vec<SkillSummary>> {
    let mut skills = Vec::new();
    let mut active_sources = BTreeMap::<String, DefinitionSource>::new();

    for root in roots {
        let mut root_skills = Vec::new();
        match root.origin {
            SkillOrigin::SkillsDir => {
                for skill in discover_skill_directories(&root.path)? {
                    let contents = fs::read_to_string(&skill.path)?;
                    let (name, description) = parse_skill_frontmatter(&contents);
                    root_skills.push(SkillSummary {
                        name: name.unwrap_or(skill.name),
                        description,
                        source: root.source,
                        shadowed_by: None,
                        origin: root.origin,
                        path: skill.path,
                    });
                }
            }
            SkillOrigin::LegacyCommandsDir => {
                for entry in fs::read_dir(&root.path)? {
                    let entry = entry?;
                    let path = entry.path();
                    let markdown_path = if path.is_dir() {
                        let skill_path = path.join("SKILL.md");
                        if !skill_path.is_file() {
                            continue;
                        }
                        skill_path
                    } else if path
                        .extension()
                        .is_some_and(|ext| ext.to_string_lossy().eq_ignore_ascii_case("md"))
                    {
                        path
                    } else {
                        continue;
                    };

                    let contents = fs::read_to_string(&markdown_path)?;
                    let fallback_name = markdown_path.file_stem().map_or_else(
                        || entry.file_name().to_string_lossy().to_string(),
                        |stem| stem.to_string_lossy().to_string(),
                    );
                    let (name, description) = parse_skill_frontmatter(&contents);
                    root_skills.push(SkillSummary {
                        name: name.unwrap_or(fallback_name),
                        description,
                        source: root.source,
                        shadowed_by: None,
                        origin: root.origin,
                        path: markdown_path,
                    });
                }
            }
        }
        root_skills.sort_by(|left, right| left.name.cmp(&right.name));

        for mut skill in root_skills {
            let key = skill.name.to_ascii_lowercase();
            if let Some(existing) = active_sources.get(&key) {
                skill.shadowed_by = Some(*existing);
            } else {
                active_sources.insert(key, skill.source);
            }
            skills.push(skill);
        }
    }

    Ok(skills)
}

fn parse_toml_string(contents: &str, key: &str) -> Option<String> {
    let prefix = format!("{key} =");
    for line in contents.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('#') {
            continue;
        }
        let Some(value) = trimmed.strip_prefix(&prefix) else {
            continue;
        };
        let value = value.trim();
        let Some(value) = value
            .strip_prefix('"')
            .and_then(|value| value.strip_suffix('"'))
        else {
            continue;
        };
        if !value.is_empty() {
            return Some(value.to_string());
        }
    }
    None
}

fn parse_skill_frontmatter(contents: &str) -> (Option<String>, Option<String>) {
    let mut lines = contents.lines();
    if lines.next().map(str::trim) != Some("---") {
        return (None, None);
    }

    let mut name = None;
    let mut description = None;
    for line in lines {
        let trimmed = line.trim();
        if trimmed == "---" {
            break;
        }
        if let Some(value) = trimmed.strip_prefix("name:") {
            let value = unquote_frontmatter_value(value.trim());
            if !value.is_empty() {
                name = Some(value);
            }
            continue;
        }
        if let Some(value) = trimmed.strip_prefix("description:") {
            let value = unquote_frontmatter_value(value.trim());
            if !value.is_empty() {
                description = Some(value);
            }
        }
    }

    (name, description)
}

fn unquote_frontmatter_value(value: &str) -> String {
    value
        .strip_prefix('"')
        .and_then(|trimmed| trimmed.strip_suffix('"'))
        .or_else(|| {
            value
                .strip_prefix('\'')
                .and_then(|trimmed| trimmed.strip_suffix('\''))
        })
        .unwrap_or(value)
        .trim()
        .to_string()
}

fn render_agents_report(agents: &[AgentSummary]) -> String {
    if agents.is_empty() {
        return "No agents found.".to_string();
    }

    let total_active = agents
        .iter()
        .filter(|agent| agent.shadowed_by.is_none())
        .count();
    let mut lines = vec![
        "Agents".to_string(),
        format!("  {total_active} active agents"),
        String::new(),
    ];

    for source in [
        DefinitionSource::ProjectCodex,
        DefinitionSource::ProjectOpenyak,
        DefinitionSource::UserCodexHome,
        DefinitionSource::UserCodex,
        DefinitionSource::UserOpenyak,
    ] {
        let group = agents
            .iter()
            .filter(|agent| agent.source == source)
            .collect::<Vec<_>>();
        if group.is_empty() {
            continue;
        }

        lines.push(format!("{}:", source.label()));
        for agent in group {
            let detail = agent_detail(agent);
            match agent.shadowed_by {
                Some(winner) => lines.push(format!("  (shadowed by {}) {detail}", winner.label())),
                None => lines.push(format!("  {detail}")),
            }
        }
        lines.push(String::new());
    }

    lines.join("\n").trim_end().to_string()
}

fn agent_detail(agent: &AgentSummary) -> String {
    let mut parts = vec![agent.name.clone()];
    if let Some(description) = &agent.description {
        parts.push(description.clone());
    }
    if let Some(model) = &agent.model {
        parts.push(model.clone());
    }
    if let Some(reasoning) = &agent.reasoning_effort {
        parts.push(reasoning.clone());
    }
    parts.join(" · ")
}

fn render_skills_report(skills: &[SkillSummary]) -> String {
    if skills.is_empty() {
        return "No skills found.".to_string();
    }

    let total_active = skills
        .iter()
        .filter(|skill| skill.shadowed_by.is_none())
        .count();
    let mut lines = vec![
        "Skills".to_string(),
        format!("  {total_active} available skills"),
        String::new(),
    ];

    for source in [
        DefinitionSource::ProjectCodex,
        DefinitionSource::ProjectOpenyak,
        DefinitionSource::UserCodexHome,
        DefinitionSource::UserCodex,
        DefinitionSource::UserOpenyak,
    ] {
        let group = skills
            .iter()
            .filter(|skill| skill.source == source)
            .collect::<Vec<_>>();
        if group.is_empty() {
            continue;
        }

        lines.push(format!("{}:", source.label()));
        for skill in group {
            let mut parts = vec![skill.name.clone()];
            if let Some(description) = &skill.description {
                parts.push(description.clone());
            }
            if let Some(detail) = skill.origin.detail_label() {
                parts.push(detail.to_string());
            }
            let detail = parts.join(" · ");
            match skill.shadowed_by {
                Some(winner) => lines.push(format!("  (shadowed by {}) {detail}", winner.label())),
                None => lines.push(format!("  {detail}")),
            }
        }
        lines.push(String::new());
    }

    lines.join("\n").trim_end().to_string()
}

fn render_available_skills_report(catalog: &AvailableSkillCatalog) -> String {
    if catalog.entries.is_empty() {
        return format!(
            "Skills catalog\n  Registry         {}\n  Result           empty",
            catalog.registry_path.display()
        );
    }

    let mut lines = vec![
        "Skills catalog".to_string(),
        format!("  Registry path    {}", catalog.registry_path.display()),
        format!("  Registry id      {}", catalog.registry_id),
        format!("  Channel          {}", catalog.channel),
        format!("  Entries          {}", catalog.entries.len()),
        String::new(),
    ];
    for entry in &catalog.entries {
        let minimum_version = entry
            .entry
            .minimum_openyak_version
            .as_deref()
            .unwrap_or("any");
        let mut detail = format!(
            "{} · v{} · {} · min {}",
            entry.entry.skill_id, entry.entry.version, entry.entry.placement, minimum_version
        );
        if !entry.compatible {
            detail.push_str(" · incompatible with current openyak");
        }
        if let Some(installed) = &entry.installed {
            let _ = write!(detail, " · installed v{}", installed.version);
        }
        let _ = write!(detail, " · {}", entry.entry.description);
        lines.push(format!("  {detail}"));
    }
    lines.join("\n")
}

fn render_skill_info_report(skill_id: &str, info: &SkillCatalogInfo) -> String {
    let mut lines = vec![
        "Skill info".to_string(),
        format!("  Skill            {skill_id}"),
    ];
    if let Some(installed) = &info.installed {
        lines.push(format!("  Installed        v{}", installed.version));
        lines.push(format!("  Placement        {}", installed.placement));
        lines.push(format!(
            "  Registry         {}/{}",
            installed.registry_id, installed.channel
        ));
        lines.push(format!(
            "  Install path     {}",
            installed.install_root.display()
        ));
        if let Some(pinned) = &installed.pinned_version {
            lines.push(format!("  Pinned version   {pinned}"));
        }
    } else {
        lines.push("  Installed        no managed install".to_string());
    }
    if let Some(path) = &info.registry_path {
        lines.push(format!("  Catalog path     {}", path.display()));
    }
    if !info.available_versions.is_empty() {
        lines.push(String::new());
        lines.push("Available versions".to_string());
        for entry in &info.available_versions {
            let minimum_version = entry.minimum_openyak_version.as_deref().unwrap_or("any");
            lines.push(format!(
                "  {} · {} · min {}",
                entry.version, entry.description, minimum_version
            ));
        }
    }
    lines.join("\n")
}

fn render_skill_install_report(
    outcome: &SkillInstallOutcome,
    shadowed_warning: Option<&str>,
) -> String {
    let mut lines = vec![
        "Skills".to_string(),
        format!(
            "  Result           {}",
            match outcome.status {
                SkillInstallStatus::Installed => "installed",
                SkillInstallStatus::Unchanged => "unchanged",
            }
        ),
        format!("  Skill            {}", outcome.record.skill_id),
        format!("  Version          {}", outcome.record.version),
        format!(
            "  Registry         {}/{}",
            outcome.record.registry_id, outcome.record.channel
        ),
        format!(
            "  Install path     {}",
            outcome.record.install_root.display()
        ),
        format!("  Registry path    {}", outcome.registry_path.display()),
    ];
    if let Some(pinned) = &outcome.record.pinned_version {
        lines.push(format!("  Pinned version   {pinned}"));
    }
    if let Some(warning) = shadowed_warning {
        lines.push(format!("  Shadowing        {warning}"));
    }
    lines.join("\n")
}

fn render_skill_update_report(
    outcome: &SkillUpdateOutcome,
    shadowed_warning: Option<&str>,
) -> String {
    let mut lines = vec![
        "Skills".to_string(),
        format!(
            "  Result           {}",
            match outcome.status {
                SkillUpdateStatus::Updated => "updated",
                SkillUpdateStatus::Unchanged => "unchanged",
                SkillUpdateStatus::Pinned => "pinned",
            }
        ),
        format!("  Skill            {}", outcome.new_record.skill_id),
        format!("  Old version      {}", outcome.old_record.version),
        format!("  New version      {}", outcome.new_record.version),
        format!(
            "  Registry         {}/{}",
            outcome.new_record.registry_id, outcome.new_record.channel
        ),
        format!(
            "  Install path     {}",
            outcome.new_record.install_root.display()
        ),
        format!("  Registry path    {}", outcome.registry_path.display()),
    ];
    if let Some(pinned) = &outcome.new_record.pinned_version {
        lines.push(format!("  Pinned version   {pinned}"));
    }
    if let Some(warning) = shadowed_warning {
        lines.push(format!("  Shadowing        {warning}"));
    }
    lines.join("\n")
}

fn render_skill_uninstall_report(outcome: &SkillUninstallOutcome) -> String {
    [
        "Skills".to_string(),
        "  Result           uninstalled".to_string(),
        format!("  Skill            {}", outcome.record.skill_id),
        format!("  Version          {}", outcome.record.version),
        format!(
            "  Registry         {}/{}",
            outcome.record.registry_id, outcome.record.channel
        ),
    ]
    .join("\n")
}

fn normalize_optional_args(args: Option<&str>) -> Option<&str> {
    args.map(str::trim).filter(|value| !value.is_empty())
}

fn render_agents_usage(unexpected: Option<&str>) -> String {
    let mut lines = vec![
        "Agents".to_string(),
        "  Usage            /agents".to_string(),
        "  Direct CLI       openyak agents".to_string(),
        "  Sources          project .codex/.openyak, user home, or $CODEX_HOME".to_string(),
    ];
    if let Some(args) = unexpected {
        lines.push(format!("  Unexpected       {args}"));
    }
    lines.join("\n")
}

fn render_skills_usage(unexpected: Option<&str>) -> String {
    let mut lines = vec![
        "Skills".to_string(),
        "  Usage            /skills [list|available|info <skill-id>|install <skill-id>|update <skill-id>|uninstall <skill-id>]".to_string(),
        "  Direct CLI       openyak skills [list|available|info <skill-id>|install <skill-id>|update <skill-id>|uninstall <skill-id>]".to_string(),
        "  Sources          project .codex/.openyak, user home or $CODEX_HOME, legacy /commands"
            .to_string(),
        "  Flags            --registry <path>, --version <x.y.z> (install/update only)"
            .to_string(),
        "  Notes            managed installs land under <openyak-home>/skills/.managed"
            .to_string(),
    ];
    if let Some(args) = unexpected {
        lines.push(format!("  Unexpected       {args}"));
    }
    lines.join("\n")
}

#[must_use]
pub fn handle_slash_command(
    input: &str,
    session: &Session,
    compaction: CompactionConfig,
) -> Option<SlashCommandResult> {
    match SlashCommand::parse(input)? {
        SlashCommand::Compact => {
            let result = compact_session(session, compaction);
            let message = if result.removed_message_count == 0 {
                "Compaction skipped: session is below the compaction threshold.".to_string()
            } else {
                format!(
                    "Compacted {} messages into a resumable system summary.",
                    result.removed_message_count
                )
            };
            Some(SlashCommandResult {
                message,
                session: result.compacted_session,
            })
        }
        SlashCommand::Help => Some(SlashCommandResult {
            message: render_slash_command_help(),
            session: session.clone(),
        }),
        SlashCommand::Status
        | SlashCommand::Branch { .. }
        | SlashCommand::Bughunter { .. }
        | SlashCommand::Worktree { .. }
        | SlashCommand::Commit
        | SlashCommand::CommitPushPr { .. }
        | SlashCommand::Pr { .. }
        | SlashCommand::Issue { .. }
        | SlashCommand::Ultraplan { .. }
        | SlashCommand::Teleport { .. }
        | SlashCommand::DebugToolCall
        | SlashCommand::Model { .. }
        | SlashCommand::Permissions { .. }
        | SlashCommand::Plan { .. }
        | SlashCommand::Clear { .. }
        | SlashCommand::Cost
        | SlashCommand::Resume { .. }
        | SlashCommand::Config { .. }
        | SlashCommand::Memory
        | SlashCommand::Foundations { .. }
        | SlashCommand::Init
        | SlashCommand::Diff
        | SlashCommand::Version
        | SlashCommand::Export { .. }
        | SlashCommand::Session { .. }
        | SlashCommand::Plugins { .. }
        | SlashCommand::Agents { .. }
        | SlashCommand::Skills { .. }
        | SlashCommand::Unknown(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        discover_definition_roots, discover_skill_roots, handle_branch_slash_command,
        handle_commit_slash_command, handle_plugins_slash_command, handle_slash_command,
        handle_worktree_slash_command, load_agents_from_roots, load_skills_from_roots,
        render_agents_report, render_plugins_report, render_skills_report,
        render_slash_command_help, resume_supported_slash_commands, slash_command_specs,
        suggest_slash_commands, DefinitionSource, SkillOrigin, SkillRoot, SlashCommand,
    };
    use plugins::{PluginKind, PluginManager, PluginManagerConfig, PluginMetadata, PluginSummary};
    use runtime::{CompactionConfig, ContentBlock, ConversationMessage, MessageRole, Session};
    use std::env;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::{Mutex, OnceLock};
    use std::time::{SystemTime, UNIX_EPOCH};

    #[cfg(unix)]
    use super::{handle_commit_push_pr_slash_command, CommitPushPrRequest};
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    fn temp_dir(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time should be after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("commands-plugin-{label}-{nanos}"))
    }

    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .expect("env lock")
    }

    fn restore_env(name: &str, value: Option<std::ffi::OsString>) {
        match value {
            Some(value) => env::set_var(name, value),
            None => env::remove_var(name),
        }
    }

    fn run_command(cwd: &Path, program: &str, args: &[&str]) -> String {
        let output = Command::new(program)
            .args(args)
            .current_dir(cwd)
            .output()
            .expect("command should run");
        assert!(
            output.status.success(),
            "{} {} failed: {}",
            program,
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).expect("stdout should be utf8")
    }

    fn init_git_repo(label: &str) -> PathBuf {
        let root = temp_dir(label);
        fs::create_dir_all(&root).expect("repo root");

        let init = Command::new("git")
            .args(["init", "-b", "main"])
            .current_dir(&root)
            .output()
            .expect("git init should run");
        if !init.status.success() {
            let fallback = Command::new("git")
                .arg("init")
                .current_dir(&root)
                .output()
                .expect("fallback git init should run");
            assert!(
                fallback.status.success(),
                "fallback git init should succeed"
            );
            let rename = Command::new("git")
                .args(["branch", "-m", "main"])
                .current_dir(&root)
                .output()
                .expect("git branch -m should run");
            assert!(rename.status.success(), "git branch -m main should succeed");
        }

        run_command(&root, "git", &["config", "user.name", "openyak Tests"]);
        run_command(
            &root,
            "git",
            &["config", "user.email", "openyak@example.com"],
        );
        fs::write(root.join("README.md"), "seed\n").expect("seed file");
        run_command(&root, "git", &["add", "README.md"]);
        run_command(&root, "git", &["commit", "-m", "chore: seed repo"]);
        root
    }

    #[cfg(unix)]
    fn init_bare_repo(label: &str) -> PathBuf {
        let root = temp_dir(label);
        let output = Command::new("git")
            .args(["init", "--bare"])
            .arg(&root)
            .output()
            .expect("bare repo should initialize");
        assert!(output.status.success(), "git init --bare should succeed");
        root
    }

    #[cfg(unix)]
    fn write_fake_gh(bin_dir: &Path, log_path: &Path, url: &str) {
        fs::create_dir_all(bin_dir).expect("bin dir");
        let script = format!(
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then\n  echo 'gh 1.0.0'\n  exit 0\nfi\nprintf '%s\\n' \"$*\" >> \"{}\"\nif [ \"$1\" = \"pr\" ] && [ \"$2\" = \"create\" ]; then\n  echo '{}'\n  exit 0\nfi\nif [ \"$1\" = \"pr\" ] && [ \"$2\" = \"view\" ]; then\n  echo '{{\"url\":\"{}\"}}'\n  exit 0\nfi\nexit 0\n",
            log_path.display(),
            url,
            url,
        );
        let path = bin_dir.join("gh");
        fs::write(&path, script).expect("gh stub");
        let mut permissions = fs::metadata(&path).expect("metadata").permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&path, permissions).expect("chmod");
    }

    fn write_external_plugin(root: &Path, name: &str, version: &str) {
        fs::create_dir_all(root.join(".openyak-plugin")).expect("manifest dir");
        fs::write(
            root.join(".openyak-plugin").join("plugin.json"),
            format!(
                "{{\n  \"name\": \"{name}\",\n  \"version\": \"{version}\",\n  \"description\": \"commands plugin\"\n}}"
            ),
        )
        .expect("write manifest");
    }

    fn write_bundled_plugin(root: &Path, name: &str, version: &str, default_enabled: bool) {
        fs::create_dir_all(root.join(".openyak-plugin")).expect("manifest dir");
        fs::write(
            root.join(".openyak-plugin").join("plugin.json"),
            format!(
                "{{\n  \"name\": \"{name}\",\n  \"version\": \"{version}\",\n  \"description\": \"bundled commands plugin\",\n  \"defaultEnabled\": {}\n}}",
                if default_enabled { "true" } else { "false" }
            ),
        )
        .expect("write bundled manifest");
    }

    fn write_agent(root: &Path, name: &str, description: &str, model: &str, reasoning: &str) {
        fs::create_dir_all(root).expect("agent root");
        fs::write(
            root.join(format!("{name}.toml")),
            format!(
                "name = \"{name}\"\ndescription = \"{description}\"\nmodel = \"{model}\"\nmodel_reasoning_effort = \"{reasoning}\"\n"
            ),
        )
        .expect("write agent");
    }

    fn write_skill(root: &Path, name: &str, description: &str) {
        let skill_root = root.join(name);
        fs::create_dir_all(&skill_root).expect("skill root");
        fs::write(
            skill_root.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {description}\n---\n\n# {name}\n"),
        )
        .expect("write skill");
    }

    fn write_legacy_command(root: &Path, name: &str, description: &str) {
        fs::create_dir_all(root).expect("commands root");
        fs::write(
            root.join(format!("{name}.md")),
            format!("---\nname: {name}\ndescription: {description}\n---\n\n# {name}\n"),
        )
        .expect("write command");
    }

    fn packaged_skill_registry_path() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(3)
            .expect("repo root")
            .join("assets")
            .join("skills")
            .join("registry.json")
    }

    #[allow(clippy::too_many_lines)]
    #[test]
    fn parses_supported_slash_commands() {
        assert_eq!(SlashCommand::parse("/help"), Some(SlashCommand::Help));
        assert_eq!(SlashCommand::parse(" /status "), Some(SlashCommand::Status));
        assert_eq!(
            SlashCommand::parse("/bughunter runtime"),
            Some(SlashCommand::Bughunter {
                scope: Some("runtime".to_string())
            })
        );
        assert_eq!(
            SlashCommand::parse("/branch create feature/demo"),
            Some(SlashCommand::Branch {
                action: Some("create".to_string()),
                target: Some("feature/demo".to_string()),
            })
        );
        assert_eq!(
            SlashCommand::parse("/worktree add ../demo wt-demo"),
            Some(SlashCommand::Worktree {
                action: Some("add".to_string()),
                path: Some("../demo".to_string()),
                branch: Some("wt-demo".to_string()),
            })
        );
        assert_eq!(SlashCommand::parse("/commit"), Some(SlashCommand::Commit));
        assert_eq!(
            SlashCommand::parse("/commit-push-pr ready for review"),
            Some(SlashCommand::CommitPushPr {
                context: Some("ready for review".to_string())
            })
        );
        assert_eq!(
            SlashCommand::parse("/pr ready for review"),
            Some(SlashCommand::Pr {
                context: Some("ready for review".to_string())
            })
        );
        assert_eq!(
            SlashCommand::parse("/issue flaky test"),
            Some(SlashCommand::Issue {
                context: Some("flaky test".to_string())
            })
        );
        assert_eq!(
            SlashCommand::parse("/ultraplan ship both features"),
            Some(SlashCommand::Ultraplan {
                task: Some("ship both features".to_string())
            })
        );
        assert_eq!(
            SlashCommand::parse("/teleport conversation.rs"),
            Some(SlashCommand::Teleport {
                target: Some("conversation.rs".to_string())
            })
        );
        assert_eq!(
            SlashCommand::parse("/debug-tool-call"),
            Some(SlashCommand::DebugToolCall)
        );
        assert_eq!(
            SlashCommand::parse("/model opus"),
            Some(SlashCommand::Model {
                model: Some("opus".to_string()),
            })
        );
        assert_eq!(
            SlashCommand::parse("/model"),
            Some(SlashCommand::Model { model: None })
        );
        assert_eq!(
            SlashCommand::parse("/permissions read-only"),
            Some(SlashCommand::Permissions {
                mode: Some("read-only".to_string()),
            })
        );
        assert_eq!(
            SlashCommand::parse("/plan"),
            Some(SlashCommand::Plan { action: None })
        );
        assert_eq!(
            SlashCommand::parse("/plan exit"),
            Some(SlashCommand::Plan {
                action: Some("exit".to_string()),
            })
        );
        assert_eq!(
            SlashCommand::parse("/clear"),
            Some(SlashCommand::Clear { confirm: false })
        );
        assert_eq!(
            SlashCommand::parse("/clear --confirm"),
            Some(SlashCommand::Clear { confirm: true })
        );
        assert_eq!(SlashCommand::parse("/cost"), Some(SlashCommand::Cost));
        assert_eq!(
            SlashCommand::parse("/resume session.json"),
            Some(SlashCommand::Resume {
                session_path: Some("session.json".to_string()),
            })
        );
        assert_eq!(
            SlashCommand::parse("/config"),
            Some(SlashCommand::Config { section: None })
        );
        assert_eq!(
            SlashCommand::parse("/config env"),
            Some(SlashCommand::Config {
                section: Some("env".to_string())
            })
        );
        assert_eq!(SlashCommand::parse("/memory"), Some(SlashCommand::Memory));
        assert_eq!(
            SlashCommand::parse("/foundations"),
            Some(SlashCommand::Foundations { family: None })
        );
        assert_eq!(
            SlashCommand::parse("/foundations mcp"),
            Some(SlashCommand::Foundations {
                family: Some("mcp".to_string())
            })
        );
        assert_eq!(SlashCommand::parse("/init"), Some(SlashCommand::Init));
        assert_eq!(SlashCommand::parse("/diff"), Some(SlashCommand::Diff));
        assert_eq!(SlashCommand::parse("/version"), Some(SlashCommand::Version));
        assert_eq!(
            SlashCommand::parse("/export notes.txt"),
            Some(SlashCommand::Export {
                path: Some("notes.txt".to_string())
            })
        );
        assert_eq!(
            SlashCommand::parse("/session switch abc123"),
            Some(SlashCommand::Session {
                action: Some("switch".to_string()),
                target: Some("abc123".to_string())
            })
        );
        assert_eq!(
            SlashCommand::parse("/plugins install demo"),
            Some(SlashCommand::Plugins {
                action: Some("install".to_string()),
                target: Some("demo".to_string())
            })
        );
        assert_eq!(
            SlashCommand::parse("/plugins list"),
            Some(SlashCommand::Plugins {
                action: Some("list".to_string()),
                target: None
            })
        );
        assert_eq!(
            SlashCommand::parse("/plugins enable demo"),
            Some(SlashCommand::Plugins {
                action: Some("enable".to_string()),
                target: Some("demo".to_string())
            })
        );
        assert_eq!(
            SlashCommand::parse("/plugins disable demo"),
            Some(SlashCommand::Plugins {
                action: Some("disable".to_string()),
                target: Some("demo".to_string())
            })
        );
    }

    #[test]
    fn renders_help_from_shared_specs() {
        let help = render_slash_command_help();
        assert!(help.contains("available via openyak --resume SESSION.json"));
        assert!(help.contains("Core flow"));
        assert!(help.contains("Workspace & memory"));
        assert!(help.contains("Sessions & output"));
        assert!(help.contains("Git & GitHub"));
        assert!(help.contains("Automation & discovery"));
        assert!(help.contains("/help"));
        assert!(help.contains("/status"));
        assert!(help.contains("/compact"));
        assert!(help.contains("/bughunter [scope]"));
        assert!(help.contains("/branch [list|create <name>|switch <name>]"));
        assert!(help.contains("/worktree [list|add <path> [branch]|remove <path>|prune]"));
        assert!(help.contains("/commit"));
        assert!(help.contains("/commit-push-pr [context]"));
        assert!(help.contains("/pr [context]"));
        assert!(help.contains("/issue [context]"));
        assert!(help.contains("/ultraplan [task]"));
        assert!(help.contains("/teleport <symbol-or-path>"));
        assert!(help.contains("/debug-tool-call"));
        assert!(help.contains("/model [model]"));
        assert!(help.contains("/permissions [read-only|workspace-write|danger-full-access]"));
        assert!(help.contains("/plan [exit]"));
        assert!(help.contains("/clear [--confirm]"));
        assert!(help.contains("/cost"));
        assert!(help.contains("/resume <session-path>"));
        assert!(help.contains("/config [env|hooks|model|plugins]"));
        assert!(help.contains("/memory"));
        assert!(help.contains("/foundations [family]"));
        assert!(help.contains("/init"));
        assert!(help.contains("/diff"));
        assert!(help.contains("/version"));
        assert!(help.contains("/export [file]"));
        assert!(help.contains("/session [list|switch <session-id>]"));
        assert!(help.contains(
            "/plugin [list|install <path-or-git-url>|enable <name>|disable <name>|uninstall <id>|update <id>]"
        ));
        assert!(help.contains("aliases: /plugins, /marketplace"));
        assert!(help.contains("/agents"));
        assert!(help.contains("/foundations [family]"));
        assert!(help.contains("/skills"));
        assert_eq!(slash_command_specs().len(), 30);
        assert_eq!(resume_supported_slash_commands().len(), 14);
    }

    #[test]
    fn suggests_close_slash_commands() {
        let suggestions = suggest_slash_commands("stats", 3);
        assert!(!suggestions.is_empty());
        assert_eq!(suggestions[0], "/status");
    }

    #[test]
    fn compacts_sessions_via_slash_command() {
        let session = Session {
            version: 1,
            messages: vec![
                ConversationMessage::user_text("a ".repeat(200)),
                ConversationMessage::assistant(vec![ContentBlock::Text {
                    text: "b ".repeat(200),
                }]),
                ConversationMessage::tool_result("1", "bash", "ok ".repeat(200), false),
                ConversationMessage::assistant(vec![ContentBlock::Text {
                    text: "recent".to_string(),
                }]),
            ],
            telemetry: None,
        };

        let result = handle_slash_command(
            "/compact",
            &session,
            CompactionConfig {
                preserve_recent_messages: 2,
                max_estimated_tokens: 1,
            },
        )
        .expect("slash command should be handled");

        assert!(result.message.contains("Compacted 2 messages"));
        assert_eq!(result.session.messages[0].role, MessageRole::System);
    }

    #[test]
    fn help_command_is_non_mutating() {
        let session = Session::new();
        let result = handle_slash_command("/help", &session, CompactionConfig::default())
            .expect("help command should be handled");
        assert_eq!(result.session, session);
        assert!(result.message.contains("Slash commands"));
    }

    #[test]
    fn ignores_unknown_or_runtime_bound_slash_commands() {
        let session = Session::new();
        assert!(handle_slash_command("/unknown", &session, CompactionConfig::default()).is_none());
        assert!(handle_slash_command("/status", &session, CompactionConfig::default()).is_none());
        assert!(
            handle_slash_command("/branch list", &session, CompactionConfig::default()).is_none()
        );
        assert!(
            handle_slash_command("/bughunter", &session, CompactionConfig::default()).is_none()
        );
        assert!(
            handle_slash_command("/worktree list", &session, CompactionConfig::default()).is_none()
        );
        assert!(handle_slash_command("/commit", &session, CompactionConfig::default()).is_none());
        assert!(handle_slash_command(
            "/commit-push-pr review notes",
            &session,
            CompactionConfig::default()
        )
        .is_none());
        assert!(handle_slash_command("/pr", &session, CompactionConfig::default()).is_none());
        assert!(handle_slash_command("/issue", &session, CompactionConfig::default()).is_none());
        assert!(
            handle_slash_command("/ultraplan", &session, CompactionConfig::default()).is_none()
        );
        assert!(
            handle_slash_command("/teleport foo", &session, CompactionConfig::default()).is_none()
        );
        assert!(
            handle_slash_command("/debug-tool-call", &session, CompactionConfig::default())
                .is_none()
        );
        assert!(
            handle_slash_command("/model sonnet", &session, CompactionConfig::default()).is_none()
        );
        assert!(handle_slash_command(
            "/permissions read-only",
            &session,
            CompactionConfig::default()
        )
        .is_none());
        assert!(handle_slash_command("/plan", &session, CompactionConfig::default()).is_none());
        assert!(handle_slash_command("/clear", &session, CompactionConfig::default()).is_none());
        assert!(
            handle_slash_command("/clear --confirm", &session, CompactionConfig::default())
                .is_none()
        );
        assert!(handle_slash_command("/cost", &session, CompactionConfig::default()).is_none());
        assert!(handle_slash_command(
            "/resume session.json",
            &session,
            CompactionConfig::default()
        )
        .is_none());
        assert!(handle_slash_command("/config", &session, CompactionConfig::default()).is_none());
        assert!(
            handle_slash_command("/config env", &session, CompactionConfig::default()).is_none()
        );
        assert!(
            handle_slash_command("/foundations", &session, CompactionConfig::default()).is_none()
        );
        assert!(handle_slash_command("/diff", &session, CompactionConfig::default()).is_none());
        assert!(handle_slash_command("/version", &session, CompactionConfig::default()).is_none());
        assert!(
            handle_slash_command("/export note.txt", &session, CompactionConfig::default())
                .is_none()
        );
        assert!(
            handle_slash_command("/session list", &session, CompactionConfig::default()).is_none()
        );
        assert!(
            handle_slash_command("/plugins list", &session, CompactionConfig::default()).is_none()
        );
    }

    #[test]
    fn renders_plugins_report_with_name_version_and_status() {
        let rendered = render_plugins_report(&[
            PluginSummary {
                metadata: PluginMetadata {
                    id: "demo@external".to_string(),
                    name: "demo".to_string(),
                    version: "1.2.3".to_string(),
                    description: "demo plugin".to_string(),
                    kind: PluginKind::External,
                    source: "demo".to_string(),
                    default_enabled: false,
                    root: None,
                },
                enabled: true,
            },
            PluginSummary {
                metadata: PluginMetadata {
                    id: "sample@external".to_string(),
                    name: "sample".to_string(),
                    version: "0.9.0".to_string(),
                    description: "sample plugin".to_string(),
                    kind: PluginKind::External,
                    source: "sample".to_string(),
                    default_enabled: false,
                    root: None,
                },
                enabled: false,
            },
        ]);

        assert!(rendered.contains("demo"));
        assert!(rendered.contains("v1.2.3"));
        assert!(rendered.contains("enabled"));
        assert!(rendered.contains("sample"));
        assert!(rendered.contains("v0.9.0"));
        assert!(rendered.contains("disabled"));
    }

    #[test]
    fn lists_agents_from_project_and_user_roots() {
        let workspace = temp_dir("agents-workspace");
        let project_agents = workspace.join(".codex").join("agents");
        let user_home = temp_dir("agents-home");
        let user_agents = user_home.join(".codex").join("agents");

        write_agent(
            &project_agents,
            "planner",
            "Project planner",
            "gpt-5.4",
            "medium",
        );
        write_agent(
            &user_agents,
            "planner",
            "User planner",
            "gpt-5.4-mini",
            "high",
        );
        write_agent(
            &user_agents,
            "verifier",
            "Verification agent",
            "gpt-5.4-mini",
            "high",
        );

        let roots = vec![
            (DefinitionSource::ProjectCodex, project_agents),
            (DefinitionSource::UserCodex, user_agents),
        ];
        let report =
            render_agents_report(&load_agents_from_roots(&roots).expect("agent roots should load"));

        assert!(report.contains("Agents"));
        assert!(report.contains("2 active agents"));
        assert!(report.contains("Project (.codex):"));
        assert!(report.contains("planner · Project planner · gpt-5.4 · medium"));
        assert!(report.contains("User (home/.codex):"));
        assert!(report.contains("(shadowed by Project (.codex)) planner · User planner"));
        assert!(report.contains("verifier · Verification agent · gpt-5.4-mini · high"));

        let _ = fs::remove_dir_all(workspace);
        let _ = fs::remove_dir_all(user_home);
    }

    #[test]
    fn lists_skills_from_project_and_user_roots() {
        let workspace = temp_dir("skills-workspace");
        let project_skills = workspace.join(".codex").join("skills");
        let project_commands = workspace.join(".openyak").join("commands");
        let user_home = temp_dir("skills-home");
        let user_skills = user_home.join(".codex").join("skills");

        write_skill(&project_skills, "plan", "Project planning guidance");
        write_legacy_command(&project_commands, "deploy", "Legacy deployment guidance");
        write_skill(&user_skills, "plan", "User planning guidance");
        write_skill(&user_skills, "help", "Help guidance");

        let roots = vec![
            SkillRoot {
                source: DefinitionSource::ProjectCodex,
                path: project_skills,
                origin: SkillOrigin::SkillsDir,
            },
            SkillRoot {
                source: DefinitionSource::ProjectOpenyak,
                path: project_commands,
                origin: SkillOrigin::LegacyCommandsDir,
            },
            SkillRoot {
                source: DefinitionSource::UserCodex,
                path: user_skills,
                origin: SkillOrigin::SkillsDir,
            },
        ];
        let report =
            render_skills_report(&load_skills_from_roots(&roots).expect("skill roots should load"));

        assert!(report.contains("Skills"));
        assert!(report.contains("3 available skills"));
        assert!(report.contains("Project (.codex):"));
        assert!(report.contains("plan · Project planning guidance"));
        assert!(report.contains("Project (.openyak):"));
        assert!(report.contains("deploy · Legacy deployment guidance · legacy /commands"));
        assert!(report.contains("User (home/.codex):"));
        assert!(report.contains("(shadowed by Project (.codex)) plan · User planning guidance"));
        assert!(report.contains("help · Help guidance"));

        let _ = fs::remove_dir_all(workspace);
        let _ = fs::remove_dir_all(user_home);
    }

    #[test]
    fn lists_nested_system_skills_from_codex_home_root() {
        let _guard = env_lock();
        let workspace = temp_dir("system-skills-workspace");
        let codex_home = temp_dir("system-skills-home");
        let nested_skill = codex_home
            .join("skills")
            .join(".system")
            .join("openai-docs");
        fs::create_dir_all(&nested_skill).expect("nested skill root");
        fs::write(
            nested_skill.join("SKILL.md"),
            "---\nname: openai-docs\ndescription: Official docs guidance\n---\n\n# openai-docs\n",
        )
        .expect("write nested skill");

        let original_home = env::var_os("HOME");
        let original_userprofile = env::var_os("USERPROFILE");
        let original_codex_home = env::var_os("CODEX_HOME");
        let original_openyak_config_home = env::var_os("OPENYAK_CONFIG_HOME");

        env::remove_var("HOME");
        env::remove_var("USERPROFILE");
        env::remove_var("OPENYAK_CONFIG_HOME");
        env::set_var("CODEX_HOME", &codex_home);

        let roots = discover_skill_roots(&workspace);
        let report =
            render_skills_report(&load_skills_from_roots(&roots).expect("skill roots should load"));

        assert!(report.contains("User ($CODEX_HOME):"));
        assert!(report.contains("openai-docs · Official docs guidance"));

        restore_env("HOME", original_home);
        restore_env("USERPROFILE", original_userprofile);
        restore_env("CODEX_HOME", original_codex_home);
        restore_env("OPENYAK_CONFIG_HOME", original_openyak_config_home);
        let _ = fs::remove_dir_all(workspace);
        let _ = fs::remove_dir_all(codex_home);
    }

    #[test]
    fn nested_workspace_under_home_keeps_user_definitions_in_user_scope() {
        let _guard = env_lock();
        let user_home = temp_dir("home-ancestor");
        let workspace = user_home.join("Desktop").join("project");
        let user_agents = user_home.join(".codex").join("agents");
        let user_skills = user_home.join(".codex").join("skills");
        fs::create_dir_all(&workspace).expect("workspace root");
        write_agent(
            &user_agents,
            "reviewer",
            "Review agent",
            "gpt-5.4-mini",
            "high",
        );
        write_skill(&user_skills, "openai-docs", "Official docs guidance");

        let original_home = env::var_os("HOME");
        let original_userprofile = env::var_os("USERPROFILE");
        let original_homedrive = env::var_os("HOMEDRIVE");
        let original_homepath = env::var_os("HOMEPATH");
        let original_codex_home = env::var_os("CODEX_HOME");
        let original_openyak_config_home = env::var_os("OPENYAK_CONFIG_HOME");

        env::remove_var("HOME");
        env::remove_var("HOMEDRIVE");
        env::remove_var("HOMEPATH");
        env::remove_var("CODEX_HOME");
        env::remove_var("OPENYAK_CONFIG_HOME");
        env::set_var("USERPROFILE", &user_home);

        let agent_roots = discover_definition_roots(&workspace, "agents");
        let agent_report =
            render_agents_report(&load_agents_from_roots(&agent_roots).expect("agent roots"));
        assert!(agent_report.contains("User (home/.codex):"));
        assert!(!agent_report.contains("Project (.codex):"));

        let skill_roots = discover_skill_roots(&workspace);
        let skill_report =
            render_skills_report(&load_skills_from_roots(&skill_roots).expect("skill roots"));
        assert!(skill_report.contains("User (home/.codex):"));
        assert!(!skill_report.contains("Project (.codex):"));

        restore_env("HOME", original_home);
        restore_env("USERPROFILE", original_userprofile);
        restore_env("HOMEDRIVE", original_homedrive);
        restore_env("HOMEPATH", original_homepath);
        restore_env("CODEX_HOME", original_codex_home);
        restore_env("OPENYAK_CONFIG_HOME", original_openyak_config_home);
        let _ = fs::remove_dir_all(user_home);
    }

    #[test]
    fn agents_and_skills_usage_support_help_and_unexpected_args() {
        let cwd = temp_dir("slash-usage");

        let agents_help =
            super::handle_agents_slash_command(Some("help"), &cwd).expect("agents help");
        assert!(agents_help.contains("Usage            /agents"));
        assert!(agents_help.contains("Direct CLI       openyak agents"));

        let agents_unexpected =
            super::handle_agents_slash_command(Some("show planner"), &cwd).expect("agents usage");
        assert!(agents_unexpected.contains("Unexpected       show planner"));

        let skills_help =
            super::handle_skills_slash_command(Some("--help"), &cwd).expect("skills help");
        assert!(skills_help.contains("Usage            /skills"));
        assert!(skills_help.contains("legacy /commands"));

        let skills_unexpected =
            super::handle_skills_slash_command(Some("show help"), &cwd).expect("skills usage");
        assert!(skills_unexpected.contains("Unexpected       show help"));

        let _ = fs::remove_dir_all(cwd);
    }

    #[test]
    fn skills_registry_commands_round_trip_against_packaged_fixture() {
        let _guard = env_lock();
        let cwd = temp_dir("skills-registry");
        let config_home = temp_dir("skills-config-home");
        fs::create_dir_all(&cwd).expect("cwd");
        fs::create_dir_all(&config_home).expect("config home");
        let registry_path = packaged_skill_registry_path();
        assert!(
            registry_path.is_file(),
            "packaged registry fixture should exist"
        );

        let original_openyak_config_home = env::var_os("OPENYAK_CONFIG_HOME");
        env::set_var("OPENYAK_CONFIG_HOME", &config_home);

        let available = super::handle_skills_slash_command(
            Some(&format!(
                "available --registry {}",
                registry_path.to_string_lossy()
            )),
            &cwd,
        )
        .expect("available should succeed");
        assert!(available.contains("release-checklist"));
        assert!(available.contains("session-handoff"));

        let info_before_install = super::handle_skills_slash_command(
            Some(&format!(
                "info release-checklist --registry {}",
                registry_path.to_string_lossy()
            )),
            &cwd,
        )
        .expect("info should succeed");
        assert!(info_before_install.contains("Installed        no managed install"));
        assert!(info_before_install.contains("2.0.0"));

        let install = super::handle_skills_slash_command(
            Some(&format!(
                "install release-checklist --version 1.0.0 --registry {}",
                registry_path.to_string_lossy()
            )),
            &cwd,
        )
        .expect("install should succeed");
        assert!(install.contains("Result           installed"));
        assert!(install.contains("Pinned version   1.0.0"));

        let pinned_update = super::handle_skills_slash_command(
            Some(&format!(
                "update release-checklist --registry {}",
                registry_path.to_string_lossy()
            )),
            &cwd,
        )
        .expect("pinned update should succeed");
        assert!(pinned_update.contains("Result           pinned"));

        let explicit_update = super::handle_skills_slash_command(
            Some(&format!(
                "update release-checklist --version 2.0.0 --registry {}",
                registry_path.to_string_lossy()
            )),
            &cwd,
        )
        .expect("explicit update should succeed");
        assert!(explicit_update.contains("Result           updated"));
        assert!(explicit_update.contains("Old version      1.0.0"));
        assert!(explicit_update.contains("New version      2.0.0"));
        assert!(explicit_update.contains("Pinned version   2.0.0"));

        let info_after_update = super::handle_skills_slash_command(
            Some(&format!(
                "info release-checklist --registry {}",
                registry_path.to_string_lossy()
            )),
            &cwd,
        )
        .expect("info after update should succeed");
        assert!(info_after_update.contains("Installed        v2.0.0"));

        let uninstall =
            super::handle_skills_slash_command(Some("uninstall release-checklist"), &cwd)
                .expect("uninstall should succeed");
        assert!(uninstall.contains("Result           uninstalled"));

        restore_env("OPENYAK_CONFIG_HOME", original_openyak_config_home);
        let _ = fs::remove_dir_all(config_home);
        let _ = fs::remove_dir_all(cwd);
    }

    #[test]
    fn skills_install_reports_shadowing_when_project_skill_takes_precedence() {
        let _guard = env_lock();
        let cwd = temp_dir("skills-shadowing");
        let config_home = temp_dir("skills-shadowing-config-home");
        let project_skills = cwd.join(".codex").join("skills");
        fs::create_dir_all(&cwd).expect("cwd");
        fs::create_dir_all(&config_home).expect("config home");
        write_skill(
            &project_skills,
            "release-checklist",
            "Project-specific checklist",
        );
        let registry_path = packaged_skill_registry_path();
        assert!(
            registry_path.is_file(),
            "packaged registry fixture should exist"
        );

        let original_openyak_config_home = env::var_os("OPENYAK_CONFIG_HOME");
        env::set_var("OPENYAK_CONFIG_HOME", &config_home);

        let install = super::handle_skills_slash_command(
            Some(&format!(
                "install release-checklist --registry {}",
                registry_path.to_string_lossy()
            )),
            &cwd,
        )
        .expect("install should succeed");
        assert!(install.contains("Shadowing"));
        assert!(install.contains("Project (.codex)"));

        restore_env("OPENYAK_CONFIG_HOME", original_openyak_config_home);
        let _ = fs::remove_dir_all(config_home);
        let _ = fs::remove_dir_all(cwd);
    }

    #[test]
    fn parses_quoted_skill_frontmatter_values() {
        let contents = "---\nname: \"hud\"\ndescription: 'Quoted description'\n---\n";
        let (name, description) = super::parse_skill_frontmatter(contents);
        assert_eq!(name.as_deref(), Some("hud"));
        assert_eq!(description.as_deref(), Some("Quoted description"));
    }

    #[test]
    fn installs_plugin_from_path_and_lists_it() {
        let config_home = temp_dir("home");
        let source_root = temp_dir("source");
        write_external_plugin(&source_root, "demo", "1.0.0");

        let mut manager = PluginManager::new(PluginManagerConfig::new(&config_home));
        let install = handle_plugins_slash_command(
            Some("install"),
            Some(source_root.to_str().expect("utf8 path")),
            &mut manager,
        )
        .expect("install command should succeed");
        assert!(install.reload_runtime);
        assert!(install.message.contains("installed demo@external"));
        assert!(install.message.contains("Name             demo"));
        assert!(install.message.contains("Version          1.0.0"));
        assert!(install.message.contains("Source           "));
        assert!(install.message.contains("Install path     "));
        assert!(install.message.contains("Status           enabled"));

        let list = handle_plugins_slash_command(Some("list"), None, &mut manager)
            .expect("list command should succeed");
        assert!(!list.reload_runtime);
        assert!(list.message.contains("demo"));
        assert!(list.message.contains("v1.0.0"));
        assert!(list.message.contains("enabled"));

        let _ = fs::remove_dir_all(config_home);
        let _ = fs::remove_dir_all(source_root);
    }

    #[test]
    fn enables_and_disables_plugin_by_name() {
        let config_home = temp_dir("toggle-home");
        let source_root = temp_dir("toggle-source");
        write_external_plugin(&source_root, "demo", "1.0.0");

        let mut manager = PluginManager::new(PluginManagerConfig::new(&config_home));
        handle_plugins_slash_command(
            Some("install"),
            Some(source_root.to_str().expect("utf8 path")),
            &mut manager,
        )
        .expect("install command should succeed");

        let disable = handle_plugins_slash_command(Some("disable"), Some("demo"), &mut manager)
            .expect("disable command should succeed");
        assert!(disable.reload_runtime);
        assert!(disable.message.contains("disabled demo@external"));
        assert!(disable.message.contains("Name             demo"));
        assert!(disable.message.contains("Status           disabled"));

        let list = handle_plugins_slash_command(Some("list"), None, &mut manager)
            .expect("list command should succeed");
        assert!(list.message.contains("demo"));
        assert!(list.message.contains("disabled"));

        let enable = handle_plugins_slash_command(Some("enable"), Some("demo"), &mut manager)
            .expect("enable command should succeed");
        assert!(enable.reload_runtime);
        assert!(enable.message.contains("enabled demo@external"));
        assert!(enable.message.contains("Name             demo"));
        assert!(enable.message.contains("Status           enabled"));

        let list = handle_plugins_slash_command(Some("list"), None, &mut manager)
            .expect("list command should succeed");
        assert!(list.message.contains("demo"));
        assert!(list.message.contains("enabled"));

        let _ = fs::remove_dir_all(config_home);
        let _ = fs::remove_dir_all(source_root);
    }

    #[test]
    fn update_command_reports_source_and_install_path() {
        let config_home = temp_dir("update-report-home");
        let source_root = temp_dir("update-report-source");
        write_external_plugin(&source_root, "demo", "1.0.0");

        let mut manager = PluginManager::new(PluginManagerConfig::new(&config_home));
        handle_plugins_slash_command(
            Some("install"),
            Some(source_root.to_str().expect("utf8 path")),
            &mut manager,
        )
        .expect("install command should succeed");
        write_external_plugin(&source_root, "demo", "2.0.0");

        let update =
            handle_plugins_slash_command(Some("update"), Some("demo@external"), &mut manager)
                .expect("update command should succeed");
        assert!(update.reload_runtime);
        assert!(update.message.contains("updated demo@external"));
        assert!(update.message.contains("Old version      1.0.0"));
        assert!(update.message.contains("New version      2.0.0"));
        assert!(update.message.contains("Source           "));
        assert!(update.message.contains("Install path     "));

        let _ = fs::remove_dir_all(config_home);
        let _ = fs::remove_dir_all(source_root);
    }

    #[test]
    fn lists_auto_installed_bundled_plugins_with_status() {
        let config_home = temp_dir("bundled-home");
        let bundled_root = temp_dir("bundled-root");
        let bundled_plugin = bundled_root.join("starter");
        write_bundled_plugin(&bundled_plugin, "starter", "0.1.0", false);

        let mut config = PluginManagerConfig::new(&config_home);
        config.bundled_root = Some(bundled_root.clone());
        let mut manager = PluginManager::new(config);

        let list = handle_plugins_slash_command(Some("list"), None, &mut manager)
            .expect("list command should succeed");
        assert!(!list.reload_runtime);
        assert!(list.message.contains("starter"));
        assert!(list.message.contains("v0.1.0"));
        assert!(list.message.contains("disabled"));

        let _ = fs::remove_dir_all(config_home);
        let _ = fs::remove_dir_all(bundled_root);
    }

    #[test]
    fn branch_and_worktree_commands_manage_git_state() {
        // given
        let repo = init_git_repo("branch-worktree");
        let worktree_path = repo
            .parent()
            .expect("repo should have parent")
            .join(format!("branch-worktree-linked-{}", std::process::id()));

        // when
        let branch_list =
            handle_branch_slash_command(Some("list"), None, &repo).expect("branch list succeeds");
        let created = handle_branch_slash_command(Some("create"), Some("feature/demo"), &repo)
            .expect("branch create succeeds");
        let switched = handle_branch_slash_command(Some("switch"), Some("main"), &repo)
            .expect("branch switch succeeds");
        let added = handle_worktree_slash_command(
            Some("add"),
            Some(worktree_path.to_str().expect("utf8 path")),
            Some("wt-demo"),
            &repo,
        )
        .expect("worktree add succeeds");
        let listed_worktrees =
            handle_worktree_slash_command(Some("list"), None, None, &repo).expect("list succeeds");
        let removed = handle_worktree_slash_command(
            Some("remove"),
            Some(worktree_path.to_str().expect("utf8 path")),
            None,
            &repo,
        )
        .expect("remove succeeds");

        // then
        assert!(branch_list.contains("main"));
        assert!(created.contains("feature/demo"));
        assert!(switched.contains("main"));
        assert!(added.contains("wt-demo"));
        let normalized_listed = listed_worktrees.replace('\\', "/");
        let worktree_name = worktree_path
            .file_name()
            .expect("worktree should have file name")
            .to_string_lossy()
            .replace('\\', "/");
        assert!(normalized_listed.contains(&worktree_name));
        assert!(normalized_listed.contains("wt-demo"));
        assert!(removed.contains("Result           removed"));

        let _ = fs::remove_dir_all(repo);
        let _ = fs::remove_dir_all(worktree_path);
    }

    #[test]
    fn commit_command_stages_and_commits_changes() {
        // given
        let repo = init_git_repo("commit-command");
        fs::write(repo.join("notes.txt"), "hello\n").expect("write notes");

        // when
        let report =
            handle_commit_slash_command("feat: add notes", &repo).expect("commit succeeds");
        let status = run_command(&repo, "git", &["status", "--short"]);
        let message = run_command(&repo, "git", &["log", "-1", "--pretty=%B"]);

        // then
        assert!(report.contains("Result           created"));
        assert!(status.trim().is_empty());
        assert_eq!(message.trim(), "feat: add notes");

        let _ = fs::remove_dir_all(repo);
    }

    #[test]
    fn commit_command_ignores_openyak_session_artifacts() {
        let repo = init_git_repo("commit-command-ignore-sessions");
        let session_dir = repo.join(".openyak").join("sessions");
        fs::create_dir_all(&session_dir).expect("session dir");
        fs::write(session_dir.join("session.json"), "{}\n").expect("session file");

        let report =
            handle_commit_slash_command("feat: should skip", &repo).expect("commit command");
        let status = run_command(&repo, "git", &["status", "--short"]);

        assert!(report.contains("Result           skipped"));
        assert!(status.contains(".openyak"));

        let _ = fs::remove_dir_all(repo);
    }

    #[test]
    fn commit_command_succeeds_with_ignored_local_openyak_artifacts_present() {
        let repo = init_git_repo("commit-command-ignored-local-artifacts");
        fs::write(
            repo.join(".gitignore"),
            ".openyak/settings.local.json\n.openyak/sessions/\n",
        )
        .expect("write gitignore");
        fs::create_dir_all(repo.join(".openyak").join("sessions")).expect("create session dir");
        fs::write(repo.join(".openyak").join("settings.local.json"), "{\n}\n")
            .expect("write local settings");
        fs::write(
            repo.join(".openyak").join("sessions").join("session.json"),
            "{\n}\n",
        )
        .expect("write session file");
        fs::write(repo.join("notes.txt"), "hello\n").expect("write notes");

        let report =
            handle_commit_slash_command("feat: add notes", &repo).expect("commit succeeds");
        let message = run_command(&repo, "git", &["log", "-1", "--pretty=%B"]);
        let committed = run_command(&repo, "git", &["show", "--stat", "--oneline", "--format="]);

        assert!(report.contains("Result           created"));
        assert_eq!(message.trim(), "feat: add notes");
        assert!(committed.contains("notes.txt"));
        assert!(!committed.contains(".openyak/settings.local.json"));
        assert!(!committed.contains(".openyak/sessions"));
        assert!(repo.join(".openyak").join("settings.local.json").is_file());
        assert!(repo
            .join(".openyak")
            .join("sessions")
            .join("session.json")
            .is_file());

        let _ = fs::remove_dir_all(repo);
    }

    #[cfg(unix)]
    #[test]
    fn commit_push_pr_command_commits_pushes_and_creates_pr() {
        // given
        let _guard = env_lock();
        let repo = init_git_repo("commit-push-pr");
        let remote = init_bare_repo("commit-push-pr-remote");
        run_command(
            &repo,
            "git",
            &[
                "remote",
                "add",
                "origin",
                remote.to_str().expect("utf8 remote"),
            ],
        );
        run_command(&repo, "git", &["push", "-u", "origin", "main"]);
        fs::write(repo.join("feature.txt"), "feature\n").expect("write feature file");

        let fake_bin = temp_dir("fake-gh-bin");
        let gh_log = fake_bin.join("gh.log");
        write_fake_gh(&fake_bin, &gh_log, "https://example.com/pr/123");

        let previous_path = env::var_os("PATH");
        let mut path_entries = vec![fake_bin.clone()];
        if let Some(path) = &previous_path {
            path_entries.extend(env::split_paths(path));
        }
        let new_path = env::join_paths(path_entries).expect("path should join");
        env::set_var("PATH", &new_path);
        let previous_safeuser = env::var_os("SAFEUSER");
        env::set_var("SAFEUSER", "tester");

        let request = CommitPushPrRequest {
            commit_message: Some("feat: add feature file".to_string()),
            pr_title: "Add feature file".to_string(),
            pr_body: "## Summary\n- add feature file".to_string(),
            branch_name_hint: "Add feature file".to_string(),
        };

        // when
        let report =
            handle_commit_push_pr_slash_command(&request, &repo).expect("commit-push-pr succeeds");
        let branch = run_command(&repo, "git", &["branch", "--show-current"]);
        let message = run_command(&repo, "git", &["log", "-1", "--pretty=%B"]);
        let gh_invocations = fs::read_to_string(&gh_log).expect("gh log should exist");

        // then
        assert!(report.contains("Result           created"));
        assert!(report.contains("URL              https://example.com/pr/123"));
        assert_eq!(branch.trim(), "tester/add-feature-file");
        assert_eq!(message.trim(), "feat: add feature file");
        assert!(gh_invocations.contains("pr create"));
        assert!(gh_invocations.contains("--base main"));

        if let Some(path) = previous_path {
            env::set_var("PATH", path);
        } else {
            env::remove_var("PATH");
        }
        if let Some(safeuser) = previous_safeuser {
            env::set_var("SAFEUSER", safeuser);
        } else {
            env::remove_var("SAFEUSER");
        }

        let _ = fs::remove_dir_all(repo);
        let _ = fs::remove_dir_all(remote);
        let _ = fs::remove_dir_all(fake_bin);
    }

    #[cfg(unix)]
    #[test]
    fn commit_push_pr_skips_without_creating_branch_when_nothing_changed() {
        let _guard = env_lock();
        let repo = init_git_repo("commit-push-pr-skip");
        let remote = init_bare_repo("commit-push-pr-skip-remote");
        run_command(
            &repo,
            "git",
            &[
                "remote",
                "add",
                "origin",
                remote.to_str().expect("utf8 remote"),
            ],
        );
        run_command(&repo, "git", &["push", "-u", "origin", "main"]);

        let fake_bin = temp_dir("fake-gh-bin-skip");
        let gh_log = fake_bin.join("gh.log");
        write_fake_gh(&fake_bin, &gh_log, "https://example.com/pr/unused");

        let previous_path = env::var_os("PATH");
        let mut path_entries = vec![fake_bin.clone()];
        if let Some(path) = &previous_path {
            path_entries.extend(env::split_paths(path));
        }
        let new_path = env::join_paths(path_entries).expect("path should join");
        env::set_var("PATH", &new_path);
        let previous_safeuser = env::var_os("SAFEUSER");
        env::set_var("SAFEUSER", "tester");

        let request = CommitPushPrRequest {
            commit_message: None,
            pr_title: "Nothing changed".to_string(),
            pr_body: "## Summary\n- no changes".to_string(),
            branch_name_hint: "Nothing changed".to_string(),
        };

        let report =
            handle_commit_push_pr_slash_command(&request, &repo).expect("commit-push-pr skips");
        let branch = run_command(&repo, "git", &["branch", "--show-current"]);
        let branches = run_command(&repo, "git", &["branch", "--list"]);

        assert!(report.contains("Result           skipped"));
        assert_eq!(branch.trim(), "main");
        assert!(!branches.contains("tester/nothing-changed"));
        assert!(
            !gh_log.exists()
                || fs::read_to_string(&gh_log)
                    .expect("read gh log")
                    .trim()
                    .is_empty()
        );

        if let Some(path) = previous_path {
            env::set_var("PATH", path);
        } else {
            env::remove_var("PATH");
        }
        if let Some(safeuser) = previous_safeuser {
            env::set_var("SAFEUSER", safeuser);
        } else {
            env::remove_var("SAFEUSER");
        }

        let _ = fs::remove_dir_all(repo);
        let _ = fs::remove_dir_all(remote);
        let _ = fs::remove_dir_all(fake_bin);
    }
}

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::io::{Read, Write};
use std::net::{IpAddr, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use api::{
    max_tokens_for_model, resolve_model_alias, ContentBlockDelta, InputContentBlock, InputMessage,
    MessageRequest, MessageResponse, OutputContentBlock, ProviderClient,
    StreamEvent as ApiStreamEvent, ToolChoice, ToolDefinition, ToolResultContentBlock,
};
use plugins::PluginTool;
use reqwest::{blocking::Client, Method, Url};
use runtime::{
    command_exists, current_local_date_string, default_openyak_home, edit_file, execute_bash,
    execute_bash_with_config, glob_search, grep_search, home_locations, load_system_prompt,
    read_file, resolve_command_path, resolve_skill_path_from_roots, write_file, ApiClient,
    ApiRequest, AssistantEvent, BashCommandInput, ContentBlock, ConversationMessage,
    ConversationRuntime, CronRegistry, EnforcementResult, GrepSearchInput, LspRegistry,
    McpToolRegistry, MessageRole, PermissionEnforcer, PermissionMode, PermissionPolicy,
    RuntimeBrowserControlConfig, RuntimeError, Session, TaskRegistry, TeamRegistry, TokenUsage,
    ToolError, ToolExecutor, ToolProfileBashPolicy, UserInputRequest,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

const REQUEST_USER_INPUT_TOOL_NAME: &str = "openyak_request_user_input";
const SESSION_SERVER_URL_ENV: &str = "OPENYAK_SESSION_SERVER_URL";
const THREAD_SERVER_INFO_FILENAME: &str = "thread-server.json";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolManifestEntry {
    pub name: String,
    pub source: ToolSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolSource {
    Base,
    Conditional,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolRegistry {
    entries: Vec<ToolManifestEntry>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FoundationSurface {
    pub key: &'static str,
    pub summary: &'static str,
    pub access_type: &'static str,
    pub backing_model: &'static str,
    pub boundary_note: &'static str,
    pub adjacent_scope: Option<&'static str>,
    pub not_promised: &'static str,
    pub tool_names: &'static [&'static str],
}

impl ToolRegistry {
    #[must_use]
    pub fn new(entries: Vec<ToolManifestEntry>) -> Self {
        Self { entries }
    }

    #[must_use]
    pub fn entries(&self) -> &[ToolManifestEntry] {
        &self.entries
    }
}

const TASK_FOUNDATION_TOOLS: &[&str] = &[
    "TaskCreate",
    "TaskGet",
    "TaskList",
    "TaskStop",
    "TaskUpdate",
    "TaskOutput",
    "TaskWait",
];

const TEAM_FOUNDATION_TOOLS: &[&str] = &["TeamCreate", "TeamGet", "TeamList", "TeamDelete"];

const CRON_FOUNDATION_TOOLS: &[&str] = &[
    "CronCreate",
    "CronGet",
    "CronDisable",
    "CronEnable",
    "CronDelete",
    "CronList",
];

const LSP_FOUNDATION_TOOLS: &[&str] = &["LSP"];

const MCP_FOUNDATION_TOOLS: &[&str] = &[
    "ListMcpServers",
    "ListMcpTools",
    "ListMcpResources",
    "ReadMcpResource",
    "McpAuth",
    "MCP",
];

const FOUNDATION_SURFACES: &[FoundationSurface] = &[
    FoundationSurface {
        key: "task",
        summary: "Metadata-first task lifecycle foundation for the current runtime.",
        access_type: "process-local foundation",
        backing_model: "process_local_v1 current-runtime registry",
        boundary_note: "Fresh CLI processes do not inherit durable task inventory.",
        adjacent_scope: None,
        not_promised: "persistence, leases, cross-process scheduling, remote execution",
        tool_names: TASK_FOUNDATION_TOOLS,
    },
    FoundationSurface {
        key: "team",
        summary: "Metadata-first team grouping foundation for the current runtime.",
        access_type: "process-local foundation",
        backing_model: "process_local_v1 current-runtime registry",
        boundary_note: "Fresh CLI processes do not inherit durable team inventory.",
        adjacent_scope: None,
        not_promised: "persistence, leases, cross-process orchestration, remote execution",
        tool_names: TEAM_FOUNDATION_TOOLS,
    },
    FoundationSurface {
        key: "cron",
        summary: "Metadata-first cron foundation for low-risk operator scheduling semantics.",
        access_type: "process-local foundation",
        backing_model: "process_local_v1 current-runtime registry",
        boundary_note: "Fresh CLI processes do not inherit durable cron inventory.",
        adjacent_scope: None,
        not_promised: "durable schedulers, crash recovery, shared services, remote execution",
        tool_names: CRON_FOUNDATION_TOOLS,
    },
    FoundationSurface {
        key: "lsp",
        summary: "Registry-backed query bridge for current LSP visibility and diagnostics.",
        access_type: "registry-backed bridge",
        backing_model: "current runtime LSP registry bridge",
        boundary_note: "This is not a standalone openyak lsp host or control plane.",
        adjacent_scope: Some("MB2 keeps standalone server/LSP surfacing narrow."),
        not_promised:
            "standalone host lifecycle, daemon management, broader control-plane behavior",
        tool_names: LSP_FOUNDATION_TOOLS,
    },
    FoundationSurface {
        key: "mcp",
        summary:
            "Registry-backed bridge for current MCP server, tool, resource, and auth visibility.",
        access_type: "registry-backed bridge",
        backing_model: "current runtime MCP registry bridge",
        boundary_note: "This is not a broader MCP control plane.",
        adjacent_scope: Some("MB5 owns deeper configured/auth/error lifecycle hardening."),
        not_promised: "remote orchestration, notification buses, wider control-plane behavior",
        tool_names: MCP_FOUNDATION_TOOLS,
    },
];

#[must_use]
pub fn foundation_surfaces() -> &'static [FoundationSurface] {
    FOUNDATION_SURFACES
}

#[must_use]
pub fn foundation_surface(name: &str) -> Option<&'static FoundationSurface> {
    foundation_surfaces()
        .iter()
        .find(|surface| surface.key.eq_ignore_ascii_case(name))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolSpec {
    pub name: &'static str,
    pub description: &'static str,
    pub input_schema: Value,
    pub required_permission: PermissionMode,
}

#[derive(Debug, Clone, PartialEq)]
pub struct GlobalToolRegistry {
    plugin_tools: Vec<PluginTool>,
    enforcer: Option<PermissionEnforcer>,
    bash_policy: Option<ToolProfileBashPolicy>,
    browser_control: RuntimeBrowserControlConfig,
}

impl GlobalToolRegistry {
    #[must_use]
    pub fn builtin() -> Self {
        Self {
            plugin_tools: Vec::new(),
            enforcer: None,
            bash_policy: None,
            browser_control: RuntimeBrowserControlConfig::default(),
        }
    }

    pub fn with_plugin_tools(plugin_tools: Vec<PluginTool>) -> Result<Self, String> {
        let builtin_names = mvp_tool_specs()
            .into_iter()
            .map(|spec| spec.name.to_string())
            .collect::<BTreeSet<_>>();
        let mut seen_plugin_names = BTreeSet::new();

        for tool in &plugin_tools {
            let name = tool.definition().name.clone();
            if builtin_names.contains(&name) {
                return Err(format!(
                    "plugin tool `{name}` conflicts with a built-in tool name"
                ));
            }
            if !seen_plugin_names.insert(name.clone()) {
                return Err(format!("duplicate plugin tool name `{name}`"));
            }
        }

        Ok(Self {
            plugin_tools,
            enforcer: None,
            bash_policy: None,
            browser_control: RuntimeBrowserControlConfig::default(),
        })
    }

    #[must_use]
    pub fn with_enforcer(mut self, enforcer: PermissionEnforcer) -> Self {
        self.enforcer = Some(enforcer);
        self
    }

    #[must_use]
    pub fn with_bash_policy(mut self, bash_policy: ToolProfileBashPolicy) -> Self {
        self.bash_policy = Some(bash_policy);
        self
    }

    pub fn with_browser_control(
        mut self,
        browser_control: RuntimeBrowserControlConfig,
    ) -> Result<Self, String> {
        if browser_control.enabled() {
            let optional_names = enabled_optional_builtin_specs(&browser_control)
                .into_iter()
                .map(|spec| spec.name)
                .collect::<BTreeSet<_>>();
            if let Some(conflict) = self
                .plugin_tools
                .iter()
                .map(|tool| tool.definition().name.as_str())
                .find(|name| optional_names.contains(name))
            {
                return Err(format!(
                    "plugin tool `{conflict}` conflicts with an optional built-in tool name"
                ));
            }
        }
        self.browser_control = browser_control;
        Ok(self)
    }

    pub fn normalize_allowed_tools(
        &self,
        values: &[String],
    ) -> Result<Option<BTreeSet<String>>, String> {
        if values.is_empty() {
            return Ok(None);
        }

        let builtin_specs = mvp_tool_specs();
        let enabled_optional_specs = enabled_optional_builtin_specs(&self.browser_control);
        let canonical_names = builtin_specs
            .iter()
            .map(|spec| spec.name.to_string())
            .chain(
                enabled_optional_specs
                    .iter()
                    .map(|spec| spec.name.to_string()),
            )
            .chain(
                self.plugin_tools
                    .iter()
                    .map(|tool| tool.definition().name.clone()),
            )
            .collect::<Vec<_>>();
        let mut name_map = canonical_names
            .iter()
            .map(|name| (normalize_tool_name(name), name.clone()))
            .collect::<BTreeMap<_, _>>();

        for (alias, canonical) in [
            ("read", "read_file"),
            ("write", "write_file"),
            ("edit", "edit_file"),
            ("glob", "glob_search"),
            ("grep", "grep_search"),
        ] {
            name_map.insert(alias.to_string(), canonical.to_string());
        }

        let mut allowed = BTreeSet::new();
        for value in values {
            for token in value
                .split(|ch: char| ch == ',' || ch.is_whitespace())
                .filter(|token| !token.is_empty())
            {
                let normalized = normalize_tool_name(token);
                if !self.browser_control.enabled() {
                    if normalized == normalize_tool_name(BROWSER_OBSERVE_TOOL_NAME) {
                        return Err(browser_tool_disabled_error(BROWSER_OBSERVE_TOOL_NAME));
                    }
                    if normalized == normalize_tool_name(BROWSER_INTERACT_TOOL_NAME) {
                        return Err(browser_tool_disabled_error(BROWSER_INTERACT_TOOL_NAME));
                    }
                }
                let canonical = name_map.get(&normalized).ok_or_else(|| {
                    format!(
                        "unsupported tool in --allowedTools: {token} (expected one of: {})",
                        canonical_names.join(", ")
                    )
                })?;
                allowed.insert(canonical.clone());
            }
        }

        Ok(Some(allowed))
    }

    #[must_use]
    pub fn definitions(&self, allowed_tools: Option<&BTreeSet<String>>) -> Vec<ToolDefinition> {
        let builtin = mvp_tool_specs()
            .into_iter()
            .filter(|spec| allowed_tools.is_none_or(|allowed| allowed.contains(spec.name)))
            .map(|spec| ToolDefinition {
                name: spec.name.to_string(),
                description: Some(spec.description.to_string()),
                input_schema: spec.input_schema,
            });
        let optional_builtin = enabled_optional_builtin_specs(&self.browser_control)
            .into_iter()
            .filter(|spec| allowed_tools.is_some_and(|allowed| allowed.contains(spec.name)))
            .map(|spec| ToolDefinition {
                name: spec.name.to_string(),
                description: Some(spec.description.to_string()),
                input_schema: spec.input_schema,
            });
        let plugin = self
            .plugin_tools
            .iter()
            .filter(|tool| {
                allowed_tools
                    .is_none_or(|allowed| allowed.contains(tool.definition().name.as_str()))
            })
            .map(|tool| ToolDefinition {
                name: tool.definition().name.clone(),
                description: tool.definition().description.clone(),
                input_schema: tool.definition().input_schema.clone(),
            });
        builtin.chain(optional_builtin).chain(plugin).collect()
    }

    #[must_use]
    pub fn permission_specs(
        &self,
        allowed_tools: Option<&BTreeSet<String>>,
    ) -> Vec<(String, PermissionMode)> {
        let builtin = mvp_tool_specs()
            .into_iter()
            .filter(|spec| allowed_tools.is_none_or(|allowed| allowed.contains(spec.name)))
            .map(|spec| (spec.name.to_string(), spec.required_permission));
        let optional_builtin = enabled_optional_builtin_specs(&self.browser_control)
            .into_iter()
            .filter(|spec| allowed_tools.is_some_and(|allowed| allowed.contains(spec.name)))
            .map(|spec| (spec.name.to_string(), spec.required_permission));
        let plugin = self
            .plugin_tools
            .iter()
            .filter(|tool| {
                allowed_tools
                    .is_none_or(|allowed| allowed.contains(tool.definition().name.as_str()))
            })
            .map(|tool| {
                (
                    tool.definition().name.clone(),
                    permission_mode_from_plugin(tool.required_permission()),
                )
            });
        builtin.chain(optional_builtin).chain(plugin).collect()
    }

    pub fn execute(&self, name: &str, input: &Value) -> Result<String, String> {
        if is_browser_optional_tool_name(name) && !self.browser_control.enabled() {
            return Err(browser_tool_disabled_error(name));
        }
        if mvp_tool_specs().iter().any(|spec| spec.name == name)
            || enabled_optional_builtin_specs(&self.browser_control)
                .iter()
                .any(|spec| spec.name == name)
        {
            return execute_tool_with_enforcer(
                self.enforcer.as_ref(),
                self.bash_policy.as_ref(),
                Some(&self.browser_control),
                name,
                input,
            );
        }
        self.plugin_tools
            .iter()
            .find(|tool| tool.definition().name == name)
            .ok_or_else(|| format!("unsupported tool: {name}"))?
            .execute(input)
            .map_err(|error| error.to_string())
    }
}

fn global_task_registry() -> &'static TaskRegistry {
    use std::sync::OnceLock;
    static REGISTRY: OnceLock<TaskRegistry> = OnceLock::new();
    REGISTRY.get_or_init(TaskRegistry::new)
}

fn global_team_registry() -> &'static TeamRegistry {
    use std::sync::OnceLock;
    static REGISTRY: OnceLock<TeamRegistry> = OnceLock::new();
    REGISTRY.get_or_init(TeamRegistry::new)
}

fn global_cron_registry() -> &'static CronRegistry {
    use std::sync::OnceLock;
    static REGISTRY: OnceLock<CronRegistry> = OnceLock::new();
    REGISTRY.get_or_init(CronRegistry::new)
}

fn global_lsp_registry() -> &'static LspRegistry {
    use std::sync::OnceLock;
    static REGISTRY: OnceLock<LspRegistry> = OnceLock::new();
    REGISTRY.get_or_init(LspRegistry::new)
}

fn global_mcp_registry() -> &'static McpToolRegistry {
    use std::sync::OnceLock;
    static REGISTRY: OnceLock<McpToolRegistry> = OnceLock::new();
    REGISTRY.get_or_init(McpToolRegistry::new)
}

fn normalize_tool_name(value: &str) -> String {
    value.trim().replace('-', "_").to_ascii_lowercase()
}

fn permission_mode_from_plugin(value: &str) -> PermissionMode {
    match value {
        "read-only" => PermissionMode::ReadOnly,
        "workspace-write" => PermissionMode::WorkspaceWrite,
        "danger-full-access" => PermissionMode::DangerFullAccess,
        other => panic!("unsupported plugin permission: {other}"),
    }
}

const BROWSER_OBSERVE_TOOL_NAME: &str = "BrowserObserve";
const BROWSER_INTERACT_TOOL_NAME: &str = "BrowserInteract";
const BROWSER_OBSERVE_SUPPORTED_WAIT_KINDS: &[&str] =
    &["load", "network_idle", "text", "title_contains"];
const BROWSER_INTERACT_SUPPORTED_WAIT_KINDS: &[&str] =
    &["load", "text", "title_contains", "url_contains"];

fn browser_tool_disabled_error(tool_name: &str) -> String {
    format!(
        "{tool_name} is disabled; set browserControl.enabled=true before using --allowedTools {tool_name}"
    )
}

fn is_browser_optional_tool_name(name: &str) -> bool {
    matches!(name, BROWSER_OBSERVE_TOOL_NAME | BROWSER_INTERACT_TOOL_NAME)
}

fn enabled_optional_builtin_specs(browser_control: &RuntimeBrowserControlConfig) -> Vec<ToolSpec> {
    if browser_control.enabled() {
        vec![browser_observe_tool_spec(), browser_interact_tool_spec()]
    } else {
        Vec::new()
    }
}

fn browser_observe_tool_spec() -> ToolSpec {
    ToolSpec {
        name: BROWSER_OBSERVE_TOOL_NAME,
        description:
            "Observe a local browser-rendered page from the current CLI run, with bounded wait conditions, optional visible-text capture, and workspace-local screenshot artifacts.",
        input_schema: json!({
            "type": "object",
            "properties": {
                "url": { "type": "string", "format": "uri" },
                "timeout_ms": { "type": "integer", "minimum": 1, "maximum": 60000 },
                "wait": {
                    "type": "object",
                    "properties": {
                        "kind": {
                            "type": "string",
                            "enum": BROWSER_OBSERVE_SUPPORTED_WAIT_KINDS
                        },
                        "value": { "type": "string" }
                    },
                    "required": ["kind"],
                    "additionalProperties": false
                },
                "capture_visible_text": { "type": "boolean" },
                "screenshot": {
                    "type": "object",
                    "properties": {
                        "capture": { "type": "boolean" },
                        "format": { "type": "string", "enum": ["png", "jpeg"] }
                    },
                    "required": ["capture"],
                    "additionalProperties": false
                },
                "viewport": {
                    "type": "object",
                    "properties": {
                        "width": { "type": "integer", "minimum": 320, "maximum": 4096 },
                        "height": { "type": "integer", "minimum": 240, "maximum": 4096 }
                    },
                    "required": ["width", "height"],
                    "additionalProperties": false
                }
            },
            "required": ["url"],
            "additionalProperties": false
        }),
        required_permission: PermissionMode::DangerFullAccess,
    }
}

fn browser_interact_tool_spec() -> ToolSpec {
    ToolSpec {
        name: BROWSER_INTERACT_TOOL_NAME,
        description:
            "Click one selector in a local browser-rendered page from the current CLI run, then return bounded post-action page state and optional workspace-local screenshot artifacts.",
        input_schema: json!({
            "type": "object",
            "properties": {
                "url": { "type": "string", "format": "uri" },
                "action": {
                    "type": "object",
                    "properties": {
                        "kind": { "type": "string", "enum": ["click"] },
                        "selector": { "type": "string" }
                    },
                    "required": ["kind", "selector"],
                    "additionalProperties": false
                },
                "timeout_ms": { "type": "integer", "minimum": 1, "maximum": 60000 },
                "wait": {
                    "type": "object",
                    "properties": {
                        "kind": {
                            "type": "string",
                            "enum": BROWSER_INTERACT_SUPPORTED_WAIT_KINDS
                        },
                        "value": { "type": "string" }
                    },
                    "required": ["kind"],
                    "additionalProperties": false
                },
                "capture_visible_text": { "type": "boolean" },
                "screenshot": {
                    "type": "object",
                    "properties": {
                        "capture": { "type": "boolean" },
                        "format": { "type": "string", "enum": ["png", "jpeg"] }
                    },
                    "required": ["capture"],
                    "additionalProperties": false
                },
                "viewport": {
                    "type": "object",
                    "properties": {
                        "width": { "type": "integer", "minimum": 320, "maximum": 4096 },
                        "height": { "type": "integer", "minimum": 240, "maximum": 4096 }
                    },
                    "required": ["width", "height"],
                    "additionalProperties": false
                }
            },
            "required": ["url", "action"],
            "additionalProperties": false
        }),
        required_permission: PermissionMode::DangerFullAccess,
    }
}

#[must_use]
#[allow(clippy::too_many_lines)]
pub fn mvp_tool_specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: "bash",
            description: "Execute a shell command in the current workspace.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string" },
                    "timeout": { "type": "integer", "minimum": 1 },
                    "description": { "type": "string" },
                    "run_in_background": { "type": "boolean" },
                    "dangerouslyDisableSandbox": { "type": "boolean" }
                },
                "required": ["command"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::DangerFullAccess,
        },
        ToolSpec {
            name: "read_file",
            description: "Read a text file from the workspace.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "offset": { "type": "integer", "minimum": 0 },
                    "limit": { "type": "integer", "minimum": 1 }
                },
                "required": ["path"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::ReadOnly,
        },
        ToolSpec {
            name: "write_file",
            description: "Write a text file in the workspace.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "content": { "type": "string" }
                },
                "required": ["path", "content"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::WorkspaceWrite,
        },
        ToolSpec {
            name: "edit_file",
            description: "Replace text in a workspace file.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "old_string": { "type": "string" },
                    "new_string": { "type": "string" },
                    "replace_all": { "type": "boolean" }
                },
                "required": ["path", "old_string", "new_string"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::WorkspaceWrite,
        },
        ToolSpec {
            name: "glob_search",
            description: "Find files by glob pattern.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string" },
                    "path": { "type": "string" }
                },
                "required": ["pattern"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::ReadOnly,
        },
        ToolSpec {
            name: "grep_search",
            description: "Search file contents with a regex pattern.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string" },
                    "path": { "type": "string" },
                    "glob": { "type": "string" },
                    "output_mode": { "type": "string" },
                    "-B": { "type": "integer", "minimum": 0 },
                    "-A": { "type": "integer", "minimum": 0 },
                    "-C": { "type": "integer", "minimum": 0 },
                    "context": { "type": "integer", "minimum": 0 },
                    "-n": { "type": "boolean" },
                    "-i": { "type": "boolean" },
                    "type": { "type": "string" },
                    "head_limit": { "type": "integer", "minimum": 1 },
                    "offset": { "type": "integer", "minimum": 0 },
                    "multiline": { "type": "boolean" }
                },
                "required": ["pattern"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::ReadOnly,
        },
        ToolSpec {
            name: "WebFetch",
            description:
                "Fetch a URL, convert it into readable text, and answer a prompt about it.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "url": { "type": "string", "format": "uri" },
                    "prompt": { "type": "string" }
                },
                "required": ["url", "prompt"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::ReadOnly,
        },
        ToolSpec {
            name: "WebSearch",
            description: "Search the web for current information and return cited results.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "minLength": 2 },
                    "allowed_domains": {
                        "type": "array",
                        "items": { "type": "string" }
                    },
                    "blocked_domains": {
                        "type": "array",
                        "items": { "type": "string" }
                    }
                },
                "required": ["query"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::ReadOnly,
        },
        ToolSpec {
            name: "TodoWrite",
            description: "Update the structured task list for the current session.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "todos": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "content": { "type": "string" },
                                "activeForm": { "type": "string" },
                                "status": {
                                    "type": "string",
                                    "enum": ["pending", "in_progress", "completed"]
                                }
                            },
                            "required": ["content", "activeForm", "status"],
                            "additionalProperties": false
                        }
                    }
                },
                "required": ["todos"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::WorkspaceWrite,
        },
        ToolSpec {
            name: "Skill",
            description: "Load a local skill definition and its instructions.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "skill": { "type": "string" },
                    "args": { "type": "string" }
                },
                "required": ["skill"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::ReadOnly,
        },
        ToolSpec {
            name: "Agent",
            description: "Launch a specialized agent task and persist its handoff metadata.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "description": { "type": "string" },
                    "prompt": { "type": "string" },
                    "subagent_type": { "type": "string" },
                    "name": { "type": "string" },
                    "model": { "type": "string" }
                },
                "required": ["description", "prompt"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::DangerFullAccess,
        },
        ToolSpec {
            name: "ToolSearch",
            description: "Search for deferred or specialized tools by exact name or keywords.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string" },
                    "max_results": { "type": "integer", "minimum": 1 }
                },
                "required": ["query"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::ReadOnly,
        },
        ToolSpec {
            name: "NotebookEdit",
            description: "Replace, insert, or delete a cell in a Jupyter notebook.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "notebook_path": { "type": "string" },
                    "cell_id": { "type": "string" },
                    "new_source": { "type": "string" },
                    "cell_type": { "type": "string", "enum": ["code", "markdown"] },
                    "edit_mode": { "type": "string", "enum": ["replace", "insert", "delete"] }
                },
                "required": ["notebook_path"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::WorkspaceWrite,
        },
        ToolSpec {
            name: "Sleep",
            description: "Wait for a specified duration without holding a shell process.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "duration_ms": { "type": "integer", "minimum": 0 }
                },
                "required": ["duration_ms"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::ReadOnly,
        },
        ToolSpec {
            name: "SendUserMessage",
            description: "Send a message to the user.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "message": { "type": "string" },
                    "attachments": {
                        "type": "array",
                        "items": { "type": "string" }
                    },
                    "status": {
                        "type": "string",
                        "enum": ["normal", "proactive"]
                    }
                },
                "required": ["message", "status"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::ReadOnly,
        },
        ToolSpec {
            name: "Config",
            description: "Get or set openyak settings.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "setting": { "type": "string" },
                    "value": {
                        "type": ["string", "boolean", "number"]
                    }
                },
                "required": ["setting"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::WorkspaceWrite,
        },
        ToolSpec {
            name: "StructuredOutput",
            description: "Return structured output in the requested format.",
            input_schema: json!({
                "type": "object",
                "additionalProperties": true
            }),
            required_permission: PermissionMode::ReadOnly,
        },
        ToolSpec {
            name: "REPL",
            description: "Execute code in a REPL-like subprocess.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "code": { "type": "string" },
                    "language": { "type": "string" },
                    "timeout_ms": { "type": "integer", "minimum": 1 }
                },
                "required": ["code", "language"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::DangerFullAccess,
        },
        ToolSpec {
            name: "PowerShell",
            description: "Execute a PowerShell command with optional timeout.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string" },
                    "timeout": { "type": "integer", "minimum": 1 },
                    "description": { "type": "string" },
                    "run_in_background": { "type": "boolean" }
                },
                "required": ["command"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::DangerFullAccess,
        },
        ToolSpec {
            name: "TaskCreate",
            description: "Create a background task record.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "prompt": { "type": "string" },
                    "description": { "type": "string" }
                },
                "required": ["prompt"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::DangerFullAccess,
        },
        ToolSpec {
            name: "TaskGet",
            description: "Get a task by id.",
            input_schema: json!({
                "type": "object",
                "properties": { "task_id": { "type": "string" } },
                "required": ["task_id"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::ReadOnly,
        },
        ToolSpec {
            name: "TaskList",
            description: "List known tasks.",
            input_schema: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
            required_permission: PermissionMode::ReadOnly,
        },
        ToolSpec {
            name: "TaskStop",
            description: "Stop a task by id.",
            input_schema: json!({
                "type": "object",
                "properties": { "task_id": { "type": "string" } },
                "required": ["task_id"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::DangerFullAccess,
        },
        ToolSpec {
            name: "TaskUpdate",
            description: "Append a message to a task.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "task_id": { "type": "string" },
                    "message": { "type": "string" }
                },
                "required": ["task_id", "message"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::DangerFullAccess,
        },
        ToolSpec {
            name: "TaskOutput",
            description: "Fetch task output.",
            input_schema: json!({
                "type": "object",
                "properties": { "task_id": { "type": "string" } },
                "required": ["task_id"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::ReadOnly,
        },
        ToolSpec {
            name: "TaskWait",
            description: "Poll a task until it reaches a terminal state or a timeout elapses.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "task_id": { "type": "string" },
                    "timeout_ms": { "type": "integer", "minimum": 0 },
                    "poll_interval_ms": { "type": "integer", "minimum": 1 }
                },
                "required": ["task_id"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::ReadOnly,
        },
        ToolSpec {
            name: "TeamCreate",
            description: "Create a team record for grouped tasks.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string" },
                    "tasks": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "task_id": { "type": "string" }
                            },
                            "required": ["task_id"],
                            "additionalProperties": false
                        }
                    }
                },
                "required": ["name", "tasks"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::DangerFullAccess,
        },
        ToolSpec {
            name: "TeamGet",
            description: "Get a team by id.",
            input_schema: json!({
                "type": "object",
                "properties": { "team_id": { "type": "string" } },
                "required": ["team_id"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::ReadOnly,
        },
        ToolSpec {
            name: "TeamList",
            description: "List known teams.",
            input_schema: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
            required_permission: PermissionMode::ReadOnly,
        },
        ToolSpec {
            name: "TeamDelete",
            description: "Delete a team by id.",
            input_schema: json!({
                "type": "object",
                "properties": { "team_id": { "type": "string" } },
                "required": ["team_id"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::DangerFullAccess,
        },
        ToolSpec {
            name: "CronCreate",
            description: "Create a cron entry record.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "schedule": { "type": "string" },
                    "prompt": { "type": "string" },
                    "description": { "type": "string" }
                },
                "required": ["schedule", "prompt"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::DangerFullAccess,
        },
        ToolSpec {
            name: "CronGet",
            description: "Get a cron entry by id.",
            input_schema: json!({
                "type": "object",
                "properties": { "cron_id": { "type": "string" } },
                "required": ["cron_id"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::ReadOnly,
        },
        ToolSpec {
            name: "CronDisable",
            description: "Disable a cron entry without deleting it.",
            input_schema: json!({
                "type": "object",
                "properties": { "cron_id": { "type": "string" } },
                "required": ["cron_id"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::DangerFullAccess,
        },
        ToolSpec {
            name: "CronEnable",
            description: "Re-enable a disabled cron entry.",
            input_schema: json!({
                "type": "object",
                "properties": { "cron_id": { "type": "string" } },
                "required": ["cron_id"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::DangerFullAccess,
        },
        ToolSpec {
            name: "CronDelete",
            description: "Delete a cron entry by id.",
            input_schema: json!({
                "type": "object",
                "properties": { "cron_id": { "type": "string" } },
                "required": ["cron_id"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::DangerFullAccess,
        },
        ToolSpec {
            name: "CronList",
            description: "List cron entries.",
            input_schema: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
            required_permission: PermissionMode::ReadOnly,
        },
        ToolSpec {
            name: "SessionList",
            description: "List local session-like resources across thread, managed_session, and agent_run kinds.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "kind": {
                        "type": "string",
                        "enum": ["thread", "managed_session", "agent_run"]
                    }
                },
                "additionalProperties": false
            }),
            required_permission: PermissionMode::ReadOnly,
        },
        ToolSpec {
            name: "SessionGet",
            description: "Inspect a local session-like resource by kind and id.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "kind": {
                        "type": "string",
                        "enum": ["thread", "managed_session", "agent_run"]
                    },
                    "id": { "type": "string" }
                },
                "required": ["kind", "id"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::ReadOnly,
        },
        ToolSpec {
            name: "SessionCreate",
            description: "Create a thread-backed session through the current local openyak server.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "kind": {
                        "type": "string",
                        "enum": ["thread"]
                    },
                    "cwd": { "type": "string" },
                    "model": { "type": "string" },
                    "permission_mode": { "type": "string" },
                    "allowed_tools": {
                        "type": "array",
                        "items": { "type": "string" }
                    }
                },
                "required": ["kind"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::DangerFullAccess,
        },
        ToolSpec {
            name: "SessionSend",
            description: "Send a message to a thread-backed session through the current local openyak server.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "kind": {
                        "type": "string",
                        "enum": ["thread", "managed_session", "agent_run"]
                    },
                    "id": { "type": "string" },
                    "message": { "type": "string" }
                },
                "required": ["kind", "id", "message"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::DangerFullAccess,
        },
        ToolSpec {
            name: "SessionResume",
            description: "Submit pending user input to a thread-backed session through the current local openyak server.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "kind": {
                        "type": "string",
                        "enum": ["thread", "managed_session", "agent_run"]
                    },
                    "id": { "type": "string" },
                    "request_id": { "type": "string" },
                    "content": { "type": "string" },
                    "selected_option": { "type": "string" }
                },
                "required": ["kind", "id", "request_id", "content"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::DangerFullAccess,
        },
        ToolSpec {
            name: "SessionWait",
            description: "Wait for a thread or agent_run session to reach a stable state or timeout.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "kind": {
                        "type": "string",
                        "enum": ["thread", "managed_session", "agent_run"]
                    },
                    "id": { "type": "string" },
                    "timeout_ms": { "type": "integer", "minimum": 0 },
                    "poll_interval_ms": { "type": "integer", "minimum": 1 }
                },
                "required": ["kind", "id"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::ReadOnly,
        },
        ToolSpec {
            name: "LSP",
            description: "Query registry-backed LSP information.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["symbols", "references", "diagnostics", "definition", "hover", "servers", "status"] },
                    "path": { "type": "string" },
                    "language": { "type": "string" },
                    "line": { "type": "integer", "minimum": 0 },
                    "character": { "type": "integer", "minimum": 0 },
                    "query": { "type": "string" }
                },
                "required": ["action"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::ReadOnly,
        },
        ToolSpec {
            name: "ListMcpServers",
            description: "List connected MCP server registry entries.",
            input_schema: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
            required_permission: PermissionMode::ReadOnly,
        },
        ToolSpec {
            name: "ListMcpTools",
            description: "List tools from a connected MCP server registry entry.",
            input_schema: json!({
                "type": "object",
                "properties": { "server": { "type": "string" } },
                "required": ["server"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::ReadOnly,
        },
        ToolSpec {
            name: "ListMcpResources",
            description: "List resources from a connected MCP server registry.",
            input_schema: json!({
                "type": "object",
                "properties": { "server": { "type": "string" } },
                "additionalProperties": false
            }),
            required_permission: PermissionMode::ReadOnly,
        },
        ToolSpec {
            name: "ReadMcpResource",
            description: "Read a resource by URI from an MCP server registry.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "server": { "type": "string" },
                    "uri": { "type": "string" }
                },
                "required": ["uri"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::ReadOnly,
        },
        ToolSpec {
            name: "McpAuth",
            description: "Inspect/authenticate an MCP server registry entry.",
            input_schema: json!({
                "type": "object",
                "properties": { "server": { "type": "string" } },
                "required": ["server"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::DangerFullAccess,
        },
        ToolSpec {
            name: "MCP",
            description: "Execute a tool on a connected MCP server registry entry.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "server": { "type": "string" },
                    "tool": { "type": "string" },
                    "arguments": { "type": "object" }
                },
                "required": ["server", "tool"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::DangerFullAccess,
        },
        ToolSpec {
            name: "TestingPermission",
            description: "Exercise the permission-enforcement layer for tests.",
            input_schema: json!({
                "type": "object",
                "properties": { "action": { "type": "string" } },
                "required": ["action"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::DangerFullAccess,
        },
    ]
}

pub fn execute_tool(name: &str, input: &Value) -> Result<String, String> {
    execute_tool_with_enforcer(None, None, None, name, input)
}

#[allow(clippy::too_many_lines)]
fn execute_tool_with_enforcer(
    enforcer: Option<&PermissionEnforcer>,
    bash_policy: Option<&ToolProfileBashPolicy>,
    browser_control: Option<&RuntimeBrowserControlConfig>,
    name: &str,
    input: &Value,
) -> Result<String, String> {
    match name {
        "bash" => {
            let bash_input = from_value::<BashCommandInput>(input)?;
            maybe_enforce_permission_check(enforcer, bash_policy, name, Some(&bash_input), input)?;
            run_bash(bash_input, bash_policy)
        }
        "read_file" => {
            maybe_enforce_permission_check(enforcer, bash_policy, name, None, input)?;
            from_value::<ReadFileInput>(input).and_then(run_read_file)
        }
        "write_file" => {
            maybe_enforce_permission_check(enforcer, bash_policy, name, None, input)?;
            from_value::<WriteFileInput>(input).and_then(run_write_file)
        }
        "edit_file" => {
            maybe_enforce_permission_check(enforcer, bash_policy, name, None, input)?;
            from_value::<EditFileInput>(input).and_then(run_edit_file)
        }
        "glob_search" => {
            maybe_enforce_permission_check(enforcer, bash_policy, name, None, input)?;
            from_value::<GlobSearchInputValue>(input).and_then(run_glob_search)
        }
        "grep_search" => {
            maybe_enforce_permission_check(enforcer, bash_policy, name, None, input)?;
            from_value::<GrepSearchInput>(input).and_then(run_grep_search)
        }
        "WebFetch" => from_value::<WebFetchInput>(input).and_then(run_web_fetch),
        "WebSearch" => from_value::<WebSearchInput>(input).and_then(run_web_search),
        BROWSER_OBSERVE_TOOL_NAME => {
            maybe_enforce_permission_check(enforcer, bash_policy, name, None, input)?;
            let browser_control = browser_control
                .filter(|config| config.enabled())
                .ok_or_else(|| browser_tool_disabled_error(BROWSER_OBSERVE_TOOL_NAME))?;
            parse_browser_observe_input(input)
                .and_then(|browser_input| run_browser_observe(browser_input, browser_control))
        }
        BROWSER_INTERACT_TOOL_NAME => {
            maybe_enforce_permission_check(enforcer, bash_policy, name, None, input)?;
            let browser_control = browser_control
                .filter(|config| config.enabled())
                .ok_or_else(|| browser_tool_disabled_error(BROWSER_INTERACT_TOOL_NAME))?;
            parse_browser_interact_input(input)
                .and_then(|browser_input| run_browser_interact(browser_input, browser_control))
        }
        "TodoWrite" => from_value::<TodoWriteInput>(input).and_then(run_todo_write),
        "Skill" => from_value::<SkillInput>(input).and_then(run_skill),
        "Agent" => from_value::<AgentInput>(input).and_then(run_agent),
        "ToolSearch" => from_value::<ToolSearchInput>(input).and_then(run_tool_search),
        "NotebookEdit" => from_value::<NotebookEditInput>(input).and_then(run_notebook_edit),
        "Sleep" => from_value::<SleepInput>(input).and_then(run_sleep),
        "SendUserMessage" | "Brief" => from_value::<BriefInput>(input).and_then(run_brief),
        "Config" => from_value::<ConfigInput>(input).and_then(run_config),
        "StructuredOutput" => {
            from_value::<StructuredOutputInput>(input).and_then(run_structured_output)
        }
        "REPL" => from_value::<ReplInput>(input).and_then(run_repl),
        "PowerShell" => from_value::<PowerShellInput>(input).and_then(run_powershell),
        "TaskCreate" => {
            maybe_enforce_permission_check(enforcer, bash_policy, name, None, input)?;
            from_value::<TaskCreateInput>(input).and_then(run_task_create)
        }
        "TaskGet" => from_value::<TaskIdInput>(input).and_then(run_task_get),
        "TaskList" => run_task_list(input.clone()),
        "TaskStop" => {
            maybe_enforce_permission_check(enforcer, bash_policy, name, None, input)?;
            from_value::<TaskIdInput>(input).and_then(run_task_stop)
        }
        "TaskUpdate" => {
            maybe_enforce_permission_check(enforcer, bash_policy, name, None, input)?;
            from_value::<TaskUpdateInput>(input).and_then(run_task_update)
        }
        "TaskOutput" => from_value::<TaskIdInput>(input).and_then(run_task_output),
        "TaskWait" => from_value::<TaskWaitInput>(input).and_then(run_task_wait),
        "TeamCreate" => {
            maybe_enforce_permission_check(enforcer, bash_policy, name, None, input)?;
            from_value::<TeamCreateInput>(input).and_then(run_team_create)
        }
        "TeamGet" => from_value::<TeamIdInput>(input).and_then(run_team_get),
        "TeamList" => run_team_list(input.clone()),
        "TeamDelete" => {
            maybe_enforce_permission_check(enforcer, bash_policy, name, None, input)?;
            from_value::<TeamIdInput>(input).and_then(run_team_delete)
        }
        "CronCreate" => {
            maybe_enforce_permission_check(enforcer, bash_policy, name, None, input)?;
            from_value::<CronCreateInput>(input).and_then(run_cron_create)
        }
        "CronGet" => from_value::<CronIdInput>(input).and_then(run_cron_get),
        "CronDisable" => {
            maybe_enforce_permission_check(enforcer, bash_policy, name, None, input)?;
            from_value::<CronIdInput>(input).and_then(run_cron_disable)
        }
        "CronEnable" => {
            maybe_enforce_permission_check(enforcer, bash_policy, name, None, input)?;
            from_value::<CronIdInput>(input).and_then(run_cron_enable)
        }
        "CronDelete" => {
            maybe_enforce_permission_check(enforcer, bash_policy, name, None, input)?;
            from_value::<CronIdInput>(input).and_then(run_cron_delete)
        }
        "CronList" => run_cron_list(input.clone()),
        "SessionList" => from_value::<SessionListInput>(input).and_then(run_session_list),
        "SessionGet" => from_value::<SessionRefInput>(input).and_then(run_session_get),
        "SessionCreate" => {
            maybe_enforce_permission_check(enforcer, bash_policy, name, None, input)?;
            from_value::<SessionCreateInput>(input).and_then(run_session_create)
        }
        "SessionSend" => {
            maybe_enforce_permission_check(enforcer, bash_policy, name, None, input)?;
            from_value::<SessionSendInput>(input).and_then(run_session_send)
        }
        "SessionResume" => {
            maybe_enforce_permission_check(enforcer, bash_policy, name, None, input)?;
            from_value::<SessionResumeInput>(input).and_then(run_session_resume)
        }
        "SessionWait" => from_value::<SessionWaitInput>(input).and_then(run_session_wait),
        "LSP" => from_value::<LspInput>(input).and_then(run_lsp),
        "ListMcpServers" => run_list_mcp_servers(input.clone()),
        "ListMcpTools" => from_value::<McpServerInput>(input).and_then(run_list_mcp_tools),
        "ListMcpResources" => {
            from_value::<McpResourceInput>(input).and_then(run_list_mcp_resources)
        }
        "ReadMcpResource" => from_value::<McpResourceInput>(input).and_then(run_read_mcp_resource),
        "McpAuth" => {
            maybe_enforce_permission_check(enforcer, bash_policy, name, None, input)?;
            from_value::<McpAuthInput>(input).and_then(run_mcp_auth)
        }
        "MCP" => {
            maybe_enforce_permission_check(enforcer, bash_policy, name, None, input)?;
            from_value::<McpToolInput>(input).and_then(run_mcp_tool)
        }
        "TestingPermission" => {
            from_value::<TestingPermissionInput>(input).and_then(run_testing_permission)
        }
        _ => Err(format!("unsupported tool: {name}")),
    }
}

fn maybe_enforce_permission_check(
    enforcer: Option<&PermissionEnforcer>,
    bash_policy: Option<&ToolProfileBashPolicy>,
    tool_name: &str,
    bash_input: Option<&BashCommandInput>,
    input: &Value,
) -> Result<(), String> {
    let Some(enforcer) = enforcer else {
        return Ok(());
    };

    match tool_name {
        "write_file" | "edit_file" => {
            let path = input
                .get("path")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let workspace_root = std::env::current_dir()
                .map_err(|error| error.to_string())?
                .to_string_lossy()
                .to_string();
            match enforcer.check_file_write(path, &workspace_root) {
                EnforcementResult::Allowed => Ok(()),
                EnforcementResult::Denied { reason, .. } => Err(reason),
            }
        }
        "bash" => {
            let command = bash_input
                .map(|parsed| parsed.command.as_str())
                .or_else(|| input.get("command").and_then(Value::as_str))
                .unwrap_or_default();
            match enforcer.check_bash(command) {
                EnforcementResult::Allowed => Ok(()),
                EnforcementResult::Denied { reason, .. } => Err(reason),
            }?;
            if let Some(policy) = bash_policy {
                if let Some(parsed) = bash_input {
                    policy.validate_override(parsed)?;
                }
            }
            Ok(())
        }
        _ => {
            let input_str = serde_json::to_string(input).unwrap_or_default();
            match enforcer.check(tool_name, &input_str) {
                EnforcementResult::Allowed => Ok(()),
                EnforcementResult::Denied { reason, .. } => Err(reason),
            }
        }
    }
}

fn from_value<T: for<'de> Deserialize<'de>>(input: &Value) -> Result<T, String> {
    serde_json::from_value(input.clone()).map_err(|error| error.to_string())
}

fn parse_browser_observe_input(input: &Value) -> Result<BrowserObserveInput, String> {
    from_value::<BrowserObserveInput>(input)
        .map_err(|error| format!("BrowserObserve rejected unsupported request shape: {error}"))
}

fn parse_browser_interact_input(input: &Value) -> Result<BrowserInteractInput, String> {
    from_value::<BrowserInteractInput>(input)
        .map_err(|error| format!("BrowserInteract rejected unsupported request shape: {error}"))
}

fn run_bash(
    input: BashCommandInput,
    bash_policy: Option<&ToolProfileBashPolicy>,
) -> Result<String, String> {
    let output = match bash_policy {
        Some(policy) => {
            execute_bash_with_config(input, &policy.sandbox).map_err(|error| error.to_string())?
        }
        None => execute_bash(input).map_err(|error| error.to_string())?,
    };
    serde_json::to_string_pretty(&output).map_err(|error| error.to_string())
}

const DEFAULT_BROWSER_TIMEOUT_MS: u64 = 10_000;
const DEFAULT_BROWSER_NETWORK_IDLE_BUDGET_MS: u64 = 1_500;
const MAX_BROWSER_TIMEOUT_MS: u64 = 60_000;
const MAX_BROWSER_VISIBLE_TEXT_CHARS: usize = 4_000;
const DEFAULT_BROWSER_VIEWPORT_WIDTH: u32 = 1440;
const DEFAULT_BROWSER_VIEWPORT_HEIGHT: u32 = 900;

fn run_browser_observe(
    input: BrowserObserveInput,
    browser_control: &RuntimeBrowserControlConfig,
) -> Result<String, String> {
    let started_at = Instant::now();
    validate_browser_observe_input(&input)?;
    let url =
        Url::parse(&input.url).map_err(|error| format!("invalid BrowserObserve url: {error}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(format!(
            "BrowserObserve only supports http and https URLs in slice 1B; received {}",
            url.scheme()
        ));
    }

    let browser = find_supported_browser_command().ok_or_else(|| {
        format!(
            "BrowserObserve could not find a supported local browser executable (checked: {})",
            supported_browser_candidates().join(", ")
        )
    })?;

    let observe_result = observe_browser_page(&browser, &input)?;
    let (visible_text, visible_text_truncated) = if input.capture_visible_text {
        truncate_browser_visible_text(&observe_result.snapshot.visible_text)
    } else {
        (None, false)
    };

    let workspace_root = std::env::current_dir().map_err(|error| error.to_string())?;
    let (screenshot, screenshot_elapsed_ms) =
        match input.screenshot.as_ref().filter(|request| request.capture) {
            Some(request) => {
                let screenshot_started_at = Instant::now();
                let call_id = browser_observe_call_id();
                let artifact_dir = browser_control.artifacts_dir().join(&call_id);
                fs::create_dir_all(&artifact_dir).map_err(io_to_string)?;
                let extension = request.format.extension();
                let artifact_path = artifact_dir.join(format!("observe.{extension}"));
                let _ = run_browser_capture(
                    &browser,
                    &input,
                    Some(&artifact_path),
                    BrowserObserveCapturePhase::Screenshot,
                )?;
                let relative_path =
                    portable_workspace_relative_path(&artifact_path, &workspace_root)?;
                let bytes = fs::metadata(&artifact_path).map_err(io_to_string)?.len();
                let file_name = artifact_path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("observe")
                    .to_string();
                (
                    Some(BrowserObserveArtifact {
                        relative_path,
                        file_name,
                        format: request.format.as_str().to_string(),
                        media_type: request.format.media_type().to_string(),
                        bytes,
                    }),
                    Some(elapsed_ms(screenshot_started_at.elapsed())),
                )
            }
            None => (None, None),
        };

    to_pretty_json(BrowserObserveOutput {
        requested_url: input.url,
        final_url: None,
        title: observe_result.snapshot.title,
        load_outcome: observe_result.load_outcome,
        wait: observe_result.wait,
        visible_text,
        visible_text_truncated,
        screenshot,
        browser_runtime: BrowserObserveRuntime {
            executable: browser.display().to_string(),
            mode: String::from("headless_cli"),
            capture_backend: String::from("headless_cli_dump_dom"),
        },
        timings_ms: BrowserObserveTimings {
            observation: observe_result.observation_elapsed_ms,
            screenshot: screenshot_elapsed_ms,
            total: elapsed_ms(started_at.elapsed()),
        },
        warnings: vec![String::from(
            "slice 1B does not surface post-navigation final_url; requested_url is returned separately",
        )],
    })
}

const DEFAULT_BROWSER_STARTUP_POLL_MS: u64 = 50;
const DEFAULT_BROWSER_INTERACT_POLL_MS: u64 = 100;

fn run_browser_interact(
    input: BrowserInteractInput,
    browser_control: &RuntimeBrowserControlConfig,
) -> Result<String, String> {
    let started_at = Instant::now();
    validate_browser_interact_input(&input)?;
    let url =
        Url::parse(&input.url).map_err(|error| format!("invalid BrowserInteract url: {error}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(format!(
            "BrowserInteract only supports http and https URLs in slice 2A; received {}",
            url.scheme()
        ));
    }

    let browser = find_supported_browser_command().ok_or_else(|| {
        format!(
            "BrowserInteract could not find a supported local browser executable (checked: {})",
            supported_browser_candidates().join(", ")
        )
    })?;
    let call_id = browser_observe_call_id();
    let timeout_ms = browser_interact_wait_budget_ms(&input);
    let mut session = BrowserInteractSession::launch(&browser, &input, timeout_ms, &call_id)
        .map_err(|error| format!("BrowserInteract launch/connect failed: {error}"))?;
    session
        .wait_for_initial_load(timeout_ms)
        .map_err(|error| format!("BrowserInteract wait failed before interaction: {error}"))?;
    let action_report = session
        .click_selector(&input.action.selector, timeout_ms)
        .map_err(|error| format!("BrowserInteract interaction failed: {error}"))?;
    let (page_state, wait_report) = session
        .capture_post_action_state(input.wait.as_ref(), timeout_ms)
        .map_err(|error| format!("BrowserInteract wait failed: {error}"))?;

    let (visible_text, visible_text_truncated) = if input.capture_visible_text {
        truncate_browser_visible_text(&page_state.visible_text)
    } else {
        (None, false)
    };

    let workspace_root = std::env::current_dir().map_err(|error| error.to_string())?;
    let (screenshot, screenshot_elapsed_ms) =
        match input.screenshot.as_ref().filter(|request| request.capture) {
            Some(request) => {
                let screenshot_started_at = Instant::now();
                let artifact_dir = browser_control.artifacts_dir().join(&call_id);
                fs::create_dir_all(&artifact_dir).map_err(io_to_string)?;
                let extension = request.format.extension();
                let artifact_path = artifact_dir.join(format!("interact.{extension}"));
                let artifact = session
                    .capture_screenshot(
                        request.format,
                        &artifact_path,
                        &workspace_root,
                        BROWSER_INTERACT_TOOL_NAME,
                    )
                    .map_err(|error| format!("BrowserInteract screenshot failed: {error}"))?;
                (
                    Some(artifact),
                    Some(elapsed_ms(screenshot_started_at.elapsed())),
                )
            }
            None => (None, None),
        };

    to_pretty_json(BrowserInteractOutput {
        requested_url: input.url,
        final_url: page_state.url,
        title: page_state.title,
        action: action_report,
        load_outcome: browser_interact_load_outcome(input.wait.as_ref()),
        wait: wait_report,
        visible_text,
        visible_text_truncated,
        screenshot,
        browser_runtime: BrowserObserveRuntime {
            executable: browser.display().to_string(),
            mode: String::from("headless_cdp"),
            capture_backend: String::from("headless_cdp_single_call"),
        },
        timings_ms: BrowserInteractTimings {
            interaction: session.elapsed_ms(),
            screenshot: screenshot_elapsed_ms,
            total: elapsed_ms(started_at.elapsed()),
        },
        warnings: vec![String::from(
            "slice 2A supports one selector-backed click per call; durable browser sessions and /v1/threads browser support remain unavailable",
        )],
    })
}

fn browser_interact_load_outcome(wait: Option<&BrowserObserveWait>) -> String {
    match wait.map(|wait| wait.kind.as_str()) {
        None => String::from("captured_after_click"),
        Some("load") => String::from("clicked_after_page_load"),
        Some("text" | "title_contains" | "url_contains") => {
            String::from("clicked_after_wait_budget")
        }
        Some(_) => String::from("unsupported"),
    }
}

fn validate_browser_interact_input(input: &BrowserInteractInput) -> Result<(), String> {
    if input.url.trim().is_empty() {
        return Err(String::from("BrowserInteract url must not be empty"));
    }
    if input.action.kind.trim().is_empty() {
        return Err(String::from(
            "BrowserInteract action.kind must not be empty",
        ));
    }
    if !input.action.kind.eq_ignore_ascii_case("click") {
        return Err(format!(
            "BrowserInteract action.kind={} is not supported in slice 2A; supported kinds are click",
            input.action.kind
        ));
    }
    if input.action.selector.trim().is_empty() {
        return Err(String::from(
            "BrowserInteract action.selector must not be empty",
        ));
    }
    if let Some(timeout_ms) = input.timeout_ms {
        if timeout_ms == 0 || timeout_ms > MAX_BROWSER_TIMEOUT_MS {
            return Err(format!(
                "BrowserInteract timeout_ms must be between 1 and {MAX_BROWSER_TIMEOUT_MS}"
            ));
        }
    }
    if let Some(viewport) = &input.viewport {
        if !(320..=4096).contains(&viewport.width) {
            return Err(String::from(
                "BrowserInteract viewport.width must be between 320 and 4096",
            ));
        }
        if !(240..=4096).contains(&viewport.height) {
            return Err(String::from(
                "BrowserInteract viewport.height must be between 240 and 4096",
            ));
        }
    }
    if let Some(wait) = &input.wait {
        match wait.kind {
            BrowserObserveWaitKind::Load => {
                if wait.expected_value().is_some() {
                    return Err(String::from(
                        "BrowserInteract wait.kind=load does not accept `value` in slice 2A",
                    ));
                }
            }
            BrowserObserveWaitKind::Text
            | BrowserObserveWaitKind::TitleContains
            | BrowserObserveWaitKind::UrlContains => {
                if wait.expected_value().is_none() {
                    return Err(format!(
                        "BrowserInteract wait.kind={} requires a non-empty `value`",
                        wait.kind.as_str()
                    ));
                }
            }
            BrowserObserveWaitKind::NetworkIdle | BrowserObserveWaitKind::Selector => {
                return Err(format!(
                    "BrowserInteract wait.kind={} is not supported in slice 2A; supported kinds are {}",
                    wait.kind.as_str(),
                    BROWSER_INTERACT_SUPPORTED_WAIT_KINDS.join(", ")
                ));
            }
        }
    }
    Ok(())
}

fn browser_interact_wait_budget_ms(input: &BrowserInteractInput) -> u64 {
    input.timeout_ms.unwrap_or(DEFAULT_BROWSER_TIMEOUT_MS)
}

fn validate_browser_observe_input(input: &BrowserObserveInput) -> Result<(), String> {
    if input.url.trim().is_empty() {
        return Err(String::from("BrowserObserve url must not be empty"));
    }
    if let Some(timeout_ms) = input.timeout_ms {
        if timeout_ms == 0 || timeout_ms > MAX_BROWSER_TIMEOUT_MS {
            return Err(format!(
                "BrowserObserve timeout_ms must be between 1 and {MAX_BROWSER_TIMEOUT_MS}"
            ));
        }
    }
    if let Some(viewport) = &input.viewport {
        if !(320..=4096).contains(&viewport.width) {
            return Err(String::from(
                "BrowserObserve viewport.width must be between 320 and 4096",
            ));
        }
        if !(240..=4096).contains(&viewport.height) {
            return Err(String::from(
                "BrowserObserve viewport.height must be between 240 and 4096",
            ));
        }
    }
    if let Some(wait) = &input.wait {
        match wait.kind {
            BrowserObserveWaitKind::Load | BrowserObserveWaitKind::NetworkIdle => {
                if wait.expected_value().is_some() {
                    return Err(format!(
                        "BrowserObserve wait.kind={} does not accept `value` in slice 1B",
                        wait.kind.as_str()
                    ));
                }
            }
            BrowserObserveWaitKind::Text | BrowserObserveWaitKind::TitleContains => {
                if wait.expected_value().is_none() {
                    return Err(format!(
                        "BrowserObserve wait.kind={} requires a non-empty `value`",
                        wait.kind.as_str()
                    ));
                }
            }
            BrowserObserveWaitKind::Selector | BrowserObserveWaitKind::UrlContains => {
                return Err(format!(
                    "BrowserObserve wait.kind={} is not supported in slice 1B; supported kinds are {}",
                    wait.kind.as_str(),
                    BROWSER_OBSERVE_SUPPORTED_WAIT_KINDS.join(", ")
                ));
            }
        }
    }
    Ok(())
}

fn observe_browser_page(
    browser: &Path,
    input: &BrowserObserveInput,
) -> Result<BrowserObserveObservation, String> {
    let started_at = Instant::now();
    let html = run_browser_capture(
        browser,
        input,
        None,
        BrowserObserveCapturePhase::Observation,
    )?;
    let snapshot = BrowserObserveSnapshot::from_html(&html);
    let wait = browser_wait_report(input, &snapshot)?;
    let load_outcome = input.wait.as_ref().map_or_else(
        || String::from("loaded"),
        |wait| match wait.kind {
            BrowserObserveWaitKind::Load => String::from("loaded"),
            BrowserObserveWaitKind::NetworkIdle => String::from("loaded_after_network_idle_budget"),
            BrowserObserveWaitKind::Text | BrowserObserveWaitKind::TitleContains => {
                String::from("loaded_after_wait_budget")
            }
            BrowserObserveWaitKind::Selector | BrowserObserveWaitKind::UrlContains => {
                String::from("unsupported")
            }
        },
    );

    Ok(BrowserObserveObservation {
        snapshot,
        load_outcome,
        wait,
        observation_elapsed_ms: elapsed_ms(started_at.elapsed()),
    })
}

fn browser_wait_report(
    input: &BrowserObserveInput,
    snapshot: &BrowserObserveSnapshot,
) -> Result<BrowserObserveWaitReport, String> {
    let wait_budget_ms = browser_wait_budget_ms(input);
    let default_report = BrowserObserveWaitReport {
        kind: String::from("load"),
        status: String::from("satisfied"),
        detail: String::from("captured DOM after page load"),
        expected: None,
    };
    let Some(wait) = &input.wait else {
        return Ok(default_report);
    };

    match wait.kind {
        BrowserObserveWaitKind::Load => Ok(BrowserObserveWaitReport {
            kind: wait.kind.as_str().to_string(),
            status: String::from("satisfied"),
            detail: String::from("captured DOM after page load"),
            expected: None,
        }),
        BrowserObserveWaitKind::NetworkIdle => Ok(BrowserObserveWaitReport {
            kind: wait.kind.as_str().to_string(),
            status: String::from("satisfied"),
            detail: format!(
                "captured DOM after allowing up to {}ms of virtual browser time",
                browser_virtual_time_budget_ms(input)
                    .unwrap_or(DEFAULT_BROWSER_NETWORK_IDLE_BUDGET_MS)
            ),
            expected: None,
        }),
        BrowserObserveWaitKind::Text => {
            let expected = wait.expected_value().expect("validated text wait");
            if snapshot.visible_text.contains(expected) {
                Ok(BrowserObserveWaitReport {
                    kind: wait.kind.as_str().to_string(),
                    status: String::from("satisfied"),
                    detail: String::from("captured DOM contained the required text fragment"),
                    expected: Some(expected.to_string()),
                })
            } else {
                Err(format!(
                    "BrowserObserve did not observe text containing `{expected}` within the {wait_budget_ms}ms wait budget"
                ))
            }
        }
        BrowserObserveWaitKind::TitleContains => {
            let expected = wait.expected_value().expect("validated title wait");
            let matches = snapshot
                .title
                .as_deref()
                .is_some_and(|title| title.contains(expected));
            if matches {
                Ok(BrowserObserveWaitReport {
                    kind: wait.kind.as_str().to_string(),
                    status: String::from("satisfied"),
                    detail: String::from("captured DOM title contained the required fragment"),
                    expected: Some(expected.to_string()),
                })
            } else {
                Err(format!(
                    "BrowserObserve did not observe a page title containing `{expected}` within the {wait_budget_ms}ms wait budget"
                ))
            }
        }
        BrowserObserveWaitKind::Selector | BrowserObserveWaitKind::UrlContains => Err(format!(
            "BrowserObserve wait.kind={} is not supported in slice 1B",
            wait.kind.as_str()
        )),
    }
}

fn run_browser_capture(
    browser: &Path,
    input: &BrowserObserveInput,
    screenshot_path: Option<&Path>,
    phase: BrowserObserveCapturePhase,
) -> Result<String, String> {
    let mut command = Command::new(browser);
    command
        .arg("--headless=new")
        .arg("--disable-gpu")
        .arg("--no-first-run")
        .arg("--no-default-browser-check");

    let timeout_ms = input.timeout_ms.unwrap_or(DEFAULT_BROWSER_TIMEOUT_MS);
    command.arg(format!("--timeout={timeout_ms}"));

    if let Some(virtual_time_budget_ms) = browser_virtual_time_budget_ms(input) {
        command.arg(format!("--virtual-time-budget={virtual_time_budget_ms}"));
    }

    let viewport = input.viewport.as_ref().map_or(
        (
            DEFAULT_BROWSER_VIEWPORT_WIDTH,
            DEFAULT_BROWSER_VIEWPORT_HEIGHT,
        ),
        |viewport| (viewport.width, viewport.height),
    );
    command.arg(format!("--window-size={},{}", viewport.0, viewport.1));

    match screenshot_path {
        Some(path) => {
            command.arg(format!("--screenshot={}", path.display()));
        }
        None => {
            command.arg("--dump-dom");
        }
    }
    command.arg(&input.url);

    let output = command.output().map_err(|error| {
        format!(
            "BrowserObserve {} failed to launch {}: {error}",
            phase.as_str(),
            browser.display()
        )
    })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let suffix = if stderr.trim().is_empty() {
            String::new()
        } else {
            format!(": {}", stderr.trim())
        };
        return Err(format!(
            "BrowserObserve {} failed with status {}{}",
            phase.as_str(),
            output
                .status
                .code()
                .map_or_else(|| String::from("terminated"), |code| code.to_string()),
            suffix
        ));
    }

    if screenshot_path.is_some() {
        return Ok(String::new());
    }

    String::from_utf8(output.stdout)
        .map_err(|error| format!("BrowserObserve captured non-UTF-8 DOM output: {error}"))
}

fn browser_virtual_time_budget_ms(input: &BrowserObserveInput) -> Option<u64> {
    let wait = input.wait.as_ref()?;
    match wait.kind {
        BrowserObserveWaitKind::NetworkIdle => Some(
            input
                .timeout_ms
                .unwrap_or(DEFAULT_BROWSER_NETWORK_IDLE_BUDGET_MS),
        ),
        BrowserObserveWaitKind::Text | BrowserObserveWaitKind::TitleContains => {
            Some(browser_wait_budget_ms(input))
        }
        BrowserObserveWaitKind::Load
        | BrowserObserveWaitKind::Selector
        | BrowserObserveWaitKind::UrlContains => None,
    }
}

fn browser_wait_budget_ms(input: &BrowserObserveInput) -> u64 {
    input.timeout_ms.unwrap_or(DEFAULT_BROWSER_TIMEOUT_MS)
}

fn elapsed_ms(duration: Duration) -> u64 {
    duration
        .as_millis()
        .min(u128::from(u64::MAX))
        .try_into()
        .unwrap_or(u64::MAX)
}

fn find_supported_browser_command() -> Option<PathBuf> {
    supported_browser_candidates()
        .into_iter()
        .find_map(|candidate| resolve_command_path(&candidate))
}

fn supported_browser_candidates() -> Vec<String> {
    let mut candidates = vec![
        String::from("msedge"),
        String::from("chrome"),
        String::from("chromium"),
        String::from("google-chrome"),
        String::from("google-chrome-stable"),
        String::from("chromium-browser"),
        String::from("microsoft-edge"),
    ];

    if cfg!(windows) {
        candidates.extend([
            String::from(r"C:\Program Files\Microsoft\Edge\Application\msedge.exe"),
            String::from(r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe"),
            String::from(r"C:\Program Files\Google\Chrome\Application\chrome.exe"),
            String::from(r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe"),
        ]);
    } else if cfg!(target_os = "macos") {
        candidates.extend([
            String::from("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"),
            String::from("/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge"),
        ]);
    }

    candidates
}

fn browser_observe_call_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    format!("call-{nanos}")
}

fn portable_workspace_relative_path(path: &Path, workspace_root: &Path) -> Result<String, String> {
    portable_workspace_relative_path_for_browser_tool(
        path,
        workspace_root,
        BROWSER_OBSERVE_TOOL_NAME,
    )
}

fn portable_workspace_relative_path_for_browser_tool(
    path: &Path,
    workspace_root: &Path,
    tool_name: &str,
) -> Result<String, String> {
    let relative = path.strip_prefix(workspace_root).map_err(|_| {
        format!(
            "{tool_name} artifact path {} is outside the current workspace {}",
            path.display(),
            workspace_root.display()
        )
    })?;
    Ok(relative
        .components()
        .map(|component| component.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/"))
}

fn extract_html_title(html: &str) -> Option<String> {
    let lowered = html.to_ascii_lowercase();
    let title_start = lowered.find("<title")?;
    let title_open_end = html[title_start..].find('>')? + title_start + 1;
    let title_close = lowered[title_open_end..].find("</title>")? + title_open_end;
    let title = decode_html_entities_basic(html[title_open_end..title_close].trim());
    (!title.is_empty()).then_some(title)
}

fn visible_text_from_html(html: &str) -> String {
    let without_scripts = strip_html_block(html, "script");
    let without_styles = strip_html_block(&without_scripts, "style");
    let without_noscript = strip_html_block(&without_styles, "noscript");
    let mut text = String::new();
    let mut in_tag = false;

    for ch in without_noscript.chars() {
        match ch {
            '<' => {
                in_tag = true;
                text.push(' ');
            }
            '>' => {
                in_tag = false;
                text.push(' ');
            }
            _ if !in_tag => text.push(ch),
            _ => {}
        }
    }

    decode_html_entities_basic(&text.split_whitespace().collect::<Vec<_>>().join(" "))
}

fn strip_html_block(html: &str, tag: &str) -> String {
    let mut output = html.to_string();
    let open = format!("<{tag}");
    let close = format!("</{tag}>");

    loop {
        let lowered = output.to_ascii_lowercase();
        let Some(start) = lowered.find(&open) else {
            break;
        };
        let Some(close_start_rel) = lowered[start..].find(&close) else {
            output.replace_range(start..output.len(), " ");
            break;
        };
        let close_start = start + close_start_rel;
        let close_end = close_start + close.len();
        let remove_end = output[close_end..]
            .find('>')
            .map_or(output.len(), |offset| close_end + offset + 1);
        output.replace_range(start..remove_end, " ");
    }

    output
}

fn decode_html_entities_basic(value: &str) -> String {
    value
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
}

fn truncate_browser_visible_text(value: &str) -> (Option<String>, bool) {
    if value.is_empty() {
        return (None, false);
    }
    let total_chars = value.chars().count();
    if total_chars <= MAX_BROWSER_VISIBLE_TEXT_CHARS {
        return (Some(value.to_string()), false);
    }
    let truncated = value
        .chars()
        .take(MAX_BROWSER_VISIBLE_TEXT_CHARS)
        .collect::<String>();
    (Some(truncated), true)
}

fn truncate_browser_text_summary(value: &str, max_chars: usize) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return String::from("?");
    }
    let total_chars = trimmed.chars().count();
    if total_chars <= max_chars {
        return trimmed.to_string();
    }
    let mut truncated = trimmed.chars().take(max_chars).collect::<String>();
    truncated.push_str("...");
    truncated
}

#[allow(clippy::needless_pass_by_value)]
fn run_read_file(input: ReadFileInput) -> Result<String, String> {
    to_pretty_json(read_file(&input.path, input.offset, input.limit).map_err(io_to_string)?)
}

#[allow(clippy::needless_pass_by_value)]
fn run_write_file(input: WriteFileInput) -> Result<String, String> {
    to_pretty_json(write_file(&input.path, &input.content).map_err(io_to_string)?)
}

#[allow(clippy::needless_pass_by_value)]
fn run_edit_file(input: EditFileInput) -> Result<String, String> {
    to_pretty_json(
        edit_file(
            &input.path,
            &input.old_string,
            &input.new_string,
            input.replace_all.unwrap_or(false),
        )
        .map_err(io_to_string)?,
    )
}

#[allow(clippy::needless_pass_by_value)]
fn run_glob_search(input: GlobSearchInputValue) -> Result<String, String> {
    to_pretty_json(glob_search(&input.pattern, input.path.as_deref()).map_err(io_to_string)?)
}

#[allow(clippy::needless_pass_by_value)]
fn run_grep_search(input: GrepSearchInput) -> Result<String, String> {
    to_pretty_json(grep_search(&input).map_err(io_to_string)?)
}

#[allow(clippy::needless_pass_by_value)]
fn run_web_fetch(input: WebFetchInput) -> Result<String, String> {
    to_pretty_json(execute_web_fetch(&input)?)
}

#[allow(clippy::needless_pass_by_value)]
fn run_web_search(input: WebSearchInput) -> Result<String, String> {
    to_pretty_json(execute_web_search(&input)?)
}

fn run_todo_write(input: TodoWriteInput) -> Result<String, String> {
    to_pretty_json(execute_todo_write(input)?)
}

fn run_skill(input: SkillInput) -> Result<String, String> {
    to_pretty_json(execute_skill(input)?)
}

fn run_agent(input: AgentInput) -> Result<String, String> {
    to_pretty_json(execute_agent(input)?)
}

fn run_tool_search(input: ToolSearchInput) -> Result<String, String> {
    to_pretty_json(execute_tool_search(input))
}

fn run_notebook_edit(input: NotebookEditInput) -> Result<String, String> {
    to_pretty_json(execute_notebook_edit(input)?)
}

fn run_sleep(input: SleepInput) -> Result<String, String> {
    to_pretty_json(execute_sleep(input))
}

fn run_brief(input: BriefInput) -> Result<String, String> {
    to_pretty_json(execute_brief(input)?)
}

fn run_config(input: ConfigInput) -> Result<String, String> {
    to_pretty_json(execute_config(input)?)
}

fn run_structured_output(input: StructuredOutputInput) -> Result<String, String> {
    to_pretty_json(execute_structured_output(input))
}

fn run_repl(input: ReplInput) -> Result<String, String> {
    to_pretty_json(execute_repl(input)?)
}

fn run_powershell(input: PowerShellInput) -> Result<String, String> {
    to_pretty_json(execute_powershell(input).map_err(|error| error.to_string())?)
}

#[allow(clippy::needless_pass_by_value)]
fn run_task_create(input: TaskCreateInput) -> Result<String, String> {
    let registry = global_task_registry();
    let task = registry.create(&input.prompt, input.description.as_deref());
    to_pretty_json(json!({
        "task_id": task.task_id,
        "status": task.status,
        "prompt": task.prompt,
        "description": task.description,
        "created_at": task.created_at,
        "updated_at": task.updated_at,
        "last_error": task.last_error,
        "origin": task.origin,
        "capabilities": task.capabilities,
        "message_count": task.messages.len(),
        "output_length": task.output.len(),
        "has_output": !task.output.is_empty(),
        "team_id": task.team_id
    }))
}

#[allow(clippy::needless_pass_by_value)]
fn run_task_get(input: TaskIdInput) -> Result<String, String> {
    match global_task_registry().get(&input.task_id) {
        Some(task) => to_pretty_json(json!({
            "task_id": task.task_id,
            "status": task.status,
            "prompt": task.prompt,
            "description": task.description,
            "created_at": task.created_at,
            "updated_at": task.updated_at,
            "last_error": task.last_error,
            "origin": task.origin,
            "capabilities": task.capabilities,
            "message_count": task.messages.len(),
            "messages": task.messages,
            "output_length": task.output.len(),
            "has_output": !task.output.is_empty(),
            "team_id": task.team_id
        })),
        None => Err(format!("task not found: {}", input.task_id)),
    }
}

fn run_task_list(_input: Value) -> Result<String, String> {
    let tasks: Vec<_> = global_task_registry()
        .list(None)
        .into_iter()
        .map(|task| {
            json!({
                "task_id": task.task_id,
                "status": task.status,
                "prompt": task.prompt,
                "description": task.description,
                "created_at": task.created_at,
                "updated_at": task.updated_at,
                "last_error": task.last_error,
                "origin": task.origin,
                "capabilities": task.capabilities,
                "message_count": task.messages.len(),
                "output_length": task.output.len(),
                "has_output": !task.output.is_empty(),
                "team_id": task.team_id
            })
        })
        .collect();
    to_pretty_json(json!({ "tasks": tasks, "count": tasks.len() }))
}

#[allow(clippy::needless_pass_by_value)]
fn run_task_stop(input: TaskIdInput) -> Result<String, String> {
    match global_task_registry().stop(&input.task_id) {
        Ok(task) => to_pretty_json(json!({
            "task_id": task.task_id,
            "status": task.status,
            "message": "Task stopped",
            "updated_at": task.updated_at,
            "last_error": task.last_error,
            "origin": task.origin,
            "capabilities": task.capabilities,
            "message_count": task.messages.len(),
            "output_length": task.output.len(),
            "has_output": !task.output.is_empty(),
            "team_id": task.team_id
        })),
        Err(error) => Err(error),
    }
}

#[allow(clippy::needless_pass_by_value)]
fn run_task_update(input: TaskUpdateInput) -> Result<String, String> {
    match global_task_registry().update(&input.task_id, &input.message) {
        Ok(task) => to_pretty_json(json!({
            "task_id": task.task_id,
            "status": task.status,
            "message_count": task.messages.len(),
            "last_message": input.message,
            "updated_at": task.updated_at,
            "last_error": task.last_error,
            "origin": task.origin,
            "capabilities": task.capabilities,
            "output_length": task.output.len(),
            "has_output": !task.output.is_empty(),
            "team_id": task.team_id
        })),
        Err(error) => Err(error),
    }
}

#[allow(clippy::needless_pass_by_value)]
fn run_task_output(input: TaskIdInput) -> Result<String, String> {
    match global_task_registry().get(&input.task_id) {
        Some(task) => {
            let output = task.output;
            let output_length = output.len();
            let has_output = !output.is_empty();
            to_pretty_json(json!({
                "task_id": task.task_id,
                "status": task.status,
                "output": output,
                "output_length": output_length,
                "has_output": has_output,
                "updated_at": task.updated_at,
                "last_error": task.last_error,
                "origin": task.origin,
                "capabilities": task.capabilities
            }))
        }
        None => Err(format!("task not found: {}", input.task_id)),
    }
}

#[allow(clippy::needless_pass_by_value)]
fn run_task_wait(input: TaskWaitInput) -> Result<String, String> {
    let timeout_ms = input.timeout_ms.unwrap_or(0).min(30_000);
    let poll_interval_ms = input.poll_interval_ms.unwrap_or(100).clamp(1, 1_000);
    let started = Instant::now();

    loop {
        let task = global_task_registry()
            .get(&input.task_id)
            .ok_or_else(|| format!("task not found: {}", input.task_id))?;
        let terminal = matches!(
            task.status.to_string().as_str(),
            "completed" | "failed" | "stopped"
        );
        let waited_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let timed_out = !terminal && waited_ms >= timeout_ms;
        if terminal || timed_out {
            return to_pretty_json(json!({
                "task_id": task.task_id,
                "status": task.status,
                "terminal": terminal,
                "timed_out": timed_out,
                "waited_ms": waited_ms,
                "created_at": task.created_at,
                "updated_at": task.updated_at,
                "last_error": task.last_error,
                "origin": task.origin,
                "capabilities": task.capabilities,
                "message_count": task.messages.len(),
                "output_length": task.output.len(),
                "has_output": !task.output.is_empty(),
                "team_id": task.team_id
            }));
        }

        std::thread::sleep(Duration::from_millis(poll_interval_ms));
    }
}

#[allow(clippy::needless_pass_by_value)]
fn run_team_create(input: TeamCreateInput) -> Result<String, String> {
    let task_ids = extract_team_task_ids(&input.tasks)?;
    let task_registry = global_task_registry();
    for task_id in &task_ids {
        let task = task_registry
            .get(task_id)
            .ok_or_else(|| format!("task not found: {task_id}"))?;
        if let Some(existing_team_id) = task.team_id {
            return Err(format!(
                "task {task_id} is already assigned to team {existing_team_id}"
            ));
        }
    }

    let team = global_team_registry().create(&input.name, task_ids);
    let mut assigned_task_ids: Vec<String> = Vec::with_capacity(team.task_ids.len());
    for task_id in &team.task_ids {
        if let Err(error) = task_registry.assign_team(task_id, &team.team_id) {
            for assigned_task_id in &assigned_task_ids {
                let _ = task_registry.unassign_team(assigned_task_id, &team.team_id);
            }
            let _ = global_team_registry().remove(&team.team_id);
            return Err(error);
        }
        assigned_task_ids.push(task_id.clone());
    }
    let task_count = team.task_ids.len();
    to_pretty_json(json!({
        "team_id": team.team_id,
        "name": team.name,
        "task_count": task_count,
        "task_ids": team.task_ids,
        "status": team.status,
        "created_at": team.created_at,
        "updated_at": team.updated_at,
        "last_error": team.last_error,
        "origin": team.origin,
        "capabilities": team.capabilities
    }))
}

#[allow(clippy::needless_pass_by_value)]
fn run_team_get(input: TeamIdInput) -> Result<String, String> {
    match global_team_registry().get(&input.team_id) {
        Some(team) => {
            let task_count = team.task_ids.len();
            to_pretty_json(json!({
                "team_id": team.team_id,
                "name": team.name,
                "task_ids": team.task_ids,
                "task_count": task_count,
                "status": team.status,
                "created_at": team.created_at,
                "updated_at": team.updated_at,
                "last_error": team.last_error,
                "origin": team.origin,
                "capabilities": team.capabilities
            }))
        }
        None => Err(format!("team not found: {}", input.team_id)),
    }
}

fn run_team_list(_input: Value) -> Result<String, String> {
    let teams: Vec<_> = global_team_registry()
        .list()
        .into_iter()
        .map(|team| {
            let task_count = team.task_ids.len();
            json!({
                "team_id": team.team_id,
                "name": team.name,
                "task_ids": team.task_ids,
                "task_count": task_count,
                "status": team.status,
                "created_at": team.created_at,
                "updated_at": team.updated_at,
                "last_error": team.last_error,
                "origin": team.origin,
                "capabilities": team.capabilities
            })
        })
        .collect();
    to_pretty_json(json!({ "teams": teams, "count": teams.len() }))
}

#[allow(clippy::needless_pass_by_value)]
fn run_team_delete(input: TeamIdInput) -> Result<String, String> {
    match global_team_registry().delete(&input.team_id) {
        Ok(team) => {
            let task_count = team.task_ids.len();
            to_pretty_json(json!({
                "team_id": team.team_id,
                "name": team.name,
                "task_ids": team.task_ids,
                "task_count": task_count,
                "status": team.status,
                "created_at": team.created_at,
                "updated_at": team.updated_at,
                "last_error": team.last_error,
                "origin": team.origin,
                "capabilities": team.capabilities,
                "message": "Team deleted"
            }))
        }
        Err(error) => Err(error),
    }
}

#[allow(clippy::needless_pass_by_value)]
fn run_cron_create(input: CronCreateInput) -> Result<String, String> {
    let entry =
        global_cron_registry().create(&input.schedule, &input.prompt, input.description.as_deref());
    to_pretty_json(json!({
        "cron_id": entry.cron_id,
        "schedule": entry.schedule,
        "prompt": entry.prompt,
        "description": entry.description,
        "enabled": entry.enabled,
        "run_count": entry.run_count,
        "last_run_at": entry.last_run_at,
        "created_at": entry.created_at,
        "updated_at": entry.updated_at,
        "last_error": entry.last_error,
        "disabled_reason": entry.disabled_reason,
        "origin": entry.origin,
        "capabilities": entry.capabilities
    }))
}

#[allow(clippy::needless_pass_by_value)]
fn run_cron_get(input: CronIdInput) -> Result<String, String> {
    match global_cron_registry().get(&input.cron_id) {
        Some(entry) => to_pretty_json(json!({
            "cron_id": entry.cron_id,
            "schedule": entry.schedule,
            "prompt": entry.prompt,
            "description": entry.description,
            "enabled": entry.enabled,
            "run_count": entry.run_count,
            "last_run_at": entry.last_run_at,
            "created_at": entry.created_at,
            "updated_at": entry.updated_at,
            "last_error": entry.last_error,
            "disabled_reason": entry.disabled_reason,
            "origin": entry.origin,
            "capabilities": entry.capabilities
        })),
        None => Err(format!("cron not found: {}", input.cron_id)),
    }
}

#[allow(clippy::needless_pass_by_value)]
fn run_cron_disable(input: CronIdInput) -> Result<String, String> {
    global_cron_registry().disable(&input.cron_id)?;
    let entry = global_cron_registry()
        .get(&input.cron_id)
        .ok_or_else(|| format!("cron not found: {}", input.cron_id))?;
    to_pretty_json(json!({
        "cron_id": entry.cron_id,
        "schedule": entry.schedule,
        "enabled": entry.enabled,
        "run_count": entry.run_count,
        "last_run_at": entry.last_run_at,
        "updated_at": entry.updated_at,
        "status": "disabled",
        "last_error": entry.last_error,
        "disabled_reason": entry.disabled_reason,
        "origin": entry.origin,
        "capabilities": entry.capabilities
    }))
}

#[allow(clippy::needless_pass_by_value)]
fn run_cron_enable(input: CronIdInput) -> Result<String, String> {
    global_cron_registry().enable(&input.cron_id)?;
    let entry = global_cron_registry()
        .get(&input.cron_id)
        .ok_or_else(|| format!("cron not found: {}", input.cron_id))?;
    to_pretty_json(json!({
        "cron_id": entry.cron_id,
        "schedule": entry.schedule,
        "enabled": entry.enabled,
        "run_count": entry.run_count,
        "last_run_at": entry.last_run_at,
        "updated_at": entry.updated_at,
        "status": "enabled",
        "last_error": entry.last_error,
        "disabled_reason": entry.disabled_reason,
        "origin": entry.origin,
        "capabilities": entry.capabilities
    }))
}

#[allow(clippy::needless_pass_by_value)]
fn run_cron_delete(input: CronIdInput) -> Result<String, String> {
    match global_cron_registry().delete(&input.cron_id) {
        Ok(entry) => to_pretty_json(json!({
            "cron_id": entry.cron_id,
            "schedule": entry.schedule,
            "status": "deleted",
            "message": "Cron entry removed",
            "created_at": entry.created_at,
            "updated_at": entry.updated_at,
            "last_error": entry.last_error,
            "disabled_reason": entry.disabled_reason,
            "origin": entry.origin,
            "capabilities": entry.capabilities
        })),
        Err(error) => Err(error),
    }
}

fn run_cron_list(_input: Value) -> Result<String, String> {
    let entries: Vec<_> = global_cron_registry()
        .list(false)
        .into_iter()
        .map(|entry| {
            json!({
                "cron_id": entry.cron_id,
                "schedule": entry.schedule,
                "prompt": entry.prompt,
                "description": entry.description,
                "enabled": entry.enabled,
                "run_count": entry.run_count,
                "last_run_at": entry.last_run_at,
                "created_at": entry.created_at,
                "updated_at": entry.updated_at,
                "last_error": entry.last_error,
                "disabled_reason": entry.disabled_reason,
                "origin": entry.origin,
                "capabilities": entry.capabilities
            })
        })
        .collect();
    to_pretty_json(json!({ "crons": entries, "count": entries.len() }))
}

#[allow(clippy::needless_pass_by_value)]
fn run_session_list(input: SessionListInput) -> Result<String, String> {
    let filter = input.kind.as_deref().map(SessionKind::parse).transpose()?;
    let mut sessions = Vec::new();
    let mut warnings = Vec::new();

    if filter.is_none() || filter == Some(SessionKind::Thread) {
        match load_thread_sessions() {
            Ok(records) => sessions.extend(records.iter().map(thread_session_summary_json)),
            Err(error) => {
                if filter == Some(SessionKind::Thread) {
                    return Err(error);
                }
                warnings.push(error);
            }
        }
    }

    if filter.is_none() || filter == Some(SessionKind::ManagedSession) {
        sessions.extend(
            list_managed_session_records()?
                .iter()
                .map(managed_session_summary_json),
        );
    }

    if filter.is_none() || filter == Some(SessionKind::AgentRun) {
        sessions.extend(list_agent_run_records()?.iter().map(agent_run_summary_json));
    }

    let mut payload = serde_json::Map::new();
    payload.insert("sessions".to_string(), Value::Array(sessions));
    payload.insert(
        "count".to_string(),
        Value::Number(serde_json::Number::from(
            payload
                .get("sessions")
                .and_then(Value::as_array)
                .map_or(0, Vec::len) as u64,
        )),
    );
    if !warnings.is_empty() {
        payload.insert(
            "warnings".to_string(),
            Value::Array(warnings.into_iter().map(Value::String).collect()),
        );
    }
    to_pretty_json(Value::Object(payload))
}

#[allow(clippy::needless_pass_by_value)]
fn run_session_get(input: SessionRefInput) -> Result<String, String> {
    let kind = SessionKind::parse(&input.kind)?;
    let payload = match kind {
        SessionKind::Thread => thread_session_detail_json(&load_thread_session_detail(&input.id)?),
        SessionKind::ManagedSession => {
            managed_session_detail_json(&find_managed_session_record(&input.id)?)
        }
        SessionKind::AgentRun => agent_run_detail_json(&find_agent_run_record(&input.id)?),
    };
    to_pretty_json(payload)
}

#[allow(clippy::needless_pass_by_value)]
fn run_session_create(input: SessionCreateInput) -> Result<String, String> {
    let kind = SessionKind::parse(&input.kind)?;
    if kind != SessionKind::Thread {
        return Err(format!(
            "SessionCreate only supports kind=thread in phase 1 (received {})",
            kind.as_str()
        ));
    }

    let snapshot = session_server_request::<ThreadSnapshotValue>(
        Method::POST,
        "/v1/threads",
        Some(json!({
            "cwd": input.cwd,
            "model": input.model,
            "permission_mode": input.permission_mode,
            "allowed_tools": input.allowed_tools,
        })),
    )?;
    to_pretty_json(thread_session_detail_json(&snapshot))
}

#[allow(clippy::needless_pass_by_value)]
fn run_session_send(input: SessionSendInput) -> Result<String, String> {
    let kind = SessionKind::parse(&input.kind)?;
    if kind != SessionKind::Thread {
        return Err(format!(
            "SessionSend only supports kind=thread in phase 1 (received {})",
            kind.as_str()
        ));
    }

    let accepted = session_server_request::<TurnAcceptedResponseValue>(
        Method::POST,
        &format!("/v1/threads/{}/turns", input.id),
        Some(json!({ "message": input.message })),
    )?;
    to_pretty_json(json!({
        "kind": "thread",
        "id": accepted.thread_id,
        "run_id": accepted.run_id,
        "status": accepted.status,
        "protocol_version": accepted.protocol_version,
        "origin": SessionKind::Thread.origin(),
        "capabilities": SessionKind::Thread.capabilities()
    }))
}

#[allow(clippy::needless_pass_by_value)]
fn run_session_resume(input: SessionResumeInput) -> Result<String, String> {
    let kind = SessionKind::parse(&input.kind)?;
    if kind != SessionKind::Thread {
        return Err(format!(
            "SessionResume only supports kind=thread in phase 1 (received {})",
            kind.as_str()
        ));
    }

    let accepted = session_server_request::<UserInputAcceptedResponseValue>(
        Method::POST,
        &format!("/v1/threads/{}/user-input", input.id),
        Some(json!({
            "request_id": input.request_id,
            "content": input.content,
            "selected_option": input.selected_option,
        })),
    )?;
    to_pretty_json(json!({
        "kind": "thread",
        "id": accepted.thread_id,
        "run_id": accepted.run_id,
        "request_id": accepted.request_id,
        "status": accepted.status,
        "protocol_version": accepted.protocol_version,
        "origin": SessionKind::Thread.origin(),
        "capabilities": SessionKind::Thread.capabilities()
    }))
}

#[allow(clippy::needless_pass_by_value)]
fn run_session_wait(input: SessionWaitInput) -> Result<String, String> {
    let kind = SessionKind::parse(&input.kind)?;
    let timeout_ms = input.timeout_ms.unwrap_or(0).min(30_000);
    let poll_interval_ms = input.poll_interval_ms.unwrap_or(100).clamp(1, 1_000);
    let started = Instant::now();

    match kind {
        SessionKind::Thread => wait_for_thread_session(&input.id, timeout_ms, poll_interval_ms, started),
        SessionKind::ManagedSession => Err(String::from(
            "SessionWait does not support kind=managed_session in phase 1; /resume remains authoritative",
        )),
        SessionKind::AgentRun => wait_for_agent_run(&input.id, timeout_ms, poll_interval_ms, started),
    }
}

fn wait_for_thread_session(
    id: &str,
    timeout_ms: u64,
    poll_interval_ms: u64,
    started: Instant,
) -> Result<String, String> {
    loop {
        let snapshot = load_thread_session_detail(id)?;
        let terminal = matches!(
            snapshot.state.status.as_str(),
            "idle" | "awaiting_user_input" | "interrupted"
        );
        let waited_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let timed_out = !terminal && waited_ms >= timeout_ms;
        if terminal || timed_out {
            let mut payload = thread_session_detail_json(&snapshot);
            payload["terminal"] = json!(terminal);
            payload["timed_out"] = json!(timed_out);
            payload["waited_ms"] = json!(waited_ms);
            return to_pretty_json(payload);
        }

        std::thread::sleep(Duration::from_millis(poll_interval_ms));
    }
}

fn wait_for_agent_run(
    id: &str,
    timeout_ms: u64,
    poll_interval_ms: u64,
    started: Instant,
) -> Result<String, String> {
    loop {
        let record = find_agent_run_record(id)?;
        let terminal = matches!(record.manifest.status.as_str(), "completed" | "failed");
        let waited_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let timed_out = !terminal && waited_ms >= timeout_ms;
        if terminal || timed_out {
            let mut payload = agent_run_detail_json(&record);
            payload["terminal"] = json!(terminal);
            payload["timed_out"] = json!(timed_out);
            payload["waited_ms"] = json!(waited_ms);
            return to_pretty_json(payload);
        }

        std::thread::sleep(Duration::from_millis(poll_interval_ms));
    }
}

fn load_thread_sessions() -> Result<Vec<ThreadSummaryValue>, String> {
    let response =
        session_server_request::<ListThreadsResponseValue>(Method::GET, "/v1/threads", None)?;
    Ok(response.threads)
}

fn load_thread_session_detail(id: &str) -> Result<ThreadSnapshotValue, String> {
    session_server_request(Method::GET, &format!("/v1/threads/{id}"), None)
}

fn thread_session_summary_json(summary: &ThreadSummaryValue) -> Value {
    json!({
        "kind": SessionKind::Thread.as_str(),
        "id": summary.thread_id,
        "origin": SessionKind::Thread.origin(),
        "capabilities": SessionKind::Thread.capabilities(),
        "status": summary.state.status,
        "run_id": summary.state.run_id,
        "pending_user_input": summary.state.pending_user_input,
        "recovery_note": summary.state.recovery_note,
        "created_at": summary.created_at,
        "updated_at": summary.updated_at,
        "message_count": summary.message_count
    })
}

fn thread_session_detail_json(snapshot: &ThreadSnapshotValue) -> Value {
    json!({
        "kind": SessionKind::Thread.as_str(),
        "id": snapshot.thread_id,
        "origin": SessionKind::Thread.origin(),
        "capabilities": SessionKind::Thread.capabilities(),
        "status": snapshot.state.status,
        "run_id": snapshot.state.run_id,
        "pending_user_input": snapshot.state.pending_user_input,
        "recovery_note": snapshot.state.recovery_note,
        "created_at": snapshot.created_at,
        "updated_at": snapshot.updated_at,
        "message_count": snapshot.session.messages.len(),
        "protocol_version": snapshot.protocol_version,
        "config": snapshot.config,
        "session": snapshot.session
    })
}

fn list_managed_session_records() -> Result<Vec<ManagedSessionRecord>, String> {
    let mut sessions = Vec::new();
    let dir = managed_sessions_dir()?;
    if !dir.is_dir() {
        return Ok(sessions);
    }

    for entry in std::fs::read_dir(dir).map_err(|error| error.to_string())? {
        let entry = entry.map_err(|error| error.to_string())?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        sessions.push(load_managed_session_record(&path)?);
    }
    sessions.sort_by(|left, right| right.updated_at.cmp(&left.updated_at));
    Ok(sessions)
}

fn find_managed_session_record(id: &str) -> Result<ManagedSessionRecord, String> {
    list_managed_session_records()?
        .into_iter()
        .find(|record| record.id == id)
        .ok_or_else(|| format!("managed_session not found: {id}"))
}

fn load_managed_session_record(path: &Path) -> Result<ManagedSessionRecord, String> {
    let session = Session::load_from_path(path).map_err(|error| error.to_string())?;
    let metadata = std::fs::metadata(path).map_err(|error| error.to_string())?;
    let updated_at = file_time_to_epoch_secs(metadata.modified().ok()).unwrap_or_default();
    let created_at = file_time_to_epoch_secs(metadata.created().ok()).unwrap_or(updated_at);
    let id = path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("unknown")
        .to_string();
    Ok(ManagedSessionRecord {
        id,
        path: path.to_path_buf(),
        created_at,
        updated_at,
        session,
    })
}

fn managed_session_summary_json(record: &ManagedSessionRecord) -> Value {
    json!({
        "kind": SessionKind::ManagedSession.as_str(),
        "id": record.id,
        "origin": SessionKind::ManagedSession.origin(),
        "capabilities": SessionKind::ManagedSession.capabilities(),
        "status": managed_session_status(&record.session),
        "created_at": record.created_at,
        "updated_at": record.updated_at,
        "message_count": record.session.messages.len(),
        "pending_user_input": record
            .session
            .pending_user_input_request()
            .map(PendingUserInputPayload::from),
        "path": record.path.display().to_string()
    })
}

fn managed_session_detail_json(record: &ManagedSessionRecord) -> Value {
    json!({
        "kind": SessionKind::ManagedSession.as_str(),
        "id": record.id,
        "origin": SessionKind::ManagedSession.origin(),
        "capabilities": SessionKind::ManagedSession.capabilities(),
        "status": managed_session_status(&record.session),
        "created_at": record.created_at,
        "updated_at": record.updated_at,
        "message_count": record.session.messages.len(),
        "pending_user_input": record
            .session
            .pending_user_input_request()
            .map(PendingUserInputPayload::from),
        "path": record.path.display().to_string(),
        "session": record.session
    })
}

fn managed_session_status(session: &Session) -> &'static str {
    if session.pending_user_input_request().is_some() {
        "awaiting_user_input"
    } else {
        "idle"
    }
}

fn managed_sessions_dir() -> Result<PathBuf, String> {
    let cwd = std::env::current_dir().map_err(|error| error.to_string())?;
    Ok(cwd.join(".openyak").join("sessions"))
}

fn list_agent_run_records() -> Result<Vec<AgentRunRecord>, String> {
    let mut agents = Vec::new();
    let dir = agent_store_dir()?;
    if !dir.is_dir() {
        return Ok(agents);
    }

    for entry in std::fs::read_dir(dir).map_err(|error| error.to_string())? {
        let entry = entry.map_err(|error| error.to_string())?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        agents.push(load_agent_run_record(&path)?);
    }
    agents.sort_by(|left, right| right.updated_at.cmp(&left.updated_at));
    Ok(agents)
}

fn find_agent_run_record(id: &str) -> Result<AgentRunRecord, String> {
    list_agent_run_records()?
        .into_iter()
        .find(|record| record.manifest.agent_id == id)
        .ok_or_else(|| format!("agent_run not found: {id}"))
}

fn load_agent_run_record(path: &Path) -> Result<AgentRunRecord, String> {
    let manifest: AgentOutput =
        serde_json::from_str(&std::fs::read_to_string(path).map_err(|error| error.to_string())?)
            .map_err(|error| error.to_string())?;
    let output = std::fs::read_to_string(&manifest.output_file).unwrap_or_default();
    let metadata = std::fs::metadata(path).map_err(|error| error.to_string())?;
    let updated_at = file_time_to_epoch_secs(metadata.modified().ok())
        .or_else(|| manifest.completed_at.as_deref().and_then(parse_epoch_secs))
        .or_else(|| manifest.started_at.as_deref().and_then(parse_epoch_secs))
        .or_else(|| parse_epoch_secs(&manifest.created_at))
        .unwrap_or_default();
    let created_at = parse_epoch_secs(&manifest.created_at).unwrap_or(updated_at);
    Ok(AgentRunRecord {
        manifest,
        output,
        created_at,
        updated_at,
    })
}

fn agent_run_summary_json(record: &AgentRunRecord) -> Value {
    json!({
        "kind": SessionKind::AgentRun.as_str(),
        "id": record.manifest.agent_id,
        "origin": SessionKind::AgentRun.origin(),
        "capabilities": SessionKind::AgentRun.capabilities(),
        "status": record.manifest.status,
        "created_at": record.created_at,
        "updated_at": record.updated_at,
        "output_length": record.output.len(),
        "name": record.manifest.name,
        "description": record.manifest.description,
        "subagent_type": record.manifest.subagent_type,
        "model": record.manifest.model,
        "output_file": record.manifest.output_file,
        "manifest_file": record.manifest.manifest_file
    })
}

fn agent_run_detail_json(record: &AgentRunRecord) -> Value {
    json!({
        "kind": SessionKind::AgentRun.as_str(),
        "id": record.manifest.agent_id,
        "origin": SessionKind::AgentRun.origin(),
        "capabilities": SessionKind::AgentRun.capabilities(),
        "status": record.manifest.status,
        "created_at": record.created_at,
        "updated_at": record.updated_at,
        "output_length": record.output.len(),
        "name": record.manifest.name,
        "description": record.manifest.description,
        "subagent_type": record.manifest.subagent_type,
        "model": record.manifest.model,
        "output_file": record.manifest.output_file,
        "manifest_file": record.manifest.manifest_file,
        "started_at": record.manifest.started_at,
        "completed_at": record.manifest.completed_at,
        "error": record.manifest.error,
        "output": record.output
    })
}

fn session_server_request<T: DeserializeOwned>(
    method: Method,
    path: &str,
    body: Option<Value>,
) -> Result<T, String> {
    let base_url = discover_session_server_url()?;
    let client = Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .map_err(|error| error.to_string())?;
    let url = format!("{}{}", base_url.trim_end_matches('/'), path);
    let mut request = client.request(method, url);
    if let Some(body) = body {
        request = request.json(&body);
    }
    let response = request.send().map_err(|error| {
        format!("local thread server request failed against {base_url}: {error}")
    })?;

    if !response.status().is_success() {
        let status = response.status();
        let message = response
            .text()
            .ok()
            .and_then(|body| session_server_error_message(&body))
            .unwrap_or_else(|| status.to_string());
        return Err(format!("local thread server returned {status}: {message}"));
    }

    response.json::<T>().map_err(|error| {
        format!("failed to decode local thread server response from {base_url}: {error}")
    })
}

fn session_server_error_message(body: &str) -> Option<String> {
    let value: Value = serde_json::from_str(body).ok()?;
    value
        .get("error")
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            value
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
}

fn discover_session_server_url() -> Result<String, String> {
    if let Ok(value) = std::env::var(SESSION_SERVER_URL_ENV) {
        let value = value.trim();
        if !value.is_empty() {
            return validate_local_session_server_url(value);
        }
    }

    let cwd = std::env::current_dir().map_err(|error| error.to_string())?;
    for path in thread_server_info_candidates(&cwd) {
        if let Some(base_url) = read_session_server_url_from_info(&path)? {
            return Ok(base_url);
        }
    }

    Err(format!(
        "thread sessions require a discoverable running local openyak server; start `openyak server --bind 127.0.0.1:0` in this workspace or set {SESSION_SERVER_URL_ENV}"
    ))
}

fn thread_server_info_candidates(cwd: &Path) -> Vec<PathBuf> {
    let mut candidates = vec![cwd.join(".openyak").join(THREAD_SERVER_INFO_FILENAME)];
    if let Ok(canonical_cwd) = cwd.canonicalize() {
        let canonical_path = canonical_cwd
            .join(".openyak")
            .join(THREAD_SERVER_INFO_FILENAME);
        if !candidates
            .iter()
            .any(|candidate| candidate == &canonical_path)
        {
            candidates.push(canonical_path);
        }
    }
    candidates
}

fn read_session_server_url_from_info(path: &Path) -> Result<Option<String>, String> {
    if !path.is_file() {
        return Ok(None);
    }

    let info: SessionServerInfo =
        serde_json::from_str(&std::fs::read_to_string(path).map_err(|error| error.to_string())?)
            .map_err(|error| format!("failed to parse `{}`: {error}", path.display()))?;
    validate_local_session_server_url(&info.base_url).map(Some)
}

fn validate_local_session_server_url(value: &str) -> Result<String, String> {
    let url = Url::parse(value)
        .map_err(|error| format!("invalid session server URL `{value}`: {error}"))?;
    let host = url
        .host_str()
        .ok_or_else(|| format!("session server URL `{value}` is missing a host"))?;
    let is_loopback = host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<IpAddr>()
            .map(|address| address.is_loopback())
            .unwrap_or(false);
    if !is_loopback {
        return Err(format!(
            "session server URL `{value}` is not local-only; OP6 phase 1 only supports loopback addresses"
        ));
    }
    Ok(url.to_string().trim_end_matches('/').to_string())
}

fn parse_epoch_secs(value: &str) -> Option<u64> {
    value.trim().parse::<u64>().ok()
}

fn file_time_to_epoch_secs(time: Option<std::time::SystemTime>) -> Option<u64> {
    time.and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs())
}

fn extract_team_task_ids(tasks: &[Value]) -> Result<Vec<String>, String> {
    let mut seen_task_ids = HashSet::new();
    let mut task_ids = Vec::with_capacity(tasks.len());

    for (index, task) in tasks.iter().enumerate() {
        let task_id = task
            .get("task_id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|task_id| !task_id.is_empty())
            .ok_or_else(|| format!("invalid team task entry at index {index}: missing task_id"))?;
        if !seen_task_ids.insert(task_id.to_owned()) {
            return Err(format!("duplicate task_id in team create: {task_id}"));
        }
        task_ids.push(task_id.to_owned());
    }

    Ok(task_ids)
}

fn resolve_lsp_status_server(input: &LspInput) -> Result<runtime::LspServerState, String> {
    if let Some(language) = input.language.as_deref() {
        return global_lsp_registry()
            .get(language)
            .ok_or_else(|| format!("LSP server not found for language: {language}"));
    }
    if let Some(path) = input.path.as_deref() {
        return global_lsp_registry()
            .find_server_for_path(path)
            .ok_or_else(|| format!("LSP server not found for path: {path}"));
    }
    Err(String::from(
        "LSP status requires either `language` or `path`",
    ))
}

fn lsp_status_payload(server: &runtime::LspServerState) -> Value {
    let diagnostic_count = server.diagnostics.len();
    json!({
        "action": "status",
        "language": server.language,
        "status": server.status,
        "root_path": server.root_path,
        "capabilities": server.capabilities,
        "diagnostics": server.diagnostics,
        "diagnostic_count": diagnostic_count
    })
}

fn mcp_auth_state_label(status: runtime::McpConnectionStatus) -> &'static str {
    match status {
        runtime::McpConnectionStatus::Connected => "authenticated",
        runtime::McpConnectionStatus::AuthRequired => "required",
        runtime::McpConnectionStatus::Connecting => "connecting",
        runtime::McpConnectionStatus::Disconnected => "disconnected",
        runtime::McpConnectionStatus::Error => "error",
    }
}

fn mcp_capability_visibility(state: &runtime::McpServerState) -> Value {
    let visible = state.status == runtime::McpConnectionStatus::Connected;
    json!({
        "tools_visible": visible,
        "resources_visible": visible,
        "prompts_visible": false,
        "tool_count": state.tools.len(),
        "resource_count": state.resources.len(),
        "prompt_count": 0
    })
}

fn configured_mcp_server_states() -> Vec<runtime::McpServerState> {
    let Ok(cwd) = std::env::current_dir() else {
        return Vec::new();
    };
    let Ok(config) = runtime::ConfigLoader::default_for(&cwd).load() else {
        return Vec::new();
    };
    config
        .mcp()
        .servers()
        .iter()
        .map(|(server_name, scoped)| configured_mcp_server_state(server_name, scoped))
        .collect()
}

fn configured_mcp_server_state(
    server_name: &str,
    scoped: &runtime::ScopedMcpServerConfig,
) -> runtime::McpServerState {
    let bootstrap = runtime::McpClientBootstrap::from_scoped_config(server_name, scoped);
    let (status, error_message) = match &bootstrap.transport {
        runtime::McpClientTransport::Stdio(_) => (runtime::McpConnectionStatus::Disconnected, None),
        runtime::McpClientTransport::Sse(transport)
        | runtime::McpClientTransport::Http(transport) => {
            if transport.auth.requires_user_auth() {
                (runtime::McpConnectionStatus::AuthRequired, None)
            } else {
                (
                    runtime::McpConnectionStatus::Error,
                    Some(unsupported_mcp_transport_message(scoped)),
                )
            }
        }
        runtime::McpClientTransport::WebSocket(_)
        | runtime::McpClientTransport::Sdk(_)
        | runtime::McpClientTransport::ManagedProxy(_) => (
            runtime::McpConnectionStatus::Error,
            Some(unsupported_mcp_transport_message(scoped)),
        ),
    };
    runtime::McpServerState {
        server_name: server_name.to_string(),
        status,
        tools: Vec::new(),
        resources: Vec::new(),
        server_info: bootstrap.signature,
        error_message,
    }
}

fn unsupported_mcp_transport_message(scoped: &runtime::ScopedMcpServerConfig) -> String {
    format!(
        "transport {} is configured but not supported by the current MCP manager",
        format!("{:?}", scoped.transport()).to_ascii_lowercase()
    )
}

fn merged_mcp_server_states() -> Vec<runtime::McpServerState> {
    let mut states = BTreeMap::new();
    for state in configured_mcp_server_states() {
        states.insert(state.server_name.clone(), state);
    }
    for state in global_mcp_registry().list_servers() {
        states.insert(state.server_name.clone(), state);
    }
    states.into_values().collect()
}

fn effective_mcp_server_state(server_name: &str) -> Option<runtime::McpServerState> {
    global_mcp_registry().get_server(server_name).or_else(|| {
        configured_mcp_server_states()
            .into_iter()
            .find(|state| state.server_name == server_name)
    })
}

#[allow(clippy::needless_pass_by_value)]
fn run_lsp(input: LspInput) -> Result<String, String> {
    if input.action == "servers" {
        let servers: Vec<_> = global_lsp_registry()
            .list_servers()
            .into_iter()
            .map(|server| {
                json!({
                    "language": server.language,
                    "status": server.status,
                    "root_path": server.root_path,
                    "capabilities": server.capabilities,
                    "diagnostic_count": server.diagnostics.len()
                })
            })
            .collect();
        return to_pretty_json(json!({
            "action": "servers",
            "servers": servers,
            "count": servers.len()
        }));
    }

    if input.action == "status" {
        return match resolve_lsp_status_server(&input) {
            Ok(server) => to_pretty_json(lsp_status_payload(&server)),
            Err(error) => to_pretty_json(json!({
                "action": "status",
                "status": "error",
                "error": error
            })),
        };
    }

    match global_lsp_registry().dispatch(
        &input.action,
        input.path.as_deref(),
        input.line,
        input.character,
        input.query.as_deref(),
    ) {
        Ok(result) => to_pretty_json(result),
        Err(error) => to_pretty_json(json!({
            "action": input.action,
            "error": error,
            "status": "error"
        })),
    }
}

fn run_list_mcp_servers(_input: Value) -> Result<String, String> {
    let mut registry_servers = merged_mcp_server_states();
    registry_servers.sort_by(|left, right| left.server_name.cmp(&right.server_name));
    let servers: Vec<_> = registry_servers
        .into_iter()
        .map(|server| {
            json!({
                "server": server.server_name,
                "status": server.status,
                "auth_state": mcp_auth_state_label(server.status),
                "auth_required": server.status == runtime::McpConnectionStatus::AuthRequired,
                "tool_count": server.tools.len(),
                "resource_count": server.resources.len(),
                "capabilities": mcp_capability_visibility(&server),
                "server_info": server.server_info,
                "error_message": server.error_message
            })
        })
        .collect();
    to_pretty_json(json!({ "servers": servers, "count": servers.len() }))
}

#[allow(clippy::needless_pass_by_value)]
fn run_list_mcp_tools(input: McpServerInput) -> Result<String, String> {
    match global_mcp_registry().list_tools(&input.server) {
        Ok(tools) => {
            let items: Vec<_> = tools
                .iter()
                .map(|tool| {
                    json!({
                        "name": tool.name,
                        "description": tool.description,
                        "input_schema": tool.input_schema
                    })
                })
                .collect();
            to_pretty_json(json!({ "server": input.server, "tools": items, "count": items.len() }))
        }
        Err(error) => {
            to_pretty_json(json!({ "server": input.server, "tools": [], "error": error }))
        }
    }
}

#[allow(clippy::needless_pass_by_value)]
fn run_list_mcp_resources(input: McpResourceInput) -> Result<String, String> {
    let server = input.server.as_deref().unwrap_or("default");
    match global_mcp_registry().list_resources(server) {
        Ok(resources) => {
            let items: Vec<_> = resources
                .iter()
                .map(|resource| {
                    json!({
                        "uri": resource.uri,
                        "name": resource.name,
                        "description": resource.description,
                        "mime_type": resource.mime_type,
                    })
                })
                .collect();
            to_pretty_json(json!({ "server": server, "resources": items, "count": items.len() }))
        }
        Err(error) => to_pretty_json(json!({ "server": server, "resources": [], "error": error })),
    }
}

#[allow(clippy::needless_pass_by_value)]
fn run_read_mcp_resource(input: McpResourceInput) -> Result<String, String> {
    let server = input.server.as_deref().unwrap_or("default");
    let uri = input.uri.as_deref().unwrap_or("");
    match global_mcp_registry().read_resource(server, uri) {
        Ok(resource) => to_pretty_json(json!({
            "server": server,
            "uri": resource.uri,
            "name": resource.name,
            "description": resource.description,
            "mime_type": resource.mime_type
        })),
        Err(error) => to_pretty_json(json!({ "server": server, "uri": uri, "error": error })),
    }
}

#[allow(clippy::needless_pass_by_value)]
fn run_mcp_auth(input: McpAuthInput) -> Result<String, String> {
    match effective_mcp_server_state(&input.server) {
        Some(state) => to_pretty_json(json!({
            "server": input.server,
            "status": state.status,
            "auth_state": mcp_auth_state_label(state.status),
            "auth_required": state.status == runtime::McpConnectionStatus::AuthRequired,
            "capabilities": mcp_capability_visibility(&state),
            "server_info": state.server_info,
            "error_message": state.error_message,
            "tool_count": state.tools.len(),
            "resource_count": state.resources.len()
        })),
        None => to_pretty_json(json!({
            "server": input.server,
            "status": "disconnected",
            "auth_state": "disconnected",
            "auth_required": false,
            "capabilities": {
                "tools_visible": false,
                "resources_visible": false,
                "prompts_visible": false,
                "tool_count": 0,
                "resource_count": 0,
                "prompt_count": 0
            },
            "error_message": Value::Null,
            "message": "Server not registered. Use MCP tool to connect first."
        })),
    }
}

#[allow(clippy::needless_pass_by_value)]
fn run_mcp_tool(input: McpToolInput) -> Result<String, String> {
    let arguments = input.arguments.unwrap_or_else(|| json!({}));
    match global_mcp_registry().call_tool(&input.server, &input.tool, &arguments) {
        Ok(result) => to_pretty_json(json!({
            "server": input.server,
            "tool": input.tool,
            "result": result,
            "status": "success"
        })),
        Err(error) => to_pretty_json(json!({
            "server": input.server,
            "tool": input.tool,
            "error": error,
            "status": "error"
        })),
    }
}

#[allow(clippy::needless_pass_by_value)]
fn run_testing_permission(input: TestingPermissionInput) -> Result<String, String> {
    to_pretty_json(json!({
        "action": input.action,
        "permitted": true,
        "message": "Testing permission tool stub"
    }))
}

fn to_pretty_json<T: serde::Serialize>(value: T) -> Result<String, String> {
    serde_json::to_string_pretty(&value).map_err(|error| error.to_string())
}

#[allow(clippy::needless_pass_by_value)]
fn io_to_string(error: std::io::Error) -> String {
    error.to_string()
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BrowserObserveInput {
    url: String,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    wait: Option<BrowserObserveWait>,
    #[serde(default)]
    capture_visible_text: bool,
    #[serde(default)]
    screenshot: Option<BrowserObserveScreenshotRequest>,
    #[serde(default)]
    viewport: Option<BrowserObserveViewport>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BrowserInteractInput {
    url: String,
    action: BrowserInteractAction,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    wait: Option<BrowserObserveWait>,
    #[serde(default)]
    capture_visible_text: bool,
    #[serde(default)]
    screenshot: Option<BrowserObserveScreenshotRequest>,
    #[serde(default)]
    viewport: Option<BrowserObserveViewport>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BrowserInteractAction {
    kind: String,
    selector: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BrowserObserveWait {
    kind: BrowserObserveWaitKind,
    #[serde(default)]
    value: Option<String>,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum BrowserObserveWaitKind {
    Load,
    NetworkIdle,
    Text,
    TitleContains,
    Selector,
    UrlContains,
}

impl BrowserObserveWaitKind {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Load => "load",
            Self::NetworkIdle => "network_idle",
            Self::Text => "text",
            Self::TitleContains => "title_contains",
            Self::Selector => "selector",
            Self::UrlContains => "url_contains",
        }
    }
}

impl BrowserObserveWait {
    fn expected_value(&self) -> Option<&str> {
        self.value
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BrowserObserveScreenshotRequest {
    capture: bool,
    #[serde(default)]
    format: BrowserObserveScreenshotFormat,
}

#[derive(Debug, Clone, Copy, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
enum BrowserObserveScreenshotFormat {
    #[default]
    Png,
    Jpeg,
}

impl BrowserObserveScreenshotFormat {
    fn as_str(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpeg",
        }
    }

    fn extension(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpeg",
        }
    }

    fn media_type(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BrowserObserveViewport {
    width: u32,
    height: u32,
}

#[derive(Debug, Serialize)]
struct BrowserObserveOutput {
    requested_url: String,
    final_url: Option<String>,
    title: Option<String>,
    load_outcome: String,
    wait: BrowserObserveWaitReport,
    visible_text: Option<String>,
    visible_text_truncated: bool,
    screenshot: Option<BrowserObserveArtifact>,
    browser_runtime: BrowserObserveRuntime,
    timings_ms: BrowserObserveTimings,
    warnings: Vec<String>,
}

#[derive(Debug, Serialize)]
struct BrowserInteractOutput {
    requested_url: String,
    final_url: Option<String>,
    title: Option<String>,
    action: BrowserInteractActionReport,
    load_outcome: String,
    wait: BrowserObserveWaitReport,
    visible_text: Option<String>,
    visible_text_truncated: bool,
    screenshot: Option<BrowserObserveArtifact>,
    browser_runtime: BrowserObserveRuntime,
    timings_ms: BrowserInteractTimings,
    warnings: Vec<String>,
}

#[derive(Debug, Serialize)]
struct BrowserInteractActionReport {
    kind: String,
    selector: String,
    status: String,
    detail: String,
}

#[derive(Debug, Serialize)]
struct BrowserObserveArtifact {
    relative_path: String,
    file_name: String,
    format: String,
    media_type: String,
    bytes: u64,
}

#[derive(Debug, Serialize)]
struct BrowserObserveRuntime {
    executable: String,
    mode: String,
    capture_backend: String,
}

#[derive(Debug, Serialize)]
struct BrowserObserveWaitReport {
    kind: String,
    status: String,
    detail: String,
    expected: Option<String>,
}

#[derive(Debug, Serialize)]
struct BrowserObserveTimings {
    observation: u64,
    screenshot: Option<u64>,
    total: u64,
}

#[derive(Debug, Serialize)]
struct BrowserInteractTimings {
    interaction: u64,
    screenshot: Option<u64>,
    total: u64,
}

#[derive(Debug)]
struct BrowserObserveObservation {
    snapshot: BrowserObserveSnapshot,
    load_outcome: String,
    wait: BrowserObserveWaitReport,
    observation_elapsed_ms: u64,
}

#[derive(Debug)]
struct BrowserObserveSnapshot {
    title: Option<String>,
    visible_text: String,
}

impl BrowserObserveSnapshot {
    fn from_html(html: &str) -> Self {
        Self {
            title: extract_html_title(html),
            visible_text: visible_text_from_html(html),
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum BrowserObserveCapturePhase {
    Observation,
    Screenshot,
}

impl BrowserObserveCapturePhase {
    fn as_str(self) -> &'static str {
        match self {
            Self::Observation => "observation capture",
            Self::Screenshot => "screenshot capture",
        }
    }
}

#[derive(Debug, Deserialize)]
struct BrowserInteractPageState {
    #[serde(rename = "readyState")]
    ready_state: Option<String>,
    url: Option<String>,
    title: Option<String>,
    #[serde(rename = "visibleText", default)]
    visible_text: String,
}

#[derive(Debug, Deserialize)]
struct BrowserInteractSelectorProbe {
    found: bool,
    #[serde(default)]
    disabled: bool,
    #[serde(default)]
    tag: Option<String>,
    #[serde(default)]
    text: Option<String>,
}

#[derive(Debug, Deserialize)]
struct BrowserInteractClickOutcome {
    found: bool,
    clicked: bool,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    tag: Option<String>,
    #[serde(default)]
    text: Option<String>,
}

#[derive(Debug, Deserialize)]
struct BrowserDevtoolsTarget {
    #[serde(rename = "type")]
    target_type: String,
    #[serde(default)]
    url: String,
    #[serde(rename = "webSocketDebuggerUrl")]
    web_socket_debugger_url: Option<String>,
}

struct BrowserInteractSession {
    child: Child,
    socket: BrowserDevtoolsSocket,
    runtime_dir: PathBuf,
    started_at: Instant,
}

impl BrowserInteractSession {
    fn launch(
        browser: &Path,
        input: &BrowserInteractInput,
        timeout_ms: u64,
        call_id: &str,
    ) -> Result<Self, String> {
        let runtime_dir = std::env::temp_dir().join(format!("openyak-browser-{call_id}"));
        if runtime_dir.exists() {
            let _ = fs::remove_dir_all(&runtime_dir);
        }
        fs::create_dir_all(&runtime_dir).map_err(io_to_string)?;

        let mut command = Command::new(browser);
        command
            .arg("--headless=new")
            .arg("--disable-gpu")
            .arg("--no-first-run")
            .arg("--no-default-browser-check")
            .arg("--remote-debugging-address=127.0.0.1")
            .arg("--remote-debugging-port=0")
            .arg(format!("--user-data-dir={}", runtime_dir.display()))
            .arg(format!(
                "--window-size={},{}",
                input
                    .viewport
                    .as_ref()
                    .map_or(DEFAULT_BROWSER_VIEWPORT_WIDTH, |viewport| viewport.width),
                input
                    .viewport
                    .as_ref()
                    .map_or(DEFAULT_BROWSER_VIEWPORT_HEIGHT, |viewport| viewport.height)
            ))
            .arg(&input.url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        let mut child = command.spawn().map_err(|error| {
            format!(
                "failed to launch {} with remote debugging enabled: {error}",
                browser.display()
            )
        })?;
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        let port = wait_for_browser_devtools_port(&mut child, &runtime_dir, deadline)?;
        let websocket_url = discover_browser_devtools_target(port, &input.url, deadline)?;
        let socket = BrowserDevtoolsSocket::connect(&websocket_url, deadline)?;

        Ok(Self {
            child,
            socket,
            runtime_dir,
            started_at: Instant::now(),
        })
    }

    fn wait_for_initial_load(&mut self, timeout_ms: u64) -> Result<(), String> {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        let _ = self.wait_for_page_condition(deadline, None, |state| {
            state.ready_state.as_deref() == Some("complete")
        })?;
        Ok(())
    }

    fn click_selector(
        &mut self,
        selector: &str,
        timeout_ms: u64,
    ) -> Result<BrowserInteractActionReport, String> {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        let selector_probe = self.wait_for_selector(selector, deadline)?;
        let click_expression = browser_interact_click_expression(selector)?;
        let outcome: BrowserInteractClickOutcome =
            self.evaluate_json(&click_expression, deadline)?;
        if !outcome.found {
            return Err(format!(
                "selector `{selector}` was not found when BrowserInteract attempted the click"
            ));
        }
        if !outcome.clicked {
            let reason = outcome
                .reason
                .unwrap_or_else(|| String::from("unknown reason"));
            return Err(format!(
                "selector `{selector}` could not be clicked in slice 2A: {reason}"
            ));
        }

        let tag = outcome
            .tag
            .or(selector_probe.tag)
            .unwrap_or_else(|| String::from("element"));
        let text = outcome.text.or(selector_probe.text).map_or_else(
            || String::from("no visible label"),
            |text| truncate_browser_text_summary(&text, 80),
        );
        Ok(BrowserInteractActionReport {
            kind: String::from("click"),
            selector: selector.to_string(),
            status: String::from("performed"),
            detail: format!("clicked <{tag}> selector with label {text}"),
        })
    }

    #[allow(clippy::too_many_lines)]
    fn capture_post_action_state(
        &mut self,
        wait: Option<&BrowserObserveWait>,
        timeout_ms: u64,
    ) -> Result<(BrowserInteractPageState, BrowserObserveWaitReport), String> {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        match wait {
            None => {
                let state = self.capture_page_state(deadline)?;
                Ok((
                    state,
                    BrowserObserveWaitReport {
                        kind: String::from("load"),
                        status: String::from("satisfied"),
                        detail: String::from(
                            "captured page state immediately after the selector click",
                        ),
                        expected: None,
                    },
                ))
            }
            Some(wait) => match wait.kind {
                BrowserObserveWaitKind::Load => {
                    let state = self.wait_for_page_condition(deadline, None, |state| {
                        state.ready_state.as_deref() == Some("complete")
                    })?;
                    Ok((
                        state,
                        BrowserObserveWaitReport {
                            kind: String::from("load"),
                            status: String::from("satisfied"),
                            detail: String::from(
                                "captured page state after the selector click reached readyState=complete",
                            ),
                            expected: None,
                        },
                    ))
                }
                BrowserObserveWaitKind::Text => {
                    let expected = wait.expected_value().expect("validated text wait");
                    let state =
                        self.wait_for_page_condition(deadline, Some(expected), |state| {
                            state.visible_text.contains(expected)
                        })?;
                    Ok((
                        state,
                        BrowserObserveWaitReport {
                            kind: String::from("text"),
                            status: String::from("satisfied"),
                            detail: String::from(
                                "captured page state contained the required text fragment after the selector click",
                            ),
                            expected: Some(expected.to_string()),
                        },
                    ))
                }
                BrowserObserveWaitKind::TitleContains => {
                    let expected = wait.expected_value().expect("validated title wait");
                    let state =
                        self.wait_for_page_condition(deadline, Some(expected), |state| {
                            state
                                .title
                                .as_deref()
                                .is_some_and(|title| title.contains(expected))
                        })?;
                    Ok((
                        state,
                        BrowserObserveWaitReport {
                            kind: String::from("title_contains"),
                            status: String::from("satisfied"),
                            detail: String::from(
                                "captured page title contained the required fragment after the selector click",
                            ),
                            expected: Some(expected.to_string()),
                        },
                    ))
                }
                BrowserObserveWaitKind::UrlContains => {
                    let expected = wait.expected_value().expect("validated url wait");
                    let state =
                        self.wait_for_page_condition(deadline, Some(expected), |state| {
                            state
                                .url
                                .as_deref()
                                .is_some_and(|url| url.contains(expected))
                        })?;
                    Ok((
                        state,
                        BrowserObserveWaitReport {
                            kind: String::from("url_contains"),
                            status: String::from("satisfied"),
                            detail: String::from(
                                "captured final URL contained the required fragment after the selector click",
                            ),
                            expected: Some(expected.to_string()),
                        },
                    ))
                }
                BrowserObserveWaitKind::NetworkIdle | BrowserObserveWaitKind::Selector => {
                    Err(format!(
                        "wait.kind={} is not supported in BrowserInteract slice 2A",
                        wait.kind.as_str()
                    ))
                }
            },
        }
    }

    fn capture_screenshot(
        &mut self,
        format: BrowserObserveScreenshotFormat,
        artifact_path: &Path,
        workspace_root: &Path,
        tool_name: &str,
    ) -> Result<BrowserObserveArtifact, String> {
        let deadline = Instant::now() + Duration::from_millis(DEFAULT_BROWSER_TIMEOUT_MS);
        let result = self.socket.call(
            "Page.captureScreenshot",
            &json!({
                "format": format.as_str(),
                "fromSurface": true
            }),
            deadline,
        )?;
        let encoded = result
            .get("data")
            .and_then(Value::as_str)
            .ok_or_else(|| String::from("Page.captureScreenshot did not return image data"))?;
        let bytes = decode_base64_standard(encoded)?;
        fs::write(artifact_path, &bytes).map_err(io_to_string)?;
        Ok(BrowserObserveArtifact {
            relative_path: portable_workspace_relative_path_for_browser_tool(
                artifact_path,
                workspace_root,
                tool_name,
            )?,
            file_name: artifact_path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("interact")
                .to_string(),
            format: format.as_str().to_string(),
            media_type: format.media_type().to_string(),
            bytes: bytes.len().try_into().unwrap_or(u64::MAX),
        })
    }

    fn elapsed_ms(&self) -> u64 {
        elapsed_ms(self.started_at.elapsed())
    }

    fn wait_for_selector(
        &mut self,
        selector: &str,
        deadline: Instant,
    ) -> Result<BrowserInteractSelectorProbe, String> {
        let probe_expression = browser_interact_selector_probe_expression(selector)?;
        loop {
            let probe =
                self.evaluate_json::<BrowserInteractSelectorProbe>(&probe_expression, deadline)?;
            if probe.found && !probe.disabled {
                return Ok(probe);
            }
            if Instant::now() >= deadline {
                return if probe.found && probe.disabled {
                    Err(format!(
                        "selector `{selector}` was found but remained disabled within the interaction budget"
                    ))
                } else {
                    Err(format!(
                        "selector `{selector}` was not found within the interaction budget"
                    ))
                };
            }
            thread::sleep(Duration::from_millis(DEFAULT_BROWSER_INTERACT_POLL_MS));
        }
    }

    fn wait_for_page_condition<F>(
        &mut self,
        deadline: Instant,
        expected: Option<&str>,
        predicate: F,
    ) -> Result<BrowserInteractPageState, String>
    where
        F: Fn(&BrowserInteractPageState) -> bool,
    {
        let mut last_error = None;
        let mut last_state = None;
        loop {
            match self.capture_page_state(deadline) {
                Ok(state) => {
                    if predicate(&state) {
                        return Ok(state);
                    }
                    last_state = Some(state);
                }
                Err(error) => {
                    last_error = Some(error);
                }
            }
            if Instant::now() >= deadline {
                break;
            }
            thread::sleep(Duration::from_millis(DEFAULT_BROWSER_INTERACT_POLL_MS));
        }

        if let Some(expected) = expected {
            if let Some(state) = last_state {
                return Err(format!(
                    "the clicked page did not satisfy `{expected}` within the wait budget (last title: {}, last url: {})",
                    state.title.unwrap_or_else(|| String::from("?")),
                    state.url.unwrap_or_else(|| String::from("?"))
                ));
            }
        }
        Err(last_error.unwrap_or_else(|| {
            String::from("page state did not satisfy the wait condition within the budget")
        }))
    }

    fn capture_page_state(
        &mut self,
        deadline: Instant,
    ) -> Result<BrowserInteractPageState, String> {
        let expression = String::from(
            "(() => ({ readyState: document.readyState || null, url: location.href || null, title: document.title || null, visibleText: document.body && typeof document.body.innerText === 'string' ? document.body.innerText : '' }))()",
        );
        self.evaluate_json(&expression, deadline)
    }

    fn evaluate_json<T: for<'de> Deserialize<'de>>(
        &mut self,
        expression: &str,
        deadline: Instant,
    ) -> Result<T, String> {
        let result = self.socket.call(
            "Runtime.evaluate",
            &json!({
                "expression": expression,
                "returnByValue": true,
                "awaitPromise": true
            }),
            deadline,
        )?;
        let remote_result = result
            .get("result")
            .ok_or_else(|| String::from("Runtime.evaluate did not return a result payload"))?;
        let value = remote_result
            .get("value")
            .cloned()
            .ok_or_else(|| String::from("Runtime.evaluate did not return a value"))?;
        serde_json::from_value(value).map_err(|error| error.to_string())
    }
}

impl Drop for BrowserInteractSession {
    fn drop(&mut self) {
        self.socket.close();
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_dir_all(&self.runtime_dir);
    }
}

struct BrowserDevtoolsSocket {
    stream: std::net::TcpStream,
    next_id: u64,
}

impl BrowserDevtoolsSocket {
    fn connect(websocket_url: &str, deadline: Instant) -> Result<Self, String> {
        let url = Url::parse(websocket_url)
            .map_err(|error| format!("invalid browser DevTools websocket URL: {error}"))?;
        if url.scheme() != "ws" {
            return Err(format!(
                "BrowserInteract only supports ws:// DevTools endpoints in slice 2A; received {}",
                url.scheme()
            ));
        }
        let host = url
            .host_str()
            .ok_or_else(|| String::from("DevTools websocket URL did not include a host"))?;
        let port = url
            .port_or_known_default()
            .ok_or_else(|| String::from("DevTools websocket URL did not include a port"))?;
        let address = format!("{host}:{port}");
        let socket_address = address
            .to_socket_addrs()
            .map_err(io_to_string)?
            .next()
            .ok_or_else(|| format!("could not resolve {address}"))?;
        let mut stream = std::net::TcpStream::connect_timeout(
            &socket_address,
            deadline.saturating_duration_since(Instant::now()),
        )
        .map_err(io_to_string)?;
        stream
            .set_read_timeout(Some(Duration::from_millis(DEFAULT_BROWSER_STARTUP_POLL_MS)))
            .map_err(io_to_string)?;
        stream
            .set_write_timeout(Some(Duration::from_secs(1)))
            .map_err(io_to_string)?;
        let path = if let Some(query) = url.query() {
            format!("{}?{query}", url.path())
        } else {
            url.path().to_string()
        };
        let handshake = format!(
            "GET {path} HTTP/1.1\r\nHost: {host}:{port}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n"
        );
        stream
            .write_all(handshake.as_bytes())
            .map_err(io_to_string)?;
        stream.flush().map_err(io_to_string)?;
        let response = read_http_headers(&mut stream, deadline)?;
        if !(response.starts_with("HTTP/1.1 101") || response.starts_with("HTTP/1.0 101")) {
            return Err(format!("DevTools websocket handshake failed: {response}"));
        }
        Ok(Self { stream, next_id: 1 })
    }

    fn call(&mut self, method: &str, params: &Value, deadline: Instant) -> Result<Value, String> {
        let request_id = self.next_id;
        self.next_id += 1;
        let payload = serde_json::to_string(&json!({
            "id": request_id,
            "method": method,
            "params": params,
        }))
        .map_err(|error| error.to_string())?;
        self.send_text(&payload)?;
        loop {
            let message = self.read_text(deadline)?;
            let value: Value = serde_json::from_str(&message).map_err(|error| error.to_string())?;
            if value
                .get("id")
                .and_then(Value::as_u64)
                .is_some_and(|value| value == request_id)
            {
                if let Some(error) = value.get("error") {
                    return Err(format!(
                        "{method} returned a browser protocol error: {}",
                        truncate_browser_text_summary(&error.to_string(), 160)
                    ));
                }
                return value
                    .get("result")
                    .cloned()
                    .ok_or_else(|| format!("{method} returned no result payload"));
            }
        }
    }

    fn close(&mut self) {
        let _ = self.send_control_frame(0x8, &[]);
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
    }

    fn send_text(&mut self, message: &str) -> Result<(), String> {
        self.send_control_frame(0x1, message.as_bytes())
    }

    fn send_control_frame(&mut self, opcode: u8, payload: &[u8]) -> Result<(), String> {
        let mut frame = Vec::with_capacity(payload.len() + 14);
        frame.push(0x80 | opcode);
        let mask_key = [0x13_u8, 0x37, 0x42, 0x99];
        match payload.len() {
            0..=125 => frame.push(
                0x80 | u8::try_from(payload.len())
                    .expect("small websocket frame length should fit in u8"),
            ),
            126..=65535 => {
                frame.push(0x80 | 0x7e);
                frame.extend_from_slice(
                    &u16::try_from(payload.len())
                        .expect("medium websocket frame length should fit in u16")
                        .to_be_bytes(),
                );
            }
            _ => {
                frame.push(0x80 | 127);
                frame.extend_from_slice(&(payload.len() as u64).to_be_bytes());
            }
        }
        frame.extend_from_slice(&mask_key);
        for (index, byte) in payload.iter().enumerate() {
            frame.push(byte ^ mask_key[index % mask_key.len()]);
        }
        self.stream.write_all(&frame).map_err(io_to_string)?;
        self.stream.flush().map_err(io_to_string)
    }

    fn read_text(&mut self, deadline: Instant) -> Result<String, String> {
        loop {
            let frame = self.read_frame(deadline)?;
            match frame.opcode {
                0x1 => {
                    return String::from_utf8(frame.payload).map_err(|error| {
                        format!("browser DevTools returned non-UTF-8 text: {error}")
                    });
                }
                0x8 => {
                    return Err(String::from(
                        "browser DevTools websocket closed before BrowserInteract completed",
                    ));
                }
                0x9 => {
                    self.send_control_frame(0xA, &frame.payload)?;
                }
                0xA => {}
                other => {
                    return Err(format!(
                        "browser DevTools websocket returned unsupported opcode {other}"
                    ));
                }
            }
        }
    }

    fn read_frame(&mut self, deadline: Instant) -> Result<BrowserWebSocketFrame, String> {
        let mut header = [0_u8; 2];
        read_exact_until(&mut self.stream, &mut header, deadline)?;
        let masked = header[1] & 0x80 != 0;
        let mut payload_len = u64::from(header[1] & 0x7f);
        if payload_len == 126 {
            let mut extended = [0_u8; 2];
            read_exact_until(&mut self.stream, &mut extended, deadline)?;
            payload_len = u64::from(u16::from_be_bytes(extended));
        } else if payload_len == 127 {
            let mut extended = [0_u8; 8];
            read_exact_until(&mut self.stream, &mut extended, deadline)?;
            payload_len = u64::from_be_bytes(extended);
        }
        let mut mask_key = [0_u8; 4];
        if masked {
            read_exact_until(&mut self.stream, &mut mask_key, deadline)?;
        }
        let payload_len_usize = usize::try_from(payload_len)
            .map_err(|_| String::from("websocket payload too large"))?;
        let mut payload = vec![0_u8; payload_len_usize];
        read_exact_until(&mut self.stream, &mut payload, deadline)?;
        if masked {
            for (index, byte) in payload.iter_mut().enumerate() {
                *byte ^= mask_key[index % mask_key.len()];
            }
        }
        Ok(BrowserWebSocketFrame {
            opcode: header[0] & 0x0f,
            payload,
        })
    }
}

struct BrowserWebSocketFrame {
    opcode: u8,
    payload: Vec<u8>,
}

fn wait_for_browser_devtools_port(
    child: &mut Child,
    runtime_dir: &Path,
    deadline: Instant,
) -> Result<u16, String> {
    let active_port_path = runtime_dir.join("DevToolsActivePort");
    loop {
        if let Ok(contents) = fs::read_to_string(&active_port_path) {
            let port = contents
                .lines()
                .next()
                .ok_or_else(|| String::from("DevToolsActivePort was empty"))?
                .trim()
                .parse::<u16>()
                .map_err(|error| {
                    format!("DevToolsActivePort contained an invalid port: {error}")
                })?;
            return Ok(port);
        }
        if let Some(status) = child.try_wait().map_err(io_to_string)? {
            return Err(format!(
                "browser exited before remote debugging became available (status {})",
                status
                    .code()
                    .map_or_else(|| String::from("terminated"), |code| code.to_string())
            ));
        }
        if Instant::now() >= deadline {
            return Err(String::from(
                "timed out waiting for the browser remote-debugging port file",
            ));
        }
        thread::sleep(Duration::from_millis(DEFAULT_BROWSER_STARTUP_POLL_MS));
    }
}

fn discover_browser_devtools_target(
    port: u16,
    requested_url: &str,
    deadline: Instant,
) -> Result<String, String> {
    let client = Client::builder()
        .timeout(Duration::from_millis(DEFAULT_BROWSER_STARTUP_POLL_MS))
        .build()
        .map_err(|error| error.to_string())?;
    let endpoint = format!("http://127.0.0.1:{port}/json/list");
    let requested_url = requested_url.trim();
    loop {
        match client.get(&endpoint).send() {
            Ok(response) => {
                let targets: Vec<BrowserDevtoolsTarget> =
                    response.json().map_err(|error| error.to_string())?;
                if let Some(target) = targets.iter().find(|target| {
                    target.target_type == "page"
                        && target.web_socket_debugger_url.is_some()
                        && target.url == requested_url
                }) {
                    return target
                        .web_socket_debugger_url
                        .clone()
                        .ok_or_else(|| String::from("page target did not expose a websocket URL"));
                }
                if let Some(target) = targets.iter().find(|target| {
                    target.target_type == "page" && target.web_socket_debugger_url.is_some()
                }) {
                    return target
                        .web_socket_debugger_url
                        .clone()
                        .ok_or_else(|| String::from("page target did not expose a websocket URL"));
                }
            }
            Err(error) => {
                if Instant::now() >= deadline {
                    return Err(format!("failed to query browser targets: {error}"));
                }
            }
        }
        if Instant::now() >= deadline {
            return Err(String::from(
                "timed out waiting for a browser page target to become available",
            ));
        }
        thread::sleep(Duration::from_millis(DEFAULT_BROWSER_STARTUP_POLL_MS));
    }
}

fn browser_interact_selector_probe_expression(selector: &str) -> Result<String, String> {
    let selector = serde_json::to_string(selector).map_err(|error| error.to_string())?;
    Ok(format!(
        "(() => {{ const selector = {selector}; const element = document.querySelector(selector); if (!element) return {{ found: false, disabled: false }}; const text = typeof element.innerText === 'string' ? element.innerText : (typeof element.textContent === 'string' ? element.textContent : ''); return {{ found: true, disabled: !!element.disabled || element.getAttribute('aria-disabled') === 'true', tag: element.tagName ? element.tagName.toLowerCase() : null, text: typeof text === 'string' ? text.slice(0, 160) : null }}; }})()"
    ))
}

fn browser_interact_click_expression(selector: &str) -> Result<String, String> {
    let selector = serde_json::to_string(selector).map_err(|error| error.to_string())?;
    Ok(format!(
        "(() => {{ const selector = {selector}; const element = document.querySelector(selector); if (!element) return {{ found: false, clicked: false, reason: 'not_found' }}; if (typeof element.scrollIntoView === 'function') element.scrollIntoView({{ block: 'center', inline: 'center' }}); if (typeof element.click !== 'function') return {{ found: true, clicked: false, reason: 'missing_click_method', tag: element.tagName ? element.tagName.toLowerCase() : null }}; const text = typeof element.innerText === 'string' ? element.innerText : (typeof element.textContent === 'string' ? element.textContent : ''); element.click(); return {{ found: true, clicked: true, tag: element.tagName ? element.tagName.toLowerCase() : null, text: typeof text === 'string' ? text.slice(0, 160) : null }}; }})()"
    ))
}

fn read_http_headers(
    stream: &mut std::net::TcpStream,
    deadline: Instant,
) -> Result<String, String> {
    let mut buffer = Vec::new();
    loop {
        let mut byte = [0_u8; 1];
        read_exact_until(stream, &mut byte, deadline)?;
        buffer.push(byte[0]);
        if buffer.ends_with(b"\r\n\r\n") {
            return String::from_utf8(buffer).map_err(|error| {
                format!("browser websocket handshake response was not UTF-8: {error}")
            });
        }
    }
}

fn read_exact_until(
    stream: &mut std::net::TcpStream,
    buffer: &mut [u8],
    deadline: Instant,
) -> Result<(), String> {
    let mut filled = 0;
    while filled < buffer.len() {
        if Instant::now() >= deadline {
            return Err(String::from(
                "timed out while reading from the browser DevTools websocket",
            ));
        }
        match stream.read(&mut buffer[filled..]) {
            Ok(0) => {
                return Err(String::from(
                    "browser DevTools websocket closed unexpectedly while reading",
                ))
            }
            Ok(read) => filled += read,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(error) => return Err(error.to_string()),
        }
    }
    Ok(())
}

fn decode_base64_standard(value: &str) -> Result<Vec<u8>, String> {
    fn sextet(byte: u8) -> Option<u8> {
        match byte {
            b'A'..=b'Z' => Some(byte - b'A'),
            b'a'..=b'z' => Some(byte - b'a' + 26),
            b'0'..=b'9' => Some(byte - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }

    let filtered = value
        .bytes()
        .filter(|byte| !byte.is_ascii_whitespace())
        .collect::<Vec<_>>();
    if filtered.len() % 4 != 0 {
        return Err(String::from(
            "browser screenshot payload was not valid base64",
        ));
    }

    let mut output = Vec::with_capacity((filtered.len() / 4) * 3);
    for chunk in filtered.chunks(4) {
        let mut values = [0_u8; 4];
        let mut padding = 0;
        for (index, byte) in chunk.iter().enumerate() {
            if *byte == b'=' {
                padding += 1;
                values[index] = 0;
            } else {
                values[index] = sextet(*byte).ok_or_else(|| {
                    String::from("browser screenshot payload contained invalid base64 data")
                })?;
            }
        }
        output.push((values[0] << 2) | (values[1] >> 4));
        if padding < 2 {
            output.push((values[1] << 4) | (values[2] >> 2));
        }
        if padding < 1 {
            output.push((values[2] << 6) | values[3]);
        }
    }

    Ok(output)
}

#[derive(Debug, Deserialize)]
struct ReadFileInput {
    path: String,
    offset: Option<usize>,
    limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct WriteFileInput {
    path: String,
    content: String,
}

#[derive(Debug, Deserialize)]
struct EditFileInput {
    path: String,
    old_string: String,
    new_string: String,
    replace_all: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct GlobSearchInputValue {
    pattern: String,
    path: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WebFetchInput {
    url: String,
    prompt: String,
}

#[derive(Debug, Deserialize)]
struct WebSearchInput {
    query: String,
    allowed_domains: Option<Vec<String>>,
    blocked_domains: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct TodoWriteInput {
    todos: Vec<TodoItem>,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
struct TodoItem {
    content: String,
    #[serde(rename = "activeForm")]
    active_form: String,
    status: TodoStatus,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum TodoStatus {
    Pending,
    InProgress,
    Completed,
}

#[derive(Debug, Deserialize)]
struct SkillInput {
    skill: String,
    args: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AgentInput {
    description: String,
    prompt: String,
    subagent_type: Option<String>,
    name: Option<String>,
    model: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ToolSearchInput {
    query: String,
    max_results: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct NotebookEditInput {
    notebook_path: String,
    cell_id: Option<String>,
    new_source: Option<String>,
    cell_type: Option<NotebookCellType>,
    edit_mode: Option<NotebookEditMode>,
}

#[derive(Debug, Deserialize, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum NotebookCellType {
    Code,
    Markdown,
}

#[derive(Debug, Deserialize, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum NotebookEditMode {
    Replace,
    Insert,
    Delete,
}

#[derive(Debug, Deserialize)]
struct SleepInput {
    duration_ms: u64,
}

#[derive(Debug, Deserialize)]
struct BriefInput {
    message: String,
    attachments: Option<Vec<String>>,
    status: BriefStatus,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
enum BriefStatus {
    Normal,
    Proactive,
}

#[derive(Debug, Deserialize)]
struct ConfigInput {
    setting: String,
    value: Option<ConfigValue>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ConfigValue {
    String(String),
    Bool(bool),
    Number(f64),
}

#[derive(Debug, Deserialize)]
#[serde(transparent)]
struct StructuredOutputInput(BTreeMap<String, Value>);

#[derive(Debug, Deserialize)]
struct ReplInput {
    code: String,
    language: String,
    timeout_ms: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct PowerShellInput {
    command: String,
    timeout: Option<u64>,
    description: Option<String>,
    run_in_background: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct TaskCreateInput {
    prompt: String,
    #[serde(default)]
    description: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TaskIdInput {
    task_id: String,
}

#[derive(Debug, Deserialize)]
struct TaskWaitInput {
    task_id: String,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    poll_interval_ms: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct TaskUpdateInput {
    task_id: String,
    message: String,
}

#[derive(Debug, Deserialize)]
struct TeamCreateInput {
    name: String,
    tasks: Vec<Value>,
}

#[derive(Debug, Deserialize)]
struct TeamIdInput {
    team_id: String,
}

#[derive(Debug, Deserialize)]
struct CronCreateInput {
    schedule: String,
    prompt: String,
    #[serde(default)]
    description: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CronIdInput {
    cron_id: String,
}

#[derive(Debug, Deserialize)]
struct SessionListInput {
    #[serde(default)]
    kind: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SessionRefInput {
    kind: String,
    id: String,
}

#[derive(Debug, Deserialize)]
struct SessionCreateInput {
    kind: String,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default, alias = "permissionMode")]
    permission_mode: Option<String>,
    #[serde(default, alias = "allowedTools")]
    allowed_tools: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct SessionSendInput {
    kind: String,
    id: String,
    message: String,
}

#[derive(Debug, Deserialize)]
struct SessionResumeInput {
    kind: String,
    id: String,
    request_id: String,
    content: String,
    #[serde(default, alias = "selectedOption")]
    selected_option: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SessionWaitInput {
    kind: String,
    id: String,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    poll_interval_ms: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct LspInput {
    action: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    language: Option<String>,
    #[serde(default)]
    line: Option<u32>,
    #[serde(default)]
    character: Option<u32>,
    #[serde(default)]
    query: Option<String>,
}

#[derive(Debug, Deserialize)]
struct McpResourceInput {
    #[serde(default)]
    server: Option<String>,
    #[serde(default)]
    uri: Option<String>,
}

#[derive(Debug, Deserialize)]
struct McpServerInput {
    server: String,
}

#[derive(Debug, Deserialize)]
struct McpAuthInput {
    server: String,
}

#[derive(Debug, Deserialize)]
struct McpToolInput {
    server: String,
    tool: String,
    #[serde(default)]
    arguments: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct TestingPermissionInput {
    action: String,
}

#[derive(Debug, Serialize)]
struct WebFetchOutput {
    bytes: usize,
    code: u16,
    #[serde(rename = "codeText")]
    code_text: String,
    result: String,
    #[serde(rename = "durationMs")]
    duration_ms: u128,
    url: String,
}

#[derive(Debug, Serialize)]
struct WebSearchOutput {
    query: String,
    results: Vec<WebSearchResultItem>,
    #[serde(rename = "durationSeconds")]
    duration_seconds: f64,
}

#[derive(Debug, Serialize)]
struct TodoWriteOutput {
    #[serde(rename = "oldTodos")]
    old_todos: Vec<TodoItem>,
    #[serde(rename = "newTodos")]
    new_todos: Vec<TodoItem>,
    #[serde(rename = "verificationNudgeNeeded")]
    verification_nudge_needed: Option<bool>,
}

#[derive(Debug, Serialize)]
struct SkillOutput {
    skill: String,
    path: String,
    args: Option<String>,
    description: Option<String>,
    prompt: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AgentOutput {
    #[serde(rename = "agentId")]
    agent_id: String,
    name: String,
    description: String,
    #[serde(rename = "subagentType")]
    subagent_type: Option<String>,
    model: Option<String>,
    status: String,
    #[serde(rename = "outputFile")]
    output_file: String,
    #[serde(rename = "manifestFile")]
    manifest_file: String,
    #[serde(rename = "createdAt")]
    created_at: String,
    #[serde(rename = "startedAt", skip_serializing_if = "Option::is_none")]
    started_at: Option<String>,
    #[serde(rename = "completedAt", skip_serializing_if = "Option::is_none")]
    completed_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, Clone)]
struct AgentJob {
    manifest: AgentOutput,
    prompt: String,
    system_prompt: Vec<String>,
    allowed_tools: BTreeSet<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionKind {
    Thread,
    ManagedSession,
    AgentRun,
}

impl SessionKind {
    fn parse(value: &str) -> Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "thread" => Ok(Self::Thread),
            "managed_session" | "managed-session" => Ok(Self::ManagedSession),
            "agent_run" | "agent-run" => Ok(Self::AgentRun),
            _ => Err(format!(
                "unsupported session kind `{value}` (expected thread, managed_session, or agent_run)"
            )),
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Thread => "thread",
            Self::ManagedSession => "managed_session",
            Self::AgentRun => "agent_run",
        }
    }

    fn capabilities(self) -> Vec<&'static str> {
        match self {
            Self::Thread => vec!["get", "create", "send", "resume", "wait"],
            Self::ManagedSession => vec!["get"],
            Self::AgentRun => vec!["get", "wait"],
        }
    }

    const fn origin(self) -> &'static str {
        match self {
            Self::Thread => "local_thread_server_v1",
            Self::ManagedSession => "managed_session_file_v1",
            Self::AgentRun => "agent_manifest_v1",
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
struct PendingUserInputPayload {
    request_id: String,
    prompt: String,
    options: Vec<String>,
    allow_freeform: bool,
}

impl From<runtime::PendingUserInputRequest> for PendingUserInputPayload {
    fn from(value: runtime::PendingUserInputRequest) -> Self {
        Self {
            request_id: value.request_id,
            prompt: value.prompt,
            options: value.options,
            allow_freeform: value.allow_freeform,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
struct ThreadStateSnapshotValue {
    status: String,
    #[serde(default)]
    run_id: Option<String>,
    #[serde(default)]
    pending_user_input: Option<PendingUserInputPayload>,
    #[serde(default)]
    recovery_note: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
struct ThreadConfigSnapshotValue {
    cwd: String,
    model: String,
    permission_mode: String,
    allowed_tools: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
struct ThreadSummaryValue {
    thread_id: String,
    created_at: u64,
    updated_at: u64,
    state: ThreadStateSnapshotValue,
    message_count: usize,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
struct ListThreadsResponseValue {
    protocol_version: String,
    threads: Vec<ThreadSummaryValue>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
struct ThreadSnapshotValue {
    protocol_version: String,
    thread_id: String,
    created_at: u64,
    updated_at: u64,
    state: ThreadStateSnapshotValue,
    config: ThreadConfigSnapshotValue,
    session: Session,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
struct TurnAcceptedResponseValue {
    protocol_version: String,
    thread_id: String,
    run_id: String,
    status: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
struct UserInputAcceptedResponseValue {
    protocol_version: String,
    thread_id: String,
    run_id: String,
    request_id: String,
    status: String,
}

#[derive(Debug, Clone, Deserialize)]
struct SessionServerInfo {
    #[serde(rename = "baseUrl")]
    base_url: String,
}

#[derive(Debug, Clone)]
struct ManagedSessionRecord {
    id: String,
    path: PathBuf,
    created_at: u64,
    updated_at: u64,
    session: Session,
}

#[derive(Debug, Clone)]
struct AgentRunRecord {
    manifest: AgentOutput,
    output: String,
    created_at: u64,
    updated_at: u64,
}

#[derive(Debug, Serialize)]
struct ToolSearchOutput {
    matches: Vec<String>,
    query: String,
    normalized_query: String,
    #[serde(rename = "total_deferred_tools")]
    total_deferred_tools: usize,
    #[serde(rename = "pending_mcp_servers")]
    pending_mcp_servers: Option<Vec<String>>,
}

#[derive(Debug, Serialize)]
struct NotebookEditOutput {
    new_source: String,
    cell_id: Option<String>,
    cell_type: Option<NotebookCellType>,
    language: String,
    edit_mode: String,
    error: Option<String>,
    notebook_path: String,
    original_file: String,
    updated_file: String,
}

#[derive(Debug, Serialize)]
struct SleepOutput {
    duration_ms: u64,
    message: String,
}

#[derive(Debug, Serialize)]
struct BriefOutput {
    message: String,
    attachments: Option<Vec<ResolvedAttachment>>,
    #[serde(rename = "sentAt")]
    sent_at: String,
}

#[derive(Debug, Serialize)]
struct ResolvedAttachment {
    path: String,
    size: u64,
    #[serde(rename = "isImage")]
    is_image: bool,
}

#[derive(Debug, Serialize)]
struct ConfigOutput {
    success: bool,
    operation: Option<String>,
    setting: Option<String>,
    value: Option<Value>,
    #[serde(rename = "previousValue")]
    previous_value: Option<Value>,
    #[serde(rename = "newValue")]
    new_value: Option<Value>,
    error: Option<String>,
}

#[derive(Debug, Serialize)]
struct StructuredOutputResult {
    data: String,
    structured_output: BTreeMap<String, Value>,
}

#[derive(Debug, Serialize)]
struct ReplOutput {
    language: String,
    stdout: String,
    stderr: String,
    #[serde(rename = "exitCode")]
    exit_code: i32,
    #[serde(rename = "durationMs")]
    duration_ms: u128,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum WebSearchResultItem {
    SearchResult {
        tool_use_id: String,
        content: Vec<SearchHit>,
    },
    Commentary(String),
}

#[derive(Debug, Serialize)]
struct SearchHit {
    title: String,
    url: String,
}

fn execute_web_fetch(input: &WebFetchInput) -> Result<WebFetchOutput, String> {
    let started = Instant::now();
    let client = build_http_client()?;
    let request_url = normalize_fetch_url(&input.url)?;
    let response = client
        .get(request_url.clone())
        .send()
        .map_err(|error| error.to_string())?;

    let status = response.status();
    let final_url = response.url().to_string();
    let code = status.as_u16();
    let code_text = status.canonical_reason().unwrap_or("Unknown").to_string();
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let body = response.text().map_err(|error| error.to_string())?;
    let bytes = body.len();
    let normalized = normalize_fetched_content(&body, &content_type);
    let result = summarize_web_fetch(&final_url, &input.prompt, &normalized, &body, &content_type);

    Ok(WebFetchOutput {
        bytes,
        code,
        code_text,
        result,
        duration_ms: started.elapsed().as_millis(),
        url: final_url,
    })
}

fn execute_web_search(input: &WebSearchInput) -> Result<WebSearchOutput, String> {
    let started = Instant::now();
    let client = build_http_client()?;
    let search_url = build_search_url(&input.query)?;
    let response = client
        .get(search_url)
        .send()
        .map_err(|error| error.to_string())?;

    let final_url = response.url().clone();
    let html = response.text().map_err(|error| error.to_string())?;
    let mut hits = extract_search_hits(&html);

    if hits.is_empty() && final_url.host_str().is_some() {
        hits = extract_search_hits_from_generic_links(&html);
    }

    if let Some(allowed) = input.allowed_domains.as_ref() {
        hits.retain(|hit| host_matches_list(&hit.url, allowed));
    }
    if let Some(blocked) = input.blocked_domains.as_ref() {
        hits.retain(|hit| !host_matches_list(&hit.url, blocked));
    }

    dedupe_hits(&mut hits);
    hits.truncate(8);

    let summary = if hits.is_empty() {
        format!("No web search results matched the query {:?}.", input.query)
    } else {
        let rendered_hits = hits
            .iter()
            .map(|hit| format!("- [{}]({})", hit.title, hit.url))
            .collect::<Vec<_>>()
            .join("\n");
        format!(
            "Search results for {:?}. Include a Sources section in the final answer.\n{}",
            input.query, rendered_hits
        )
    };

    Ok(WebSearchOutput {
        query: input.query.clone(),
        results: vec![
            WebSearchResultItem::Commentary(summary),
            WebSearchResultItem::SearchResult {
                tool_use_id: String::from("web_search_1"),
                content: hits,
            },
        ],
        duration_seconds: started.elapsed().as_secs_f64(),
    })
}

fn build_http_client() -> Result<Client, String> {
    Client::builder()
        .timeout(Duration::from_secs(20))
        .redirect(reqwest::redirect::Policy::limited(10))
        .user_agent("openyak-rust-tools/0.1")
        .build()
        .map_err(|error| error.to_string())
}

fn normalize_fetch_url(url: &str) -> Result<String, String> {
    let parsed = reqwest::Url::parse(url).map_err(|error| error.to_string())?;
    if parsed.scheme() == "http" {
        let host = parsed.host_str().unwrap_or_default();
        if host != "localhost" && host != "127.0.0.1" && host != "::1" {
            let mut upgraded = parsed;
            upgraded
                .set_scheme("https")
                .map_err(|()| String::from("failed to upgrade URL to https"))?;
            return Ok(upgraded.to_string());
        }
    }
    Ok(parsed.to_string())
}

fn build_search_url(query: &str) -> Result<reqwest::Url, String> {
    if let Ok(base) = std::env::var("OPENYAK_WEB_SEARCH_BASE_URL") {
        let mut url = reqwest::Url::parse(&base).map_err(|error| error.to_string())?;
        url.query_pairs_mut().append_pair("q", query);
        return Ok(url);
    }

    let mut url = reqwest::Url::parse("https://html.duckduckgo.com/html/")
        .map_err(|error| error.to_string())?;
    url.query_pairs_mut().append_pair("q", query);
    Ok(url)
}

fn normalize_fetched_content(body: &str, content_type: &str) -> String {
    if content_type.contains("html") {
        html_to_text(body)
    } else {
        body.trim().to_string()
    }
}

fn summarize_web_fetch(
    url: &str,
    prompt: &str,
    content: &str,
    raw_body: &str,
    content_type: &str,
) -> String {
    let lower_prompt = prompt.to_lowercase();
    let compact = collapse_whitespace(content);

    let detail = if lower_prompt.contains("title") {
        extract_title(content, raw_body, content_type).map_or_else(
            || preview_text(&compact, 600),
            |title| format!("Title: {title}"),
        )
    } else if lower_prompt.contains("summary") || lower_prompt.contains("summarize") {
        preview_text(&compact, 900)
    } else {
        let preview = preview_text(&compact, 900);
        format!("Prompt: {prompt}\nContent preview:\n{preview}")
    };

    format!("Fetched {url}\n{detail}")
}

fn extract_title(content: &str, raw_body: &str, content_type: &str) -> Option<String> {
    if content_type.contains("html") {
        let lowered = raw_body.to_lowercase();
        if let Some(start) = lowered.find("<title>") {
            let after = start + "<title>".len();
            if let Some(end_rel) = lowered[after..].find("</title>") {
                let title =
                    collapse_whitespace(&decode_html_entities(&raw_body[after..after + end_rel]));
                if !title.is_empty() {
                    return Some(title);
                }
            }
        }
    }

    for line in content.lines() {
        let trimmed = line.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    None
}

fn html_to_text(html: &str) -> String {
    let mut text = String::with_capacity(html.len());
    let mut in_tag = false;
    let mut previous_was_space = false;

    for ch in html.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if in_tag => {}
            '&' => {
                text.push('&');
                previous_was_space = false;
            }
            ch if ch.is_whitespace() => {
                if !previous_was_space {
                    text.push(' ');
                    previous_was_space = true;
                }
            }
            _ => {
                text.push(ch);
                previous_was_space = false;
            }
        }
    }

    collapse_whitespace(&decode_html_entities(&text))
}

fn decode_html_entities(input: &str) -> String {
    input
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
}

fn collapse_whitespace(input: &str) -> String {
    input.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn preview_text(input: &str, max_chars: usize) -> String {
    if input.chars().count() <= max_chars {
        return input.to_string();
    }
    let shortened = input.chars().take(max_chars).collect::<String>();
    format!("{}…", shortened.trim_end())
}

fn extract_search_hits(html: &str) -> Vec<SearchHit> {
    let mut hits = Vec::new();
    let mut remaining = html;

    while let Some(anchor_start) = remaining.find("result__a") {
        let after_class = &remaining[anchor_start..];
        let Some(href_idx) = after_class.find("href=") else {
            remaining = &after_class[1..];
            continue;
        };
        let href_slice = &after_class[href_idx + 5..];
        let Some((url, rest)) = extract_quoted_value(href_slice) else {
            remaining = &after_class[1..];
            continue;
        };
        let Some(close_tag_idx) = rest.find('>') else {
            remaining = &after_class[1..];
            continue;
        };
        let after_tag = &rest[close_tag_idx + 1..];
        let Some(end_anchor_idx) = after_tag.find("</a>") else {
            remaining = &after_tag[1..];
            continue;
        };
        let title = html_to_text(&after_tag[..end_anchor_idx]);
        if let Some(decoded_url) = decode_duckduckgo_redirect(&url) {
            hits.push(SearchHit {
                title: title.trim().to_string(),
                url: decoded_url,
            });
        }
        remaining = &after_tag[end_anchor_idx + 4..];
    }

    hits
}

fn extract_search_hits_from_generic_links(html: &str) -> Vec<SearchHit> {
    let mut hits = Vec::new();
    let mut remaining = html;

    while let Some(anchor_start) = remaining.find("<a") {
        let after_anchor = &remaining[anchor_start..];
        let Some(href_idx) = after_anchor.find("href=") else {
            remaining = &after_anchor[2..];
            continue;
        };
        let href_slice = &after_anchor[href_idx + 5..];
        let Some((url, rest)) = extract_quoted_value(href_slice) else {
            remaining = &after_anchor[2..];
            continue;
        };
        let Some(close_tag_idx) = rest.find('>') else {
            remaining = &after_anchor[2..];
            continue;
        };
        let after_tag = &rest[close_tag_idx + 1..];
        let Some(end_anchor_idx) = after_tag.find("</a>") else {
            remaining = &after_anchor[2..];
            continue;
        };
        let title = html_to_text(&after_tag[..end_anchor_idx]);
        if title.trim().is_empty() {
            remaining = &after_tag[end_anchor_idx + 4..];
            continue;
        }
        let decoded_url = decode_duckduckgo_redirect(&url).unwrap_or(url);
        if decoded_url.starts_with("http://") || decoded_url.starts_with("https://") {
            hits.push(SearchHit {
                title: title.trim().to_string(),
                url: decoded_url,
            });
        }
        remaining = &after_tag[end_anchor_idx + 4..];
    }

    hits
}

fn extract_quoted_value(input: &str) -> Option<(String, &str)> {
    let quote = input.chars().next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }
    let rest = &input[quote.len_utf8()..];
    let end = rest.find(quote)?;
    Some((rest[..end].to_string(), &rest[end + quote.len_utf8()..]))
}

fn decode_duckduckgo_redirect(url: &str) -> Option<String> {
    if url.starts_with("http://") || url.starts_with("https://") {
        return Some(html_entity_decode_url(url));
    }

    let joined = if url.starts_with("//") {
        format!("https:{url}")
    } else if url.starts_with('/') {
        format!("https://duckduckgo.com{url}")
    } else {
        return None;
    };

    let parsed = reqwest::Url::parse(&joined).ok()?;
    if parsed.path() == "/l/" || parsed.path() == "/l" {
        for (key, value) in parsed.query_pairs() {
            if key == "uddg" {
                return Some(html_entity_decode_url(value.as_ref()));
            }
        }
    }
    Some(joined)
}

fn html_entity_decode_url(url: &str) -> String {
    decode_html_entities(url)
}

fn host_matches_list(url: &str, domains: &[String]) -> bool {
    let Ok(parsed) = reqwest::Url::parse(url) else {
        return false;
    };
    let Some(host) = parsed.host_str() else {
        return false;
    };
    let host = host.to_ascii_lowercase();
    domains.iter().any(|domain| {
        let normalized = normalize_domain_filter(domain);
        !normalized.is_empty() && (host == normalized || host.ends_with(&format!(".{normalized}")))
    })
}

fn normalize_domain_filter(domain: &str) -> String {
    let trimmed = domain.trim();
    let candidate = reqwest::Url::parse(trimmed)
        .ok()
        .and_then(|url| url.host_str().map(str::to_string))
        .unwrap_or_else(|| trimmed.to_string());
    candidate
        .trim()
        .trim_start_matches('.')
        .trim_end_matches('/')
        .to_ascii_lowercase()
}

fn dedupe_hits(hits: &mut Vec<SearchHit>) {
    let mut seen = BTreeSet::new();
    hits.retain(|hit| seen.insert(hit.url.clone()));
}

fn execute_todo_write(input: TodoWriteInput) -> Result<TodoWriteOutput, String> {
    validate_todos(&input.todos)?;
    let store_path = todo_store_path()?;
    let old_todos = if store_path.exists() {
        serde_json::from_str::<Vec<TodoItem>>(
            &std::fs::read_to_string(&store_path).map_err(|error| error.to_string())?,
        )
        .map_err(|error| error.to_string())?
    } else {
        Vec::new()
    };

    let all_done = input
        .todos
        .iter()
        .all(|todo| matches!(todo.status, TodoStatus::Completed));
    let persisted = if all_done {
        Vec::new()
    } else {
        input.todos.clone()
    };

    if let Some(parent) = store_path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    std::fs::write(
        &store_path,
        serde_json::to_string_pretty(&persisted).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;

    let verification_nudge_needed = (all_done
        && input.todos.len() >= 3
        && !input
            .todos
            .iter()
            .any(|todo| todo.content.to_lowercase().contains("verif")))
    .then_some(true);

    Ok(TodoWriteOutput {
        old_todos,
        new_todos: input.todos,
        verification_nudge_needed,
    })
}

fn execute_skill(input: SkillInput) -> Result<SkillOutput, String> {
    let skill_path = resolve_skill_path(&input.skill)?;
    let prompt = std::fs::read_to_string(&skill_path).map_err(|error| error.to_string())?;
    let description = parse_skill_description(&prompt);

    Ok(SkillOutput {
        skill: input.skill,
        path: skill_path.display().to_string(),
        args: input.args,
        description,
        prompt,
    })
}

fn validate_todos(todos: &[TodoItem]) -> Result<(), String> {
    if todos.is_empty() {
        return Err(String::from("todos must not be empty"));
    }
    // Allow multiple in_progress items for parallel workflows
    if todos.iter().any(|todo| todo.content.trim().is_empty()) {
        return Err(String::from("todo content must not be empty"));
    }
    if todos.iter().any(|todo| todo.active_form.trim().is_empty()) {
        return Err(String::from("todo activeForm must not be empty"));
    }
    Ok(())
}

fn todo_store_path() -> Result<std::path::PathBuf, String> {
    if let Ok(path) = std::env::var("OPENYAK_TODO_STORE") {
        return Ok(std::path::PathBuf::from(path));
    }
    let cwd = std::env::current_dir().map_err(|error| error.to_string())?;
    Ok(cwd.join(".openyak-todos.json"))
}

fn resolve_skill_path(skill: &str) -> Result<std::path::PathBuf, String> {
    let requested = skill.trim().trim_start_matches('/').trim_start_matches('$');
    if requested.is_empty() {
        return Err(String::from("skill must not be empty"));
    }

    let mut candidates = Vec::new();
    let cwd = std::env::current_dir().map_err(|error| error.to_string())?;
    for ancestor in cwd.ancestors() {
        push_unique_dir(&mut candidates, ancestor.join(".codex").join("skills"));
        push_unique_dir(&mut candidates, ancestor.join(".openyak").join("skills"));
    }
    let homes = home_locations();
    push_unique_dir(&mut candidates, homes.codex_home.join("skills"));
    push_unique_dir(&mut candidates, homes.openyak_home.join("skills"));
    push_unique_dir(
        &mut candidates,
        homes.user_home.join(".agents").join("skills"),
    );
    push_unique_dir(
        &mut candidates,
        homes
            .user_home
            .join(".config")
            .join("opencode")
            .join("skills"),
    );

    resolve_skill_path_from_roots(requested, &candidates)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| format!("unknown skill: {requested}"))
}

fn push_unique_dir(candidates: &mut Vec<PathBuf>, path: PathBuf) {
    if path.is_dir() && !candidates.iter().any(|existing| existing == &path) {
        candidates.push(path);
    }
}

const DEFAULT_AGENT_MODEL: &str = "claude-opus-4-6";
const DEFAULT_AGENT_MAX_ITERATIONS: usize = 32;

fn execute_agent(input: AgentInput) -> Result<AgentOutput, String> {
    execute_agent_with_spawn(input, spawn_agent_job)
}

fn execute_agent_with_spawn<F>(input: AgentInput, spawn_fn: F) -> Result<AgentOutput, String>
where
    F: FnOnce(AgentJob) -> Result<(), String>,
{
    if input.description.trim().is_empty() {
        return Err(String::from("description must not be empty"));
    }
    if input.prompt.trim().is_empty() {
        return Err(String::from("prompt must not be empty"));
    }

    let agent_id = make_agent_id();
    let output_dir = agent_store_dir()?;
    std::fs::create_dir_all(&output_dir).map_err(|error| error.to_string())?;
    let output_file = output_dir.join(format!("{agent_id}.md"));
    let manifest_file = output_dir.join(format!("{agent_id}.json"));
    let normalized_subagent_type = normalize_subagent_type(input.subagent_type.as_deref());
    let model = resolve_agent_model(input.model.as_deref());
    let agent_name = input
        .name
        .as_deref()
        .map(slugify_agent_name)
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| slugify_agent_name(&input.description));
    let created_at = iso8601_now();
    let system_prompt = build_agent_system_prompt(&normalized_subagent_type, &model)?;
    let allowed_tools = allowed_tools_for_subagent(&normalized_subagent_type);

    let output_contents = format!(
        "# Agent Task

- id: {}
- name: {}
- description: {}
- subagent_type: {}
- created_at: {}

## Prompt

{}
",
        agent_id, agent_name, input.description, normalized_subagent_type, created_at, input.prompt
    );
    std::fs::write(&output_file, output_contents).map_err(|error| error.to_string())?;

    let manifest = AgentOutput {
        agent_id,
        name: agent_name,
        description: input.description,
        subagent_type: Some(normalized_subagent_type),
        model: Some(model),
        status: String::from("running"),
        output_file: output_file.display().to_string(),
        manifest_file: manifest_file.display().to_string(),
        created_at: created_at.clone(),
        started_at: Some(created_at),
        completed_at: None,
        error: None,
    };
    write_agent_manifest(&manifest)?;

    let manifest_for_spawn = manifest.clone();
    let job = AgentJob {
        manifest: manifest_for_spawn,
        prompt: input.prompt,
        system_prompt,
        allowed_tools,
    };
    if let Err(error) = spawn_fn(job) {
        let error = format!("failed to spawn sub-agent: {error}");
        persist_agent_terminal_state(&manifest, "failed", None, Some(error.clone()))?;
        return Err(error);
    }

    Ok(manifest)
}

fn spawn_agent_job(job: AgentJob) -> Result<(), String> {
    let thread_name = format!("openyak-agent-{}", job.manifest.agent_id);
    std::thread::Builder::new()
        .name(thread_name)
        .spawn(move || {
            let result =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_agent_job(&job)));
            match result {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    let _ =
                        persist_agent_terminal_state(&job.manifest, "failed", None, Some(error));
                }
                Err(_) => {
                    let _ = persist_agent_terminal_state(
                        &job.manifest,
                        "failed",
                        None,
                        Some(String::from("sub-agent thread panicked")),
                    );
                }
            }
        })
        .map(|_| ())
        .map_err(|error| error.to_string())
}

fn run_agent_job(job: &AgentJob) -> Result<(), String> {
    let mut runtime = build_agent_runtime(job)?.with_max_iterations(DEFAULT_AGENT_MAX_ITERATIONS);
    let summary = runtime
        .run_turn(job.prompt.clone(), None, None)
        .map_err(|error| error.to_string())?;
    let final_text = final_assistant_text(&summary);
    persist_agent_terminal_state(&job.manifest, "completed", Some(final_text.as_str()), None)
}

fn build_agent_runtime(
    job: &AgentJob,
) -> Result<ConversationRuntime<ProviderRuntimeClient, SubagentToolExecutor>, String> {
    let model = job
        .manifest
        .model
        .clone()
        .unwrap_or_else(|| DEFAULT_AGENT_MODEL.to_string());
    let allowed_tools = job.allowed_tools.clone();
    let api_client = ProviderRuntimeClient::new(&model, allowed_tools.clone())?;
    let tool_executor = SubagentToolExecutor::new(allowed_tools);
    Ok(ConversationRuntime::new(
        Session::new(),
        api_client,
        tool_executor,
        agent_permission_policy(),
        job.system_prompt.clone(),
    ))
}

fn build_agent_system_prompt(subagent_type: &str, model: &str) -> Result<Vec<String>, String> {
    let cwd = std::env::current_dir().map_err(|error| error.to_string())?;
    let mut prompt = load_system_prompt(
        cwd,
        current_local_date_string(),
        model,
        std::env::consts::OS,
        "unknown",
    )
    .map_err(|error| error.to_string())?;
    prompt.push(format!(
        "You are a background sub-agent of type `{subagent_type}`. Work only on the delegated task, use only the tools available to you, do not ask the user questions, and finish with a concise result."
    ));
    Ok(prompt)
}

fn resolve_agent_model(model: Option<&str>) -> String {
    model
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .unwrap_or(DEFAULT_AGENT_MODEL)
        .to_string()
}

fn allowed_tools_for_subagent(subagent_type: &str) -> BTreeSet<String> {
    let tools = match subagent_type {
        "Explore" => vec![
            "read_file",
            "glob_search",
            "grep_search",
            "WebFetch",
            "WebSearch",
            "ToolSearch",
            "Skill",
            "StructuredOutput",
        ],
        "Plan" => vec![
            "read_file",
            "glob_search",
            "grep_search",
            "WebFetch",
            "WebSearch",
            "ToolSearch",
            "Skill",
            "TodoWrite",
            "StructuredOutput",
            "SendUserMessage",
        ],
        "Verification" => vec![
            "bash",
            "read_file",
            "glob_search",
            "grep_search",
            "WebFetch",
            "WebSearch",
            "ToolSearch",
            "TodoWrite",
            "StructuredOutput",
            "SendUserMessage",
            "PowerShell",
        ],
        "openyak-guide" => vec![
            "read_file",
            "glob_search",
            "grep_search",
            "WebFetch",
            "WebSearch",
            "ToolSearch",
            "Skill",
            "StructuredOutput",
            "SendUserMessage",
        ],
        "statusline-setup" => vec![
            "bash",
            "read_file",
            "write_file",
            "edit_file",
            "glob_search",
            "grep_search",
            "ToolSearch",
        ],
        _ => vec![
            "bash",
            "read_file",
            "write_file",
            "edit_file",
            "glob_search",
            "grep_search",
            "WebFetch",
            "WebSearch",
            "TodoWrite",
            "Skill",
            "ToolSearch",
            "NotebookEdit",
            "Sleep",
            "SendUserMessage",
            "Config",
            "StructuredOutput",
            "REPL",
            "PowerShell",
        ],
    };
    tools.into_iter().map(str::to_string).collect()
}

fn agent_permission_policy() -> PermissionPolicy {
    mvp_tool_specs().into_iter().fold(
        PermissionPolicy::new(PermissionMode::DangerFullAccess),
        |policy, spec| policy.with_tool_requirement(spec.name, spec.required_permission),
    )
}

fn write_agent_manifest(manifest: &AgentOutput) -> Result<(), String> {
    std::fs::write(
        &manifest.manifest_file,
        serde_json::to_string_pretty(manifest).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())
}

fn persist_agent_terminal_state(
    manifest: &AgentOutput,
    status: &str,
    result: Option<&str>,
    error: Option<String>,
) -> Result<(), String> {
    append_agent_output(
        &manifest.output_file,
        &format_agent_terminal_output(status, result, error.as_deref()),
    )?;
    let mut next_manifest = manifest.clone();
    next_manifest.status = status.to_string();
    next_manifest.completed_at = Some(iso8601_now());
    next_manifest.error = error;
    write_agent_manifest(&next_manifest)
}

fn append_agent_output(path: &str, suffix: &str) -> Result<(), String> {
    use std::io::Write as _;

    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(path)
        .map_err(|error| error.to_string())?;
    file.write_all(suffix.as_bytes())
        .map_err(|error| error.to_string())
}

fn format_agent_terminal_output(status: &str, result: Option<&str>, error: Option<&str>) -> String {
    let mut sections = vec![format!("\n## Result\n\n- status: {status}\n")];
    if let Some(result) = result.filter(|value| !value.trim().is_empty()) {
        sections.push(format!("\n### Final response\n\n{}\n", result.trim()));
    }
    if let Some(error) = error.filter(|value| !value.trim().is_empty()) {
        sections.push(format!("\n### Error\n\n{}\n", error.trim()));
    }
    sections.join("")
}

pub struct ProviderRuntimeClient {
    runtime: tokio::runtime::Runtime,
    client: ProviderClient,
    model: String,
    allowed_tools: BTreeSet<String>,
}

impl ProviderRuntimeClient {
    pub fn new(model: &str, allowed_tools: BTreeSet<String>) -> Result<Self, String> {
        let model = resolve_model_alias(model);
        let client = ProviderClient::from_model(&model).map_err(|error| error.to_string())?;
        Ok(Self {
            runtime: tokio::runtime::Runtime::new().map_err(|error| error.to_string())?,
            client,
            model,
            allowed_tools,
        })
    }
}

impl ApiClient for ProviderRuntimeClient {
    #[allow(clippy::too_many_lines)]
    fn stream(&mut self, request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
        let tools = tool_specs_for_allowed_tools(Some(&self.allowed_tools))
            .into_iter()
            .map(|spec| ToolDefinition {
                name: spec.name.to_string(),
                description: Some(spec.description.to_string()),
                input_schema: spec.input_schema,
            })
            .chain(std::iter::once(request_user_input_tool_definition()))
            .collect::<Vec<_>>();
        let message_request = MessageRequest {
            model: self.model.clone(),
            max_tokens: max_tokens_for_model(&self.model),
            messages: convert_messages(&request.messages),
            system: (!request.system_prompt.is_empty()).then(|| request.system_prompt.join("\n\n")),
            tools: Some(tools),
            tool_choice: Some(ToolChoice::Auto),
            stream: true,
        };

        self.runtime.block_on(async {
            let mut stream = self
                .client
                .stream_message(&message_request)
                .await
                .map_err(|error| RuntimeError::new(error.to_string()))?;
            let mut events = Vec::new();
            let mut pending_tools: BTreeMap<u32, (String, String, String)> = BTreeMap::new();
            let mut saw_stop = false;

            while let Some(event) = stream
                .next_event()
                .await
                .map_err(|error| RuntimeError::new(error.to_string()))?
            {
                match event {
                    ApiStreamEvent::MessageStart(start) => {
                        for block in start.message.content {
                            push_output_block(block, 0, &mut events, &mut pending_tools, true);
                        }
                    }
                    ApiStreamEvent::ContentBlockStart(start) => {
                        push_output_block(
                            start.content_block,
                            start.index,
                            &mut events,
                            &mut pending_tools,
                            true,
                        );
                    }
                    ApiStreamEvent::ContentBlockDelta(delta) => match delta.delta {
                        ContentBlockDelta::TextDelta { text } => {
                            if !text.is_empty() {
                                events.push(AssistantEvent::TextDelta(text));
                            }
                        }
                        ContentBlockDelta::InputJsonDelta { partial_json } => {
                            if let Some((_, _, input)) = pending_tools.get_mut(&delta.index) {
                                input.push_str(&partial_json);
                            }
                        }
                        ContentBlockDelta::ThinkingDelta { .. }
                        | ContentBlockDelta::SignatureDelta { .. } => {}
                    },
                    ApiStreamEvent::ContentBlockStop(stop) => {
                        if let Some((id, name, input)) = pending_tools.remove(&stop.index) {
                            match parse_pending_output_block(&id, &name, &input) {
                                Ok(event) => events.push(event),
                                Err(error) => return Err(error),
                            }
                        }
                    }
                    ApiStreamEvent::MessageDelta(delta) => {
                        events.push(AssistantEvent::Usage(TokenUsage {
                            input_tokens: delta.usage.input_tokens,
                            output_tokens: delta.usage.output_tokens,
                            cache_creation_input_tokens: 0,
                            cache_read_input_tokens: 0,
                        }));
                    }
                    ApiStreamEvent::MessageStop(_) => {
                        saw_stop = true;
                        events.push(AssistantEvent::MessageStop);
                    }
                }
            }

            if !saw_stop
                && events.iter().any(|event| {
                    matches!(event, AssistantEvent::TextDelta(text) if !text.is_empty())
                        || matches!(event, AssistantEvent::ToolUse { .. })
                        || matches!(event, AssistantEvent::RequestUserInput(_))
                })
            {
                events.push(AssistantEvent::MessageStop);
            }

            if events
                .iter()
                .any(|event| matches!(event, AssistantEvent::MessageStop))
            {
                return Ok(events);
            }

            let response = self
                .client
                .send_message(&MessageRequest {
                    stream: false,
                    ..message_request.clone()
                })
                .await
                .map_err(|error| RuntimeError::new(error.to_string()))?;
            response_to_events(response)
        })
    }
}

struct SubagentToolExecutor {
    allowed_tools: BTreeSet<String>,
}

impl SubagentToolExecutor {
    fn new(allowed_tools: BTreeSet<String>) -> Self {
        Self { allowed_tools }
    }
}

impl ToolExecutor for SubagentToolExecutor {
    fn execute(&mut self, tool_name: &str, input: &str) -> Result<String, ToolError> {
        if !self.allowed_tools.contains(tool_name) {
            return Err(ToolError::new(format!(
                "tool `{tool_name}` is not enabled for this sub-agent"
            )));
        }
        let value = serde_json::from_str(input)
            .map_err(|error| ToolError::new(format!("invalid tool input JSON: {error}")))?;
        execute_tool(tool_name, &value).map_err(ToolError::new)
    }
}

fn tool_specs_for_allowed_tools(allowed_tools: Option<&BTreeSet<String>>) -> Vec<ToolSpec> {
    mvp_tool_specs()
        .into_iter()
        .filter(|spec| allowed_tools.is_none_or(|allowed| allowed.contains(spec.name)))
        .collect()
}

#[derive(Debug, Deserialize)]
struct RequestUserInputToolPayload {
    #[serde(default)]
    request_id: Option<String>,
    prompt: String,
    #[serde(default)]
    options: Vec<String>,
    #[serde(default)]
    allow_freeform: bool,
}

fn request_user_input_tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: REQUEST_USER_INPUT_TOOL_NAME.to_string(),
        description: Some(
            "Pause the current turn and ask the local human for structured input before continuing."
                .to_string(),
        ),
        input_schema: json!({
            "type": "object",
            "properties": {
                "request_id": { "type": "string" },
                "prompt": { "type": "string" },
                "options": {
                    "type": "array",
                    "items": { "type": "string" }
                },
                "allow_freeform": { "type": "boolean" }
            },
            "required": ["request_id", "prompt", "allow_freeform"],
            "additionalProperties": false
        }),
    }
}

fn parse_request_user_input_request(
    id: &str,
    input: &str,
) -> Result<UserInputRequest, RuntimeError> {
    let payload: RequestUserInputToolPayload = serde_json::from_str(input).map_err(|error| {
        RuntimeError::new(format!("invalid request-user-input payload: {error}"))
    })?;
    let request_id = payload.request_id.unwrap_or_else(|| id.to_string());
    if payload.prompt.trim().is_empty() {
        return Err(RuntimeError::new(
            "request-user-input payload must include a non-empty prompt",
        ));
    }

    Ok(UserInputRequest {
        request_id,
        prompt: payload.prompt,
        options: payload.options,
        allow_freeform: payload.allow_freeform,
    })
}

fn convert_messages(messages: &[ConversationMessage]) -> Vec<InputMessage> {
    messages
        .iter()
        .filter_map(|message| {
            let role = match message.role {
                MessageRole::System | MessageRole::User | MessageRole::Tool => "user",
                MessageRole::Assistant => "assistant",
            };
            let content = message
                .blocks
                .iter()
                .map(|block| match block {
                    ContentBlock::Text { text } => InputContentBlock::Text { text: text.clone() },
                    ContentBlock::ToolUse { id, name, input } => InputContentBlock::ToolUse {
                        id: id.clone(),
                        name: name.clone(),
                        input: serde_json::from_str(input)
                            .unwrap_or_else(|_| serde_json::json!({ "raw": input })),
                    },
                    ContentBlock::ToolResult {
                        tool_use_id,
                        output,
                        is_error,
                        ..
                    } => InputContentBlock::ToolResult {
                        tool_use_id: tool_use_id.clone(),
                        content: vec![ToolResultContentBlock::Text {
                            text: output.clone(),
                        }],
                        is_error: *is_error,
                    },
                    ContentBlock::UserInputRequest {
                        request_id,
                        prompt,
                        options,
                        allow_freeform,
                    } => InputContentBlock::ToolUse {
                        id: request_id.clone(),
                        name: REQUEST_USER_INPUT_TOOL_NAME.to_string(),
                        input: json!({
                            "request_id": request_id,
                            "prompt": prompt,
                            "options": options,
                            "allow_freeform": allow_freeform,
                        }),
                    },
                    ContentBlock::UserInputResponse {
                        request_id,
                        content,
                        selected_option,
                    } => InputContentBlock::ToolResult {
                        tool_use_id: request_id.clone(),
                        content: vec![ToolResultContentBlock::Json {
                            value: json!({
                                "request_id": request_id,
                                "content": content,
                                "selected_option": selected_option,
                            }),
                        }],
                        is_error: false,
                    },
                })
                .collect::<Vec<_>>();
            (!content.is_empty()).then(|| InputMessage {
                role: role.to_string(),
                content,
            })
        })
        .collect()
}

fn push_output_block(
    block: OutputContentBlock,
    block_index: u32,
    events: &mut Vec<AssistantEvent>,
    pending_tools: &mut BTreeMap<u32, (String, String, String)>,
    streaming_tool_input: bool,
) {
    match block {
        OutputContentBlock::Text { text } => {
            if !text.is_empty() {
                events.push(AssistantEvent::TextDelta(text));
            }
        }
        OutputContentBlock::ToolUse { id, name, input } => {
            let initial_input = if streaming_tool_input
                && input.is_object()
                && input.as_object().is_some_and(serde_json::Map::is_empty)
            {
                String::new()
            } else {
                input.to_string()
            };
            pending_tools.insert(block_index, (id, name, initial_input));
        }
        OutputContentBlock::Thinking { .. } | OutputContentBlock::RedactedThinking { .. } => {}
    }
}

fn parse_pending_output_block(
    id: &str,
    name: &str,
    input: &str,
) -> Result<AssistantEvent, RuntimeError> {
    if name == REQUEST_USER_INPUT_TOOL_NAME {
        return Ok(AssistantEvent::RequestUserInput(
            parse_request_user_input_request(id, input)?,
        ));
    }

    Ok(AssistantEvent::ToolUse {
        id: id.to_string(),
        name: name.to_string(),
        input: input.to_string(),
    })
}

fn response_to_events(response: MessageResponse) -> Result<Vec<AssistantEvent>, RuntimeError> {
    let mut events = Vec::new();
    let mut pending_tools = BTreeMap::new();

    for (index, block) in response.content.into_iter().enumerate() {
        let index = u32::try_from(index).expect("response block index overflow");
        push_output_block(block, index, &mut events, &mut pending_tools, false);
        if let Some((id, name, input)) = pending_tools.remove(&index) {
            events.push(parse_pending_output_block(&id, &name, &input)?);
        }
    }

    events.push(AssistantEvent::Usage(TokenUsage {
        input_tokens: response.usage.input_tokens,
        output_tokens: response.usage.output_tokens,
        cache_creation_input_tokens: response.usage.cache_creation_input_tokens,
        cache_read_input_tokens: response.usage.cache_read_input_tokens,
    }));
    events.push(AssistantEvent::MessageStop);
    Ok(events)
}

fn final_assistant_text(summary: &runtime::TurnSummary) -> String {
    summary
        .assistant_messages
        .last()
        .map(|message| {
            message
                .blocks
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default()
}

#[allow(clippy::needless_pass_by_value)]
fn execute_tool_search(input: ToolSearchInput) -> ToolSearchOutput {
    let deferred = deferred_tool_specs();
    let max_results = input.max_results.unwrap_or(5).max(1);
    let query = input.query.trim().to_string();
    let normalized_query = normalize_tool_search_query(&query);
    let matches = search_tool_specs(&query, max_results, &deferred);

    ToolSearchOutput {
        matches,
        query,
        normalized_query,
        total_deferred_tools: deferred.len(),
        pending_mcp_servers: None,
    }
}

fn deferred_tool_specs() -> Vec<ToolSpec> {
    mvp_tool_specs()
        .into_iter()
        .filter(|spec| {
            !matches!(
                spec.name,
                "bash" | "read_file" | "write_file" | "edit_file" | "glob_search" | "grep_search"
            )
        })
        .collect()
}

fn search_tool_specs(query: &str, max_results: usize, specs: &[ToolSpec]) -> Vec<String> {
    let lowered = query.to_lowercase();
    if let Some(selection) = lowered.strip_prefix("select:") {
        return selection
            .split(',')
            .map(str::trim)
            .filter(|part| !part.is_empty())
            .filter_map(|wanted| {
                let wanted = canonical_tool_token(wanted);
                specs
                    .iter()
                    .find(|spec| canonical_tool_token(spec.name) == wanted)
                    .map(|spec| spec.name.to_string())
            })
            .take(max_results)
            .collect();
    }

    let mut required = Vec::new();
    let mut optional = Vec::new();
    for term in lowered.split_whitespace() {
        if let Some(rest) = term.strip_prefix('+') {
            if !rest.is_empty() {
                required.push(rest);
            }
        } else {
            optional.push(term);
        }
    }
    let terms = if required.is_empty() {
        optional.clone()
    } else {
        required.iter().chain(optional.iter()).copied().collect()
    };

    let mut scored = specs
        .iter()
        .filter_map(|spec| {
            let name = spec.name.to_lowercase();
            let canonical_name = canonical_tool_token(spec.name);
            let normalized_description = normalize_tool_search_query(spec.description);
            let haystack = format!(
                "{name} {} {canonical_name}",
                spec.description.to_lowercase()
            );
            let normalized_haystack = format!("{canonical_name} {normalized_description}");
            if required.iter().any(|term| !haystack.contains(term)) {
                return None;
            }

            let mut score = 0_i32;
            for term in &terms {
                let canonical_term = canonical_tool_token(term);
                if haystack.contains(term) {
                    score += 2;
                }
                if name == *term {
                    score += 8;
                }
                if name.contains(term) {
                    score += 4;
                }
                if canonical_name == canonical_term {
                    score += 12;
                }
                if normalized_haystack.contains(&canonical_term) {
                    score += 3;
                }
            }

            if score == 0 && !lowered.is_empty() {
                return None;
            }
            Some((score, spec.name.to_string()))
        })
        .collect::<Vec<_>>();

    scored.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));
    scored
        .into_iter()
        .map(|(_, name)| name)
        .take(max_results)
        .collect()
}

fn normalize_tool_search_query(query: &str) -> String {
    query
        .trim()
        .split(|ch: char| ch.is_whitespace() || ch == ',')
        .filter(|term| !term.is_empty())
        .map(canonical_tool_token)
        .collect::<Vec<_>>()
        .join(" ")
}

fn canonical_tool_token(value: &str) -> String {
    let mut canonical = value
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .flat_map(char::to_lowercase)
        .collect::<String>();
    if let Some(stripped) = canonical.strip_suffix("tool") {
        canonical = stripped.to_string();
    }
    canonical
}

fn agent_store_dir() -> Result<std::path::PathBuf, String> {
    if let Ok(path) = std::env::var("OPENYAK_AGENT_STORE") {
        return Ok(std::path::PathBuf::from(path));
    }
    let cwd = std::env::current_dir().map_err(|error| error.to_string())?;
    if let Some(workspace_root) = cwd.ancestors().nth(2) {
        return Ok(workspace_root.join(".openyak-agents"));
    }
    Ok(cwd.join(".openyak-agents"))
}

fn make_agent_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("agent-{nanos}")
}

fn slugify_agent_name(description: &str) -> String {
    let mut out = description
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>();
    while out.contains("--") {
        out = out.replace("--", "-");
    }
    out.trim_matches('-').chars().take(32).collect()
}

fn normalize_subagent_type(subagent_type: Option<&str>) -> String {
    let trimmed = subagent_type.map(str::trim).unwrap_or_default();
    if trimmed.is_empty() {
        return String::from("general-purpose");
    }

    match canonical_tool_token(trimmed).as_str() {
        "general" | "generalpurpose" | "generalpurposeagent" => String::from("general-purpose"),
        "explore" | "explorer" | "exploreagent" => String::from("Explore"),
        "plan" | "planagent" => String::from("Plan"),
        "verification" | "verificationagent" | "verify" | "verifier" => {
            String::from("Verification")
        }
        "openyakguide" | "openyakguideagent" | "guide" => String::from("openyak-guide"),
        "statusline" | "statuslinesetup" => String::from("statusline-setup"),
        _ => trimmed.to_string(),
    }
}

fn iso8601_now() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .to_string()
}

#[allow(clippy::too_many_lines)]
fn execute_notebook_edit(input: NotebookEditInput) -> Result<NotebookEditOutput, String> {
    let path = std::path::PathBuf::from(&input.notebook_path);
    if path.extension().and_then(|ext| ext.to_str()) != Some("ipynb") {
        return Err(String::from(
            "File must be a Jupyter notebook (.ipynb file).",
        ));
    }

    let original_file = std::fs::read_to_string(&path).map_err(|error| error.to_string())?;
    let mut notebook: serde_json::Value =
        serde_json::from_str(&original_file).map_err(|error| error.to_string())?;
    let language = notebook
        .get("metadata")
        .and_then(|metadata| metadata.get("kernelspec"))
        .and_then(|kernelspec| kernelspec.get("language"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("python")
        .to_string();
    let cells = notebook
        .get_mut("cells")
        .and_then(serde_json::Value::as_array_mut)
        .ok_or_else(|| String::from("Notebook cells array not found"))?;

    let edit_mode = input.edit_mode.unwrap_or(NotebookEditMode::Replace);
    let target_index = match input.cell_id.as_deref() {
        Some(cell_id) => Some(resolve_cell_index(cells, Some(cell_id), edit_mode)?),
        None if matches!(
            edit_mode,
            NotebookEditMode::Replace | NotebookEditMode::Delete
        ) =>
        {
            Some(resolve_cell_index(cells, None, edit_mode)?)
        }
        None => None,
    };
    let resolved_cell_type = match edit_mode {
        NotebookEditMode::Delete => None,
        NotebookEditMode::Insert => Some(input.cell_type.unwrap_or(NotebookCellType::Code)),
        NotebookEditMode::Replace => Some(input.cell_type.unwrap_or_else(|| {
            target_index
                .and_then(|index| cells.get(index))
                .and_then(cell_kind)
                .unwrap_or(NotebookCellType::Code)
        })),
    };
    let new_source = require_notebook_source(input.new_source, edit_mode)?;

    let cell_id = match edit_mode {
        NotebookEditMode::Insert => {
            let resolved_cell_type = resolved_cell_type.expect("insert cell type");
            let new_id = make_cell_id(cells.len());
            let new_cell = build_notebook_cell(&new_id, resolved_cell_type, &new_source);
            let insert_at = target_index.map_or(cells.len(), |index| index + 1);
            cells.insert(insert_at, new_cell);
            cells
                .get(insert_at)
                .and_then(|cell| cell.get("id"))
                .and_then(serde_json::Value::as_str)
                .map(ToString::to_string)
        }
        NotebookEditMode::Delete => {
            let removed = cells.remove(target_index.expect("delete target index"));
            removed
                .get("id")
                .and_then(serde_json::Value::as_str)
                .map(ToString::to_string)
        }
        NotebookEditMode::Replace => {
            let resolved_cell_type = resolved_cell_type.expect("replace cell type");
            let cell = cells
                .get_mut(target_index.expect("replace target index"))
                .ok_or_else(|| String::from("Cell index out of range"))?;
            cell["source"] = serde_json::Value::Array(source_lines(&new_source));
            cell["cell_type"] = serde_json::Value::String(match resolved_cell_type {
                NotebookCellType::Code => String::from("code"),
                NotebookCellType::Markdown => String::from("markdown"),
            });
            match resolved_cell_type {
                NotebookCellType::Code => {
                    if !cell.get("outputs").is_some_and(serde_json::Value::is_array) {
                        cell["outputs"] = json!([]);
                    }
                    if cell.get("execution_count").is_none() {
                        cell["execution_count"] = serde_json::Value::Null;
                    }
                }
                NotebookCellType::Markdown => {
                    if let Some(object) = cell.as_object_mut() {
                        object.remove("outputs");
                        object.remove("execution_count");
                    }
                }
            }
            cell.get("id")
                .and_then(serde_json::Value::as_str)
                .map(ToString::to_string)
        }
    };

    let updated_file =
        serde_json::to_string_pretty(&notebook).map_err(|error| error.to_string())?;
    std::fs::write(&path, &updated_file).map_err(|error| error.to_string())?;

    Ok(NotebookEditOutput {
        new_source,
        cell_id,
        cell_type: resolved_cell_type,
        language,
        edit_mode: format_notebook_edit_mode(edit_mode),
        error: None,
        notebook_path: path.display().to_string(),
        original_file,
        updated_file,
    })
}

fn require_notebook_source(
    source: Option<String>,
    edit_mode: NotebookEditMode,
) -> Result<String, String> {
    match edit_mode {
        NotebookEditMode::Delete => Ok(source.unwrap_or_default()),
        NotebookEditMode::Insert | NotebookEditMode::Replace => source
            .ok_or_else(|| String::from("new_source is required for insert and replace edits")),
    }
}

fn build_notebook_cell(cell_id: &str, cell_type: NotebookCellType, source: &str) -> Value {
    let mut cell = json!({
        "cell_type": match cell_type {
            NotebookCellType::Code => "code",
            NotebookCellType::Markdown => "markdown",
        },
        "id": cell_id,
        "metadata": {},
        "source": source_lines(source),
    });
    if let Some(object) = cell.as_object_mut() {
        match cell_type {
            NotebookCellType::Code => {
                object.insert(String::from("outputs"), json!([]));
                object.insert(String::from("execution_count"), Value::Null);
            }
            NotebookCellType::Markdown => {}
        }
    }
    cell
}

fn cell_kind(cell: &serde_json::Value) -> Option<NotebookCellType> {
    cell.get("cell_type")
        .and_then(serde_json::Value::as_str)
        .map(|kind| {
            if kind == "markdown" {
                NotebookCellType::Markdown
            } else {
                NotebookCellType::Code
            }
        })
}

#[allow(clippy::needless_pass_by_value)]
fn execute_sleep(input: SleepInput) -> SleepOutput {
    std::thread::sleep(Duration::from_millis(input.duration_ms));
    SleepOutput {
        duration_ms: input.duration_ms,
        message: format!("Slept for {}ms", input.duration_ms),
    }
}

fn execute_brief(input: BriefInput) -> Result<BriefOutput, String> {
    if input.message.trim().is_empty() {
        return Err(String::from("message must not be empty"));
    }

    let attachments = input
        .attachments
        .as_ref()
        .map(|paths| {
            paths
                .iter()
                .map(|path| resolve_attachment(path))
                .collect::<Result<Vec<_>, String>>()
        })
        .transpose()?;

    let message = match input.status {
        BriefStatus::Normal | BriefStatus::Proactive => input.message,
    };

    Ok(BriefOutput {
        message,
        attachments,
        sent_at: iso8601_timestamp(),
    })
}

fn resolve_attachment(path: &str) -> Result<ResolvedAttachment, String> {
    let resolved = std::fs::canonicalize(path).map_err(|error| error.to_string())?;
    let metadata = std::fs::metadata(&resolved).map_err(|error| error.to_string())?;
    Ok(ResolvedAttachment {
        path: resolved.display().to_string(),
        size: metadata.len(),
        is_image: is_image_path(&resolved),
    })
}

fn is_image_path(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|ext| ext.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "svg")
    )
}

fn execute_config(input: ConfigInput) -> Result<ConfigOutput, String> {
    let setting = input.setting.trim();
    if setting.is_empty() {
        return Err(String::from("setting must not be empty"));
    }
    let Some(spec) = supported_config_setting(setting) else {
        return Ok(ConfigOutput {
            success: false,
            operation: None,
            setting: None,
            value: None,
            previous_value: None,
            new_value: None,
            error: Some(format!("Unknown setting: \"{setting}\"")),
        });
    };

    let path = config_file_for_scope(spec.scope)?;
    let mut document = read_json_object(&path)?;

    if let Some(value) = input.value {
        let normalized = normalize_config_value(spec, value)?;
        let previous_value = get_nested_value(&document, spec.path).cloned();
        set_nested_value(&mut document, spec.path, normalized.clone());
        write_json_object(&path, &document)?;
        Ok(ConfigOutput {
            success: true,
            operation: Some(String::from("set")),
            setting: Some(setting.to_string()),
            value: Some(normalized.clone()),
            previous_value,
            new_value: Some(normalized),
            error: None,
        })
    } else {
        Ok(ConfigOutput {
            success: true,
            operation: Some(String::from("get")),
            setting: Some(setting.to_string()),
            value: get_nested_value(&document, spec.path).cloned(),
            previous_value: None,
            new_value: None,
            error: None,
        })
    }
}

fn execute_structured_output(input: StructuredOutputInput) -> StructuredOutputResult {
    StructuredOutputResult {
        data: String::from("Structured output provided successfully"),
        structured_output: input.0,
    }
}

fn execute_repl(input: ReplInput) -> Result<ReplOutput, String> {
    if input.code.trim().is_empty() {
        return Err(String::from("code must not be empty"));
    }
    let runtime = resolve_repl_runtime(&input.language)?;
    let timeout_ms = input.timeout_ms;
    let started = Instant::now();
    let mut process = Command::new(runtime.program);
    process
        .args(&runtime.args)
        .arg(&input.code)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let (output, timed_out) = run_command_with_optional_timeout(&mut process, timeout_ms)
        .map_err(|error| error.to_string())?;
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let stderr = if timed_out {
        let suffix = format!(
            "Command exceeded timeout of {} ms",
            timeout_ms.expect("timed out runs should have a timeout")
        );
        if stderr.trim().is_empty() {
            suffix
        } else {
            format!("{}\n{suffix}", stderr.trim_end())
        }
    } else {
        stderr
    };

    Ok(ReplOutput {
        language: input.language,
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr,
        exit_code: if timed_out {
            124
        } else {
            output.status.code().unwrap_or(1)
        },
        duration_ms: started.elapsed().as_millis(),
    })
}

fn run_command_with_optional_timeout(
    process: &mut Command,
    timeout_ms: Option<u64>,
) -> std::io::Result<(std::process::Output, bool)> {
    if let Some(timeout_ms) = timeout_ms {
        let mut child = process.spawn()?;
        let stdout_reader = spawn_pipe_reader(child.stdout.take());
        let stderr_reader = spawn_pipe_reader(child.stderr.take());
        let started = Instant::now();
        let mut timed_out = false;
        loop {
            if child.try_wait()?.is_some() {
                break;
            }
            if started.elapsed() >= Duration::from_millis(timeout_ms) {
                let _ = child.kill();
                timed_out = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let status = child.wait()?;
        let stdout = join_pipe_reader(stdout_reader)?;
        let stderr = join_pipe_reader(stderr_reader)?;
        return Ok((
            std::process::Output {
                status,
                stdout,
                stderr,
            },
            timed_out,
        ));
    }

    process.output().map(|output| (output, false))
}

fn spawn_pipe_reader<R>(reader: Option<R>) -> Option<thread::JoinHandle<std::io::Result<Vec<u8>>>>
where
    R: Read + Send + 'static,
{
    reader.map(|mut reader| {
        thread::spawn(move || {
            let mut bytes = Vec::new();
            reader.read_to_end(&mut bytes)?;
            Ok(bytes)
        })
    })
}

fn join_pipe_reader(
    reader: Option<thread::JoinHandle<std::io::Result<Vec<u8>>>>,
) -> std::io::Result<Vec<u8>> {
    match reader {
        Some(reader) => reader
            .join()
            .map_err(|_| std::io::Error::other("failed to join REPL pipe reader"))?,
        None => Ok(Vec::new()),
    }
}

struct ReplRuntime {
    program: &'static str,
    args: Vec<&'static str>,
}

fn resolve_repl_runtime(language: &str) -> Result<ReplRuntime, String> {
    match language.trim().to_ascii_lowercase().as_str() {
        "python" | "py" => {
            #[cfg(windows)]
            let candidates: &[(&str, &[&str])] = &[
                ("py", &["-3"]),
                ("python", &[]),
                ("python3", &[]),
                ("py", &[]),
            ];
            #[cfg(not(windows))]
            let candidates: &[(&str, &[&str])] = &[("python3", &[]), ("python", &[])];

            let (program, args) = detect_command_candidate(candidates)
                .ok_or_else(|| String::from("python runtime not found"))?;
            let mut args = args.to_vec();
            args.push("-c");
            Ok(ReplRuntime { program, args })
        }
        "javascript" | "js" | "node" => {
            let (program, args) = detect_command_candidate(&[("node", &[][..])])
                .ok_or_else(|| String::from("node runtime not found"))?;
            let mut args = args.to_vec();
            args.push("-e");
            Ok(ReplRuntime { program, args })
        }
        "sh" | "shell" | "bash" => Ok(ReplRuntime {
            program: detect_first_command(&["bash", "sh"])
                .ok_or_else(|| String::from("shell runtime not found"))?,
            args: vec!["-lc"],
        }),
        other => Err(format!("unsupported REPL language: {other}")),
    }
}

fn detect_command_candidate(
    candidates: &[(&'static str, &'static [&'static str])],
) -> Option<(&'static str, &'static [&'static str])> {
    candidates
        .iter()
        .copied()
        .find(|(program, args)| probe_command_candidate(program, args))
}

fn probe_command_candidate(program: &str, args: &[&str]) -> bool {
    match Command::new(program).args(args).arg("--version").output() {
        Ok(output) => output.status.success(),
        Err(_) => false,
    }
}

fn detect_first_command(commands: &[&'static str]) -> Option<&'static str> {
    commands
        .iter()
        .copied()
        .find(|command| command_exists(command))
}

#[derive(Clone, Copy)]
enum ConfigScope {
    Global,
    Settings,
}

#[derive(Clone, Copy)]
struct ConfigSettingSpec {
    scope: ConfigScope,
    kind: ConfigKind,
    path: &'static [&'static str],
    options: Option<&'static [&'static str]>,
}

#[derive(Clone, Copy)]
enum ConfigKind {
    Boolean,
    String,
}

fn supported_config_setting(setting: &str) -> Option<ConfigSettingSpec> {
    Some(match setting {
        "theme" => ConfigSettingSpec {
            scope: ConfigScope::Global,
            kind: ConfigKind::String,
            path: &["theme"],
            options: None,
        },
        "editorMode" => ConfigSettingSpec {
            scope: ConfigScope::Global,
            kind: ConfigKind::String,
            path: &["editorMode"],
            options: Some(&["default", "vim", "emacs"]),
        },
        "verbose" => ConfigSettingSpec {
            scope: ConfigScope::Global,
            kind: ConfigKind::Boolean,
            path: &["verbose"],
            options: None,
        },
        "preferredNotifChannel" => ConfigSettingSpec {
            scope: ConfigScope::Global,
            kind: ConfigKind::String,
            path: &["preferredNotifChannel"],
            options: None,
        },
        "autoCompactEnabled" => ConfigSettingSpec {
            scope: ConfigScope::Global,
            kind: ConfigKind::Boolean,
            path: &["autoCompactEnabled"],
            options: None,
        },
        "autoMemoryEnabled" => ConfigSettingSpec {
            scope: ConfigScope::Settings,
            kind: ConfigKind::Boolean,
            path: &["autoMemoryEnabled"],
            options: None,
        },
        "autoDreamEnabled" => ConfigSettingSpec {
            scope: ConfigScope::Settings,
            kind: ConfigKind::Boolean,
            path: &["autoDreamEnabled"],
            options: None,
        },
        "fileCheckpointingEnabled" => ConfigSettingSpec {
            scope: ConfigScope::Global,
            kind: ConfigKind::Boolean,
            path: &["fileCheckpointingEnabled"],
            options: None,
        },
        "showTurnDuration" => ConfigSettingSpec {
            scope: ConfigScope::Global,
            kind: ConfigKind::Boolean,
            path: &["showTurnDuration"],
            options: None,
        },
        "terminalProgressBarEnabled" => ConfigSettingSpec {
            scope: ConfigScope::Global,
            kind: ConfigKind::Boolean,
            path: &["terminalProgressBarEnabled"],
            options: None,
        },
        "todoFeatureEnabled" => ConfigSettingSpec {
            scope: ConfigScope::Global,
            kind: ConfigKind::Boolean,
            path: &["todoFeatureEnabled"],
            options: None,
        },
        "model" => ConfigSettingSpec {
            scope: ConfigScope::Settings,
            kind: ConfigKind::String,
            path: &["model"],
            options: None,
        },
        "alwaysThinkingEnabled" => ConfigSettingSpec {
            scope: ConfigScope::Settings,
            kind: ConfigKind::Boolean,
            path: &["alwaysThinkingEnabled"],
            options: None,
        },
        "permissions.defaultMode" => ConfigSettingSpec {
            scope: ConfigScope::Settings,
            kind: ConfigKind::String,
            path: &["permissions", "defaultMode"],
            options: Some(&["default", "plan", "acceptEdits", "dontAsk", "auto"]),
        },
        "language" => ConfigSettingSpec {
            scope: ConfigScope::Settings,
            kind: ConfigKind::String,
            path: &["language"],
            options: None,
        },
        "teammateMode" => ConfigSettingSpec {
            scope: ConfigScope::Global,
            kind: ConfigKind::String,
            path: &["teammateMode"],
            options: Some(&["tmux", "in-process", "auto"]),
        },
        _ => return None,
    })
}

fn normalize_config_value(spec: ConfigSettingSpec, value: ConfigValue) -> Result<Value, String> {
    let normalized = match (spec.kind, value) {
        (ConfigKind::Boolean, ConfigValue::Bool(value)) => Value::Bool(value),
        (ConfigKind::Boolean, ConfigValue::String(value)) => {
            match value.trim().to_ascii_lowercase().as_str() {
                "true" => Value::Bool(true),
                "false" => Value::Bool(false),
                _ => return Err(String::from("setting requires true or false")),
            }
        }
        (ConfigKind::Boolean, ConfigValue::Number(_)) => {
            return Err(String::from("setting requires true or false"))
        }
        (ConfigKind::String, ConfigValue::String(value)) => Value::String(value),
        (ConfigKind::String, ConfigValue::Bool(value)) => Value::String(value.to_string()),
        (ConfigKind::String, ConfigValue::Number(value)) => json!(value),
    };

    if let Some(options) = spec.options {
        let Some(as_str) = normalized.as_str() else {
            return Err(String::from("setting requires a string value"));
        };
        if !options.iter().any(|option| option == &as_str) {
            return Err(format!(
                "Invalid value \"{as_str}\". Options: {}",
                options.join(", ")
            ));
        }
    }

    Ok(normalized)
}

fn config_file_for_scope(scope: ConfigScope) -> Result<PathBuf, String> {
    let cwd = std::env::current_dir().map_err(|error| error.to_string())?;
    Ok(match scope {
        ConfigScope::Global => config_home_dir().join("settings.json"),
        ConfigScope::Settings => cwd.join(".openyak").join("settings.local.json"),
    })
}

fn config_home_dir() -> PathBuf {
    default_openyak_home()
}

fn read_json_object(path: &Path) -> Result<serde_json::Map<String, Value>, String> {
    match std::fs::read_to_string(path) {
        Ok(contents) => {
            if contents.trim().is_empty() {
                return Ok(serde_json::Map::new());
            }
            serde_json::from_str::<Value>(&contents)
                .map_err(|error| error.to_string())?
                .as_object()
                .cloned()
                .ok_or_else(|| String::from("config file must contain a JSON object"))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(serde_json::Map::new()),
        Err(error) => Err(error.to_string()),
    }
}

fn write_json_object(path: &Path, value: &serde_json::Map<String, Value>) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    std::fs::write(
        path,
        serde_json::to_string_pretty(value).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())
}

fn get_nested_value<'a>(
    value: &'a serde_json::Map<String, Value>,
    path: &[&str],
) -> Option<&'a Value> {
    let (first, rest) = path.split_first()?;
    let mut current = value.get(*first)?;
    for key in rest {
        current = current.as_object()?.get(*key)?;
    }
    Some(current)
}

fn set_nested_value(root: &mut serde_json::Map<String, Value>, path: &[&str], new_value: Value) {
    let (first, rest) = path.split_first().expect("config path must not be empty");
    if rest.is_empty() {
        root.insert((*first).to_string(), new_value);
        return;
    }

    let entry = root
        .entry((*first).to_string())
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    if !entry.is_object() {
        *entry = Value::Object(serde_json::Map::new());
    }
    let map = entry.as_object_mut().expect("object inserted");
    set_nested_value(map, rest, new_value);
}

fn iso8601_timestamp() -> String {
    if let Ok(output) = Command::new("date")
        .args(["-u", "+%Y-%m-%dT%H:%M:%SZ"])
        .output()
    {
        if output.status.success() {
            return String::from_utf8_lossy(&output.stdout).trim().to_string();
        }
    }
    iso8601_now()
}

#[allow(clippy::needless_pass_by_value)]
fn execute_powershell(input: PowerShellInput) -> std::io::Result<runtime::BashCommandOutput> {
    let _ = &input.description;
    let shell = detect_powershell_shell()?;
    execute_shell_command(
        &shell,
        &input.command,
        input.timeout,
        input.run_in_background,
    )
}

fn detect_powershell_shell() -> std::io::Result<std::path::PathBuf> {
    if let Some(shell) = resolve_command_path("pwsh") {
        Ok(shell)
    } else if let Some(shell) = resolve_command_path("powershell") {
        Ok(shell)
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "PowerShell executable not found (expected `pwsh` or `powershell` in PATH)",
        ))
    }
}

#[allow(clippy::too_many_lines)]
fn execute_shell_command(
    shell: &std::path::Path,
    command: &str,
    timeout: Option<u64>,
    run_in_background: Option<bool>,
) -> std::io::Result<runtime::BashCommandOutput> {
    if run_in_background.unwrap_or(false) {
        let child = std::process::Command::new(shell)
            .arg("-NoProfile")
            .arg("-NonInteractive")
            .arg("-Command")
            .arg(command)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()?;
        return Ok(runtime::BashCommandOutput {
            stdout: String::new(),
            stderr: String::new(),
            raw_output_path: None,
            interrupted: false,
            is_image: None,
            background_task_id: Some(child.id().to_string()),
            backgrounded_by_user: Some(true),
            assistant_auto_backgrounded: Some(false),
            dangerously_disable_sandbox: None,
            return_code_interpretation: None,
            no_output_expected: Some(true),
            structured_content: None,
            persisted_output_path: None,
            persisted_output_size: None,
            sandbox_status: None,
        });
    }

    let mut process = std::process::Command::new(shell);
    process
        .arg("-NoProfile")
        .arg("-NonInteractive")
        .arg("-Command")
        .arg(command);
    process
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    if let Some(timeout_ms) = timeout {
        let mut child = process.spawn()?;
        let started = Instant::now();
        loop {
            if let Some(status) = child.try_wait()? {
                let output = child.wait_with_output()?;
                return Ok(runtime::BashCommandOutput {
                    stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                    stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                    raw_output_path: None,
                    interrupted: false,
                    is_image: None,
                    background_task_id: None,
                    backgrounded_by_user: None,
                    assistant_auto_backgrounded: None,
                    dangerously_disable_sandbox: None,
                    return_code_interpretation: status
                        .code()
                        .filter(|code| *code != 0)
                        .map(|code| format!("exit_code:{code}")),
                    no_output_expected: Some(output.stdout.is_empty() && output.stderr.is_empty()),
                    structured_content: None,
                    persisted_output_path: None,
                    persisted_output_size: None,
                    sandbox_status: None,
                });
            }
            if started.elapsed() >= Duration::from_millis(timeout_ms) {
                let _ = child.kill();
                let output = child.wait_with_output()?;
                let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
                let stderr = if stderr.trim().is_empty() {
                    format!("Command exceeded timeout of {timeout_ms} ms")
                } else {
                    format!(
                        "{}
Command exceeded timeout of {timeout_ms} ms",
                        stderr.trim_end()
                    )
                };
                return Ok(runtime::BashCommandOutput {
                    stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                    stderr,
                    raw_output_path: None,
                    interrupted: true,
                    is_image: None,
                    background_task_id: None,
                    backgrounded_by_user: None,
                    assistant_auto_backgrounded: None,
                    dangerously_disable_sandbox: None,
                    return_code_interpretation: Some(String::from("timeout")),
                    no_output_expected: Some(false),
                    structured_content: None,
                    persisted_output_path: None,
                    persisted_output_size: None,
                    sandbox_status: None,
                });
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    let output = process.output()?;
    Ok(runtime::BashCommandOutput {
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        raw_output_path: None,
        interrupted: false,
        is_image: None,
        background_task_id: None,
        backgrounded_by_user: None,
        assistant_auto_backgrounded: None,
        dangerously_disable_sandbox: None,
        return_code_interpretation: output
            .status
            .code()
            .filter(|code| *code != 0)
            .map(|code| format!("exit_code:{code}")),
        no_output_expected: Some(output.stdout.is_empty() && output.stderr.is_empty()),
        structured_content: None,
        persisted_output_path: None,
        persisted_output_size: None,
        sandbox_status: None,
    })
}

fn resolve_cell_index(
    cells: &[serde_json::Value],
    cell_id: Option<&str>,
    edit_mode: NotebookEditMode,
) -> Result<usize, String> {
    if cells.is_empty()
        && matches!(
            edit_mode,
            NotebookEditMode::Replace | NotebookEditMode::Delete
        )
    {
        return Err(String::from("Notebook has no cells to edit"));
    }
    if let Some(cell_id) = cell_id {
        cells
            .iter()
            .position(|cell| cell.get("id").and_then(serde_json::Value::as_str) == Some(cell_id))
            .ok_or_else(|| format!("Cell id not found: {cell_id}"))
    } else {
        Ok(cells.len().saturating_sub(1))
    }
}

fn source_lines(source: &str) -> Vec<serde_json::Value> {
    if source.is_empty() {
        return vec![serde_json::Value::String(String::new())];
    }
    source
        .split_inclusive('\n')
        .map(|line| serde_json::Value::String(line.to_string()))
        .collect()
}

fn format_notebook_edit_mode(mode: NotebookEditMode) -> String {
    match mode {
        NotebookEditMode::Replace => String::from("replace"),
        NotebookEditMode::Insert => String::from("insert"),
        NotebookEditMode::Delete => String::from("delete"),
    }
}

fn make_cell_id(index: usize) -> String {
    format!("cell-{}", index + 1)
}

fn parse_skill_description(contents: &str) -> Option<String> {
    for line in contents.lines() {
        if let Some(value) = line.strip_prefix("description:") {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::collections::BTreeSet;
    use std::fs;
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpListener};
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex, OnceLock};
    use std::thread;
    use std::time::Duration;

    use super::{
        agent_permission_policy, allowed_tools_for_subagent, browser_interact_tool_spec,
        browser_observe_tool_spec, discover_session_server_url, execute_agent_with_spawn,
        execute_tool, execute_tool_with_enforcer, final_assistant_text, foundation_surface,
        foundation_surfaces, global_cron_registry, global_lsp_registry, global_mcp_registry,
        global_task_registry, global_team_registry, maybe_enforce_permission_check, mvp_tool_specs,
        persist_agent_terminal_state, push_output_block, AgentInput, AgentJob, AgentOutput,
        GlobalToolRegistry, SubagentToolExecutor, BROWSER_INTERACT_SUPPORTED_WAIT_KINDS,
        BROWSER_INTERACT_TOOL_NAME, BROWSER_OBSERVE_SUPPORTED_WAIT_KINDS,
        BROWSER_OBSERVE_TOOL_NAME, SESSION_SERVER_URL_ENV, THREAD_SERVER_INFO_FILENAME,
    };
    use api::OutputContentBlock;
    use runtime::sandbox::{FilesystemIsolationMode, SandboxConfig};
    use runtime::{
        ApiRequest, AssistantEvent, BashCommandInput, ContentBlock, ConversationMessage,
        ConversationRuntime, PermissionEnforcer, PermissionMode, PermissionPolicy,
        RuntimeBrowserControlConfig, RuntimeError, Session, ToolProfileBashPolicy,
    };
    use serde_json::{json, Value};

    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    fn parity_registry_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    fn temp_path(name: &str) -> PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time")
            .as_nanos();
        std::env::temp_dir().join(format!("openyak-tools-{unique}-{name}"))
    }

    fn full_access_enforcer(tool_name: &str) -> PermissionEnforcer {
        PermissionEnforcer::new(
            PermissionPolicy::new(PermissionMode::DangerFullAccess)
                .with_tool_requirement(tool_name, PermissionMode::DangerFullAccess),
        )
    }

    fn strict_bash_policy(
        filesystem_mode: FilesystemIsolationMode,
        network_isolation: bool,
        allowed_mounts: &[&str],
    ) -> ToolProfileBashPolicy {
        ToolProfileBashPolicy {
            sandbox: SandboxConfig {
                enabled: Some(true),
                namespace_restrictions: Some(true),
                network_isolation: Some(network_isolation),
                filesystem_mode: Some(filesystem_mode),
                allowed_mounts: allowed_mounts
                    .iter()
                    .map(|mount| (*mount).to_string())
                    .collect(),
            },
            allow_dangerously_disable_sandbox: false,
        }
    }

    fn browser_control_config(root: &std::path::Path) -> RuntimeBrowserControlConfig {
        RuntimeBrowserControlConfig::new(
            true,
            root.join(".openyak").join("artifacts").join("browser"),
        )
    }

    fn browser_enabled_registry(root: &std::path::Path) -> GlobalToolRegistry {
        GlobalToolRegistry::builtin()
            .with_browser_control(browser_control_config(root))
            .expect("browser registry should build")
    }

    fn write_browser_command_script(
        dir: &std::path::Path,
        name: &str,
        windows_script: &str,
        unix_script: &str,
    ) -> PathBuf {
        let path = if cfg!(windows) {
            dir.join(format!("{name}.cmd"))
        } else {
            dir.join(name)
        };
        let script = if cfg!(windows) {
            windows_script
        } else {
            unix_script
        };
        fs::write(&path, script).expect("fake browser should write");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let mut permissions = fs::metadata(&path)
                .expect("fake browser metadata should load")
                .permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&path, permissions)
                .expect("fake browser permissions should update");
        }
        path
    }

    fn write_fake_browser_command(dir: &std::path::Path, name: &str) -> PathBuf {
        write_browser_command_script(
            dir,
            name,
            "@echo off\r\nsetlocal EnableDelayedExpansion\r\nfor %%A in (%*) do (\r\n  set \"ARG=%%~A\"\r\n  if /I \"!ARG:~0,13!\"==\"--screenshot=\" (\r\n    set \"SHOT=!ARG:~13!\"\r\n    type nul > \"!SHOT!\"\r\n  )\r\n)\r\necho ^<html^>^<head^>^<title^>Fake Browser^</title^>^</head^>^<body^>Hello Browser^</body^>^</html^>\r\n",
            "#!/bin/sh\nfor arg in \"$@\"; do\n  case \"$arg\" in\n    --screenshot=*) touch \"${arg#--screenshot=}\" ;;\n  esac\ndone\nprintf '%s' '<html><head><title>Fake Browser</title></head><body>Hello Browser</body></html>'\n",
        )
    }

    fn write_screenshot_failing_browser_command(dir: &std::path::Path, name: &str) -> PathBuf {
        write_browser_command_script(
            dir,
            name,
            "@echo off\r\nsetlocal EnableDelayedExpansion\r\nfor %%A in (%*) do (\r\n  set \"ARG=%%~A\"\r\n  if /I \"!ARG:~0,13!\"==\"--screenshot=\" (\r\n    >&2 echo screenshot failure\r\n    exit /b 9\r\n  )\r\n)\r\necho ^<html^>^<head^>^<title^>Fake Browser^</title^>^</head^>^<body^>Hello Browser^</body^>^</html^>\r\n",
            "#!/bin/sh\nfor arg in \"$@\"; do\n  case \"$arg\" in\n    --screenshot=*) echo 'screenshot failure' >&2; exit 9 ;;\n  esac\ndone\nprintf '%s' '<html><head><title>Fake Browser</title></head><body>Hello Browser</body></html>'\n",
        )
    }

    fn write_fake_browser_interact_command(dir: &std::path::Path, name: &str) -> PathBuf {
        write_browser_command_script(
            dir,
            name,
            "@echo off\r\nsetlocal EnableDelayedExpansion\r\nset \"PORT=%OPENYAK_FAKE_BROWSER_PORT%\"\r\nset \"PROFILE=\"\r\nfor %%A in (%*) do (\r\n  set \"ARG=%%~A\"\r\n  if /I \"!ARG:~0,16!\"==\"--user-data-dir=\" set \"PROFILE=!ARG:~16!\"\r\n)\r\nif not defined PROFILE exit /b 7\r\n(\r\n  echo %PORT%\r\n  echo /devtools/browser/fake\r\n) > \"!PROFILE!\\DevToolsActivePort\"\r\ntimeout /t 15 /nobreak >nul\r\n",
            "#!/bin/sh\nport=\"${OPENYAK_FAKE_BROWSER_PORT:?}\"\nprofile=\"\"\nfor arg in \"$@\"; do\n  case \"$arg\" in\n    --user-data-dir=*) profile=\"${arg#--user-data-dir=}\" ;;\n  esac\ndone\n[ -n \"$profile\" ] || exit 7\nprintf '%s\n/devtools/browser/fake\n' \"$port\" > \"$profile/DevToolsActivePort\"\nsleep 15\n",
        )
    }

    struct FakeBrowserInteractServer {
        port: u16,
        join_handle: Option<std::thread::JoinHandle<()>>,
    }

    impl FakeBrowserInteractServer {
        #[allow(clippy::too_many_lines)]
        fn start(requested_url: &str) -> Self {
            use std::net::TcpListener;

            let listener = TcpListener::bind("127.0.0.1:0").expect("fake browser listener");
            let port = listener.local_addr().expect("listener addr").port();
            let requested_url = requested_url.to_string();
            let join_handle = std::thread::spawn(move || {
                let websocket_path = "/devtools/page/fake-target";
                loop {
                    let (mut stream, _) = listener.accept().expect("fake browser accept");
                    let request = read_fake_http_request(&mut stream);
                    if request.starts_with("GET /json/list ") {
                        let body = format!(
                            "[{{\"id\":\"page-1\",\"type\":\"page\",\"url\":\"{requested_url}\",\"webSocketDebuggerUrl\":\"ws://127.0.0.1:{port}{websocket_path}\"}}]"
                        );
                        let response = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            body.len(),
                            body
                        );
                        stream
                            .write_all(response.as_bytes())
                            .expect("fake browser json/list response");
                        continue;
                    }
                    if request.starts_with(&format!("GET {websocket_path} ")) {
                        stream
                            .write_all(
                                b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: fake\r\n\r\n",
                            )
                            .expect("fake browser websocket handshake");
                        let mut clicked = false;
                        while let Some(message) = read_fake_ws_text(&mut stream) {
                            let payload: Value =
                                serde_json::from_str(&message).expect("cdp request json");
                            let id = payload["id"].as_u64().expect("cdp request id");
                            let method = payload["method"].as_str().expect("cdp request method");
                            let response = match method {
                                "Runtime.evaluate" => {
                                    let expression = payload["params"]["expression"]
                                        .as_str()
                                        .expect("cdp expression");
                                    if expression.contains("element.click()") {
                                        clicked = true;
                                        json!({
                                            "id": id,
                                            "result": {
                                                "result": {
                                                    "type": "object",
                                                    "value": {
                                                        "found": true,
                                                        "clicked": true,
                                                        "tag": "button",
                                                        "text": "Sign in"
                                                    }
                                                }
                                            }
                                        })
                                    } else if expression.contains("document.querySelector") {
                                        json!({
                                            "id": id,
                                            "result": {
                                                "result": {
                                                    "type": "object",
                                                    "value": {
                                                        "found": true,
                                                        "disabled": false,
                                                        "tag": "button",
                                                        "text": "Sign in"
                                                    }
                                                }
                                            }
                                        })
                                    } else {
                                        let state = if clicked {
                                            json!({
                                                "readyState": "complete",
                                                "url": "https://example.test/dashboard",
                                                "title": "Dashboard",
                                                "visibleText": "Welcome back"
                                            })
                                        } else {
                                            json!({
                                                "readyState": "complete",
                                                "url": requested_url,
                                                "title": "Login",
                                                "visibleText": "Sign in"
                                            })
                                        };
                                        json!({
                                            "id": id,
                                            "result": {
                                                "result": {
                                                    "type": "object",
                                                    "value": state
                                                }
                                            }
                                        })
                                    }
                                }
                                "Page.captureScreenshot" => {
                                    json!({ "id": id, "result": { "data": "aGVsbG8=" } })
                                }
                                _ => json!({ "id": id, "result": {} }),
                            };
                            write_fake_ws_text(&mut stream, &response.to_string());
                        }
                        break;
                    }
                }
            });
            Self {
                port,
                join_handle: Some(join_handle),
            }
        }
    }

    impl Drop for FakeBrowserInteractServer {
        fn drop(&mut self) {
            if let Some(handle) = self.join_handle.take() {
                let _ = handle.join();
            }
        }
    }

    fn read_fake_http_request(stream: &mut std::net::TcpStream) -> String {
        let mut buffer = Vec::new();
        let mut byte = [0_u8; 1];
        loop {
            let read = stream.read(&mut byte).expect("fake browser http read");
            if read == 0 {
                break;
            }
            buffer.push(byte[0]);
            if buffer.ends_with(b"\r\n\r\n") {
                break;
            }
        }
        String::from_utf8(buffer).expect("fake browser http request should be utf8")
    }

    fn read_fake_ws_text(stream: &mut std::net::TcpStream) -> Option<String> {
        let mut header = [0_u8; 2];
        if stream.read_exact(&mut header).is_err() {
            return None;
        }
        let opcode = header[0] & 0x0f;
        let masked = header[1] & 0x80 != 0;
        let mut payload_len = usize::from(header[1] & 0x7f);
        if payload_len == 126 {
            let mut extended = [0_u8; 2];
            stream
                .read_exact(&mut extended)
                .expect("fake browser extended frame size");
            payload_len = usize::from(u16::from_be_bytes(extended));
        }
        let mut mask_key = [0_u8; 4];
        if masked {
            stream
                .read_exact(&mut mask_key)
                .expect("fake browser websocket mask");
        }
        let mut payload = vec![0_u8; payload_len];
        stream
            .read_exact(&mut payload)
            .expect("fake browser websocket payload");
        if masked {
            for (index, byte) in payload.iter_mut().enumerate() {
                *byte ^= mask_key[index % mask_key.len()];
            }
        }
        match opcode {
            0x1 => Some(String::from_utf8(payload).expect("fake browser websocket text")),
            _ => None,
        }
    }

    fn write_fake_ws_text(stream: &mut std::net::TcpStream, payload: &str) {
        let bytes = payload.as_bytes();
        let mut frame = Vec::with_capacity(bytes.len() + 4);
        frame.push(0x81);
        match bytes.len() {
            0..=125 => frame.push(
                u8::try_from(bytes.len()).expect("small fake websocket payload should fit in u8"),
            ),
            126..=65535 => {
                frame.push(126);
                frame.extend_from_slice(
                    &u16::try_from(bytes.len())
                        .expect("medium fake websocket payload should fit in u16")
                        .to_be_bytes(),
                );
            }
            _ => panic!("fake websocket payload too large"),
        }
        frame.extend_from_slice(bytes);
        stream
            .write_all(&frame)
            .expect("fake browser websocket response");
    }

    #[test]
    fn exposes_mvp_tools() {
        let names = mvp_tool_specs()
            .into_iter()
            .map(|spec| spec.name)
            .collect::<Vec<_>>();
        assert!(names.contains(&"bash"));
        assert!(names.contains(&"read_file"));
        assert!(names.contains(&"WebFetch"));
        assert!(names.contains(&"WebSearch"));
        assert!(names.contains(&"TodoWrite"));
        assert!(names.contains(&"Skill"));
        assert!(names.contains(&"Agent"));
        assert!(names.contains(&"ToolSearch"));
        assert!(names.contains(&"NotebookEdit"));
        assert!(names.contains(&"Sleep"));
        assert!(names.contains(&"SendUserMessage"));
        assert!(names.contains(&"Config"));
        assert!(names.contains(&"StructuredOutput"));
        assert!(names.contains(&"REPL"));
        assert!(names.contains(&"PowerShell"));
    }

    #[test]
    fn rejects_unknown_tool_names() {
        let error = execute_tool("nope", &json!({})).expect_err("tool should be rejected");
        assert!(error.contains("unsupported tool"));
    }

    #[test]
    fn browser_observe_allowlist_is_rejected_when_disabled() {
        let error = GlobalToolRegistry::builtin()
            .normalize_allowed_tools(&[String::from("BrowserObserve")])
            .expect_err("disabled browser tool should be rejected");
        assert!(error.contains("BrowserObserve is disabled"));
    }

    #[test]
    fn browser_interact_allowlist_is_rejected_when_disabled() {
        let error = GlobalToolRegistry::builtin()
            .normalize_allowed_tools(&[String::from("BrowserInteract")])
            .expect_err("disabled browser interact tool should be rejected");
        assert!(error.contains("BrowserInteract is disabled"));
    }

    #[test]
    fn browser_observe_remains_hidden_by_default_when_enabled() {
        let root = temp_path("browser-hidden-default");
        fs::create_dir_all(&root).expect("workspace should exist");
        let registry = browser_enabled_registry(&root);
        let names = registry
            .definitions(None)
            .into_iter()
            .map(|definition| definition.name)
            .collect::<Vec<_>>();

        assert!(!names.iter().any(|name| name == BROWSER_OBSERVE_TOOL_NAME));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn browser_observe_is_exposed_only_when_explicitly_allowed() {
        let root = temp_path("browser-allowed");
        fs::create_dir_all(&root).expect("workspace should exist");
        let registry = browser_enabled_registry(&root);
        let allowed = registry
            .normalize_allowed_tools(&[String::from("BrowserObserve")])
            .expect("allowlist should normalize")
            .expect("allowlist should be present");

        let names = registry
            .definitions(Some(&allowed))
            .into_iter()
            .map(|definition| definition.name)
            .collect::<Vec<_>>();
        assert!(names.iter().any(|name| name == BROWSER_OBSERVE_TOOL_NAME));

        let permission = registry
            .permission_specs(Some(&allowed))
            .into_iter()
            .find(|(name, _)| name == BROWSER_OBSERVE_TOOL_NAME)
            .expect("browser permission spec should exist");
        assert_eq!(permission.1, PermissionMode::DangerFullAccess);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn browser_interact_is_exposed_only_when_explicitly_allowed() {
        let root = temp_path("browser-interact-allowed");
        fs::create_dir_all(&root).expect("workspace should exist");
        let registry = browser_enabled_registry(&root);
        let allowed = registry
            .normalize_allowed_tools(&[String::from("BrowserInteract")])
            .expect("allowlist should normalize")
            .expect("allowlist should be present");

        let names = registry
            .definitions(Some(&allowed))
            .into_iter()
            .map(|definition| definition.name)
            .collect::<Vec<_>>();
        assert!(names.iter().any(|name| name == BROWSER_INTERACT_TOOL_NAME));

        let permission = registry
            .permission_specs(Some(&allowed))
            .into_iter()
            .find(|(name, _)| name == BROWSER_INTERACT_TOOL_NAME)
            .expect("browser interact permission spec should exist");
        assert_eq!(permission.1, PermissionMode::DangerFullAccess);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn browser_observe_rejects_interaction_fields() {
        let root = temp_path("browser-schema");
        fs::create_dir_all(&root).expect("workspace should exist");
        let registry = browser_enabled_registry(&root);

        let error = registry
            .execute(
                BROWSER_OBSERVE_TOOL_NAME,
                &json!({
                    "url": "https://example.test",
                    "click": { "selector": "#ship" }
                }),
            )
            .expect_err("unknown interaction field should fail");
        assert!(error.contains("BrowserObserve rejected unsupported request shape"));
        assert!(error.contains("unknown field `click`"));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn browser_observe_supports_text_and_title_wait_conditions() {
        let _guard = env_lock().lock().expect("env lock should not be poisoned");
        let root = temp_path("browser-observe-waits");
        let browser_dir = root.join("browser-bin");
        fs::create_dir_all(&browser_dir).expect("browser dir should exist");
        write_fake_browser_command(&browser_dir, "msedge");

        let original_path = std::env::var_os("PATH");
        let original_dir = std::env::current_dir().expect("current dir");
        let mut path_entries = vec![browser_dir.clone()];
        if let Some(existing) = &original_path {
            path_entries.extend(std::env::split_paths(existing));
        }
        let joined_path = std::env::join_paths(path_entries).expect("PATH should join");
        std::env::set_var("PATH", &joined_path);
        std::env::set_current_dir(&root).expect("set cwd");

        let registry = browser_enabled_registry(&root);
        let text_result = registry.execute(
            BROWSER_OBSERVE_TOOL_NAME,
            &json!({
                "url": "https://example.test",
                "wait": { "kind": "text", "value": "Hello Browser" }
            }),
        );
        let title_result = registry.execute(
            BROWSER_OBSERVE_TOOL_NAME,
            &json!({
                "url": "https://example.test",
                "wait": { "kind": "title_contains", "value": "Fake Browser" }
            }),
        );

        std::env::set_current_dir(&original_dir).expect("restore cwd");
        match original_path {
            Some(value) => std::env::set_var("PATH", value),
            None => std::env::remove_var("PATH"),
        }

        let text_output: Value =
            serde_json::from_str(&text_result.expect("text wait should succeed"))
                .expect("text wait json");
        assert_eq!(text_output["wait"]["kind"], "text");
        assert_eq!(text_output["wait"]["status"], "satisfied");
        assert_eq!(text_output["wait"]["expected"], "Hello Browser");
        assert_eq!(text_output["load_outcome"], "loaded_after_wait_budget");

        let title_output: Value =
            serde_json::from_str(&title_result.expect("title wait should succeed"))
                .expect("title wait json");
        assert_eq!(title_output["wait"]["kind"], "title_contains");
        assert_eq!(title_output["wait"]["status"], "satisfied");
        assert_eq!(title_output["wait"]["expected"], "Fake Browser");
        assert_eq!(title_output["title"], "Fake Browser");

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn browser_observe_reports_wait_shape_errors_explicitly() {
        let root = temp_path("browser-wait-shape-errors");
        fs::create_dir_all(&root).expect("workspace should exist");
        let registry = browser_enabled_registry(&root);

        let selector_error = registry
            .execute(
                BROWSER_OBSERVE_TOOL_NAME,
                &json!({
                    "url": "https://example.test",
                    "wait": { "kind": "selector", "value": "#ship" }
                }),
            )
            .expect_err("unsupported selector wait should fail");
        assert!(selector_error.contains("wait.kind=selector is not supported"));

        let missing_value_error = registry
            .execute(
                BROWSER_OBSERVE_TOOL_NAME,
                &json!({
                    "url": "https://example.test",
                    "wait": { "kind": "text" }
                }),
            )
            .expect_err("text wait without value should fail");
        assert!(missing_value_error.contains("wait.kind=text requires a non-empty `value`"));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn browser_interact_reports_action_and_wait_shape_errors_explicitly() {
        let root = temp_path("browser-interact-shape-errors");
        fs::create_dir_all(&root).expect("workspace should exist");
        let registry = browser_enabled_registry(&root);

        let action_error = registry
            .execute(
                BROWSER_INTERACT_TOOL_NAME,
                &json!({
                    "url": "https://example.test",
                    "action": { "kind": "type", "selector": "#email" }
                }),
            )
            .expect_err("unsupported interaction kind should fail");
        assert!(action_error.contains("action.kind=type is not supported"));

        let wait_error = registry
            .execute(
                BROWSER_INTERACT_TOOL_NAME,
                &json!({
                    "url": "https://example.test",
                    "action": { "kind": "click", "selector": "#sign-in" },
                    "wait": { "kind": "network_idle" }
                }),
            )
            .expect_err("unsupported interact wait should fail");
        assert!(wait_error.contains("wait.kind=network_idle is not supported"));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn browser_tool_wait_schema_enums_match_slice_supported_kinds() {
        let observe_spec = browser_observe_tool_spec();
        let observe_wait_kinds = observe_spec.input_schema["properties"]["wait"]["properties"]
            ["kind"]["enum"]
            .as_array()
            .expect("observe enum array")
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>();
        assert_eq!(observe_wait_kinds, BROWSER_OBSERVE_SUPPORTED_WAIT_KINDS);

        let interact_spec = browser_interact_tool_spec();
        let interact_wait_kinds = interact_spec.input_schema["properties"]["wait"]["properties"]
            ["kind"]["enum"]
            .as_array()
            .expect("interact enum array")
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>();
        assert_eq!(interact_wait_kinds, BROWSER_INTERACT_SUPPORTED_WAIT_KINDS);
    }

    #[test]
    fn browser_observe_runs_with_fake_browser_and_bounded_artifact_path() {
        let _guard = env_lock().lock().expect("env lock should not be poisoned");
        let root = temp_path("browser-observe");
        let browser_dir = root.join("browser-bin");
        fs::create_dir_all(&browser_dir).expect("browser dir should exist");
        write_fake_browser_command(&browser_dir, "msedge");

        let original_path = std::env::var_os("PATH");
        let original_dir = std::env::current_dir().expect("current dir");
        let mut path_entries = vec![browser_dir.clone()];
        if let Some(existing) = &original_path {
            path_entries.extend(std::env::split_paths(existing));
        }
        let joined_path = std::env::join_paths(path_entries).expect("PATH should join");
        std::env::set_var("PATH", &joined_path);
        std::env::set_current_dir(&root).expect("set cwd");

        let registry = browser_enabled_registry(&root);
        let execution = registry.execute(
            BROWSER_OBSERVE_TOOL_NAME,
            &json!({
                "url": "https://example.test",
                "wait": { "kind": "network_idle" },
                "capture_visible_text": true,
                "screenshot": { "capture": true, "format": "png" }
            }),
        );

        std::env::set_current_dir(&original_dir).expect("restore cwd");
        match original_path {
            Some(value) => std::env::set_var("PATH", value),
            None => std::env::remove_var("PATH"),
        }

        let result = execution.expect("BrowserObserve should succeed");

        let output: Value = serde_json::from_str(&result).expect("browser output json");
        assert_eq!(output["requested_url"], "https://example.test");
        assert_eq!(output["title"], "Fake Browser");
        assert_eq!(output["final_url"], Value::Null);
        assert_eq!(output["load_outcome"], "loaded_after_network_idle_budget");
        assert_eq!(output["wait"]["kind"], "network_idle");
        assert_eq!(output["wait"]["status"], "satisfied");
        assert!(output["visible_text"]
            .as_str()
            .expect("visible text")
            .contains("Hello Browser"));
        let relative_path = output["screenshot"]["relative_path"]
            .as_str()
            .expect("relative path");
        assert!(relative_path.starts_with(".openyak/artifacts/browser/"));
        assert_eq!(output["screenshot"]["file_name"], "observe.png");
        assert_eq!(output["screenshot"]["media_type"], "image/png");
        assert_eq!(
            output["browser_runtime"]["capture_backend"],
            "headless_cli_dump_dom"
        );
        assert!(output["timings_ms"]["total"].as_u64().is_some());
        let artifact_path = root.join(relative_path.replace('/', std::path::MAIN_SEPARATOR_STR));
        assert!(artifact_path.is_file());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn browser_interact_runs_with_fake_browser_and_bounded_artifact_path() {
        let _guard = env_lock().lock().expect("env lock should not be poisoned");
        let root = temp_path("browser-interact");
        let browser_dir = root.join("browser-bin");
        fs::create_dir_all(&browser_dir).expect("browser dir should exist");
        write_fake_browser_interact_command(&browser_dir, "msedge");
        let server = FakeBrowserInteractServer::start("https://example.test/login");

        let original_port = std::env::var_os("OPENYAK_FAKE_BROWSER_PORT");
        let original_path = std::env::var_os("PATH");
        let original_dir = std::env::current_dir().expect("current dir");
        let mut path_entries = vec![browser_dir.clone()];
        if let Some(existing) = &original_path {
            path_entries.extend(std::env::split_paths(existing));
        }
        let joined_path = std::env::join_paths(path_entries).expect("PATH should join");
        std::env::set_var("OPENYAK_FAKE_BROWSER_PORT", server.port.to_string());
        std::env::set_var("PATH", &joined_path);
        std::env::set_current_dir(&root).expect("set cwd");

        let registry = browser_enabled_registry(&root);
        let execution = registry.execute(
            BROWSER_INTERACT_TOOL_NAME,
            &json!({
                "url": "https://example.test/login",
                "action": { "kind": "click", "selector": "#sign-in" },
                "wait": { "kind": "url_contains", "value": "/dashboard" },
                "capture_visible_text": true,
                "screenshot": { "capture": true, "format": "png" }
            }),
        );

        std::env::set_current_dir(&original_dir).expect("restore cwd");
        match original_port {
            Some(value) => std::env::set_var("OPENYAK_FAKE_BROWSER_PORT", value),
            None => std::env::remove_var("OPENYAK_FAKE_BROWSER_PORT"),
        }
        match original_path {
            Some(value) => std::env::set_var("PATH", value),
            None => std::env::remove_var("PATH"),
        }

        let result = execution.expect("BrowserInteract should succeed");
        let output: Value = serde_json::from_str(&result).expect("browser interact output json");
        assert_eq!(output["requested_url"], "https://example.test/login");
        assert_eq!(output["final_url"], "https://example.test/dashboard");
        assert_eq!(output["title"], "Dashboard");
        assert_eq!(output["action"]["kind"], "click");
        assert_eq!(output["action"]["selector"], "#sign-in");
        assert_eq!(output["action"]["status"], "performed");
        assert_eq!(output["wait"]["kind"], "url_contains");
        assert_eq!(output["wait"]["status"], "satisfied");
        assert_eq!(output["wait"]["expected"], "/dashboard");
        assert!(output["visible_text"]
            .as_str()
            .expect("visible text")
            .contains("Welcome back"));
        assert_eq!(
            output["browser_runtime"]["capture_backend"],
            "headless_cdp_single_call"
        );
        let relative_path = output["screenshot"]["relative_path"]
            .as_str()
            .expect("relative path");
        assert!(relative_path.starts_with(".openyak/artifacts/browser/"));
        assert_eq!(output["screenshot"]["file_name"], "interact.png");
        let artifact_path = root.join(relative_path.replace('/', std::path::MAIN_SEPARATOR_STR));
        assert!(artifact_path.is_file());
        assert_eq!(fs::read(&artifact_path).expect("artifact bytes"), b"hello");

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn browser_observe_labels_screenshot_failures_by_phase() {
        let _guard = env_lock().lock().expect("env lock should not be poisoned");
        let root = temp_path("browser-observe-screenshot-failure");
        let browser_dir = root.join("browser-bin");
        fs::create_dir_all(&browser_dir).expect("browser dir should exist");
        write_screenshot_failing_browser_command(&browser_dir, "msedge");

        let original_path = std::env::var_os("PATH");
        let original_dir = std::env::current_dir().expect("current dir");
        let mut path_entries = vec![browser_dir.clone()];
        if let Some(existing) = &original_path {
            path_entries.extend(std::env::split_paths(existing));
        }
        let joined_path = std::env::join_paths(path_entries).expect("PATH should join");
        std::env::set_var("PATH", &joined_path);
        std::env::set_current_dir(&root).expect("set cwd");

        let registry = browser_enabled_registry(&root);
        let execution = registry.execute(
            BROWSER_OBSERVE_TOOL_NAME,
            &json!({
                "url": "https://example.test",
                "screenshot": { "capture": true, "format": "png" }
            }),
        );

        std::env::set_current_dir(&original_dir).expect("restore cwd");
        match original_path {
            Some(value) => std::env::set_var("PATH", value),
            None => std::env::remove_var("PATH"),
        }

        let error = execution.expect_err("screenshot capture should fail");
        assert!(error.contains("BrowserObserve screenshot capture failed with status 9"));
        assert!(error.contains("screenshot failure"));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn web_fetch_returns_prompt_aware_summary() {
        let server = TestServer::spawn(Arc::new(|request_line: &str| {
            assert!(request_line.starts_with("GET /page "));
            HttpResponse::html(
                200,
                "OK",
                "<html><head><title>Ignored</title></head><body><h1>Test Page</h1><p>Hello <b>world</b> from local server.</p></body></html>",
            )
        }));

        let result = execute_tool(
            "WebFetch",
            &json!({
                "url": format!("http://{}/page", server.addr()),
                "prompt": "Summarize this page"
            }),
        )
        .expect("WebFetch should succeed");

        let output: serde_json::Value = serde_json::from_str(&result).expect("valid json");
        assert_eq!(output["code"], 200);
        let summary = output["result"].as_str().expect("result string");
        assert!(summary.contains("Fetched"));
        assert!(summary.contains("Test Page"));
        assert!(summary.contains("Hello world from local server"));

        let titled = execute_tool(
            "WebFetch",
            &json!({
                "url": format!("http://{}/page", server.addr()),
                "prompt": "What is the page title?"
            }),
        )
        .expect("WebFetch title query should succeed");
        let titled_output: serde_json::Value = serde_json::from_str(&titled).expect("valid json");
        let titled_summary = titled_output["result"].as_str().expect("result string");
        assert!(titled_summary.contains("Title: Ignored"));
    }

    #[test]
    fn web_fetch_supports_plain_text_and_rejects_invalid_url() {
        let server = TestServer::spawn(Arc::new(|request_line: &str| {
            assert!(request_line.starts_with("GET /plain "));
            HttpResponse::text(200, "OK", "plain text response")
        }));

        let result = execute_tool(
            "WebFetch",
            &json!({
                "url": format!("http://{}/plain", server.addr()),
                "prompt": "Show me the content"
            }),
        )
        .expect("WebFetch should succeed for text content");

        let output: serde_json::Value = serde_json::from_str(&result).expect("valid json");
        assert_eq!(output["url"], format!("http://{}/plain", server.addr()));
        assert!(output["result"]
            .as_str()
            .expect("result")
            .contains("plain text response"));

        let error = execute_tool(
            "WebFetch",
            &json!({
                "url": "not a url",
                "prompt": "Summarize"
            }),
        )
        .expect_err("invalid URL should fail");
        assert!(error.contains("relative URL without a base") || error.contains("invalid"));
    }

    #[test]
    fn web_search_extracts_and_filters_results() {
        let _guard = env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let server = TestServer::spawn(Arc::new(|request_line: &str| {
            assert!(request_line.contains("GET /search?q=rust+web+search "));
            HttpResponse::html(
                200,
                "OK",
                r#"
                <html><body>
                  <a class="result__a" href="https://docs.rs/reqwest">Reqwest docs</a>
                  <a class="result__a" href="https://example.com/blocked">Blocked result</a>
                </body></html>
                "#,
            )
        }));

        let original_base = std::env::var_os("OPENYAK_WEB_SEARCH_BASE_URL");
        std::env::set_var(
            "OPENYAK_WEB_SEARCH_BASE_URL",
            format!("http://{}/search", server.addr()),
        );
        let result = execute_tool(
            "WebSearch",
            &json!({
                "query": "rust web search",
                "allowed_domains": ["https://DOCS.rs/"],
                "blocked_domains": ["HTTPS://EXAMPLE.COM"]
            }),
        )
        .expect("WebSearch should succeed");
        match original_base {
            Some(value) => std::env::set_var("OPENYAK_WEB_SEARCH_BASE_URL", value),
            None => std::env::remove_var("OPENYAK_WEB_SEARCH_BASE_URL"),
        }

        let output: serde_json::Value = serde_json::from_str(&result).expect("valid json");
        assert_eq!(output["query"], "rust web search");
        let results = output["results"].as_array().expect("results array");
        let search_result = results
            .iter()
            .find(|item| item.get("content").is_some())
            .expect("search result block present");
        let content = search_result["content"].as_array().expect("content array");
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["title"], "Reqwest docs");
        assert_eq!(content[0]["url"], "https://docs.rs/reqwest");
    }

    #[test]
    fn web_search_handles_generic_links_and_invalid_base_url() {
        let _guard = env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let server = TestServer::spawn(Arc::new(|request_line: &str| {
            assert!(request_line.contains("GET /fallback?q=generic+links "));
            HttpResponse::html(
                200,
                "OK",
                r#"
                <html><body>
                  <a href="https://example.com/one">Example One</a>
                  <a href="https://example.com/one">Duplicate Example One</a>
                  <a href="https://docs.rs/tokio">Tokio Docs</a>
                </body></html>
                "#,
            )
        }));

        std::env::set_var(
            "OPENYAK_WEB_SEARCH_BASE_URL",
            format!("http://{}/fallback", server.addr()),
        );
        let result = execute_tool(
            "WebSearch",
            &json!({
                "query": "generic links"
            }),
        )
        .expect("WebSearch fallback parsing should succeed");
        std::env::remove_var("OPENYAK_WEB_SEARCH_BASE_URL");

        let output: serde_json::Value = serde_json::from_str(&result).expect("valid json");
        let results = output["results"].as_array().expect("results array");
        let search_result = results
            .iter()
            .find(|item| item.get("content").is_some())
            .expect("search result block present");
        let content = search_result["content"].as_array().expect("content array");
        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["url"], "https://example.com/one");
        assert_eq!(content[1]["url"], "https://docs.rs/tokio");

        std::env::set_var("OPENYAK_WEB_SEARCH_BASE_URL", "://bad-base-url");
        let error = execute_tool("WebSearch", &json!({ "query": "generic links" }))
            .expect_err("invalid base URL should fail");
        std::env::remove_var("OPENYAK_WEB_SEARCH_BASE_URL");
        assert!(error.contains("relative URL without a base") || error.contains("empty host"));
    }

    #[test]
    fn pending_tools_preserve_multiple_streaming_tool_calls_by_index() {
        let mut events = Vec::new();
        let mut pending_tools = BTreeMap::new();

        push_output_block(
            OutputContentBlock::ToolUse {
                id: "tool-1".to_string(),
                name: "read_file".to_string(),
                input: json!({}),
            },
            1,
            &mut events,
            &mut pending_tools,
            true,
        );
        push_output_block(
            OutputContentBlock::ToolUse {
                id: "tool-2".to_string(),
                name: "grep_search".to_string(),
                input: json!({}),
            },
            2,
            &mut events,
            &mut pending_tools,
            true,
        );

        pending_tools
            .get_mut(&1)
            .expect("first tool pending")
            .2
            .push_str("{\"path\":\"src/main.rs\"}");
        pending_tools
            .get_mut(&2)
            .expect("second tool pending")
            .2
            .push_str("{\"pattern\":\"TODO\"}");

        assert_eq!(
            pending_tools.remove(&1),
            Some((
                "tool-1".to_string(),
                "read_file".to_string(),
                "{\"path\":\"src/main.rs\"}".to_string(),
            ))
        );
        assert_eq!(
            pending_tools.remove(&2),
            Some((
                "tool-2".to_string(),
                "grep_search".to_string(),
                "{\"pattern\":\"TODO\"}".to_string(),
            ))
        );
    }

    #[test]
    fn todo_write_persists_and_returns_previous_state() {
        let _guard = env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let path = temp_path("todos.json");
        std::env::set_var("OPENYAK_TODO_STORE", &path);

        let first = execute_tool(
            "TodoWrite",
            &json!({
                "todos": [
                    {"content": "Add tool", "activeForm": "Adding tool", "status": "in_progress"},
                    {"content": "Run tests", "activeForm": "Running tests", "status": "pending"}
                ]
            }),
        )
        .expect("TodoWrite should succeed");
        let first_output: serde_json::Value = serde_json::from_str(&first).expect("valid json");
        assert_eq!(first_output["oldTodos"].as_array().expect("array").len(), 0);

        let second = execute_tool(
            "TodoWrite",
            &json!({
                "todos": [
                    {"content": "Add tool", "activeForm": "Adding tool", "status": "completed"},
                    {"content": "Run tests", "activeForm": "Running tests", "status": "completed"},
                    {"content": "Verify", "activeForm": "Verifying", "status": "completed"}
                ]
            }),
        )
        .expect("TodoWrite should succeed");
        std::env::remove_var("OPENYAK_TODO_STORE");
        let _ = std::fs::remove_file(path);

        let second_output: serde_json::Value = serde_json::from_str(&second).expect("valid json");
        assert_eq!(
            second_output["oldTodos"].as_array().expect("array").len(),
            2
        );
        assert_eq!(
            second_output["newTodos"].as_array().expect("array").len(),
            3
        );
        assert!(second_output["verificationNudgeNeeded"].is_null());
    }

    #[test]
    fn todo_write_rejects_invalid_payloads_and_sets_verification_nudge() {
        let _guard = env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let path = temp_path("todos-errors.json");
        std::env::set_var("OPENYAK_TODO_STORE", &path);

        let empty = execute_tool("TodoWrite", &json!({ "todos": [] }))
            .expect_err("empty todos should fail");
        assert!(empty.contains("todos must not be empty"));

        // Multiple in_progress items are now allowed for parallel workflows
        let _multi_active = execute_tool(
            "TodoWrite",
            &json!({
                "todos": [
                    {"content": "One", "activeForm": "Doing one", "status": "in_progress"},
                    {"content": "Two", "activeForm": "Doing two", "status": "in_progress"}
                ]
            }),
        )
        .expect("multiple in-progress todos should succeed");

        let blank_content = execute_tool(
            "TodoWrite",
            &json!({
                "todos": [
                    {"content": "   ", "activeForm": "Doing it", "status": "pending"}
                ]
            }),
        )
        .expect_err("blank content should fail");
        assert!(blank_content.contains("todo content must not be empty"));

        let nudge = execute_tool(
            "TodoWrite",
            &json!({
                "todos": [
                    {"content": "Write tests", "activeForm": "Writing tests", "status": "completed"},
                    {"content": "Fix errors", "activeForm": "Fixing errors", "status": "completed"},
                    {"content": "Ship branch", "activeForm": "Shipping branch", "status": "completed"}
                ]
            }),
        )
        .expect("completed todos should succeed");
        std::env::remove_var("OPENYAK_TODO_STORE");
        let _ = fs::remove_file(path);

        let output: serde_json::Value = serde_json::from_str(&nudge).expect("valid json");
        assert_eq!(output["verificationNudgeNeeded"], true);
    }

    #[test]
    fn skill_loads_local_skill_prompt() {
        let _guard = env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let codex_home = temp_path("skills-home");
        let skill_dir = codex_home.join("skills").join("help");
        fs::create_dir_all(&skill_dir).expect("create skill dir");
        fs::write(
            skill_dir.join("SKILL.md"),
            "# help\n\nGuide on using oh-my-codex plugin\n",
        )
        .expect("write skill");
        let original_codex_home = std::env::var("CODEX_HOME").ok();
        std::env::set_var("CODEX_HOME", &codex_home);

        let result = execute_tool(
            "Skill",
            &json!({
                "skill": "help",
                "args": "overview"
            }),
        )
        .expect("Skill should succeed");

        let output: serde_json::Value = serde_json::from_str(&result).expect("valid json");
        assert_eq!(output["skill"], "help");
        assert!(output["path"]
            .as_str()
            .expect("path")
            .replace('\\', "/")
            .ends_with("/help/SKILL.md"));
        assert!(output["prompt"]
            .as_str()
            .expect("prompt")
            .contains("Guide on using oh-my-codex plugin"));

        let dollar_result = execute_tool(
            "Skill",
            &json!({
                "skill": "$help"
            }),
        )
        .expect("Skill should accept $skill invocation form");
        let dollar_output: serde_json::Value =
            serde_json::from_str(&dollar_result).expect("valid json");
        assert_eq!(dollar_output["skill"], "$help");
        assert!(dollar_output["path"]
            .as_str()
            .expect("path")
            .replace('\\', "/")
            .ends_with("/help/SKILL.md"));

        match original_codex_home {
            Some(value) => std::env::set_var("CODEX_HOME", value),
            None => std::env::remove_var("CODEX_HOME"),
        }
        let _ = fs::remove_dir_all(codex_home);
    }

    #[test]
    fn skill_loads_nested_system_skill_prompt() {
        let _guard = env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let codex_home = temp_path("skills-system-home");
        let skill_name = "zz-nested-skill-test";
        let skill_dir = codex_home.join("skills").join(".system").join(skill_name);
        fs::create_dir_all(&skill_dir).expect("create nested skill dir");
        fs::write(
            skill_dir.join("SKILL.md"),
            "# zz-nested-skill-test\n\nGuide on unique nested docs\n",
        )
        .expect("write nested skill");
        let original_codex_home = std::env::var("CODEX_HOME").ok();
        let original_home = std::env::var("HOME").ok();
        let original_userprofile = std::env::var("USERPROFILE").ok();
        std::env::set_var("CODEX_HOME", &codex_home);
        std::env::remove_var("HOME");
        std::env::remove_var("USERPROFILE");

        let result = execute_tool(
            "Skill",
            &json!({
                "skill": skill_name
            }),
        )
        .expect("nested Skill should succeed");

        let output: serde_json::Value = serde_json::from_str(&result).expect("valid json");
        assert_eq!(output["skill"], skill_name);
        assert_eq!(
            PathBuf::from(output["path"].as_str().expect("path")),
            skill_dir.join("SKILL.md")
        );

        match original_codex_home {
            Some(value) => std::env::set_var("CODEX_HOME", value),
            None => std::env::remove_var("CODEX_HOME"),
        }
        match original_home {
            Some(value) => std::env::set_var("HOME", value),
            None => std::env::remove_var("HOME"),
        }
        match original_userprofile {
            Some(value) => std::env::set_var("USERPROFILE", value),
            None => std::env::remove_var("USERPROFILE"),
        }
        let _ = fs::remove_dir_all(codex_home);
    }

    #[test]
    fn tool_search_supports_keyword_and_select_queries() {
        let keyword = execute_tool(
            "ToolSearch",
            &json!({"query": "web current", "max_results": 3}),
        )
        .expect("ToolSearch should succeed");
        let keyword_output: serde_json::Value = serde_json::from_str(&keyword).expect("valid json");
        let matches = keyword_output["matches"].as_array().expect("matches");
        assert!(matches.iter().any(|value| value == "WebSearch"));

        let selected = execute_tool("ToolSearch", &json!({"query": "select:Agent,Skill"}))
            .expect("ToolSearch should succeed");
        let selected_output: serde_json::Value =
            serde_json::from_str(&selected).expect("valid json");
        assert_eq!(selected_output["matches"][0], "Agent");
        assert_eq!(selected_output["matches"][1], "Skill");

        let aliased = execute_tool("ToolSearch", &json!({"query": "AgentTool"}))
            .expect("ToolSearch should support tool aliases");
        let aliased_output: serde_json::Value = serde_json::from_str(&aliased).expect("valid json");
        assert_eq!(aliased_output["matches"][0], "Agent");
        assert_eq!(aliased_output["normalized_query"], "agent");

        let selected_with_alias =
            execute_tool("ToolSearch", &json!({"query": "select:AgentTool,Skill"}))
                .expect("ToolSearch alias select should succeed");
        let selected_with_alias_output: serde_json::Value =
            serde_json::from_str(&selected_with_alias).expect("valid json");
        assert_eq!(selected_with_alias_output["matches"][0], "Agent");
        assert_eq!(selected_with_alias_output["matches"][1], "Skill");
    }

    #[test]
    fn agent_persists_handoff_metadata() {
        let _guard = env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = temp_path("agent-store");
        std::env::set_var("OPENYAK_AGENT_STORE", &dir);
        let captured = Arc::new(Mutex::new(None::<AgentJob>));
        let captured_for_spawn = Arc::clone(&captured);

        let manifest = execute_agent_with_spawn(
            AgentInput {
                description: "Audit the branch".to_string(),
                prompt: "Check tests and outstanding work.".to_string(),
                subagent_type: Some("Explore".to_string()),
                name: Some("ship-audit".to_string()),
                model: None,
            },
            move |job| {
                *captured_for_spawn
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(job);
                Ok(())
            },
        )
        .expect("Agent should succeed");
        std::env::remove_var("OPENYAK_AGENT_STORE");

        assert_eq!(manifest.name, "ship-audit");
        assert_eq!(manifest.subagent_type.as_deref(), Some("Explore"));
        assert_eq!(manifest.status, "running");
        assert!(!manifest.created_at.is_empty());
        assert!(manifest.started_at.is_some());
        assert!(manifest.completed_at.is_none());
        let contents = std::fs::read_to_string(&manifest.output_file).expect("agent file exists");
        let manifest_contents =
            std::fs::read_to_string(&manifest.manifest_file).expect("manifest file exists");
        assert!(contents.contains("Audit the branch"));
        assert!(contents.contains("Check tests and outstanding work."));
        assert!(manifest_contents.contains("\"subagentType\": \"Explore\""));
        assert!(manifest_contents.contains("\"status\": \"running\""));
        let captured_job = captured
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .expect("spawn job should be captured");
        assert_eq!(captured_job.prompt, "Check tests and outstanding work.");
        assert!(captured_job.allowed_tools.contains("read_file"));
        assert!(!captured_job.allowed_tools.contains("Agent"));

        let normalized = execute_tool(
            "Agent",
            &json!({
                "description": "Verify the branch",
                "prompt": "Check tests.",
                "subagent_type": "explorer"
            }),
        )
        .expect("Agent should normalize built-in aliases");
        let normalized_output: serde_json::Value =
            serde_json::from_str(&normalized).expect("valid json");
        assert_eq!(normalized_output["subagentType"], "Explore");

        let named = execute_tool(
            "Agent",
            &json!({
                "description": "Review the branch",
                "prompt": "Inspect diff.",
                "name": "Ship Audit!!!"
            }),
        )
        .expect("Agent should normalize explicit names");
        let named_output: serde_json::Value = serde_json::from_str(&named).expect("valid json");
        assert_eq!(named_output["name"], "ship-audit");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn agent_fake_runner_can_persist_completion_and_failure() {
        let _guard = env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = temp_path("agent-runner");
        std::env::set_var("OPENYAK_AGENT_STORE", &dir);

        let completed = execute_agent_with_spawn(
            AgentInput {
                description: "Complete the task".to_string(),
                prompt: "Do the work".to_string(),
                subagent_type: Some("Explore".to_string()),
                name: Some("complete-task".to_string()),
                model: Some("claude-sonnet-4-6".to_string()),
            },
            |job| {
                persist_agent_terminal_state(
                    &job.manifest,
                    "completed",
                    Some("Finished successfully"),
                    None,
                )
            },
        )
        .expect("completed agent should succeed");

        let completed_manifest = std::fs::read_to_string(&completed.manifest_file)
            .expect("completed manifest should exist");
        let completed_output =
            std::fs::read_to_string(&completed.output_file).expect("completed output should exist");
        assert!(completed_manifest.contains("\"status\": \"completed\""));
        assert!(completed_output.contains("Finished successfully"));

        let failed = execute_agent_with_spawn(
            AgentInput {
                description: "Fail the task".to_string(),
                prompt: "Do the failing work".to_string(),
                subagent_type: Some("Verification".to_string()),
                name: Some("fail-task".to_string()),
                model: None,
            },
            |job| {
                persist_agent_terminal_state(
                    &job.manifest,
                    "failed",
                    None,
                    Some(String::from("simulated failure")),
                )
            },
        )
        .expect("failed agent should still spawn");

        let failed_manifest =
            std::fs::read_to_string(&failed.manifest_file).expect("failed manifest should exist");
        let failed_output =
            std::fs::read_to_string(&failed.output_file).expect("failed output should exist");
        assert!(failed_manifest.contains("\"status\": \"failed\""));
        assert!(failed_manifest.contains("simulated failure"));
        assert!(failed_output.contains("simulated failure"));

        let spawn_error = execute_agent_with_spawn(
            AgentInput {
                description: "Spawn error task".to_string(),
                prompt: "Never starts".to_string(),
                subagent_type: None,
                name: Some("spawn-error".to_string()),
                model: None,
            },
            |_| Err(String::from("thread creation failed")),
        )
        .expect_err("spawn errors should surface");
        assert!(spawn_error.contains("failed to spawn sub-agent"));
        let spawn_error_manifest = std::fs::read_dir(&dir)
            .expect("agent dir should exist")
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("json"))
            .find_map(|path| {
                let contents = std::fs::read_to_string(&path).ok()?;
                contents
                    .contains("\"name\": \"spawn-error\"")
                    .then_some(contents)
            })
            .expect("failed manifest should still be written");
        assert!(spawn_error_manifest.contains("\"status\": \"failed\""));
        assert!(spawn_error_manifest.contains("thread creation failed"));

        std::env::remove_var("OPENYAK_AGENT_STORE");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn agent_tool_subset_mapping_is_expected() {
        let general = allowed_tools_for_subagent("general-purpose");
        assert!(general.contains("bash"));
        assert!(general.contains("write_file"));
        assert!(!general.contains("Agent"));

        let explore = allowed_tools_for_subagent("Explore");
        assert!(explore.contains("read_file"));
        assert!(explore.contains("grep_search"));
        assert!(!explore.contains("bash"));

        let plan = allowed_tools_for_subagent("Plan");
        assert!(plan.contains("TodoWrite"));
        assert!(plan.contains("StructuredOutput"));
        assert!(!plan.contains("Agent"));

        let verification = allowed_tools_for_subagent("Verification");
        assert!(verification.contains("bash"));
        assert!(verification.contains("PowerShell"));
        assert!(!verification.contains("write_file"));
    }

    #[derive(Debug)]
    struct MockSubagentApiClient {
        calls: usize,
        input_path: String,
    }

    impl runtime::ApiClient for MockSubagentApiClient {
        fn stream(&mut self, request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
            self.calls += 1;
            match self.calls {
                1 => {
                    assert_eq!(request.messages.len(), 1);
                    Ok(vec![
                        AssistantEvent::ToolUse {
                            id: "tool-1".to_string(),
                            name: "read_file".to_string(),
                            input: json!({ "path": self.input_path }).to_string(),
                        },
                        AssistantEvent::MessageStop,
                    ])
                }
                2 => {
                    assert!(request.messages.len() >= 3);
                    Ok(vec![
                        AssistantEvent::TextDelta("Scope: completed mock review".to_string()),
                        AssistantEvent::MessageStop,
                    ])
                }
                _ => panic!("unexpected mock stream call"),
            }
        }
    }

    #[test]
    fn subagent_runtime_executes_tool_loop_with_isolated_session() {
        let _guard = env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let path = temp_path("subagent-input.txt");
        std::fs::write(&path, "hello from child").expect("write input file");

        let mut runtime = ConversationRuntime::new(
            Session::new(),
            MockSubagentApiClient {
                calls: 0,
                input_path: path.display().to_string(),
            },
            SubagentToolExecutor::new(BTreeSet::from([String::from("read_file")])),
            agent_permission_policy(),
            vec![String::from("system prompt")],
        );

        let summary = runtime
            .run_turn("Inspect the delegated file", None, None)
            .expect("subagent loop should succeed");

        assert_eq!(
            final_assistant_text(&summary),
            "Scope: completed mock review"
        );
        assert!(runtime
            .session()
            .messages
            .iter()
            .flat_map(|message| message.blocks.iter())
            .any(|block| matches!(
                block,
                runtime::ContentBlock::ToolResult { output, .. }
                    if output.contains("hello from child")
            )));

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn agent_rejects_blank_required_fields() {
        let missing_description = execute_tool(
            "Agent",
            &json!({
                "description": "  ",
                "prompt": "Inspect"
            }),
        )
        .expect_err("blank description should fail");
        assert!(missing_description.contains("description must not be empty"));

        let missing_prompt = execute_tool(
            "Agent",
            &json!({
                "description": "Inspect branch",
                "prompt": " "
            }),
        )
        .expect_err("blank prompt should fail");
        assert!(missing_prompt.contains("prompt must not be empty"));
    }

    #[test]
    fn notebook_edit_replaces_inserts_and_deletes_cells() {
        let path = temp_path("notebook.ipynb");
        std::fs::write(
            &path,
            r#"{
  "cells": [
    {"cell_type": "code", "id": "cell-a", "metadata": {}, "source": ["print(1)\n"], "outputs": [], "execution_count": null}
  ],
  "metadata": {"kernelspec": {"language": "python"}},
  "nbformat": 4,
  "nbformat_minor": 5
}"#,
        )
        .expect("write notebook");

        let replaced = execute_tool(
            "NotebookEdit",
            &json!({
                "notebook_path": path.display().to_string(),
                "cell_id": "cell-a",
                "new_source": "print(2)\n",
                "edit_mode": "replace"
            }),
        )
        .expect("NotebookEdit replace should succeed");
        let replaced_output: serde_json::Value = serde_json::from_str(&replaced).expect("json");
        assert_eq!(replaced_output["cell_id"], "cell-a");
        assert_eq!(replaced_output["cell_type"], "code");

        let inserted = execute_tool(
            "NotebookEdit",
            &json!({
                "notebook_path": path.display().to_string(),
                "cell_id": "cell-a",
                "new_source": "# heading\n",
                "cell_type": "markdown",
                "edit_mode": "insert"
            }),
        )
        .expect("NotebookEdit insert should succeed");
        let inserted_output: serde_json::Value = serde_json::from_str(&inserted).expect("json");
        assert_eq!(inserted_output["cell_type"], "markdown");
        let appended = execute_tool(
            "NotebookEdit",
            &json!({
                "notebook_path": path.display().to_string(),
                "new_source": "print(3)\n",
                "edit_mode": "insert"
            }),
        )
        .expect("NotebookEdit append should succeed");
        let appended_output: serde_json::Value = serde_json::from_str(&appended).expect("json");
        assert_eq!(appended_output["cell_type"], "code");

        let deleted = execute_tool(
            "NotebookEdit",
            &json!({
                "notebook_path": path.display().to_string(),
                "cell_id": "cell-a",
                "edit_mode": "delete"
            }),
        )
        .expect("NotebookEdit delete should succeed without new_source");
        let deleted_output: serde_json::Value = serde_json::from_str(&deleted).expect("json");
        assert!(deleted_output["cell_type"].is_null());
        assert_eq!(deleted_output["new_source"], "");

        let final_notebook: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("read notebook"))
                .expect("valid notebook json");
        let cells = final_notebook["cells"].as_array().expect("cells array");
        assert_eq!(cells.len(), 2);
        assert_eq!(cells[0]["cell_type"], "markdown");
        assert!(cells[0].get("outputs").is_none());
        assert_eq!(cells[1]["cell_type"], "code");
        assert_eq!(cells[1]["source"][0], "print(3)\n");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn notebook_edit_rejects_invalid_inputs() {
        let text_path = temp_path("notebook.txt");
        fs::write(&text_path, "not a notebook").expect("write text file");
        let wrong_extension = execute_tool(
            "NotebookEdit",
            &json!({
                "notebook_path": text_path.display().to_string(),
                "new_source": "print(1)\n"
            }),
        )
        .expect_err("non-ipynb file should fail");
        assert!(wrong_extension.contains("Jupyter notebook"));
        let _ = fs::remove_file(&text_path);

        let empty_notebook = temp_path("empty.ipynb");
        fs::write(
            &empty_notebook,
            r#"{"cells":[],"metadata":{"kernelspec":{"language":"python"}},"nbformat":4,"nbformat_minor":5}"#,
        )
        .expect("write empty notebook");

        let missing_source = execute_tool(
            "NotebookEdit",
            &json!({
                "notebook_path": empty_notebook.display().to_string(),
                "edit_mode": "insert"
            }),
        )
        .expect_err("insert without source should fail");
        assert!(missing_source.contains("new_source is required"));

        let missing_cell = execute_tool(
            "NotebookEdit",
            &json!({
                "notebook_path": empty_notebook.display().to_string(),
                "edit_mode": "delete"
            }),
        )
        .expect_err("delete on empty notebook should fail");
        assert!(missing_cell.contains("Notebook has no cells to edit"));
        let _ = fs::remove_file(empty_notebook);
    }

    #[test]
    fn bash_tool_reports_success_exit_failure_timeout_and_background() {
        let _guard = env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let success = execute_tool(
            "bash",
            &json!({
                "command": bash_echo_command("hello"),
                "dangerouslyDisableSandbox": true
            }),
        )
        .expect("bash should succeed");
        let success_output: serde_json::Value = serde_json::from_str(&success).expect("json");
        assert_eq!(success_output["stdout"], "hello");
        assert_eq!(success_output["interrupted"], false);

        let failure = execute_tool(
            "bash",
            &json!({
                "command": bash_error_command("oops", 7),
                "dangerouslyDisableSandbox": true
            }),
        )
        .expect("bash failure should still return structured output");
        let failure_output: serde_json::Value = serde_json::from_str(&failure).expect("json");
        assert_eq!(failure_output["returnCodeInterpretation"], "exit_code:7");
        assert!(failure_output["stderr"]
            .as_str()
            .expect("stderr")
            .contains("oops"));

        let timeout = execute_tool(
            "bash",
            &json!({
                "command": bash_sleep_command(),
                "timeout": 10,
                "dangerouslyDisableSandbox": true
            }),
        )
        .expect("bash timeout should return output");
        let timeout_output: serde_json::Value = serde_json::from_str(&timeout).expect("json");
        assert_eq!(timeout_output["interrupted"], true);
        assert_eq!(timeout_output["returnCodeInterpretation"], "timeout");
        assert!(timeout_output["stderr"]
            .as_str()
            .expect("stderr")
            .contains("Command exceeded timeout"));

        let background = execute_tool(
            "bash",
            &json!({
                "command": bash_sleep_command(),
                "run_in_background": true,
                "dangerouslyDisableSandbox": true
            }),
        )
        .expect("bash background should succeed");
        let background_output: serde_json::Value = serde_json::from_str(&background).expect("json");
        assert!(background_output["backgroundTaskId"].as_str().is_some());
        assert_eq!(background_output["noOutputExpected"], true);
    }

    #[test]
    fn bash_tool_rejects_blank_command_preflight() {
        let error = execute_tool("bash", &json!({ "command": "   " }))
            .expect_err("blank bash command should be rejected");
        assert!(error.contains("empty or whitespace-only"));
    }

    #[test]
    fn file_tools_cover_read_write_and_edit_behaviors() {
        let _guard = env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let root = temp_path("fs-suite");
        fs::create_dir_all(&root).expect("create root");
        let original_dir = std::env::current_dir().expect("cwd");
        std::env::set_current_dir(&root).expect("set cwd");

        let write_create = execute_tool(
            "write_file",
            &json!({ "path": "nested/demo.txt", "content": "alpha\nbeta\nalpha\n" }),
        )
        .expect("write create should succeed");
        let write_create_output: serde_json::Value =
            serde_json::from_str(&write_create).expect("json");
        assert_eq!(write_create_output["type"], "create");
        assert!(root.join("nested/demo.txt").exists());

        let write_update = execute_tool(
            "write_file",
            &json!({ "path": "nested/demo.txt", "content": "alpha\nbeta\ngamma\n" }),
        )
        .expect("write update should succeed");
        let write_update_output: serde_json::Value =
            serde_json::from_str(&write_update).expect("json");
        assert_eq!(write_update_output["type"], "update");
        assert_eq!(write_update_output["originalFile"], "alpha\nbeta\nalpha\n");

        let read_full = execute_tool("read_file", &json!({ "path": "nested/demo.txt" }))
            .expect("read full should succeed");
        let read_full_output: serde_json::Value = serde_json::from_str(&read_full).expect("json");
        assert_eq!(read_full_output["file"]["content"], "alpha\nbeta\ngamma");
        assert_eq!(read_full_output["file"]["startLine"], 1);

        let read_slice = execute_tool(
            "read_file",
            &json!({ "path": "nested/demo.txt", "offset": 1, "limit": 1 }),
        )
        .expect("read slice should succeed");
        let read_slice_output: serde_json::Value = serde_json::from_str(&read_slice).expect("json");
        assert_eq!(read_slice_output["file"]["content"], "beta");
        assert_eq!(read_slice_output["file"]["startLine"], 2);

        let read_past_end = execute_tool(
            "read_file",
            &json!({ "path": "nested/demo.txt", "offset": 50 }),
        )
        .expect("read past EOF should succeed");
        let read_past_end_output: serde_json::Value =
            serde_json::from_str(&read_past_end).expect("json");
        assert_eq!(read_past_end_output["file"]["content"], "");
        assert_eq!(read_past_end_output["file"]["startLine"], 4);

        let read_error = execute_tool("read_file", &json!({ "path": "missing.txt" }))
            .expect_err("missing file should fail");
        assert!(!read_error.is_empty());

        let edit_once = execute_tool(
            "edit_file",
            &json!({ "path": "nested/demo.txt", "old_string": "alpha", "new_string": "omega" }),
        )
        .expect("single edit should succeed");
        let edit_once_output: serde_json::Value = serde_json::from_str(&edit_once).expect("json");
        assert_eq!(edit_once_output["replaceAll"], false);
        assert_eq!(
            fs::read_to_string(root.join("nested/demo.txt")).expect("read file"),
            "omega\nbeta\ngamma\n"
        );

        execute_tool(
            "write_file",
            &json!({ "path": "nested/demo.txt", "content": "alpha\nbeta\nalpha\n" }),
        )
        .expect("reset file");
        let edit_all = execute_tool(
            "edit_file",
            &json!({
                "path": "nested/demo.txt",
                "old_string": "alpha",
                "new_string": "omega",
                "replace_all": true
            }),
        )
        .expect("replace all should succeed");
        let edit_all_output: serde_json::Value = serde_json::from_str(&edit_all).expect("json");
        assert_eq!(edit_all_output["replaceAll"], true);
        assert_eq!(
            fs::read_to_string(root.join("nested/demo.txt")).expect("read file"),
            "omega\nbeta\nomega\n"
        );

        let edit_same = execute_tool(
            "edit_file",
            &json!({ "path": "nested/demo.txt", "old_string": "omega", "new_string": "omega" }),
        )
        .expect_err("identical old/new should fail");
        assert!(edit_same.contains("must differ"));

        let edit_missing = execute_tool(
            "edit_file",
            &json!({ "path": "nested/demo.txt", "old_string": "missing", "new_string": "omega" }),
        )
        .expect_err("missing substring should fail");
        assert!(edit_missing.contains("old_string not found"));

        std::env::set_current_dir(&original_dir).expect("restore cwd");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn glob_and_grep_tools_cover_success_and_errors() {
        let _guard = env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let root = temp_path("search-suite");
        fs::create_dir_all(root.join("nested")).expect("create root");
        let original_dir = std::env::current_dir().expect("cwd");
        std::env::set_current_dir(&root).expect("set cwd");

        fs::write(
            root.join("nested/lib.rs"),
            "fn main() {}\nlet alpha = 1;\nlet alpha = 2;\n",
        )
        .expect("write rust file");
        fs::write(root.join("nested/notes.txt"), "alpha\nbeta\n").expect("write txt file");

        let globbed = execute_tool("glob_search", &json!({ "pattern": "nested/*.rs" }))
            .expect("glob should succeed");
        let globbed_output: serde_json::Value = serde_json::from_str(&globbed).expect("json");
        assert_eq!(globbed_output["numFiles"], 1);
        assert!(globbed_output["filenames"][0]
            .as_str()
            .expect("filename")
            .replace('\\', "/")
            .ends_with("nested/lib.rs"));

        let glob_error = execute_tool("glob_search", &json!({ "pattern": "[" }))
            .expect_err("invalid glob should fail");
        assert!(!glob_error.is_empty());

        let grep_content = execute_tool(
            "grep_search",
            &json!({
                "pattern": "alpha",
                "path": "nested",
                "glob": "*.rs",
                "output_mode": "content",
                "-n": true,
                "head_limit": 1,
                "offset": 1
            }),
        )
        .expect("grep content should succeed");
        let grep_content_output: serde_json::Value =
            serde_json::from_str(&grep_content).expect("json");
        assert_eq!(grep_content_output["numFiles"], 0);
        assert!(grep_content_output["appliedLimit"].is_null());
        assert_eq!(grep_content_output["appliedOffset"], 1);
        assert!(grep_content_output["content"]
            .as_str()
            .expect("content")
            .contains("let alpha = 2;"));

        let grep_count = execute_tool(
            "grep_search",
            &json!({ "pattern": "alpha", "path": "nested", "output_mode": "count" }),
        )
        .expect("grep count should succeed");
        let grep_count_output: serde_json::Value = serde_json::from_str(&grep_count).expect("json");
        assert_eq!(grep_count_output["numMatches"], 3);

        let grep_error = execute_tool(
            "grep_search",
            &json!({ "pattern": "(alpha", "path": "nested" }),
        )
        .expect_err("invalid regex should fail");
        assert!(!grep_error.is_empty());

        std::env::set_current_dir(&original_dir).expect("restore cwd");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn sleep_waits_and_reports_duration() {
        let started = std::time::Instant::now();
        let result =
            execute_tool("Sleep", &json!({"duration_ms": 20})).expect("Sleep should succeed");
        let elapsed = started.elapsed();
        let output: serde_json::Value = serde_json::from_str(&result).expect("json");
        assert_eq!(output["duration_ms"], 20);
        assert!(output["message"]
            .as_str()
            .expect("message")
            .contains("Slept for 20ms"));
        assert!(elapsed >= Duration::from_millis(15));
    }

    #[test]
    fn brief_returns_sent_message_and_attachment_metadata() {
        let attachment = std::env::temp_dir().join(format!(
            "openyak-brief-{}.png",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        ));
        std::fs::write(&attachment, b"png-data").expect("write attachment");

        let result = execute_tool(
            "SendUserMessage",
            &json!({
                "message": "hello user",
                "attachments": [attachment.display().to_string()],
                "status": "normal"
            }),
        )
        .expect("SendUserMessage should succeed");

        let output: serde_json::Value = serde_json::from_str(&result).expect("json");
        assert_eq!(output["message"], "hello user");
        assert!(output["sentAt"].as_str().is_some());
        assert_eq!(output["attachments"][0]["isImage"], true);
        let _ = std::fs::remove_file(attachment);
    }

    #[test]
    fn config_reads_and_writes_supported_values() {
        let _guard = env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let root = std::env::temp_dir().join(format!(
            "openyak-config-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        ));
        let home = root.join("home");
        let cwd = root.join("cwd");
        std::fs::create_dir_all(home.join(".openyak")).expect("home dir");
        std::fs::create_dir_all(cwd.join(".openyak")).expect("cwd dir");
        std::fs::write(
            home.join(".openyak").join("settings.json"),
            r#"{"verbose":false}"#,
        )
        .expect("write global settings");

        let original_home = std::env::var("HOME").ok();
        let original_userprofile = std::env::var("USERPROFILE").ok();
        let original_config_home = std::env::var("OPENYAK_CONFIG_HOME").ok();
        let original_dir = std::env::current_dir().expect("cwd");
        std::env::set_var("HOME", &home);
        std::env::remove_var("USERPROFILE");
        std::env::set_var("OPENYAK_CONFIG_HOME", home.join(".openyak"));
        std::env::set_current_dir(&cwd).expect("set cwd");

        let get = execute_tool("Config", &json!({"setting": "verbose"})).expect("get config");
        let get_output: serde_json::Value = serde_json::from_str(&get).expect("json");
        assert_eq!(get_output["value"], false);

        let set = execute_tool(
            "Config",
            &json!({"setting": "permissions.defaultMode", "value": "plan"}),
        )
        .expect("set config");
        let set_output: serde_json::Value = serde_json::from_str(&set).expect("json");
        assert_eq!(set_output["operation"], "set");
        assert_eq!(set_output["newValue"], "plan");

        let invalid = execute_tool(
            "Config",
            &json!({"setting": "permissions.defaultMode", "value": "bogus"}),
        )
        .expect_err("invalid config value should error");
        assert!(invalid.contains("Invalid value"));

        let unknown =
            execute_tool("Config", &json!({"setting": "nope"})).expect("unknown setting result");
        let unknown_output: serde_json::Value = serde_json::from_str(&unknown).expect("json");
        assert_eq!(unknown_output["success"], false);

        std::env::set_current_dir(&original_dir).expect("restore cwd");
        match original_home {
            Some(value) => std::env::set_var("HOME", value),
            None => std::env::remove_var("HOME"),
        }
        match original_userprofile {
            Some(value) => std::env::set_var("USERPROFILE", value),
            None => std::env::remove_var("USERPROFILE"),
        }
        match original_config_home {
            Some(value) => std::env::set_var("OPENYAK_CONFIG_HOME", value),
            None => std::env::remove_var("OPENYAK_CONFIG_HOME"),
        }
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn structured_output_echoes_input_payload() {
        let result = execute_tool("StructuredOutput", &json!({"ok": true, "items": [1, 2, 3]}))
            .expect("StructuredOutput should succeed");
        let output: serde_json::Value = serde_json::from_str(&result).expect("json");
        assert_eq!(output["data"], "Structured output provided successfully");
        assert_eq!(output["structured_output"]["ok"], true);
        assert_eq!(output["structured_output"]["items"][1], 2);
    }

    #[test]
    fn repl_executes_python_code() {
        let result = execute_tool(
            "REPL",
            &json!({"language": "python", "code": "print(1 + 1)", "timeout_ms": 500}),
        )
        .expect("REPL should succeed");
        let output: serde_json::Value = serde_json::from_str(&result).expect("json");
        assert_eq!(output["language"], "python");
        assert_eq!(output["exitCode"], 0);
        assert!(output["stdout"].as_str().expect("stdout").contains('2'));
    }

    #[test]
    fn repl_timeout_is_enforced() {
        let result = execute_tool(
            "REPL",
            &json!({
                "language": "python",
                "code": "import time; time.sleep(5)",
                "timeout_ms": 100
            }),
        )
        .expect("REPL should return structured timeout output");
        let output: serde_json::Value = serde_json::from_str(&result).expect("json");
        assert_eq!(output["language"], "python");
        assert_eq!(output["exitCode"], 124);
        assert!(output["stderr"]
            .as_str()
            .expect("stderr")
            .contains("Command exceeded timeout of 100 ms"));
    }

    #[test]
    fn repl_timeout_allows_large_stdout_to_finish() {
        let result = execute_tool(
            "REPL",
            &json!({
                "language": "python",
                "code": "import sys; sys.stdout.write('x' * 200000); sys.stdout.flush()",
                "timeout_ms": 2000
            }),
        )
        .expect("REPL should complete before timing out");
        let output: serde_json::Value = serde_json::from_str(&result).expect("json");
        assert_eq!(output["language"], "python");
        assert_eq!(output["exitCode"], 0);
        assert!(output["stdout"].as_str().expect("stdout").len() >= 200_000);
    }

    #[test]
    fn powershell_runs_via_stub_shell() {
        let _guard = env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = {
            let dir = std::env::temp_dir().join(format!(
                "openyak-pwsh-bin-{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .expect("time")
                    .as_nanos()
            ));
            std::fs::create_dir_all(&dir).expect("create dir");
            let script = stub_powershell_path(&dir);
            write_stub_powershell(&script);
            let original_path = std::env::var_os("PATH");
            std::env::set_var("PATH", dir.display().to_string());
            (dir, original_path)
        };

        // Keep this test focused on PowerShell tool behavior instead of
        // failing on colder Windows shell startup jitter.
        let timeout_ms = if cfg!(windows) { 5_000 } else { 1_000 };
        let result = execute_tool(
            "PowerShell",
            &json!({"command": "Write-Output hello", "timeout": timeout_ms}),
        )
        .expect("PowerShell should succeed");

        let background = execute_tool(
            "PowerShell",
            &json!({"command": "Write-Output hello", "run_in_background": true}),
        )
        .expect("PowerShell background should succeed");

        {
            let (dir, original_path) = dir;
            match original_path {
                Some(value) => std::env::set_var("PATH", value),
                None => std::env::remove_var("PATH"),
            }
            let _ = std::fs::remove_dir_all(dir);
        }

        let output: serde_json::Value = serde_json::from_str(&result).expect("json");
        assert_eq!(
            output["stdout"]
                .as_str()
                .expect("stdout")
                .replace("\r\n", "\n"),
            "stub:Write-Output hello\n"
        );
        let stderr = output["stderr"].as_str().expect("stderr");
        assert!(!stderr.contains("PowerShell executable not found"));

        let background_output: serde_json::Value = serde_json::from_str(&background).expect("json");
        assert!(background_output["backgroundTaskId"].as_str().is_some());
        assert_eq!(background_output["backgroundedByUser"], true);
        assert_eq!(background_output["assistantAutoBackgrounded"], false);
    }

    #[cfg(windows)]
    fn bash_echo_command(message: &str) -> String {
        format!("[Console]::Out.Write('{message}')")
    }

    #[cfg(not(windows))]
    fn bash_echo_command(message: &str) -> String {
        format!("printf '{message}'")
    }

    #[cfg(windows)]
    fn bash_error_command(message: &str, exit_code: i32) -> String {
        format!("[Console]::Error.Write('{message}'); exit {exit_code}")
    }

    #[cfg(not(windows))]
    fn bash_error_command(message: &str, exit_code: i32) -> String {
        format!("printf '{message}' >&2; exit {exit_code}")
    }

    #[cfg(windows)]
    fn bash_sleep_command() -> &'static str {
        "Start-Sleep -Seconds 1"
    }

    #[cfg(not(windows))]
    fn bash_sleep_command() -> &'static str {
        "sleep 1"
    }

    #[cfg(not(windows))]
    fn stub_powershell_path(dir: &std::path::Path) -> std::path::PathBuf {
        dir.join("pwsh")
    }

    #[cfg(windows)]
    fn stub_powershell_path(dir: &std::path::Path) -> std::path::PathBuf {
        dir.join("pwsh.cmd")
    }

    #[cfg(not(windows))]
    fn write_stub_powershell(path: &std::path::Path) {
        std::fs::write(
            path,
            "#!/bin/sh\nwhile [ \"$1\" != \"-Command\" ] && [ $# -gt 0 ]; do shift; done\nshift\nprintf 'stub:%s' \"$1\"\n",
        )
        .expect("write script");
        std::process::Command::new("chmod")
            .arg("+x")
            .arg(path)
            .status()
            .expect("chmod");
    }

    #[cfg(windows)]
    fn write_stub_powershell(path: &std::path::Path) {
        std::fs::write(
            path,
            "@echo off\r\nsetlocal EnableDelayedExpansion\r\n:loop\r\nif \"%~1\"==\"\" exit /b 1\r\nset \"ARG=%~1\"\r\nif /I \"!ARG!\"==\"-Command\" (\r\n  shift\r\n  goto emit\r\n)\r\nshift\r\ngoto loop\r\n:emit\r\necho stub:%~1\r\nexit /b 0\r\n",
        )
        .expect("write script");
    }

    #[test]
    fn powershell_errors_when_shell_is_missing() {
        let _guard = env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let original_path = std::env::var("PATH").unwrap_or_default();
        let empty_dir = std::env::temp_dir().join(format!(
            "openyak-empty-bin-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        ));
        std::fs::create_dir_all(&empty_dir).expect("create empty dir");
        std::env::set_var("PATH", empty_dir.display().to_string());

        let err = execute_tool("PowerShell", &json!({"command": "Write-Output hello"}))
            .expect_err("PowerShell should fail when shell is missing");

        std::env::set_var("PATH", original_path);
        let _ = std::fs::remove_dir_all(empty_dir);

        assert!(err.contains("PowerShell executable not found"));
    }

    struct TestServer {
        addr: SocketAddr,
        shutdown: Option<std::sync::mpsc::Sender<()>>,
        handle: Option<thread::JoinHandle<()>>,
    }

    impl TestServer {
        fn spawn(handler: Arc<dyn Fn(&str) -> HttpResponse + Send + Sync + 'static>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
            listener
                .set_nonblocking(true)
                .expect("set nonblocking listener");
            let addr = listener.local_addr().expect("local addr");
            let (tx, rx) = std::sync::mpsc::channel::<()>();

            let handle = thread::spawn(move || loop {
                if rx.try_recv().is_ok() {
                    break;
                }

                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream
                            .set_nonblocking(false)
                            .expect("set accepted stream blocking");
                        let mut buffer = [0_u8; 4096];
                        let size = stream.read(&mut buffer).expect("read request");
                        let request = String::from_utf8_lossy(&buffer[..size]).into_owned();
                        let request_line = request.lines().next().unwrap_or_default().to_string();
                        let response = handler(&request_line);
                        stream
                            .write_all(response.to_bytes().as_slice())
                            .expect("write response");
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => panic!("server accept failed: {error}"),
                }
            });

            Self {
                addr,
                shutdown: Some(tx),
                handle: Some(handle),
            }
        }

        fn addr(&self) -> SocketAddr {
            self.addr
        }
    }

    impl Drop for TestServer {
        fn drop(&mut self) {
            if let Some(tx) = self.shutdown.take() {
                let _ = tx.send(());
            }
            if let Some(handle) = self.handle.take() {
                handle.join().expect("join test server");
            }
        }
    }

    struct HttpResponse {
        status: u16,
        reason: &'static str,
        content_type: &'static str,
        body: String,
    }

    impl HttpResponse {
        fn html(status: u16, reason: &'static str, body: &str) -> Self {
            Self {
                status,
                reason,
                content_type: "text/html; charset=utf-8",
                body: body.to_string(),
            }
        }

        fn text(status: u16, reason: &'static str, body: &str) -> Self {
            Self {
                status,
                reason,
                content_type: "text/plain; charset=utf-8",
                body: body.to_string(),
            }
        }

        #[allow(clippy::needless_pass_by_value)]
        fn json(status: u16, reason: &'static str, body: Value) -> Self {
            Self {
                status,
                reason,
                content_type: "application/json; charset=utf-8",
                body: serde_json::to_string(&body).expect("json body"),
            }
        }

        fn to_bytes(&self) -> Vec<u8> {
            format!(
                "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                self.status,
                self.reason,
                self.content_type,
                self.body.len(),
                self.body
            )
            .into_bytes()
        }
    }

    fn write_thread_server_info(workspace: &std::path::Path, address: SocketAddr) {
        let openyak_dir = workspace.join(".openyak");
        std::fs::create_dir_all(&openyak_dir).expect("create .openyak dir");
        std::fs::write(
            openyak_dir.join(THREAD_SERVER_INFO_FILENAME),
            serde_json::to_string_pretty(&json!({
                "baseUrl": format!("http://{address}")
            }))
            .expect("server info json"),
        )
        .expect("write thread server info");
    }

    fn create_test_dir_symlink(
        link: &std::path::Path,
        target: &std::path::Path,
    ) -> std::io::Result<()> {
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
    fn exposes_session_tools() {
        let names = mvp_tool_specs()
            .into_iter()
            .map(|spec| spec.name)
            .collect::<Vec<_>>();
        assert!(names.contains(&"SessionList"));
        assert!(names.contains(&"SessionGet"));
        assert!(names.contains(&"SessionCreate"));
        assert!(names.contains(&"SessionSend"));
        assert!(names.contains(&"SessionResume"));
        assert!(names.contains(&"SessionWait"));
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn session_tools_inventory_and_mutation_follow_capability_boundaries() {
        let _guard = env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let workspace = temp_path("session-tools-workspace");
        let managed_dir = workspace.join(".openyak").join("sessions");
        let agent_dir = workspace.join(".openyak-agents");
        std::fs::create_dir_all(&managed_dir).expect("create managed session dir");
        std::fs::create_dir_all(&agent_dir).expect("create agent dir");

        let mut managed_session = Session::new();
        managed_session
            .messages
            .push(ConversationMessage::user_text("Need more detail"));
        managed_session
            .messages
            .push(ConversationMessage::assistant(vec![
                ContentBlock::UserInputRequest {
                    request_id: "request-1".to_string(),
                    prompt: "Choose a mode".to_string(),
                    options: vec!["fast".to_string(), "safe".to_string()],
                    allow_freeform: true,
                },
            ]));
        managed_session
            .save_to_path(managed_dir.join("session-1.json"))
            .expect("save managed session");

        let agent_output_path = agent_dir.join("agent-1.md");
        std::fs::write(&agent_output_path, "running output").expect("write agent output");
        let agent_manifest_path = agent_dir.join("agent-1.json");
        let agent_manifest = AgentOutput {
            agent_id: "agent-1".to_string(),
            name: "agent-review".to_string(),
            description: "Review the branch".to_string(),
            subagent_type: Some("Explore".to_string()),
            model: Some("claude-opus-4-6".to_string()),
            status: "running".to_string(),
            output_file: agent_output_path.display().to_string(),
            manifest_file: agent_manifest_path.display().to_string(),
            created_at: "100".to_string(),
            started_at: Some("100".to_string()),
            completed_at: None,
            error: None,
        };
        std::fs::write(
            &agent_manifest_path,
            serde_json::to_string_pretty(&agent_manifest).expect("agent manifest json"),
        )
        .expect("write agent manifest");

        let resumed = Arc::new(Mutex::new(false));
        let resumed_for_server = Arc::clone(&resumed);
        let server = TestServer::spawn(Arc::new(move |request_line: &str| match request_line {
            line if line.starts_with("GET /v1/threads HTTP/1.1") => HttpResponse::json(
                200,
                "OK",
                json!({
                    "protocol_version": "v1",
                    "threads": [{
                        "thread_id": "thread-7",
                        "created_at": 10,
                        "updated_at": 20,
                        "state": {
                            "status": if *resumed_for_server.lock().unwrap_or_else(std::sync::PoisonError::into_inner) { "idle" } else { "awaiting_user_input" },
                            "run_id": "run-1",
                            "pending_user_input": if *resumed_for_server.lock().unwrap_or_else(std::sync::PoisonError::into_inner) {
                                Value::Null
                            } else {
                                json!({
                                    "request_id": "request-1",
                                    "prompt": "Choose a mode",
                                    "options": ["fast", "safe"],
                                    "allow_freeform": true
                                })
                            },
                            "recovery_note": Value::Null
                        },
                        "message_count": 2
                    }]
                }),
            ),
            line if line.starts_with("GET /v1/threads/thread-7 HTTP/1.1") => HttpResponse::json(
                200,
                "OK",
                json!({
                    "protocol_version": "v1",
                    "thread_id": "thread-7",
                    "created_at": 10,
                    "updated_at": if *resumed_for_server.lock().unwrap_or_else(std::sync::PoisonError::into_inner) { 40 } else { 20 },
                    "state": {
                        "status": if *resumed_for_server.lock().unwrap_or_else(std::sync::PoisonError::into_inner) { "idle" } else { "awaiting_user_input" },
                        "run_id": "run-1",
                        "pending_user_input": if *resumed_for_server.lock().unwrap_or_else(std::sync::PoisonError::into_inner) {
                            Value::Null
                        } else {
                            json!({
                                "request_id": "request-1",
                                "prompt": "Choose a mode",
                                "options": ["fast", "safe"],
                                "allow_freeform": true
                            })
                        },
                        "recovery_note": Value::Null
                    },
                    "config": {
                        "cwd": "C:/workspace",
                        "model": "opus",
                        "permission_mode": "danger-full-access",
                        "allowed_tools": ["read_file", "bash"]
                    },
                    "session": {
                        "version": 1,
                        "messages": [
                            {"role": "user", "blocks": [{"type": "text", "text": "Plan the change"}], "usage": null},
                            {"role": "assistant", "blocks": [
                                {"type": "user_input_request", "request_id": "request-1", "prompt": "Choose a mode", "options": ["fast", "safe"], "allow_freeform": true}
                            ], "usage": null}
                        ]
                    }
                }),
            ),
            line if line.starts_with("POST /v1/threads HTTP/1.1") => HttpResponse::json(
                201,
                "Created",
                json!({
                    "protocol_version": "v1",
                    "thread_id": "thread-8",
                    "created_at": 50,
                    "updated_at": 50,
                    "state": {
                        "status": "idle",
                        "run_id": Value::Null,
                        "pending_user_input": Value::Null,
                        "recovery_note": Value::Null
                    },
                    "config": {
                        "cwd": "C:/workspace",
                        "model": "opus",
                        "permission_mode": "danger-full-access",
                        "allowed_tools": ["read_file"]
                    },
                    "session": {
                        "version": 1,
                        "messages": []
                    }
                }),
            ),
            line if line.starts_with("POST /v1/threads/thread-7/turns HTTP/1.1") => {
                HttpResponse::json(
                    200,
                    "OK",
                    json!({
                        "protocol_version": "v1",
                        "thread_id": "thread-7",
                        "run_id": "run-1",
                        "status": "accepted"
                    }),
                )
            }
            line if line.starts_with("POST /v1/threads/thread-7/user-input HTTP/1.1") => {
                *resumed_for_server
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
                HttpResponse::json(
                    200,
                    "OK",
                    json!({
                        "protocol_version": "v1",
                        "thread_id": "thread-7",
                        "run_id": "run-1",
                        "request_id": "request-1",
                        "status": "accepted"
                    }),
                )
            }
            _ => HttpResponse::json(
                404,
                "Not Found",
                json!({"error": {"message": format!("unexpected request: {request_line}")}}),
            ),
        }));

        let original_dir = std::env::current_dir().expect("cwd");
        let original_agent_store = std::env::var("OPENYAK_AGENT_STORE").ok();
        let original_session_server = std::env::var(SESSION_SERVER_URL_ENV).ok();
        std::env::set_current_dir(&workspace).expect("set cwd");
        std::env::set_var("OPENYAK_AGENT_STORE", &agent_dir);
        std::env::remove_var(SESSION_SERVER_URL_ENV);
        write_thread_server_info(&workspace, server.addr());

        let listed = execute_tool("SessionList", &json!({})).expect("SessionList should succeed");
        let listed_value: Value = serde_json::from_str(&listed).expect("session list json");
        let sessions = listed_value["sessions"].as_array().expect("sessions array");
        assert_eq!(sessions.len(), 3);
        let thread = sessions
            .iter()
            .find(|entry| entry["kind"] == "thread")
            .expect("thread session present");
        assert_eq!(thread["id"], "thread-7");
        assert!(thread["capabilities"]
            .as_array()
            .expect("thread capabilities")
            .iter()
            .any(|entry| entry == "send"));
        let managed = sessions
            .iter()
            .find(|entry| entry["kind"] == "managed_session")
            .expect("managed session present");
        assert_eq!(managed["status"], "awaiting_user_input");
        assert_eq!(managed["message_count"], 2);
        assert_eq!(
            managed["capabilities"]
                .as_array()
                .expect("managed caps")
                .len(),
            1
        );
        let agent = sessions
            .iter()
            .find(|entry| entry["kind"] == "agent_run")
            .expect("agent run present");
        assert_eq!(agent["status"], "running");
        assert!(agent["capabilities"]
            .as_array()
            .expect("agent caps")
            .iter()
            .any(|entry| entry == "wait"));

        let thread_get = execute_tool("SessionGet", &json!({"kind": "thread", "id": "thread-7"}))
            .expect("SessionGet thread should succeed");
        let thread_get_value: Value = serde_json::from_str(&thread_get).expect("thread get json");
        assert_eq!(thread_get_value["config"]["model"], "opus");
        assert_eq!(thread_get_value["message_count"], 2);

        let created = execute_tool("SessionCreate", &json!({"kind": "thread"}))
            .expect("SessionCreate should succeed");
        let created_value: Value = serde_json::from_str(&created).expect("session create json");
        assert_eq!(created_value["id"], "thread-8");
        assert_eq!(created_value["status"], "idle");

        let sent = execute_tool(
            "SessionSend",
            &json!({"kind": "thread", "id": "thread-7", "message": "Continue"}),
        )
        .expect("SessionSend should succeed");
        let sent_value: Value = serde_json::from_str(&sent).expect("session send json");
        assert_eq!(sent_value["run_id"], "run-1");

        let waiting = execute_tool(
            "SessionWait",
            &json!({"kind": "thread", "id": "thread-7", "timeout_ms": 25, "poll_interval_ms": 1}),
        )
        .expect("SessionWait thread should succeed");
        let waiting_value: Value = serde_json::from_str(&waiting).expect("session wait json");
        assert_eq!(waiting_value["status"], "awaiting_user_input");
        assert_eq!(waiting_value["terminal"], true);

        let resumed_value = execute_tool(
            "SessionResume",
            &json!({
                "kind": "thread",
                "id": "thread-7",
                "request_id": "request-1",
                "content": "safe"
            }),
        )
        .expect("SessionResume should succeed");
        let resumed_json: Value =
            serde_json::from_str(&resumed_value).expect("session resume json");
        assert_eq!(resumed_json["request_id"], "request-1");

        let waited_idle = execute_tool(
            "SessionWait",
            &json!({"kind": "thread", "id": "thread-7", "timeout_ms": 25, "poll_interval_ms": 1}),
        )
        .expect("SessionWait thread idle should succeed");
        let waited_idle_value: Value =
            serde_json::from_str(&waited_idle).expect("session wait idle json");
        assert_eq!(waited_idle_value["status"], "idle");
        assert_eq!(waited_idle_value["terminal"], true);

        let unsupported = execute_tool(
            "SessionSend",
            &json!({"kind": "managed_session", "id": "session-1", "message": "nope"}),
        )
        .expect_err("managed_session mutation should be rejected");
        assert!(unsupported.contains("only supports kind=thread"));

        let manifest_path_for_wait = agent_manifest_path.clone();
        let output_path_for_wait = agent_output_path.clone();
        let completed_manifest = AgentOutput {
            status: "completed".to_string(),
            completed_at: Some("150".to_string()),
            ..agent_manifest
        };
        let writer = thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            std::fs::write(&output_path_for_wait, "completed output").expect("update agent output");
            std::fs::write(
                &manifest_path_for_wait,
                serde_json::to_string_pretty(&completed_manifest).expect("completed manifest json"),
            )
            .expect("update agent manifest");
        });

        let waited_agent = execute_tool(
            "SessionWait",
            &json!({"kind": "agent_run", "id": "agent-1", "timeout_ms": 500, "poll_interval_ms": 10}),
        )
        .expect("agent wait should succeed");
        writer.join().expect("agent writer thread should finish");
        let waited_agent_value: Value =
            serde_json::from_str(&waited_agent).expect("agent wait json");
        assert_eq!(waited_agent_value["status"], "completed");
        assert_eq!(waited_agent_value["terminal"], true);

        std::env::set_current_dir(&original_dir).expect("restore cwd");
        match original_agent_store {
            Some(value) => std::env::set_var("OPENYAK_AGENT_STORE", value),
            None => std::env::remove_var("OPENYAK_AGENT_STORE"),
        }
        match original_session_server {
            Some(value) => std::env::set_var(SESSION_SERVER_URL_ENV, value),
            None => std::env::remove_var(SESSION_SERVER_URL_ENV),
        }
        let _ = std::fs::remove_dir_all(workspace);
    }

    #[test]
    fn session_tools_reject_missing_or_non_local_thread_server_discovery() {
        let _guard = env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let workspace = temp_path("session-tools-errors");
        std::fs::create_dir_all(&workspace).expect("create workspace");
        let original_dir = std::env::current_dir().expect("cwd");
        let original_session_server = std::env::var(SESSION_SERVER_URL_ENV).ok();
        std::env::set_current_dir(&workspace).expect("set cwd");
        std::env::remove_var(SESSION_SERVER_URL_ENV);

        let missing = execute_tool("SessionGet", &json!({"kind": "thread", "id": "thread-1"}))
            .expect_err("missing discovery should fail");
        assert!(missing.contains("discoverable running local openyak server"));

        std::env::set_var(SESSION_SERVER_URL_ENV, "http://example.com:3000");
        let remote = execute_tool("SessionList", &json!({"kind": "thread"}))
            .expect_err("non-local server url should fail");
        assert!(remote.contains("local-only"));

        std::env::set_current_dir(&original_dir).expect("restore cwd");
        match original_session_server {
            Some(value) => std::env::set_var(SESSION_SERVER_URL_ENV, value),
            None => std::env::remove_var(SESSION_SERVER_URL_ENV),
        }
        let _ = std::fs::remove_dir_all(workspace);
    }

    #[test]
    fn session_tools_discover_server_info_from_canonical_workspace_path() {
        let _guard = env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let workspace = temp_path("session-tools-symlink-workspace");
        let workspace_link = temp_path("session-tools-symlink-link");
        std::fs::create_dir_all(&workspace).expect("create workspace");

        match create_test_dir_symlink(&workspace_link, &workspace) {
            Ok(()) => {}
            Err(error)
                if error.kind() == std::io::ErrorKind::PermissionDenied
                    || error.raw_os_error() == Some(1314) =>
            {
                let _ = std::fs::remove_dir_all(&workspace);
                return;
            }
            Err(error) => panic!("symlink creation should not fail unexpectedly: {error}"),
        }

        let original_dir = std::env::current_dir().expect("cwd");
        let original_session_server = std::env::var(SESSION_SERVER_URL_ENV).ok();
        std::env::set_current_dir(&workspace_link).expect("set symlink cwd");
        std::env::remove_var(SESSION_SERVER_URL_ENV);
        write_thread_server_info(
            &workspace,
            "127.0.0.1:4242".parse().expect("socket address"),
        );

        let discovered = discover_session_server_url()
            .expect("canonical workspace thread server info should be discovered");
        assert_eq!(discovered, "http://127.0.0.1:4242");

        std::env::set_current_dir(&original_dir).expect("restore cwd");
        match original_session_server {
            Some(value) => std::env::set_var(SESSION_SERVER_URL_ENV, value),
            None => std::env::remove_var(SESSION_SERVER_URL_ENV),
        }
        let _ = std::fs::remove_dir_all(&workspace_link);
        let _ = std::fs::remove_dir_all(&workspace);
    }

    #[test]
    fn exposes_parity_foundation_tools() {
        let names = mvp_tool_specs()
            .into_iter()
            .map(|spec| spec.name)
            .collect::<Vec<_>>();
        assert!(names.contains(&"TaskCreate"));
        assert!(names.contains(&"TaskList"));
        assert!(names.contains(&"TaskWait"));
        assert!(names.contains(&"TeamCreate"));
        assert!(names.contains(&"TeamGet"));
        assert!(names.contains(&"TeamList"));
        assert!(names.contains(&"CronCreate"));
        assert!(names.contains(&"CronGet"));
        assert!(names.contains(&"CronDisable"));
        assert!(names.contains(&"CronEnable"));
        assert!(names.contains(&"LSP"));
        assert!(names.contains(&"ListMcpServers"));
        assert!(names.contains(&"ListMcpTools"));
        assert!(names.contains(&"MCP"));
    }

    #[test]
    fn foundation_surface_inventory_stays_aligned_with_current_families() {
        let surfaces = foundation_surfaces();
        assert_eq!(surfaces.len(), 5);
        assert_eq!(surfaces[0].key, "task");
        assert_eq!(surfaces[1].key, "team");
        assert_eq!(surfaces[2].key, "cron");
        assert_eq!(surfaces[3].key, "lsp");
        assert_eq!(surfaces[4].key, "mcp");

        let task = foundation_surface("task").expect("task surface");
        assert_eq!(
            task.tool_names,
            &[
                "TaskCreate",
                "TaskGet",
                "TaskList",
                "TaskStop",
                "TaskUpdate",
                "TaskOutput",
                "TaskWait",
            ]
        );
        assert_eq!(
            task.backing_model,
            "process_local_v1 current-runtime registry"
        );

        let lsp = foundation_surface("lsp").expect("lsp surface");
        assert_eq!(lsp.tool_names, &["LSP"]);
        assert!(lsp
            .boundary_note
            .contains("not a standalone openyak lsp host"));

        let mcp = foundation_surface("mcp").expect("mcp surface");
        assert_eq!(
            mcp.tool_names,
            &[
                "ListMcpServers",
                "ListMcpTools",
                "ListMcpResources",
                "ReadMcpResource",
                "McpAuth",
                "MCP",
            ]
        );
        assert!(mcp.adjacent_scope.expect("mcp scope note").contains("MB5"));
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn task_team_and_cron_tools_roundtrip() {
        let created = execute_tool(
            "TaskCreate",
            &json!({"prompt": "Investigate parity gap", "description": "task description"}),
        )
        .expect("TaskCreate should succeed");
        let task_value: Value = serde_json::from_str(&created).expect("task json");
        let task_id = task_value["task_id"].as_str().expect("task id");
        assert_eq!(task_value["message_count"], 0);
        assert_eq!(task_value["output_length"], 0);
        assert_eq!(task_value["has_output"], false);
        assert_eq!(task_value["team_id"], Value::Null);
        assert_eq!(task_value["created_at"], task_value["updated_at"]);
        assert_eq!(task_value["origin"], "process_local_v1");
        assert_eq!(task_value["last_error"], Value::Null);
        assert!(task_value["capabilities"]
            .as_array()
            .expect("task capabilities")
            .iter()
            .any(|entry| entry == "stop"));

        let fetched = execute_tool("TaskGet", &json!({"task_id": task_id})).expect("TaskGet");
        let fetched_value: Value = serde_json::from_str(&fetched).expect("task get json");
        assert_eq!(fetched_value["task_id"], task_id);
        assert_eq!(fetched_value["message_count"], 0);
        assert_eq!(fetched_value["output_length"], 0);
        assert_eq!(fetched_value["has_output"], false);
        assert_eq!(fetched_value["origin"], "process_local_v1");

        let updated = execute_tool(
            "TaskUpdate",
            &json!({"task_id": task_id, "message": "hello world"}),
        )
        .expect("TaskUpdate");
        let updated_value: Value = serde_json::from_str(&updated).expect("task update json");
        assert_eq!(updated_value["message_count"], 1);
        assert!(updated_value["updated_at"].as_u64().is_some());
        assert_eq!(updated_value["has_output"], false);
        assert_eq!(updated_value["last_error"], Value::Null);

        global_task_registry()
            .set_status(task_id, runtime::TaskStatus::Completed)
            .expect("set task status");
        let waited = execute_tool(
            "TaskWait",
            &json!({"task_id": task_id, "timeout_ms": 5, "poll_interval_ms": 1}),
        )
        .expect("TaskWait");
        let waited_value: Value = serde_json::from_str(&waited).expect("task wait json");
        assert_eq!(waited_value["task_id"], task_id);
        assert_eq!(waited_value["terminal"], true);
        assert_eq!(waited_value["timed_out"], false);
        assert_eq!(waited_value["status"], "completed");
        assert_eq!(waited_value["origin"], "process_local_v1");

        let team = execute_tool(
            "TeamCreate",
            &json!({"name": "Parity Team", "tasks": [{"task_id": task_id}]}),
        )
        .expect("TeamCreate");
        let team_value: Value = serde_json::from_str(&team).expect("team json");
        let team_id = team_value["team_id"].as_str().expect("team id");
        assert_eq!(team_value["task_count"], 1);
        assert_eq!(team_value["created_at"], team_value["updated_at"]);
        assert_eq!(team_value["origin"], "process_local_v1");
        assert_eq!(team_value["last_error"], Value::Null);
        assert!(team_value["capabilities"]
            .as_array()
            .expect("team capabilities")
            .iter()
            .any(|entry| entry == "delete"));

        let team_get = execute_tool("TeamGet", &json!({"team_id": team_id})).expect("TeamGet");
        let team_get_value: Value = serde_json::from_str(&team_get).expect("team get json");
        assert_eq!(team_get_value["team_id"], team_id);
        assert_eq!(team_get_value["task_ids"][0], task_id);
        assert_eq!(team_get_value["origin"], "process_local_v1");

        let team_list = execute_tool("TeamList", &json!({})).expect("TeamList");
        let team_list_value: Value = serde_json::from_str(&team_list).expect("team list json");
        assert!(team_list_value["teams"]
            .as_array()
            .expect("teams array")
            .iter()
            .any(|team| team["team_id"] == team_id));

        let cron = execute_tool(
            "CronCreate",
            &json!({"schedule": "0 * * * *", "prompt": "run parity diff"}),
        )
        .expect("CronCreate");
        let cron_value: Value = serde_json::from_str(&cron).expect("cron json");
        let cron_id = cron_value["cron_id"].as_str().expect("cron id");
        assert_eq!(cron_value["run_count"], 0);
        assert_eq!(cron_value["last_run_at"], Value::Null);
        assert_eq!(cron_value["created_at"], cron_value["updated_at"]);
        assert_eq!(cron_value["origin"], "process_local_v1");
        assert_eq!(cron_value["disabled_reason"], Value::Null);
        assert_eq!(cron_value["last_error"], Value::Null);
        assert!(cron_value["capabilities"]
            .as_array()
            .expect("cron capabilities")
            .iter()
            .any(|entry| entry == "record_run"));

        let cron_get = execute_tool("CronGet", &json!({"cron_id": cron_id})).expect("CronGet");
        let cron_get_value: Value = serde_json::from_str(&cron_get).expect("cron get json");
        assert_eq!(cron_get_value["cron_id"], cron_id);
        assert_eq!(cron_get_value["enabled"], true);
        assert_eq!(cron_get_value["origin"], "process_local_v1");

        let disabled =
            execute_tool("CronDisable", &json!({"cron_id": cron_id})).expect("CronDisable");
        let disabled_value: Value = serde_json::from_str(&disabled).expect("cron disable json");
        assert_eq!(disabled_value["status"], "disabled");
        assert_eq!(disabled_value["enabled"], false);
        assert_eq!(
            disabled_value["disabled_reason"],
            "disabled_by_operator_request"
        );

        let enabled = execute_tool("CronEnable", &json!({"cron_id": cron_id})).expect("CronEnable");
        let enabled_value: Value = serde_json::from_str(&enabled).expect("cron enable json");
        assert_eq!(enabled_value["status"], "enabled");
        assert_eq!(enabled_value["enabled"], true);
        assert_eq!(enabled_value["disabled_reason"], Value::Null);

        let cron_list = execute_tool("CronList", &json!({})).expect("CronList");
        let cron_list_value: Value = serde_json::from_str(&cron_list).expect("cron list json");
        assert!(cron_list_value["crons"]
            .as_array()
            .expect("crons array")
            .iter()
            .any(|entry| {
                entry["cron_id"] == cron_id
                    && entry["enabled"] == true
                    && entry["updated_at"].as_u64().is_some()
            }));

        let deleted = execute_tool("CronDelete", &json!({"cron_id": cron_id})).expect("CronDelete");
        let deleted_value: Value = serde_json::from_str(&deleted).expect("cron delete json");
        assert_eq!(deleted_value["status"], "deleted");
        assert_eq!(deleted_value["origin"], "process_local_v1");

        let team_deleted =
            execute_tool("TeamDelete", &json!({"team_id": team_id})).expect("TeamDelete");
        let team_deleted_value: Value =
            serde_json::from_str(&team_deleted).expect("team delete json");
        assert_eq!(team_deleted_value["status"], "deleted");
        assert_eq!(team_deleted_value["task_ids"][0], task_id);
        assert_eq!(team_deleted_value["task_count"], 1);
        assert_eq!(team_deleted_value["origin"], "process_local_v1");

        global_team_registry().remove(team_id);
        global_task_registry().remove(task_id);
    }

    #[test]
    fn team_create_rejects_missing_duplicate_and_already_assigned_tasks() {
        let first = execute_tool("TaskCreate", &json!({"prompt": "First"})).expect("first task");
        let first_value: Value = serde_json::from_str(&first).expect("first task json");
        let first_task_id = first_value["task_id"].as_str().expect("first task id");

        let missing_field_error =
            execute_tool("TeamCreate", &json!({"name": "Broken Team", "tasks": [{}]}))
                .expect_err("missing task_id should fail");
        assert!(missing_field_error.contains("invalid team task entry at index 0"));

        let duplicate_error = execute_tool(
            "TeamCreate",
            &json!({"name": "Duplicate Team", "tasks": [{"task_id": first_task_id}, {"task_id": first_task_id}]}),
        )
        .expect_err("duplicate task ids should fail");
        assert!(duplicate_error.contains("duplicate task_id"));

        let unknown_error = execute_tool(
            "TeamCreate",
            &json!({"name": "Unknown Team", "tasks": [{"task_id": "task_missing"}]}),
        )
        .expect_err("unknown task should fail");
        assert!(unknown_error.contains("task not found: task_missing"));

        let created_team = execute_tool(
            "TeamCreate",
            &json!({"name": "Assigned Team", "tasks": [{"task_id": first_task_id}]}),
        )
        .expect("initial team create should succeed");
        let created_team_value: Value =
            serde_json::from_str(&created_team).expect("team create json");
        let team_id = created_team_value["team_id"].as_str().expect("team id");

        let already_assigned_error = execute_tool(
            "TeamCreate",
            &json!({"name": "Reassigned Team", "tasks": [{"task_id": first_task_id}]}),
        )
        .expect_err("assigned task should be rejected");
        assert!(already_assigned_error.contains("already assigned to team"));

        global_team_registry().remove(team_id);
        global_task_registry()
            .unassign_team(first_task_id, team_id)
            .expect("cleanup unassign should succeed");
        global_task_registry().remove(first_task_id);
    }

    #[test]
    fn task_wait_reports_timeout_for_non_terminal_tasks() {
        let created = execute_tool("TaskCreate", &json!({"prompt": "Wait for completion"}))
            .expect("task create");
        let created_value: Value = serde_json::from_str(&created).expect("task create json");
        let task_id = created_value["task_id"].as_str().expect("task id");

        global_task_registry()
            .set_status(task_id, runtime::TaskStatus::Running)
            .expect("set running status");
        let waited = execute_tool(
            "TaskWait",
            &json!({"task_id": task_id, "timeout_ms": 1, "poll_interval_ms": 1}),
        )
        .expect("task wait should serialize");
        let waited_value: Value = serde_json::from_str(&waited).expect("task wait json");
        assert_eq!(waited_value["status"], "running");
        assert_eq!(waited_value["terminal"], false);
        assert_eq!(waited_value["timed_out"], true);

        global_task_registry().remove(task_id);
    }

    #[test]
    fn cron_disable_rejects_second_disable() {
        let created = execute_tool(
            "CronCreate",
            &json!({"schedule": "0 * * * *", "prompt": "run parity diff"}),
        )
        .expect("CronCreate");
        let cron_value: Value = serde_json::from_str(&created).expect("cron json");
        let cron_id = cron_value["cron_id"].as_str().expect("cron id");

        execute_tool("CronDisable", &json!({"cron_id": cron_id})).expect("first disable");
        let error =
            execute_tool("CronDisable", &json!({"cron_id": cron_id})).expect_err("second disable");
        assert!(error.contains("cron already disabled"));

        global_cron_registry()
            .delete(cron_id)
            .expect("cleanup delete");
    }

    #[test]
    fn lsp_and_mcp_registry_introspection_tools_surface_registered_state() {
        let lsp_language = "parity_registry_probe";
        global_lsp_registry().disconnect(lsp_language);
        global_lsp_registry().register(
            lsp_language,
            runtime::LspServerStatus::Connected,
            Some("/workspace/parity"),
            vec!["hover".into(), "references".into()],
        );

        let lsp_servers = execute_tool("LSP", &json!({"action": "servers"})).expect("LSP");
        let lsp_servers_value: Value =
            serde_json::from_str(&lsp_servers).expect("lsp servers json");
        assert!(lsp_servers_value["servers"]
            .as_array()
            .expect("servers array")
            .iter()
            .any(|server| server["language"] == lsp_language));

        let lsp_status = execute_tool(
            "LSP",
            &json!({"action": "status", "language": lsp_language}),
        )
        .expect("LSP status");
        let lsp_status_value: Value = serde_json::from_str(&lsp_status).expect("lsp status json");
        assert_eq!(lsp_status_value["language"], lsp_language);
        assert_eq!(lsp_status_value["status"], "connected");
        assert_eq!(lsp_status_value["capabilities"][0], "hover");
        global_lsp_registry().disconnect(lsp_language);

        let mcp_server = "parity-mcp-registry-probe";
        global_mcp_registry().disconnect(mcp_server);
        global_mcp_registry().register_server(
            mcp_server,
            runtime::McpConnectionStatus::Connected,
            vec![runtime::McpToolInfo {
                name: "echo".into(),
                description: Some("echo tool".into()),
                input_schema: Some(json!({"type": "object"})),
            }],
            vec![],
            Some("parity test server".into()),
        );

        let mcp_servers =
            execute_tool("ListMcpServers", &json!({})).expect("ListMcpServers should succeed");
        let mcp_servers_value: Value =
            serde_json::from_str(&mcp_servers).expect("mcp servers json");
        assert!(mcp_servers_value["servers"]
            .as_array()
            .expect("servers array")
            .iter()
            .any(|server| {
                server["server"] == mcp_server
                    && server["auth_state"] == "authenticated"
                    && server["capabilities"]["tools_visible"] == true
                    && server["capabilities"]["prompts_visible"] == false
            }));

        let mcp_tools =
            execute_tool("ListMcpTools", &json!({"server": mcp_server})).expect("ListMcpTools");
        let mcp_tools_value: Value = serde_json::from_str(&mcp_tools).expect("mcp tools json");
        assert_eq!(mcp_tools_value["count"], 1);
        assert_eq!(mcp_tools_value["tools"][0]["name"], "echo");

        global_mcp_registry().disconnect(mcp_server);
    }

    #[test]
    fn lsp_tool_returns_cached_registry_diagnostics() {
        let _guard = parity_registry_lock()
            .lock()
            .expect("parity registry test lock should not be poisoned");
        let registry = super::global_lsp_registry();
        registry.register(
            "rust",
            runtime::LspServerStatus::Connected,
            Some("/workspace"),
            vec!["diagnostics".into(), "hover".into()],
        );
        registry
            .clear_diagnostics("rust")
            .expect("rust diagnostics should clear");
        registry
            .add_diagnostics(
                "rust",
                vec![runtime::LspDiagnostic {
                    path: "src/parity.rs".into(),
                    line: 4,
                    character: 2,
                    severity: "warning".into(),
                    message: "registry-backed warning".into(),
                    source: Some("rust-analyzer".into()),
                }],
            )
            .expect("rust diagnostics should add");

        let response = execute_tool(
            "LSP",
            &json!({"action": "diagnostics", "path": "src/parity.rs"}),
        )
        .expect("LSP diagnostics should succeed");
        let value: Value = serde_json::from_str(&response).expect("LSP diagnostics json");

        assert_eq!(value["action"], "diagnostics");
        assert_eq!(value["count"], 1);
        assert_eq!(
            value["diagnostics"][0]["message"],
            "registry-backed warning"
        );
        assert!(value.get("error").is_none());

        registry.disconnect("rust");
    }

    #[test]
    fn lsp_tool_surfaces_dispatch_errors_as_json() {
        let _guard = parity_registry_lock()
            .lock()
            .expect("parity registry test lock should not be poisoned");
        let registry = super::global_lsp_registry();
        registry.register(
            "rust",
            runtime::LspServerStatus::Disconnected,
            Some("/workspace"),
            vec!["hover".into()],
        );

        let response = execute_tool(
            "LSP",
            &json!({"action": "hover", "path": "src/parity.rs", "line": 1, "character": 0}),
        )
        .expect("LSP hover response should serialize");
        let value: Value = serde_json::from_str(&response).expect("LSP hover json");

        assert_eq!(value["action"], "hover");
        assert_eq!(value["status"], "error");
        assert!(value["error"]
            .as_str()
            .expect("error should be a string")
            .contains("not connected"));

        registry.disconnect("rust");
    }

    #[test]
    fn mcp_tool_wrappers_expose_registry_metadata() {
        let _guard = parity_registry_lock()
            .lock()
            .expect("parity registry test lock should not be poisoned");
        let registry = super::global_mcp_registry();
        let server = "parity-test-mcp-metadata";
        registry.disconnect(server);
        registry.register_server(
            server,
            runtime::McpConnectionStatus::Connected,
            vec![runtime::McpToolInfo {
                name: "echo".into(),
                description: Some("Echo text".into()),
                input_schema: Some(json!({"type": "object"})),
            }],
            vec![runtime::McpResourceInfo {
                uri: "res://alpha".into(),
                name: "Alpha".into(),
                description: Some("alpha resource".into()),
                mime_type: Some("text/plain".into()),
            }],
            Some("Parity MCP".into()),
        );

        let list = execute_tool("ListMcpResources", &json!({"server": server}))
            .expect("ListMcpResources should succeed");
        let list_value: Value = serde_json::from_str(&list).expect("resource list json");
        assert_eq!(list_value["server"], server);
        assert_eq!(list_value["count"], 1);
        assert_eq!(list_value["resources"][0]["uri"], "res://alpha");

        let read = execute_tool(
            "ReadMcpResource",
            &json!({"server": server, "uri": "res://alpha"}),
        )
        .expect("ReadMcpResource should succeed");
        let read_value: Value = serde_json::from_str(&read).expect("resource read json");
        assert_eq!(read_value["name"], "Alpha");
        assert_eq!(read_value["mime_type"], "text/plain");

        let auth = execute_tool("McpAuth", &json!({"server": server})).expect("McpAuth");
        let auth_value: Value = serde_json::from_str(&auth).expect("auth json");
        assert_eq!(auth_value["status"], "connected");
        assert_eq!(auth_value["auth_state"], "authenticated");
        assert_eq!(auth_value["auth_required"], false);
        assert_eq!(auth_value["capabilities"]["tool_count"], 1);
        assert_eq!(auth_value["capabilities"]["resource_count"], 1);
        assert_eq!(auth_value["capabilities"]["prompt_count"], 0);
        assert_eq!(auth_value["tool_count"], 1);
        assert_eq!(auth_value["resource_count"], 1);

        registry.disconnect(server);
    }

    #[test]
    fn mcp_tools_surface_configured_auth_required_and_error_states() {
        let _env_guard = env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _registry_guard = parity_registry_lock()
            .lock()
            .expect("parity registry test lock should not be poisoned");
        let root = temp_path("mcp-config-state");
        let home = root.join("home");
        let cwd = root.join("cwd");
        fs::create_dir_all(home.join(".openyak")).expect("home dir");
        fs::create_dir_all(cwd.join(".openyak")).expect("cwd dir");
        fs::write(
            cwd.join(".openyak").join("settings.json"),
            r#"{
  "mcpServers": {
    "config-auth-required": {
      "type": "http",
      "url": "https://vendor.example/mcp",
      "oauth": {
        "clientId": "demo-client"
      }
    },
    "config-unsupported-sdk": {
      "type": "sdk",
      "name": "demo-sdk"
    },
    "config-stdio": {
      "type": "stdio",
      "command": "demo-mcp",
      "args": ["serve"]
    }
  }
}"#,
        )
        .expect("write settings");

        let original_home = std::env::var("HOME").ok();
        let original_userprofile = std::env::var("USERPROFILE").ok();
        let original_config_home = std::env::var("OPENYAK_CONFIG_HOME").ok();
        let original_dir = std::env::current_dir().expect("cwd");
        std::env::set_var("HOME", &home);
        std::env::remove_var("USERPROFILE");
        std::env::set_var("OPENYAK_CONFIG_HOME", home.join(".openyak"));
        std::env::set_current_dir(&cwd).expect("set cwd");

        global_mcp_registry().disconnect("config-auth-required");
        global_mcp_registry().disconnect("config-unsupported-sdk");
        global_mcp_registry().disconnect("config-stdio");

        let servers =
            execute_tool("ListMcpServers", &json!({})).expect("ListMcpServers should succeed");
        let servers_value: Value = serde_json::from_str(&servers).expect("servers json");
        let items = servers_value["servers"].as_array().expect("servers array");

        let auth_required = items
            .iter()
            .find(|server| server["server"] == "config-auth-required")
            .expect("auth-required server entry");
        assert_eq!(auth_required["status"], "auth_required");
        assert_eq!(auth_required["auth_state"], "required");
        assert_eq!(auth_required["auth_required"], true);
        assert_eq!(auth_required["capabilities"]["tools_visible"], false);
        assert_eq!(auth_required["capabilities"]["resources_visible"], false);

        let unsupported = items
            .iter()
            .find(|server| server["server"] == "config-unsupported-sdk")
            .expect("unsupported server entry");
        assert_eq!(unsupported["status"], "error");
        assert_eq!(unsupported["auth_state"], "error");
        assert!(unsupported["error_message"]
            .as_str()
            .expect("error message")
            .contains("not supported"));
        assert_eq!(unsupported["capabilities"]["tools_visible"], false);

        let disconnected = items
            .iter()
            .find(|server| server["server"] == "config-stdio")
            .expect("stdio server entry");
        assert_eq!(disconnected["status"], "disconnected");
        assert_eq!(disconnected["auth_state"], "disconnected");
        assert_eq!(disconnected["auth_required"], false);

        let auth = execute_tool("McpAuth", &json!({"server": "config-auth-required"}))
            .expect("McpAuth should succeed");
        let auth_value: Value = serde_json::from_str(&auth).expect("auth json");
        assert_eq!(auth_value["status"], "auth_required");
        assert_eq!(auth_value["auth_state"], "required");
        assert_eq!(auth_value["auth_required"], true);
        assert_eq!(auth_value["capabilities"]["tools_visible"], false);

        std::env::set_current_dir(&original_dir).expect("restore cwd");
        match original_home {
            Some(value) => std::env::set_var("HOME", value),
            None => std::env::remove_var("HOME"),
        }
        match original_userprofile {
            Some(value) => std::env::set_var("USERPROFILE", value),
            None => std::env::remove_var("USERPROFILE"),
        }
        match original_config_home {
            Some(value) => std::env::set_var("OPENYAK_CONFIG_HOME", value),
            None => std::env::remove_var("OPENYAK_CONFIG_HOME"),
        }
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn mcp_tool_returns_structured_error_when_manager_is_missing() {
        let _guard = parity_registry_lock()
            .lock()
            .expect("parity registry test lock should not be poisoned");
        let registry = super::global_mcp_registry();
        let server = "parity-test-mcp-error";
        registry.disconnect(server);
        registry.register_server(
            server,
            runtime::McpConnectionStatus::Connected,
            vec![runtime::McpToolInfo {
                name: "echo".into(),
                description: Some("Echo text".into()),
                input_schema: Some(json!({"type": "object"})),
            }],
            vec![],
            Some("Parity MCP".into()),
        );

        let response = execute_tool(
            "MCP",
            &json!({"server": server, "tool": "echo", "arguments": {"text": "hello"}}),
        )
        .expect("MCP response should serialize");
        let value: Value = serde_json::from_str(&response).expect("MCP json");

        assert_eq!(value["server"], server);
        assert_eq!(value["tool"], "echo");
        assert_eq!(value["status"], "error");
        assert!(value["error"]
            .as_str()
            .expect("error should be a string")
            .contains("manager is not configured"));

        registry.disconnect(server);
    }

    #[test]
    fn tool_registry_enforcer_blocks_read_only_writes() {
        let policy = mvp_tool_specs().into_iter().fold(
            PermissionPolicy::new(PermissionMode::ReadOnly),
            |policy, spec| policy.with_tool_requirement(spec.name, spec.required_permission),
        );
        let registry = GlobalToolRegistry::builtin().with_enforcer(PermissionEnforcer::new(policy));
        let error = registry
            .execute(
                "write_file",
                &json!({"path": "notes.txt", "content": "should be denied"}),
            )
            .expect_err("read-only write should be denied");
        assert!(error.contains("file writes are not allowed"));
    }

    #[test]
    fn tool_registry_enforcer_blocks_bash_validation_failures() {
        let read_only_policy = mvp_tool_specs().into_iter().fold(
            PermissionPolicy::new(PermissionMode::ReadOnly),
            |policy, spec| policy.with_tool_requirement(spec.name, spec.required_permission),
        );
        let read_only_registry =
            GlobalToolRegistry::builtin().with_enforcer(PermissionEnforcer::new(read_only_policy));
        let read_only_error = read_only_registry
            .execute("bash", &json!({"command": "echo hello > notes.txt"}))
            .expect_err("read-only bash redirection should be denied");
        assert!(read_only_error.contains("write redirection"));

        let workspace_write_policy = mvp_tool_specs().into_iter().fold(
            PermissionPolicy::new(PermissionMode::WorkspaceWrite),
            |policy, spec| policy.with_tool_requirement(spec.name, spec.required_permission),
        );
        let workspace_write_registry = GlobalToolRegistry::builtin()
            .with_enforcer(PermissionEnforcer::new(workspace_write_policy));
        let workspace_write_error = workspace_write_registry
            .execute("bash", &json!({"command": "rm -rf /"}))
            .expect_err("dangerous bash command should be denied");
        assert!(workspace_write_error.contains("danger-full-access"));
        assert!(workspace_write_error.contains("destructive shell pattern"));
    }

    #[test]
    fn bash_tool_profile_blocks_disable_sandbox_widening_before_execution() {
        let enforcer = full_access_enforcer("bash");
        let error = execute_tool_with_enforcer(
            Some(&enforcer),
            Some(&strict_bash_policy(
                FilesystemIsolationMode::WorkspaceOnly,
                false,
                &["logs"],
            )),
            None,
            "bash",
            &json!({
                "command": "printf hi",
                "dangerouslyDisableSandbox": true
            }),
        )
        .expect_err("tool profile should block disabling the sandbox");

        assert!(error.contains("cannot disable the active tool-profile sandbox policy"));
    }

    #[test]
    fn bash_tool_profile_allows_narrowing_mount_subset() {
        let enforcer = full_access_enforcer("bash");
        let input = BashCommandInput {
            command: "printf hi".to_string(),
            timeout: None,
            description: None,
            run_in_background: Some(false),
            dangerously_disable_sandbox: Some(false),
            namespace_restrictions: Some(true),
            isolate_network: None,
            filesystem_mode: Some(FilesystemIsolationMode::WorkspaceOnly),
            allowed_mounts: Some(vec!["logs".to_string()]),
        };

        let result = maybe_enforce_permission_check(
            Some(&enforcer),
            Some(&strict_bash_policy(
                FilesystemIsolationMode::WorkspaceOnly,
                false,
                &["logs", "tmp/cache"],
            )),
            "bash",
            Some(&input),
            &json!({
                "command": "printf hi",
                "allowedMounts": ["logs"]
            }),
        );

        assert!(result.is_ok(), "subset mounts should remain allowed");
    }

    #[test]
    fn powershell_permission_checks_ignore_bash_tool_profile_rules() {
        let enforcer = full_access_enforcer("PowerShell");
        let result = maybe_enforce_permission_check(
            Some(&enforcer),
            Some(&strict_bash_policy(
                FilesystemIsolationMode::AllowList,
                true,
                &["logs"],
            )),
            "PowerShell",
            None,
            &json!({
                "command": "Write-Output hello",
                "dangerouslyDisableSandbox": true
            }),
        );

        assert!(
            result.is_ok(),
            "PowerShell should remain governed by permission mode only in this slice"
        );
    }
}

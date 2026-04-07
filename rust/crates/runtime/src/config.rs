use std::collections::BTreeMap;
use std::fmt::{Display, Formatter};
use std::fs;
use std::path::{Path, PathBuf};

use crate::json::JsonValue;
use crate::sandbox::{FilesystemIsolationMode, SandboxConfig};
use crate::tool_profile::{ToolProfileBashPolicy, ToolProfileConfig};
use serde_json::{Map as JsonMap, Value as JsonSerdeValue};

pub const OPENYAK_SETTINGS_SCHEMA_NAME: &str = "SettingsSchema";
const DEFAULT_BROWSER_ARTIFACTS_DIR: &str = ".openyak/artifacts/browser";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ConfigSource {
    User,
    Project,
    Local,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolvedPermissionMode {
    ReadOnly,
    WorkspaceWrite,
    DangerFullAccess,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigEntry {
    pub source: ConfigSource,
    pub path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeConfig {
    merged: BTreeMap<String, JsonValue>,
    loaded_entries: Vec<ConfigEntry>,
    feature_config: RuntimeFeatureConfig,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RuntimePluginConfig {
    enabled_plugins: BTreeMap<String, bool>,
    external_directories: Vec<String>,
    install_root: Option<String>,
    registry_path: Option<String>,
    bundled_root: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RuntimeSkillConfig {
    registry_path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeBrowserControlConfig {
    enabled: bool,
    artifacts_dir: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RuntimeFeatureConfig {
    hooks: RuntimeHookConfig,
    plugins: RuntimePluginConfig,
    skills: RuntimeSkillConfig,
    mcp: McpConfigCollection,
    tool_profiles: BTreeMap<String, ToolProfileConfig>,
    browser_control: RuntimeBrowserControlConfig,
    oauth: Option<OAuthConfig>,
    oauth_override: Option<OAuthConfigOverride>,
    model: Option<String>,
    permission_mode: Option<ResolvedPermissionMode>,
    sandbox: SandboxConfig,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RuntimeHookConfig {
    pre_tool_use: Vec<String>,
    post_tool_use: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct McpConfigCollection {
    servers: BTreeMap<String, ScopedMcpServerConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopedMcpServerConfig {
    pub scope: ConfigSource,
    pub config: McpServerConfig,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpTransport {
    Stdio,
    Sse,
    Http,
    Ws,
    Sdk,
    ManagedProxy,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpServerConfig {
    Stdio(McpStdioServerConfig),
    Sse(McpRemoteServerConfig),
    Http(McpRemoteServerConfig),
    Ws(McpWebSocketServerConfig),
    Sdk(McpSdkServerConfig),
    ManagedProxy(McpManagedProxyServerConfig),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpStdioServerConfig {
    pub command: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpRemoteServerConfig {
    pub url: String,
    pub headers: BTreeMap<String, String>,
    pub headers_helper: Option<String>,
    pub oauth: Option<McpOAuthConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpWebSocketServerConfig {
    pub url: String,
    pub headers: BTreeMap<String, String>,
    pub headers_helper: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpSdkServerConfig {
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpManagedProxyServerConfig {
    pub url: String,
    pub id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpOAuthConfig {
    pub client_id: Option<String>,
    pub callback_port: Option<u16>,
    pub auth_server_metadata_url: Option<String>,
    pub xaa: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuthConfig {
    pub client_id: String,
    pub authorize_url: String,
    pub token_url: String,
    pub callback_port: Option<u16>,
    pub manual_redirect_url: Option<String>,
    pub scopes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct OAuthConfigOverride {
    pub client_id: Option<String>,
    pub authorize_url: Option<String>,
    pub token_url: Option<String>,
    pub callback_port: Option<u16>,
    pub manual_redirect_url: Option<String>,
    pub scopes: Option<Vec<String>>,
}

impl OAuthConfigOverride {
    #[must_use]
    pub fn merged_with_defaults(&self, defaults: &OAuthConfig) -> OAuthConfig {
        OAuthConfig {
            client_id: self
                .client_id
                .clone()
                .unwrap_or_else(|| defaults.client_id.clone()),
            authorize_url: self
                .authorize_url
                .clone()
                .unwrap_or_else(|| defaults.authorize_url.clone()),
            token_url: self
                .token_url
                .clone()
                .unwrap_or_else(|| defaults.token_url.clone()),
            callback_port: self.callback_port.or(defaults.callback_port),
            manual_redirect_url: self
                .manual_redirect_url
                .clone()
                .or_else(|| defaults.manual_redirect_url.clone()),
            scopes: self
                .scopes
                .clone()
                .unwrap_or_else(|| defaults.scopes.clone()),
        }
    }

    #[must_use]
    pub fn resolved(&self) -> Option<OAuthConfig> {
        Some(OAuthConfig {
            client_id: self.client_id.clone()?,
            authorize_url: self.authorize_url.clone()?,
            token_url: self.token_url.clone()?,
            callback_port: self.callback_port,
            manual_redirect_url: self.manual_redirect_url.clone(),
            scopes: self.scopes.clone().unwrap_or_default(),
        })
    }

    #[must_use]
    fn is_empty(&self) -> bool {
        self.client_id.is_none()
            && self.authorize_url.is_none()
            && self.token_url.is_none()
            && self.callback_port.is_none()
            && self.manual_redirect_url.is_none()
            && self.scopes.is_none()
    }
}

#[derive(Debug)]
pub enum ConfigError {
    Io(std::io::Error),
    Parse(String),
}

impl Display for ConfigError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "{error}"),
            Self::Parse(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for ConfigError {}

impl From<std::io::Error> for ConfigError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

impl Default for RuntimeBrowserControlConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            artifacts_dir: PathBuf::from(DEFAULT_BROWSER_ARTIFACTS_DIR),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigLoader {
    cwd: PathBuf,
    config_home: PathBuf,
}

impl ConfigLoader {
    #[must_use]
    pub fn new(cwd: impl Into<PathBuf>, config_home: impl Into<PathBuf>) -> Self {
        Self {
            cwd: cwd.into(),
            config_home: config_home.into(),
        }
    }

    #[must_use]
    pub fn default_for(cwd: impl Into<PathBuf>) -> Self {
        let cwd = cwd.into();
        let config_home = default_config_home();
        Self { cwd, config_home }
    }

    #[must_use]
    pub fn config_home(&self) -> &Path {
        &self.config_home
    }

    #[must_use]
    pub fn user_settings_path(&self) -> PathBuf {
        self.config_home.join("settings.json")
    }

    #[must_use]
    pub fn discover(&self) -> Vec<ConfigEntry> {
        let user_legacy_path = self.config_home.parent().map_or_else(
            || PathBuf::from(".openyak.json"),
            |parent| parent.join(".openyak.json"),
        );
        vec![
            ConfigEntry {
                source: ConfigSource::User,
                path: user_legacy_path,
            },
            ConfigEntry {
                source: ConfigSource::User,
                path: self.config_home.join("settings.json"),
            },
            ConfigEntry {
                source: ConfigSource::Project,
                path: self.cwd.join(".openyak.json"),
            },
            ConfigEntry {
                source: ConfigSource::Project,
                path: self.cwd.join(".openyak").join("settings.json"),
            },
            ConfigEntry {
                source: ConfigSource::Local,
                path: self.cwd.join(".openyak").join("settings.local.json"),
            },
        ]
    }

    pub fn load(&self) -> Result<RuntimeConfig, ConfigError> {
        let mut merged = BTreeMap::new();
        let mut loaded_entries = Vec::new();
        let mut mcp_servers = BTreeMap::new();

        for entry in self.discover() {
            let Some(value) = read_optional_json_object(&entry.path)? else {
                continue;
            };
            merge_mcp_servers(&mut mcp_servers, entry.source, &value, &entry.path)?;
            deep_merge_objects(&mut merged, &value);
            loaded_entries.push(entry);
        }

        let merged_value = JsonValue::Object(merged.clone());

        let oauth_override =
            parse_optional_oauth_override_config(&merged_value, "merged settings.oauth")?;

        let feature_config = RuntimeFeatureConfig {
            hooks: parse_optional_hooks_config(&merged_value)?,
            plugins: parse_optional_plugin_config(&merged_value)?,
            skills: parse_optional_skill_config(&merged_value)?,
            mcp: McpConfigCollection {
                servers: mcp_servers,
            },
            tool_profiles: parse_optional_tool_profiles(&merged_value)?,
            browser_control: parse_optional_browser_control(&merged_value, &self.cwd)?,
            oauth: oauth_override
                .as_ref()
                .and_then(OAuthConfigOverride::resolved),
            oauth_override,
            model: parse_optional_model(&merged_value),
            permission_mode: parse_optional_permission_mode(&merged_value)?,
            sandbox: parse_optional_sandbox_config(&merged_value)?,
        };

        Ok(RuntimeConfig {
            merged,
            loaded_entries,
            feature_config,
        })
    }

    pub fn write_user_model(&self, model: &str) -> Result<PathBuf, ConfigError> {
        let path = self.user_settings_path();
        let mut root = read_optional_serde_json_object(&path)?.unwrap_or_default();
        root.insert(
            "model".to_string(),
            JsonSerdeValue::String(model.to_string()),
        );
        fs::create_dir_all(&self.config_home)?;
        let rendered = serde_json::to_string_pretty(&JsonSerdeValue::Object(root))
            .map_err(|error| ConfigError::Parse(format!("{}: {error}", path.display())))?;
        fs::write(&path, format!("{rendered}\n"))?;
        Ok(path)
    }
}

impl RuntimeConfig {
    #[must_use]
    pub fn empty() -> Self {
        Self {
            merged: BTreeMap::new(),
            loaded_entries: Vec::new(),
            feature_config: RuntimeFeatureConfig::default(),
        }
    }

    #[must_use]
    pub fn merged(&self) -> &BTreeMap<String, JsonValue> {
        &self.merged
    }

    #[must_use]
    pub fn loaded_entries(&self) -> &[ConfigEntry] {
        &self.loaded_entries
    }

    #[must_use]
    pub fn get(&self, key: &str) -> Option<&JsonValue> {
        self.merged.get(key)
    }

    #[must_use]
    pub fn as_json(&self) -> JsonValue {
        JsonValue::Object(self.merged.clone())
    }

    #[must_use]
    pub fn feature_config(&self) -> &RuntimeFeatureConfig {
        &self.feature_config
    }

    #[must_use]
    pub fn mcp(&self) -> &McpConfigCollection {
        &self.feature_config.mcp
    }

    #[must_use]
    pub fn hooks(&self) -> &RuntimeHookConfig {
        &self.feature_config.hooks
    }

    #[must_use]
    pub fn plugins(&self) -> &RuntimePluginConfig {
        &self.feature_config.plugins
    }

    #[must_use]
    pub fn skills(&self) -> &RuntimeSkillConfig {
        &self.feature_config.skills
    }

    #[must_use]
    pub fn oauth(&self) -> Option<&OAuthConfig> {
        self.feature_config.oauth.as_ref()
    }

    #[must_use]
    pub fn tool_profiles(&self) -> &BTreeMap<String, ToolProfileConfig> {
        &self.feature_config.tool_profiles
    }

    #[must_use]
    pub fn browser_control(&self) -> &RuntimeBrowserControlConfig {
        &self.feature_config.browser_control
    }

    #[must_use]
    pub fn oauth_override(&self) -> Option<&OAuthConfigOverride> {
        self.feature_config.oauth_override.as_ref()
    }

    #[must_use]
    pub fn model(&self) -> Option<&str> {
        self.feature_config.model.as_deref()
    }

    #[must_use]
    pub fn permission_mode(&self) -> Option<ResolvedPermissionMode> {
        self.feature_config.permission_mode
    }

    #[must_use]
    pub fn sandbox(&self) -> &SandboxConfig {
        &self.feature_config.sandbox
    }
}

impl RuntimeFeatureConfig {
    #[must_use]
    pub fn with_hooks(mut self, hooks: RuntimeHookConfig) -> Self {
        self.hooks = hooks;
        self
    }

    #[must_use]
    pub fn with_plugins(mut self, plugins: RuntimePluginConfig) -> Self {
        self.plugins = plugins;
        self
    }

    #[must_use]
    pub fn with_skills(mut self, skills: RuntimeSkillConfig) -> Self {
        self.skills = skills;
        self
    }

    #[must_use]
    pub fn hooks(&self) -> &RuntimeHookConfig {
        &self.hooks
    }

    #[must_use]
    pub fn plugins(&self) -> &RuntimePluginConfig {
        &self.plugins
    }

    #[must_use]
    pub fn skills(&self) -> &RuntimeSkillConfig {
        &self.skills
    }

    #[must_use]
    pub fn mcp(&self) -> &McpConfigCollection {
        &self.mcp
    }

    #[must_use]
    pub fn tool_profiles(&self) -> &BTreeMap<String, ToolProfileConfig> {
        &self.tool_profiles
    }

    #[must_use]
    pub fn browser_control(&self) -> &RuntimeBrowserControlConfig {
        &self.browser_control
    }

    #[must_use]
    pub fn oauth(&self) -> Option<&OAuthConfig> {
        self.oauth.as_ref()
    }

    #[must_use]
    pub fn oauth_override(&self) -> Option<&OAuthConfigOverride> {
        self.oauth_override.as_ref()
    }

    #[must_use]
    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    #[must_use]
    pub fn permission_mode(&self) -> Option<ResolvedPermissionMode> {
        self.permission_mode
    }

    #[must_use]
    pub fn sandbox(&self) -> &SandboxConfig {
        &self.sandbox
    }
}

impl RuntimePluginConfig {
    #[must_use]
    pub fn enabled_plugins(&self) -> &BTreeMap<String, bool> {
        &self.enabled_plugins
    }

    #[must_use]
    pub fn external_directories(&self) -> &[String] {
        &self.external_directories
    }

    #[must_use]
    pub fn install_root(&self) -> Option<&str> {
        self.install_root.as_deref()
    }

    #[must_use]
    pub fn registry_path(&self) -> Option<&str> {
        self.registry_path.as_deref()
    }

    #[must_use]
    pub fn bundled_root(&self) -> Option<&str> {
        self.bundled_root.as_deref()
    }

    pub fn set_plugin_state(&mut self, plugin_id: String, enabled: bool) {
        self.enabled_plugins.insert(plugin_id, enabled);
    }

    #[must_use]
    pub fn state_for(&self, plugin_id: &str, default_enabled: bool) -> bool {
        self.enabled_plugins
            .get(plugin_id)
            .copied()
            .unwrap_or(default_enabled)
    }
}

impl RuntimeSkillConfig {
    #[must_use]
    pub fn registry_path(&self) -> Option<&str> {
        self.registry_path.as_deref()
    }
}

impl RuntimeBrowserControlConfig {
    #[must_use]
    pub fn new(enabled: bool, artifacts_dir: impl Into<PathBuf>) -> Self {
        Self {
            enabled,
            artifacts_dir: artifacts_dir.into(),
        }
    }

    #[must_use]
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    #[must_use]
    pub fn artifacts_dir(&self) -> &Path {
        &self.artifacts_dir
    }
}

#[must_use]
pub fn default_config_home() -> PathBuf {
    crate::paths::default_openyak_home()
}

impl RuntimeHookConfig {
    #[must_use]
    pub fn new(pre_tool_use: Vec<String>, post_tool_use: Vec<String>) -> Self {
        Self {
            pre_tool_use,
            post_tool_use,
        }
    }

    #[must_use]
    pub fn pre_tool_use(&self) -> &[String] {
        &self.pre_tool_use
    }

    #[must_use]
    pub fn post_tool_use(&self) -> &[String] {
        &self.post_tool_use
    }

    #[must_use]
    pub fn merged(&self, other: &Self) -> Self {
        let mut merged = self.clone();
        merged.extend(other);
        merged
    }

    pub fn extend(&mut self, other: &Self) {
        extend_unique(&mut self.pre_tool_use, other.pre_tool_use());
        extend_unique(&mut self.post_tool_use, other.post_tool_use());
    }
}

impl McpConfigCollection {
    #[must_use]
    pub fn servers(&self) -> &BTreeMap<String, ScopedMcpServerConfig> {
        &self.servers
    }

    #[must_use]
    pub fn get(&self, name: &str) -> Option<&ScopedMcpServerConfig> {
        self.servers.get(name)
    }
}

impl ScopedMcpServerConfig {
    #[must_use]
    pub fn transport(&self) -> McpTransport {
        self.config.transport()
    }
}

impl McpServerConfig {
    #[must_use]
    pub fn transport(&self) -> McpTransport {
        match self {
            Self::Stdio(_) => McpTransport::Stdio,
            Self::Sse(_) => McpTransport::Sse,
            Self::Http(_) => McpTransport::Http,
            Self::Ws(_) => McpTransport::Ws,
            Self::Sdk(_) => McpTransport::Sdk,
            Self::ManagedProxy(_) => McpTransport::ManagedProxy,
        }
    }
}

fn read_optional_json_object(
    path: &Path,
) -> Result<Option<BTreeMap<String, JsonValue>>, ConfigError> {
    let is_legacy_config = path.file_name().and_then(|name| name.to_str()) == Some(".openyak.json");
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(ConfigError::Io(error)),
    };

    if contents.trim().is_empty() {
        return Ok(Some(BTreeMap::new()));
    }

    let parsed = match JsonValue::parse(&contents) {
        Ok(parsed) => parsed,
        Err(_error) if is_legacy_config => return Ok(None),
        Err(error) => return Err(ConfigError::Parse(format!("{}: {error}", path.display()))),
    };
    let Some(object) = parsed.as_object() else {
        if is_legacy_config {
            return Ok(None);
        }
        return Err(ConfigError::Parse(format!(
            "{}: top-level settings value must be a JSON object",
            path.display()
        )));
    };
    Ok(Some(object.clone()))
}

fn read_optional_serde_json_object(
    path: &Path,
) -> Result<Option<JsonMap<String, JsonSerdeValue>>, ConfigError> {
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(ConfigError::Io(error)),
    };

    if contents.trim().is_empty() {
        return Ok(Some(JsonMap::new()));
    }

    let parsed: JsonSerdeValue = serde_json::from_str(&contents)
        .map_err(|error| ConfigError::Parse(format!("{}: {error}", path.display())))?;
    let Some(object) = parsed.as_object() else {
        return Err(ConfigError::Parse(format!(
            "{}: top-level settings value must be a JSON object",
            path.display()
        )));
    };
    Ok(Some(object.clone()))
}

fn merge_mcp_servers(
    target: &mut BTreeMap<String, ScopedMcpServerConfig>,
    source: ConfigSource,
    root: &BTreeMap<String, JsonValue>,
    path: &Path,
) -> Result<(), ConfigError> {
    let Some(mcp_servers) = root.get("mcpServers") else {
        return Ok(());
    };
    let servers = expect_object(mcp_servers, &format!("{}: mcpServers", path.display()))?;
    for (name, value) in servers {
        let parsed = parse_mcp_server_config(
            name,
            value,
            &format!("{}: mcpServers.{name}", path.display()),
        )?;
        target.insert(
            name.clone(),
            ScopedMcpServerConfig {
                scope: source,
                config: parsed,
            },
        );
    }
    Ok(())
}

fn parse_optional_model(root: &JsonValue) -> Option<String> {
    root.as_object()
        .and_then(|object| object.get("model"))
        .and_then(JsonValue::as_str)
        .map(ToOwned::to_owned)
}

fn parse_optional_hooks_config(root: &JsonValue) -> Result<RuntimeHookConfig, ConfigError> {
    let Some(object) = root.as_object() else {
        return Ok(RuntimeHookConfig::default());
    };
    let Some(hooks_value) = object.get("hooks") else {
        return Ok(RuntimeHookConfig::default());
    };
    let hooks = expect_object(hooks_value, "merged settings.hooks")?;
    Ok(RuntimeHookConfig {
        pre_tool_use: optional_string_array(hooks, "PreToolUse", "merged settings.hooks")?
            .unwrap_or_default(),
        post_tool_use: optional_string_array(hooks, "PostToolUse", "merged settings.hooks")?
            .unwrap_or_default(),
    })
}

fn parse_optional_plugin_config(root: &JsonValue) -> Result<RuntimePluginConfig, ConfigError> {
    let Some(object) = root.as_object() else {
        return Ok(RuntimePluginConfig::default());
    };

    let mut config = RuntimePluginConfig::default();
    if let Some(enabled_plugins) = object.get("enabledPlugins") {
        config.enabled_plugins = parse_bool_map(enabled_plugins, "merged settings.enabledPlugins")?;
    }

    let Some(plugins_value) = object.get("plugins") else {
        return Ok(config);
    };
    let plugins = expect_object(plugins_value, "merged settings.plugins")?;

    if let Some(enabled_value) = plugins.get("enabled") {
        config.enabled_plugins = parse_bool_map(enabled_value, "merged settings.plugins.enabled")?;
    }
    config.external_directories =
        optional_string_array(plugins, "externalDirectories", "merged settings.plugins")?
            .unwrap_or_default();
    config.install_root =
        optional_string(plugins, "installRoot", "merged settings.plugins")?.map(str::to_string);
    config.registry_path =
        optional_string(plugins, "registryPath", "merged settings.plugins")?.map(str::to_string);
    config.bundled_root =
        optional_string(plugins, "bundledRoot", "merged settings.plugins")?.map(str::to_string);
    Ok(config)
}

fn parse_optional_skill_config(root: &JsonValue) -> Result<RuntimeSkillConfig, ConfigError> {
    let Some(object) = root.as_object() else {
        return Ok(RuntimeSkillConfig::default());
    };
    let Some(skills_value) = object.get("skills") else {
        return Ok(RuntimeSkillConfig::default());
    };
    let skills = expect_object(skills_value, "merged settings.skills")?;
    Ok(RuntimeSkillConfig {
        registry_path: optional_string(skills, "registryPath", "merged settings.skills")?
            .map(str::to_string),
    })
}

fn parse_optional_tool_profiles(
    root: &JsonValue,
) -> Result<BTreeMap<String, ToolProfileConfig>, ConfigError> {
    let Some(object) = root.as_object() else {
        return Ok(BTreeMap::new());
    };
    let Some(profiles_value) = object.get("toolProfiles") else {
        return Ok(BTreeMap::new());
    };
    let profiles = expect_object(profiles_value, "merged settings.toolProfiles")?;
    let mut parsed = BTreeMap::new();

    for (profile_id, profile_value) in profiles {
        let context = format!("merged settings.toolProfiles.{profile_id}");
        let profile = expect_object(profile_value, &context)?;
        let permission_mode = optional_string(profile, "permissionMode", &context)?
            .ok_or_else(|| ConfigError::Parse(format!("{context}: missing field permissionMode")))
            .and_then(|value| {
                parse_permission_mode_label(value, &format!("{context}.permissionMode"))
            })?;
        let allowed_tools = optional_string_array(profile, "allowedTools", &context)?
            .ok_or_else(|| ConfigError::Parse(format!("{context}: missing field allowedTools")))?;
        let bash_policy = parse_optional_tool_profile_bash_policy(profile, &context)?;
        parsed.insert(
            profile_id.clone(),
            ToolProfileConfig {
                description: optional_string(profile, "description", &context)?.map(str::to_string),
                permission_mode,
                allowed_tools,
                bash_policy,
            },
        );
    }

    Ok(parsed)
}

fn parse_optional_browser_control(
    root: &JsonValue,
    workspace_root: &Path,
) -> Result<RuntimeBrowserControlConfig, ConfigError> {
    let default_config = default_browser_control_config(workspace_root)?;
    let Some(object) = root.as_object() else {
        return Ok(default_config);
    };
    let Some(browser_value) = object.get("browserControl") else {
        return Ok(default_config);
    };
    let browser = expect_object(browser_value, "merged settings.browserControl")?;
    let enabled =
        optional_bool(browser, "enabled", "merged settings.browserControl")?.unwrap_or(false);
    let artifacts_dir = optional_string(browser, "artifactsDir", "merged settings.browserControl")?
        .map_or(Ok(default_config.artifacts_dir().to_path_buf()), |value| {
            resolve_workspace_bound_path(
                workspace_root,
                value,
                "merged settings.browserControl.artifactsDir",
            )
        })?;

    Ok(RuntimeBrowserControlConfig {
        enabled,
        artifacts_dir,
    })
}

fn default_browser_control_config(
    workspace_root: &Path,
) -> Result<RuntimeBrowserControlConfig, ConfigError> {
    Ok(RuntimeBrowserControlConfig::new(
        false,
        resolve_workspace_bound_path(
            workspace_root,
            DEFAULT_BROWSER_ARTIFACTS_DIR,
            "merged settings.browserControl.artifactsDir",
        )?,
    ))
}

fn parse_optional_tool_profile_bash_policy(
    profile: &BTreeMap<String, JsonValue>,
    context: &str,
) -> Result<Option<ToolProfileBashPolicy>, ConfigError> {
    let Some(policy_value) = profile.get("bashPolicy") else {
        return Ok(None);
    };
    let policy_context = format!("{context}.bashPolicy");
    let policy = expect_object(policy_value, &policy_context)?;
    let sandbox_context = format!("{policy_context}.sandbox");
    let sandbox = policy
        .get("sandbox")
        .ok_or_else(|| ConfigError::Parse(format!("{policy_context}: missing field sandbox")))
        .and_then(|value| parse_sandbox_config_value(value, &sandbox_context))?;
    Ok(Some(ToolProfileBashPolicy {
        sandbox,
        allow_dangerously_disable_sandbox: optional_bool(
            policy,
            "allowDangerouslyDisableSandbox",
            &policy_context,
        )?
        .unwrap_or(false),
    }))
}

fn parse_optional_permission_mode(
    root: &JsonValue,
) -> Result<Option<ResolvedPermissionMode>, ConfigError> {
    let Some(object) = root.as_object() else {
        return Ok(None);
    };
    if let Some(mode) = object.get("permissionMode").and_then(JsonValue::as_str) {
        return parse_permission_mode_label(mode, "merged settings.permissionMode").map(Some);
    }
    let Some(mode) = object
        .get("permissions")
        .and_then(JsonValue::as_object)
        .and_then(|permissions| permissions.get("defaultMode"))
        .and_then(JsonValue::as_str)
    else {
        return Ok(None);
    };
    parse_permission_mode_label(mode, "merged settings.permissions.defaultMode").map(Some)
}

fn parse_permission_mode_label(
    mode: &str,
    context: &str,
) -> Result<ResolvedPermissionMode, ConfigError> {
    match mode {
        "default" | "plan" | "read-only" => Ok(ResolvedPermissionMode::ReadOnly),
        "acceptEdits" | "auto" | "workspace-write" => Ok(ResolvedPermissionMode::WorkspaceWrite),
        "dontAsk" | "danger-full-access" => Ok(ResolvedPermissionMode::DangerFullAccess),
        other => Err(ConfigError::Parse(format!(
            "{context}: unsupported permission mode {other}"
        ))),
    }
}

fn parse_optional_sandbox_config(root: &JsonValue) -> Result<SandboxConfig, ConfigError> {
    let Some(object) = root.as_object() else {
        return Ok(SandboxConfig::default());
    };
    let Some(sandbox_value) = object.get("sandbox") else {
        return Ok(SandboxConfig::default());
    };
    parse_sandbox_config_value(sandbox_value, "merged settings.sandbox")
}

fn resolve_workspace_bound_path(
    workspace_root: &Path,
    value: &str,
    context: &str,
) -> Result<PathBuf, ConfigError> {
    if value.trim().is_empty() {
        return Err(ConfigError::Parse(format!(
            "{context}: path must not be empty"
        )));
    }

    let path = PathBuf::from(value);
    let joined = if path.is_absolute() {
        path
    } else {
        workspace_root.join(path)
    };
    let normalized_workspace = normalize_virtual_path(workspace_root);
    let normalized_path = normalize_virtual_path(&joined);
    if !normalized_path.starts_with(&normalized_workspace) {
        return Err(ConfigError::Parse(format!(
            "{context}: resolved path must stay inside the workspace root {}",
            workspace_root.display()
        )));
    }
    let canonical_workspace = workspace_root.canonicalize().map_err(|error| {
        ConfigError::Parse(format!(
            "{context}: failed to canonicalize workspace root {}: {error}",
            workspace_root.display()
        ))
    })?;
    let existing_ancestor = deepest_existing_ancestor(&normalized_path).ok_or_else(|| {
        ConfigError::Parse(format!(
            "{context}: could not resolve an existing ancestor under workspace root {}",
            workspace_root.display()
        ))
    })?;
    let canonical_ancestor = existing_ancestor.canonicalize().map_err(|error| {
        ConfigError::Parse(format!(
            "{context}: failed to canonicalize existing ancestor {}: {error}",
            existing_ancestor.display()
        ))
    })?;
    if !canonical_ancestor.starts_with(&canonical_workspace) {
        return Err(ConfigError::Parse(format!(
            "{context}: resolved path must stay inside the workspace root {}",
            workspace_root.display()
        )));
    }
    Ok(normalized_path)
}

fn deepest_existing_ancestor(path: &Path) -> Option<&Path> {
    let mut current = Some(path);
    while let Some(candidate) = current {
        if candidate.exists() {
            return Some(candidate);
        }
        current = candidate.parent();
    }
    None
}

fn normalize_virtual_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                let _ = normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

fn parse_filesystem_mode_label(value: &str) -> Result<FilesystemIsolationMode, ConfigError> {
    match value {
        "off" => Ok(FilesystemIsolationMode::Off),
        "workspace-only" => Ok(FilesystemIsolationMode::WorkspaceOnly),
        "allow-list" => Ok(FilesystemIsolationMode::AllowList),
        other => Err(ConfigError::Parse(format!(
            "merged settings.sandbox.filesystemMode: unsupported filesystem mode {other}"
        ))),
    }
}

fn parse_sandbox_config_value(
    value: &JsonValue,
    context: &str,
) -> Result<SandboxConfig, ConfigError> {
    let sandbox = expect_object(value, context)?;
    let filesystem_mode = optional_string(sandbox, "filesystemMode", context)?
        .map(parse_filesystem_mode_label)
        .transpose()?;
    Ok(SandboxConfig {
        enabled: optional_bool(sandbox, "enabled", context)?,
        namespace_restrictions: optional_bool(sandbox, "namespaceRestrictions", context)?,
        network_isolation: optional_bool(sandbox, "networkIsolation", context)?,
        filesystem_mode,
        allowed_mounts: optional_string_array(sandbox, "allowedMounts", context)?
            .unwrap_or_default(),
    })
}

fn parse_optional_oauth_override_config(
    root: &JsonValue,
    context: &str,
) -> Result<Option<OAuthConfigOverride>, ConfigError> {
    let Some(oauth_value) = root.as_object().and_then(|object| object.get("oauth")) else {
        return Ok(None);
    };
    let object = expect_object(oauth_value, context)?;
    let oauth_override = OAuthConfigOverride {
        client_id: optional_string(object, "clientId", context)?.map(str::to_string),
        authorize_url: optional_string(object, "authorizeUrl", context)?.map(str::to_string),
        token_url: optional_string(object, "tokenUrl", context)?.map(str::to_string),
        callback_port: optional_u16(object, "callbackPort", context)?,
        manual_redirect_url: optional_string(object, "manualRedirectUrl", context)?
            .map(str::to_string),
        scopes: optional_string_array(object, "scopes", context)?,
    };
    Ok((!oauth_override.is_empty()).then_some(oauth_override))
}

fn parse_mcp_server_config(
    server_name: &str,
    value: &JsonValue,
    context: &str,
) -> Result<McpServerConfig, ConfigError> {
    let object = expect_object(value, context)?;
    let server_type = optional_string(object, "type", context)?.unwrap_or("stdio");
    match server_type {
        "stdio" => Ok(McpServerConfig::Stdio(McpStdioServerConfig {
            command: expect_string(object, "command", context)?.to_string(),
            args: optional_string_array(object, "args", context)?.unwrap_or_default(),
            env: optional_string_map(object, "env", context)?.unwrap_or_default(),
        })),
        "sse" => Ok(McpServerConfig::Sse(parse_mcp_remote_server_config(
            object, context,
        )?)),
        "http" => Ok(McpServerConfig::Http(parse_mcp_remote_server_config(
            object, context,
        )?)),
        "ws" => Ok(McpServerConfig::Ws(McpWebSocketServerConfig {
            url: expect_string(object, "url", context)?.to_string(),
            headers: optional_string_map(object, "headers", context)?.unwrap_or_default(),
            headers_helper: optional_string(object, "headersHelper", context)?.map(str::to_string),
        })),
        "sdk" => Ok(McpServerConfig::Sdk(McpSdkServerConfig {
            name: expect_string(object, "name", context)?.to_string(),
        })),
        "claudeai-proxy" => Ok(McpServerConfig::ManagedProxy(McpManagedProxyServerConfig {
            url: expect_string(object, "url", context)?.to_string(),
            id: expect_string(object, "id", context)?.to_string(),
        })),
        other => Err(ConfigError::Parse(format!(
            "{context}: unsupported MCP server type for {server_name}: {other}"
        ))),
    }
}

fn parse_mcp_remote_server_config(
    object: &BTreeMap<String, JsonValue>,
    context: &str,
) -> Result<McpRemoteServerConfig, ConfigError> {
    Ok(McpRemoteServerConfig {
        url: expect_string(object, "url", context)?.to_string(),
        headers: optional_string_map(object, "headers", context)?.unwrap_or_default(),
        headers_helper: optional_string(object, "headersHelper", context)?.map(str::to_string),
        oauth: parse_optional_mcp_oauth_config(object, context)?,
    })
}

fn parse_optional_mcp_oauth_config(
    object: &BTreeMap<String, JsonValue>,
    context: &str,
) -> Result<Option<McpOAuthConfig>, ConfigError> {
    let Some(value) = object.get("oauth") else {
        return Ok(None);
    };
    let oauth = expect_object(value, &format!("{context}.oauth"))?;
    Ok(Some(McpOAuthConfig {
        client_id: optional_string(oauth, "clientId", context)?.map(str::to_string),
        callback_port: optional_u16(oauth, "callbackPort", context)?,
        auth_server_metadata_url: optional_string(oauth, "authServerMetadataUrl", context)?
            .map(str::to_string),
        xaa: optional_bool(oauth, "xaa", context)?,
    }))
}

fn expect_object<'a>(
    value: &'a JsonValue,
    context: &str,
) -> Result<&'a BTreeMap<String, JsonValue>, ConfigError> {
    value
        .as_object()
        .ok_or_else(|| ConfigError::Parse(format!("{context}: expected JSON object")))
}

fn expect_string<'a>(
    object: &'a BTreeMap<String, JsonValue>,
    key: &str,
    context: &str,
) -> Result<&'a str, ConfigError> {
    object
        .get(key)
        .and_then(JsonValue::as_str)
        .ok_or_else(|| ConfigError::Parse(format!("{context}: missing string field {key}")))
}

fn optional_string<'a>(
    object: &'a BTreeMap<String, JsonValue>,
    key: &str,
    context: &str,
) -> Result<Option<&'a str>, ConfigError> {
    match object.get(key) {
        Some(value) => value
            .as_str()
            .map(Some)
            .ok_or_else(|| ConfigError::Parse(format!("{context}: field {key} must be a string"))),
        None => Ok(None),
    }
}

fn optional_bool(
    object: &BTreeMap<String, JsonValue>,
    key: &str,
    context: &str,
) -> Result<Option<bool>, ConfigError> {
    match object.get(key) {
        Some(value) => value
            .as_bool()
            .map(Some)
            .ok_or_else(|| ConfigError::Parse(format!("{context}: field {key} must be a boolean"))),
        None => Ok(None),
    }
}

fn optional_u16(
    object: &BTreeMap<String, JsonValue>,
    key: &str,
    context: &str,
) -> Result<Option<u16>, ConfigError> {
    match object.get(key) {
        Some(value) => {
            let Some(number) = value.as_i64() else {
                return Err(ConfigError::Parse(format!(
                    "{context}: field {key} must be an integer"
                )));
            };
            let number = u16::try_from(number).map_err(|_| {
                ConfigError::Parse(format!("{context}: field {key} is out of range"))
            })?;
            Ok(Some(number))
        }
        None => Ok(None),
    }
}

fn parse_bool_map(value: &JsonValue, context: &str) -> Result<BTreeMap<String, bool>, ConfigError> {
    let Some(map) = value.as_object() else {
        return Err(ConfigError::Parse(format!(
            "{context}: expected JSON object"
        )));
    };
    map.iter()
        .map(|(key, value)| {
            value
                .as_bool()
                .map(|enabled| (key.clone(), enabled))
                .ok_or_else(|| {
                    ConfigError::Parse(format!("{context}: field {key} must be a boolean"))
                })
        })
        .collect()
}

fn optional_string_array(
    object: &BTreeMap<String, JsonValue>,
    key: &str,
    context: &str,
) -> Result<Option<Vec<String>>, ConfigError> {
    match object.get(key) {
        Some(value) => {
            let Some(array) = value.as_array() else {
                return Err(ConfigError::Parse(format!(
                    "{context}: field {key} must be an array"
                )));
            };
            array
                .iter()
                .map(|item| {
                    item.as_str().map(ToOwned::to_owned).ok_or_else(|| {
                        ConfigError::Parse(format!(
                            "{context}: field {key} must contain only strings"
                        ))
                    })
                })
                .collect::<Result<Vec<_>, _>>()
                .map(Some)
        }
        None => Ok(None),
    }
}

fn optional_string_map(
    object: &BTreeMap<String, JsonValue>,
    key: &str,
    context: &str,
) -> Result<Option<BTreeMap<String, String>>, ConfigError> {
    match object.get(key) {
        Some(value) => {
            let Some(map) = value.as_object() else {
                return Err(ConfigError::Parse(format!(
                    "{context}: field {key} must be an object"
                )));
            };
            map.iter()
                .map(|(entry_key, entry_value)| {
                    entry_value
                        .as_str()
                        .map(|text| (entry_key.clone(), text.to_string()))
                        .ok_or_else(|| {
                            ConfigError::Parse(format!(
                                "{context}: field {key} must contain only string values"
                            ))
                        })
                })
                .collect::<Result<BTreeMap<_, _>, _>>()
                .map(Some)
        }
        None => Ok(None),
    }
}

fn deep_merge_objects(
    target: &mut BTreeMap<String, JsonValue>,
    source: &BTreeMap<String, JsonValue>,
) {
    for (key, value) in source {
        match (target.get_mut(key), value) {
            (Some(JsonValue::Object(existing)), JsonValue::Object(incoming)) => {
                deep_merge_objects(existing, incoming);
            }
            _ => {
                target.insert(key.clone(), value.clone());
            }
        }
    }
}

fn extend_unique(target: &mut Vec<String>, values: &[String]) {
    for value in values {
        push_unique(target, value.clone());
    }
}

fn push_unique(target: &mut Vec<String>, value: String) {
    if !target.iter().any(|existing| existing == &value) {
        target.push(value);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ConfigLoader, ConfigSource, McpServerConfig, McpTransport, ResolvedPermissionMode,
        OPENYAK_SETTINGS_SCHEMA_NAME,
    };
    use crate::json::JsonValue;
    use crate::sandbox::FilesystemIsolationMode;
    use std::fs;
    use std::io;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir() -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time should be after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("runtime-config-{nanos}"))
    }

    fn create_test_dir_symlink(link: &std::path::Path, target: &std::path::Path) -> io::Result<()> {
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
    fn rejects_non_object_settings_files() {
        let root = temp_dir();
        let cwd = root.join("project");
        let home = root.join("home").join(".openyak");
        fs::create_dir_all(&home).expect("home config dir");
        fs::create_dir_all(&cwd).expect("project dir");
        fs::write(home.join("settings.json"), "[]").expect("write bad settings");

        let error = ConfigLoader::new(&cwd, &home)
            .load()
            .expect_err("config should fail");
        assert!(error
            .to_string()
            .contains("top-level settings value must be a JSON object"));

        fs::remove_dir_all(root).expect("cleanup temp dir");
    }

    #[test]
    fn loads_and_merges_openyak_config_files_by_precedence() {
        let root = temp_dir();
        let cwd = root.join("project");
        let home = root.join("home").join(".openyak");
        fs::create_dir_all(cwd.join(".openyak")).expect("project config dir");
        fs::create_dir_all(&home).expect("home config dir");

        fs::write(
            home.parent().expect("home parent").join(".openyak.json"),
            r#"{"model":"haiku","env":{"A":"1"},"mcpServers":{"home":{"command":"uvx","args":["home"]}}}"#,
        )
        .expect("write user compat config");
        fs::write(
            home.join("settings.json"),
            r#"{"model":"sonnet","env":{"A2":"1"},"hooks":{"PreToolUse":["base"]},"permissions":{"defaultMode":"plan"}}"#,
        )
        .expect("write user settings");
        fs::write(
            cwd.join(".openyak.json"),
            r#"{"model":"project-compat","env":{"B":"2"}}"#,
        )
        .expect("write project compat config");
        fs::write(
            cwd.join(".openyak").join("settings.json"),
            r#"{"env":{"C":"3"},"hooks":{"PostToolUse":["project"]},"mcpServers":{"project":{"command":"uvx","args":["project"]}}}"#,
        )
        .expect("write project settings");
        fs::write(
            cwd.join(".openyak").join("settings.local.json"),
            r#"{"model":"opus","permissionMode":"acceptEdits"}"#,
        )
        .expect("write local settings");

        let loaded = ConfigLoader::new(&cwd, &home)
            .load()
            .expect("config should load");

        assert_eq!(OPENYAK_SETTINGS_SCHEMA_NAME, "SettingsSchema");
        assert_eq!(loaded.loaded_entries().len(), 5);
        assert_eq!(loaded.loaded_entries()[0].source, ConfigSource::User);
        assert_eq!(
            loaded.get("model"),
            Some(&JsonValue::String("opus".to_string()))
        );
        assert_eq!(loaded.model(), Some("opus"));
        assert_eq!(
            loaded.permission_mode(),
            Some(ResolvedPermissionMode::WorkspaceWrite)
        );
        assert_eq!(
            loaded
                .get("env")
                .and_then(JsonValue::as_object)
                .expect("env object")
                .len(),
            4
        );
        assert!(loaded
            .get("hooks")
            .and_then(JsonValue::as_object)
            .expect("hooks object")
            .contains_key("PreToolUse"));
        assert!(loaded
            .get("hooks")
            .and_then(JsonValue::as_object)
            .expect("hooks object")
            .contains_key("PostToolUse"));
        assert_eq!(loaded.hooks().pre_tool_use(), &["base".to_string()]);
        assert_eq!(loaded.hooks().post_tool_use(), &["project".to_string()]);
        assert!(loaded.mcp().get("home").is_some());
        assert!(loaded.mcp().get("project").is_some());

        fs::remove_dir_all(root).expect("cleanup temp dir");
    }

    #[test]
    fn parses_sandbox_config() {
        let root = temp_dir();
        let cwd = root.join("project");
        let home = root.join("home").join(".openyak");
        fs::create_dir_all(cwd.join(".openyak")).expect("project config dir");
        fs::create_dir_all(&home).expect("home config dir");

        fs::write(
            cwd.join(".openyak").join("settings.local.json"),
            r#"{
              "sandbox": {
                "enabled": true,
                "namespaceRestrictions": false,
                "networkIsolation": true,
                "filesystemMode": "allow-list",
                "allowedMounts": ["logs", "tmp/cache"]
              }
            }"#,
        )
        .expect("write local settings");

        let loaded = ConfigLoader::new(&cwd, &home)
            .load()
            .expect("config should load");

        assert_eq!(loaded.sandbox().enabled, Some(true));
        assert_eq!(loaded.sandbox().namespace_restrictions, Some(false));
        assert_eq!(loaded.sandbox().network_isolation, Some(true));
        assert_eq!(
            loaded.sandbox().filesystem_mode,
            Some(FilesystemIsolationMode::AllowList)
        );
        assert_eq!(loaded.sandbox().allowed_mounts, vec!["logs", "tmp/cache"]);

        fs::remove_dir_all(root).expect("cleanup temp dir");
    }

    #[test]
    fn browser_control_defaults_to_disabled_with_workspace_bounded_artifacts_dir() {
        let root = temp_dir();
        let cwd = root.join("project");
        let home = root.join("home").join(".openyak");
        fs::create_dir_all(&cwd).expect("project dir");
        fs::create_dir_all(&home).expect("home config dir");

        let loaded = ConfigLoader::new(&cwd, &home)
            .load()
            .expect("config should load");

        assert!(!loaded.browser_control().enabled());
        assert_eq!(
            loaded.browser_control().artifacts_dir(),
            &cwd.join(".openyak").join("artifacts").join("browser")
        );

        fs::remove_dir_all(root).expect("cleanup temp dir");
    }

    #[test]
    fn parses_browser_control_config() {
        let root = temp_dir();
        let cwd = root.join("project");
        let home = root.join("home").join(".openyak");
        fs::create_dir_all(cwd.join(".openyak")).expect("project config dir");
        fs::create_dir_all(&home).expect("home config dir");

        fs::write(
            cwd.join(".openyak").join("settings.local.json"),
            r#"{
              "browserControl": {
                "enabled": true,
                "artifactsDir": "tmp/browser-output"
              }
            }"#,
        )
        .expect("write local settings");

        let loaded = ConfigLoader::new(&cwd, &home)
            .load()
            .expect("config should load");

        assert!(loaded.browser_control().enabled());
        assert_eq!(
            loaded.browser_control().artifacts_dir(),
            &cwd.join("tmp").join("browser-output")
        );

        fs::remove_dir_all(root).expect("cleanup temp dir");
    }

    #[test]
    fn rejects_browser_control_artifacts_dir_outside_workspace() {
        let root = temp_dir();
        let cwd = root.join("project");
        let home = root.join("home").join(".openyak");
        fs::create_dir_all(cwd.join(".openyak")).expect("project config dir");
        fs::create_dir_all(&home).expect("home config dir");

        fs::write(
            cwd.join(".openyak").join("settings.local.json"),
            r#"{
              "browserControl": {
                "enabled": true,
                "artifactsDir": "../escape"
              }
            }"#,
        )
        .expect("write local settings");

        let error = ConfigLoader::new(&cwd, &home)
            .load()
            .expect_err("config should fail");
        assert!(error
            .to_string()
            .contains("resolved path must stay inside the workspace root"));

        fs::remove_dir_all(root).expect("cleanup temp dir");
    }

    #[test]
    fn rejects_browser_control_artifacts_dir_through_symlinked_parent() {
        let root = temp_dir();
        let cwd = root.join("project");
        let home = root.join("home").join(".openyak");
        let outside = root.join("outside-artifacts");
        fs::create_dir_all(cwd.join(".openyak")).expect("project config dir");
        fs::create_dir_all(&home).expect("home config dir");
        fs::create_dir_all(&outside).expect("outside dir");

        let symlink_path = cwd.join("linked-artifacts");
        match create_test_dir_symlink(&symlink_path, &outside) {
            Ok(()) => {}
            Err(_error) if cfg!(windows) => {
                let _ = fs::remove_dir_all(root);
                return;
            }
            Err(error) => panic!("create test symlink: {error}"),
        }

        fs::write(
            cwd.join(".openyak").join("settings.local.json"),
            r#"{
              "browserControl": {
                "enabled": true,
                "artifactsDir": "linked-artifacts/browser-output"
              }
            }"#,
        )
        .expect("write local settings");

        let error = ConfigLoader::new(&cwd, &home)
            .load()
            .expect_err("config should fail");
        assert!(error
            .to_string()
            .contains("resolved path must stay inside the workspace root"));

        fs::remove_dir_all(root).expect("cleanup temp dir");
    }

    #[test]
    fn rejects_malformed_browser_control_config() {
        let root = temp_dir();
        let cwd = root.join("project");
        let home = root.join("home").join(".openyak");
        fs::create_dir_all(cwd.join(".openyak")).expect("project config dir");
        fs::create_dir_all(&home).expect("home config dir");

        fs::write(
            cwd.join(".openyak").join("settings.local.json"),
            r#"{
              "browserControl": true
            }"#,
        )
        .expect("write local settings");

        let error = ConfigLoader::new(&cwd, &home)
            .load()
            .expect_err("config should fail");
        assert!(error
            .to_string()
            .contains("merged settings.browserControl: expected JSON object"));

        fs::remove_dir_all(root).expect("cleanup temp dir");
    }

    #[test]
    fn parses_tool_profiles_and_bash_policy() {
        let root = temp_dir();
        let cwd = root.join("project");
        let home = root.join("home").join(".openyak");
        fs::create_dir_all(cwd.join(".openyak")).expect("project config dir");
        fs::create_dir_all(&home).expect("home config dir");

        fs::write(
            cwd.join(".openyak").join("settings.local.json"),
            r#"{
              "toolProfiles": {
                "audit": {
                  "description": "Local audit profile",
                  "permissionMode": "workspace-write",
                  "allowedTools": ["read_file", "glob_search", "bash"],
                  "bashPolicy": {
                    "sandbox": {
                      "enabled": true,
                      "namespaceRestrictions": true,
                      "networkIsolation": true,
                      "filesystemMode": "workspace-only",
                      "allowedMounts": ["logs", "tmp/cache"]
                    },
                    "allowDangerouslyDisableSandbox": false
                  }
                }
              }
            }"#,
        )
        .expect("write local settings");

        let loaded = ConfigLoader::new(&cwd, &home)
            .load()
            .expect("config should load");
        let profile = loaded
            .tool_profiles()
            .get("audit")
            .expect("tool profile should exist");

        assert_eq!(profile.description.as_deref(), Some("Local audit profile"));
        assert_eq!(
            profile.permission_mode,
            ResolvedPermissionMode::WorkspaceWrite
        );
        assert_eq!(
            profile.allowed_tools,
            vec!["read_file", "glob_search", "bash"]
        );
        let bash_policy = profile
            .bash_policy
            .as_ref()
            .expect("bash policy should exist");
        assert_eq!(bash_policy.sandbox.enabled, Some(true));
        assert_eq!(bash_policy.sandbox.namespace_restrictions, Some(true));
        assert_eq!(bash_policy.sandbox.network_isolation, Some(true));
        assert_eq!(
            bash_policy.sandbox.filesystem_mode,
            Some(FilesystemIsolationMode::WorkspaceOnly)
        );
        assert_eq!(
            bash_policy.sandbox.allowed_mounts,
            vec!["logs", "tmp/cache"]
        );
        assert!(!bash_policy.allow_dangerously_disable_sandbox);

        fs::remove_dir_all(root).expect("cleanup temp dir");
    }

    #[test]
    fn parses_typed_mcp_and_oauth_config() {
        let root = temp_dir();
        let cwd = root.join("project");
        let home = root.join("home").join(".openyak");
        fs::create_dir_all(cwd.join(".openyak")).expect("project config dir");
        fs::create_dir_all(&home).expect("home config dir");

        fs::write(
            home.join("settings.json"),
            r#"{
              "mcpServers": {
                "stdio-server": {
                  "command": "uvx",
                  "args": ["mcp-server"],
                  "env": {"TOKEN": "secret"}
                },
                "remote-server": {
                  "type": "http",
                  "url": "https://example.test/mcp",
                  "headers": {"Authorization": "Bearer token"},
                  "headersHelper": "helper.sh",
                  "oauth": {
                    "clientId": "mcp-client",
                    "callbackPort": 7777,
                    "authServerMetadataUrl": "https://issuer.test/.well-known/oauth-authorization-server",
                    "xaa": true
                  }
                }
              },
              "oauth": {
                "clientId": "runtime-client",
                "authorizeUrl": "https://console.test/oauth/authorize",
                "tokenUrl": "https://console.test/oauth/token",
                "callbackPort": 54545,
                "manualRedirectUrl": "https://console.test/oauth/callback",
                "scopes": ["org:read", "user:write"]
              }
            }"#,
        )
        .expect("write user settings");
        fs::write(
            cwd.join(".openyak").join("settings.local.json"),
            r#"{
              "mcpServers": {
                "remote-server": {
                  "type": "ws",
                  "url": "wss://override.test/mcp",
                  "headers": {"X-Env": "local"}
                }
              }
            }"#,
        )
        .expect("write local settings");

        let loaded = ConfigLoader::new(&cwd, &home)
            .load()
            .expect("config should load");

        let stdio_server = loaded
            .mcp()
            .get("stdio-server")
            .expect("stdio server should exist");
        assert_eq!(stdio_server.scope, ConfigSource::User);
        assert_eq!(stdio_server.transport(), McpTransport::Stdio);

        let remote_server = loaded
            .mcp()
            .get("remote-server")
            .expect("remote server should exist");
        assert_eq!(remote_server.scope, ConfigSource::Local);
        assert_eq!(remote_server.transport(), McpTransport::Ws);
        match &remote_server.config {
            McpServerConfig::Ws(config) => {
                assert_eq!(config.url, "wss://override.test/mcp");
                assert_eq!(
                    config.headers.get("X-Env").map(String::as_str),
                    Some("local")
                );
            }
            other => panic!("expected ws config, got {other:?}"),
        }

        let oauth = loaded.oauth().expect("oauth config should exist");
        assert_eq!(oauth.client_id, "runtime-client");
        assert_eq!(oauth.callback_port, Some(54_545));
        assert_eq!(oauth.scopes, vec!["org:read", "user:write"]);

        fs::remove_dir_all(root).expect("cleanup temp dir");
    }

    #[test]
    fn partial_oauth_override_does_not_fail_config_loading() {
        let root = temp_dir();
        let cwd = root.join("project");
        let home = root.join("home").join(".openyak");
        fs::create_dir_all(cwd.join(".openyak")).expect("project config dir");
        fs::create_dir_all(&home).expect("home config dir");

        fs::write(
            home.join("settings.json"),
            r#"{
              "oauth": {
                "callbackPort": 4557
              }
            }"#,
        )
        .expect("write user settings");

        let loaded = ConfigLoader::new(&cwd, &home)
            .load()
            .expect("config should load");

        assert!(loaded.oauth().is_none());
        let oauth_override = loaded
            .oauth_override()
            .expect("oauth override should exist");
        assert_eq!(oauth_override.callback_port, Some(4557));
        assert!(oauth_override.client_id.is_none());

        fs::remove_dir_all(root).expect("cleanup temp dir");
    }

    #[test]
    fn parses_plugin_config_from_enabled_plugins() {
        let root = temp_dir();
        let cwd = root.join("project");
        let home = root.join("home").join(".openyak");
        fs::create_dir_all(cwd.join(".openyak")).expect("project config dir");
        fs::create_dir_all(&home).expect("home config dir");

        fs::write(
            home.join("settings.json"),
            r#"{
              "enabledPlugins": {
                "tool-guard@builtin": true,
                "sample-plugin@external": false
              }
            }"#,
        )
        .expect("write user settings");

        let loaded = ConfigLoader::new(&cwd, &home)
            .load()
            .expect("config should load");

        assert_eq!(
            loaded.plugins().enabled_plugins().get("tool-guard@builtin"),
            Some(&true)
        );
        assert_eq!(
            loaded
                .plugins()
                .enabled_plugins()
                .get("sample-plugin@external"),
            Some(&false)
        );

        fs::remove_dir_all(root).expect("cleanup temp dir");
    }

    #[test]
    fn parses_plugin_config() {
        let root = temp_dir();
        let cwd = root.join("project");
        let home = root.join("home").join(".openyak");
        fs::create_dir_all(cwd.join(".openyak")).expect("project config dir");
        fs::create_dir_all(&home).expect("home config dir");

        fs::write(
            home.join("settings.json"),
            r#"{
              "enabledPlugins": {
                "core-helpers@builtin": true
              },
              "plugins": {
                "externalDirectories": ["./external-plugins"],
                "installRoot": "plugin-cache/installed",
                "registryPath": "plugin-cache/installed.json",
                "bundledRoot": "./bundled-plugins"
              }
            }"#,
        )
        .expect("write plugin settings");

        let loaded = ConfigLoader::new(&cwd, &home)
            .load()
            .expect("config should load");

        assert_eq!(
            loaded
                .plugins()
                .enabled_plugins()
                .get("core-helpers@builtin"),
            Some(&true)
        );
        assert_eq!(
            loaded.plugins().external_directories(),
            &["./external-plugins".to_string()]
        );
        assert_eq!(
            loaded.plugins().install_root(),
            Some("plugin-cache/installed")
        );
        assert_eq!(
            loaded.plugins().registry_path(),
            Some("plugin-cache/installed.json")
        );
        assert_eq!(loaded.plugins().bundled_root(), Some("./bundled-plugins"));

        fs::remove_dir_all(root).expect("cleanup temp dir");
    }

    #[test]
    fn rejects_invalid_mcp_server_shapes() {
        let root = temp_dir();
        let cwd = root.join("project");
        let home = root.join("home").join(".openyak");
        fs::create_dir_all(&home).expect("home config dir");
        fs::create_dir_all(&cwd).expect("project dir");
        fs::write(
            home.join("settings.json"),
            r#"{"mcpServers":{"broken":{"type":"http","url":123}}}"#,
        )
        .expect("write broken settings");

        let error = ConfigLoader::new(&cwd, &home)
            .load()
            .expect_err("config should fail");
        assert!(error
            .to_string()
            .contains("mcpServers.broken: missing string field url"));

        fs::remove_dir_all(root).expect("cleanup temp dir");
    }

    #[test]
    fn write_user_model_creates_settings_file_and_preserves_other_keys() {
        let root = temp_dir();
        let cwd = root.join("project");
        let home = root.join("home").join(".openyak");
        fs::create_dir_all(&cwd).expect("project dir");
        fs::create_dir_all(&home).expect("home config dir");
        fs::write(
            home.join("settings.json"),
            r#"{
  "permissions": {
    "defaultMode": "plan"
  },
  "plugins": {
    "installRoot": "plugins/cache"
  }
}"#,
        )
        .expect("write user settings");

        let loader = ConfigLoader::new(&cwd, &home);
        let path = loader
            .write_user_model("claude-sonnet-4-6")
            .expect("write should succeed");

        assert_eq!(path, home.join("settings.json"));
        let reloaded = loader.load().expect("config should reload");
        assert_eq!(reloaded.model(), Some("claude-sonnet-4-6"));
        assert_eq!(
            reloaded.permission_mode(),
            Some(ResolvedPermissionMode::ReadOnly)
        );
        assert_eq!(reloaded.plugins().install_root(), Some("plugins/cache"));

        fs::remove_dir_all(root).expect("cleanup temp dir");
    }

    #[test]
    fn write_user_model_rejects_non_object_settings_files() {
        let root = temp_dir();
        let cwd = root.join("project");
        let home = root.join("home").join(".openyak");
        fs::create_dir_all(&cwd).expect("project dir");
        fs::create_dir_all(&home).expect("home config dir");
        fs::write(home.join("settings.json"), "[]").expect("write invalid settings");

        let error = ConfigLoader::new(&cwd, &home)
            .write_user_model("claude-opus-4-6")
            .expect_err("write should fail");

        assert!(error
            .to_string()
            .contains("top-level settings value must be a JSON object"));

        fs::remove_dir_all(root).expect("cleanup temp dir");
    }
}

mod bash;
mod bash_validation;
mod bootstrap;
mod compact;
mod config;
mod conversation;
mod date;
mod file_ops;
mod hooks;
mod json;
mod lifecycle;
mod lsp_client;
mod mcp;
mod mcp_client;
mod mcp_stdio;
mod mcp_tool_bridge;
mod oauth;
mod paths;
mod permission_enforcer;
mod permissions;
mod process;
mod prompt;
mod remote;
pub mod sandbox;
mod session;
mod skill_registry;
mod skills;
mod task_registry;
mod team_cron_registry;
mod tool_profile;
mod usage;

pub use bash::{execute_bash, execute_bash_with_config, BashCommandInput, BashCommandOutput};
pub use bootstrap::{BootstrapPhase, BootstrapPlan};
pub use compact::{
    compact_session, estimate_session_tokens, format_compact_summary,
    get_compact_continuation_message, should_compact, CompactionConfig, CompactionResult,
    CompactionSummaryMode,
};
pub use config::{
    ConfigEntry, ConfigError, ConfigLoader, ConfigSource, McpConfigCollection,
    McpManagedProxyServerConfig, McpOAuthConfig, McpRemoteServerConfig, McpSdkServerConfig,
    McpServerConfig, McpStdioServerConfig, McpTransport, McpWebSocketServerConfig, OAuthConfig,
    OAuthConfigOverride, ResolvedPermissionMode, RuntimeBrowserControlConfig, RuntimeConfig,
    RuntimeFeatureConfig, RuntimeHookConfig, RuntimePluginConfig, RuntimeSkillConfig,
    ScopedMcpServerConfig, OPENYAK_SETTINGS_SCHEMA_NAME,
};
pub use conversation::{
    ApiClient, ApiRequest, AssistantEvent, ConversationRuntime, RuntimeError, StaticToolExecutor,
    ToolError, ToolExecutor, TurnSummary, UserInputOutcome, UserInputPrompter, UserInputRequest,
    UserInputResponse,
};
pub use date::current_local_date_string;
pub use file_ops::{
    edit_file, glob_search, grep_search, read_file, write_file, EditFileOutput, GlobSearchOutput,
    GrepSearchInput, GrepSearchOutput, ReadFileOutput, StructuredPatchHunk, TextFilePayload,
    WriteFileOutput,
};
pub use hooks::{HookEvent, HookRunResult, HookRunner};
pub use lifecycle::{
    LifecycleContractSnapshot, LifecycleStateSnapshot, RecoveryGuidanceSnapshot,
    ThreadContractSnapshot, DAEMON_LOCAL_TRUTH_LAYER, LOCAL_LOOPBACK_OPERATOR_PLANE,
    LOCAL_RUNTIME_FOUNDATION_OPERATOR_PLANE, PROCESS_LOCAL_TRUTH_LAYER,
    PROCESS_MEMORY_PERSISTENCE_LAYER, THREAD_ATTACH_API, WORKSPACE_SQLITE_PERSISTENCE_LAYER,
};
pub use lsp::{
    FileDiagnostics, LspContextEnrichment, LspError, LspManager, LspServerConfig, SymbolLocation,
    WorkspaceDiagnostics,
};
pub use lsp_client::{
    LspAction, LspCompletionItem, LspDiagnostic, LspHoverResult, LspLocation, LspRegistry,
    LspServerState, LspServerStatus, LspSymbol,
};
pub use mcp::{
    mcp_server_signature, mcp_tool_name, mcp_tool_prefix, normalize_name_for_mcp,
    scoped_mcp_config_hash, unwrap_ccr_proxy_url,
};
pub use mcp_client::{
    McpClientAuth, McpClientBootstrap, McpClientTransport, McpManagedProxyTransport,
    McpRemoteTransport, McpSdkTransport, McpStdioTransport,
};
pub use mcp_stdio::{
    spawn_mcp_stdio_process, JsonRpcError, JsonRpcId, JsonRpcRequest, JsonRpcResponse,
    ManagedMcpTool, McpInitializeClientInfo, McpInitializeParams, McpInitializeResult,
    McpInitializeServerInfo, McpListResourcesParams, McpListResourcesResult, McpListToolsParams,
    McpListToolsResult, McpReadResourceParams, McpReadResourceResult, McpResource,
    McpResourceContents, McpServerManager, McpServerManagerError, McpStdioProcess, McpTool,
    McpToolCallContent, McpToolCallParams, McpToolCallResult, UnsupportedMcpServer,
};
pub use mcp_tool_bridge::{
    McpConnectionStatus, McpResourceInfo, McpServerState, McpToolInfo, McpToolRegistry,
};
pub use oauth::{
    clear_oauth_credentials, code_challenge_s256, credentials_path, generate_pkce_pair,
    generate_state, load_oauth_credentials, loopback_redirect_uri, parse_oauth_callback_input,
    parse_oauth_callback_query, parse_oauth_callback_request_target, save_oauth_credentials,
    OAuthAuthorizationRequest, OAuthCallbackParams, OAuthRefreshRequest, OAuthTokenExchangeRequest,
    OAuthTokenSet, PkceChallengeMethod, PkceCodePair,
};
pub use paths::{
    default_codex_home, default_openyak_home, home_locations, platform_user_home_dir, HomeLocations,
};
pub use permission_enforcer::{EnforcementResult, PermissionEnforcer};
pub use permissions::{
    PermissionMode, PermissionOutcome, PermissionPolicy, PermissionPromptDecision,
    PermissionPrompter, PermissionRequest,
};
pub use process::{command_exists, resolve_command_path};
pub use prompt::{
    load_system_prompt, prepend_bullets, ContextFile, ProjectContext, PromptBuildError,
    SystemPromptBuilder, FRONTIER_MODEL_NAME, SYSTEM_PROMPT_DYNAMIC_BOUNDARY,
};
pub use remote::{
    inherited_upstream_proxy_env, no_proxy_list, read_token, upstream_proxy_ws_url,
    RemoteSessionContext, UpstreamProxyBootstrap, UpstreamProxyState, DEFAULT_REMOTE_BASE_URL,
    DEFAULT_SESSION_TOKEN_PATH, DEFAULT_SYSTEM_CA_BUNDLE, NO_PROXY_HOSTS, UPSTREAM_PROXY_ENV_KEYS,
};
pub use session::{
    ContentBlock, ConversationMessage, MessageRole, PendingUserInputRequest, Session,
    SessionAccountingStatus, SessionError, SessionTelemetry,
};
pub use skill_registry::{
    default_managed_skills_root, default_packaged_skill_registry_path, find_installed_skill_record,
    install_managed_skill, load_installed_skill_registry, load_skill_registry,
    resolve_skill_registry_path, save_installed_skill_registry, uninstall_managed_skill,
    update_managed_skill, AvailableSkillCatalog, AvailableSkillEntry, InstalledSkillRecord,
    InstalledSkillRegistry, SkillCatalogInfo, SkillInstallOutcome, SkillInstallRequest,
    SkillInstallStatus, SkillRegistry, SkillRegistryEntry, SkillRegistryError,
    SkillRegistryManager, SkillUninstallOutcome, SkillUpdateOutcome, SkillUpdateRequest,
    SkillUpdateStatus,
};
pub use skills::{
    discover_skill_directories, parse_skill_frontmatter, read_skill_package_metadata,
    resolve_skill_path_from_roots, SkillDirectory,
};
pub use task_registry::{Task, TaskMessage, TaskRegistry, TaskStatus};
pub use team_cron_registry::{CronEntry, CronRegistry, Team, TeamRegistry, TeamStatus};
pub use tool_profile::{
    resolved_permission_mode_to_permission_mode, ToolProfileBashPolicy, ToolProfileConfig,
};
pub use usage::{
    format_usd, pricing_for_model, ModelPricing, TokenUsage, UsageCostEstimate, UsageTracker,
};

#[cfg(test)]
pub(crate) fn test_env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

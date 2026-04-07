mod init;
mod input;
mod onboard;
mod render;

use std::collections::BTreeSet;
use std::env;
use std::fmt::Write as _;
use std::fs;
use std::io::{self, IsTerminal, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use api::{
    resolve_startup_auth_source, AuthSource, ContentBlockDelta, InputContentBlock, InputMessage,
    MessageRequest, MessageResponse, OpenyakApiClient, OutputContentBlock, ProviderClient,
    StreamEvent as ApiStreamEvent, ToolChoice, ToolDefinition, ToolResultContentBlock,
};

use commands::{
    detect_default_branch, handle_agents_slash_command, handle_branch_slash_command,
    handle_commit_push_pr_slash_command, handle_commit_slash_command, handle_plugins_slash_command,
    handle_skills_slash_command, handle_worktree_slash_command, render_slash_command_help,
    resume_supported_slash_commands, slash_command_specs, suggest_slash_commands,
    CommitPushPrRequest, SlashCommand,
};
use compat_harness::{extract_manifest, UpstreamPaths};
use init::initialize_repo;
use plugins::{PluginHooks, PluginManager, PluginManagerConfig};
use render::{MarkdownStreamState, Spinner, TerminalRenderer};
use runtime::{
    clear_oauth_credentials, command_exists, credentials_path, current_local_date_string,
    format_usd, generate_pkce_pair, generate_state, load_oauth_credentials, load_system_prompt,
    parse_oauth_callback_request_target, pricing_for_model, resolve_command_path,
    save_oauth_credentials, ApiClient, ApiRequest, AssistantEvent, CompactionConfig,
    CompactionSummaryMode, ConfigLoader, ConfigSource, ContentBlock, ConversationMessage,
    ConversationRuntime, MessageRole, OAuthAuthorizationRequest, OAuthConfig,
    OAuthTokenExchangeRequest, PendingUserInputRequest, PermissionMode, PermissionPolicy,
    ProjectContext, RuntimeError, Session, SessionAccountingStatus, TokenUsage, ToolError,
    ToolExecutor, UsageTracker, UserInputOutcome, UserInputPrompter, UserInputRequest,
    UserInputResponse,
};
use serde_json::json;
use tools::{mvp_tool_specs, GlobalToolRegistry};

const DEFAULT_MODEL: &str = "claude-opus-4-6";
const DEFAULT_SERVER_BIND: &str = "127.0.0.1:3000";
const DEFAULT_RELEASE_OUTPUT_DIR: &str = "dist";
const REQUEST_USER_INPUT_TOOL_NAME: &str = "openyak_request_user_input";
const REQUEST_USER_INPUT_PROMPT: &str = "answer> ";
fn max_tokens_for_model(model: &str) -> u32 {
    if model.contains("opus") {
        32_000
    } else {
        64_000
    }
}
const DEFAULT_OAUTH_CALLBACK_PORT: u16 = 4545;
const VERSION: &str = env!("CARGO_PKG_VERSION");
const BUILD_TARGET: Option<&str> = option_env!("TARGET");
const GIT_SHA: Option<&str> = option_env!("GIT_SHA");
const BUILD_DATE: Option<&str> = option_env!("BUILD_DATE");
const INTERNAL_PROGRESS_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(3);
const THREAD_SERVER_INFO_FILENAME: &str = "thread-server.json";

type AllowedToolSet = BTreeSet<String>;

#[cfg(test)]
pub(crate) fn test_env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
pub(crate) fn cleanup_temp_dir(path: impl AsRef<Path>) {
    let path = path.as_ref();
    for attempt in 0..10 {
        match fs::remove_dir_all(path) {
            Ok(()) => return,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return,
            Err(error) if cfg!(windows) && attempt < 9 => {
                let _ = error;
                thread::sleep(Duration::from_millis(50));
            }
            Err(error) => panic!("cleanup temp dir: {error}"),
        }
    }
    if cfg!(windows) {
        return;
    }
    panic!("cleanup temp dir: exhausted retries for {}", path.display());
}

fn main() {
    if let Err(error) = run() {
        eprintln!("{}", render_cli_error(&error.to_string()));
        std::process::exit(1);
    }
}

fn render_cli_error(problem: &str) -> String {
    let mut lines = vec!["Error".to_string()];
    for (index, line) in problem.lines().enumerate() {
        let label = if index == 0 {
            "  Problem          "
        } else {
            "                   "
        };
        lines.push(format!("{label}{line}"));
    }
    lines.push("  Help             openyak --help".to_string());
    lines.join("\n")
}

fn run_server(bind: &str) -> Result<(), Box<dyn std::error::Error>> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async move {
        let listener = tokio::net::TcpListener::bind(bind).await?;
        let local_addr = listener.local_addr()?;
        let state = server::AppState::load_for_current_dir()?;
        let _info_guard = write_thread_server_info(local_addr)?;
        let mut stdout = io::stdout();
        writeln!(
            stdout,
            "Local thread server listening on http://{local_addr}"
        )?;
        stdout.flush()?;
        server::serve(listener, state).await?;
        Ok(())
    })
}

struct ThreadServerInfoGuard {
    path: PathBuf,
    pid: u32,
}

impl Drop for ThreadServerInfoGuard {
    fn drop(&mut self) {
        if thread_server_info_matches_pid(&self.path, self.pid) {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn write_thread_server_info(
    local_addr: std::net::SocketAddr,
) -> Result<ThreadServerInfoGuard, Box<dyn std::error::Error>> {
    let cwd = env::current_dir()?;
    let openyak_dir = cwd.join(".openyak");
    fs::create_dir_all(&openyak_dir)?;
    let path = openyak_dir.join(THREAD_SERVER_INFO_FILENAME);
    let pid = std::process::id();
    fs::write(
        &path,
        serde_json::to_string_pretty(&json!({
            "baseUrl": format!("http://{local_addr}"),
            "pid": pid,
        }))?,
    )?;
    Ok(ThreadServerInfoGuard { path, pid })
}

fn thread_server_info_matches_pid(path: &Path, pid: u32) -> bool {
    let Ok(contents) = fs::read_to_string(path) else {
        return false;
    };
    let Ok(info) = serde_json::from_str::<serde_json::Value>(&contents) else {
        return false;
    };
    info.get("pid").and_then(serde_json::Value::as_u64) == Some(u64::from(pid))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ReleasePackageOutput {
    artifact_dir: PathBuf,
    packaged_binary: PathBuf,
}

fn run_package_release(
    binary: Option<&Path>,
    output_dir: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let binary_path = match binary {
        Some(path) => path.to_path_buf(),
        None => env::current_exe()?,
    };
    let output = stage_release_artifact(&binary_path, output_dir)?;
    println!(
        "Release artifact staged at {}",
        output.artifact_dir.display()
    );
    println!("Packaged binary: {}", output.packaged_binary.display());
    Ok(())
}

fn stage_release_artifact(
    binary_path: &Path,
    output_dir: &Path,
) -> io::Result<ReleasePackageOutput> {
    if !binary_path.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("release binary not found: {}", binary_path.display()),
        ));
    }

    let binary_name = binary_path.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("binary path has no file name: {}", binary_path.display()),
        )
    })?;
    let target_label = BUILD_TARGET.map_or_else(
        || format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS),
        str::to_string,
    );
    let artifact_dir = output_dir.join(format!("openyak-{VERSION}-{target_label}"));
    if artifact_dir.exists() {
        let canonical_binary = binary_path.canonicalize()?;
        let canonical_artifact_dir = artifact_dir.canonicalize()?;
        if canonical_binary.starts_with(&canonical_artifact_dir) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "cannot package binary `{}` into `{}` because the destination artifact directory `{}` already contains the source binary; choose a different --output-dir or a --binary outside that directory",
                    binary_path.display(),
                    output_dir.display(),
                    artifact_dir.display()
                ),
            ));
        }
        fs::remove_dir_all(&artifact_dir)?;
    }
    fs::create_dir_all(&artifact_dir)?;

    let packaged_binary = artifact_dir.join(binary_name);
    let packaged_binary_name = packaged_binary
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("openyak");
    fs::copy(binary_path, &packaged_binary)?;
    fs::write(
        artifact_dir.join("INSTALL.txt"),
        format!(
            "openyak {VERSION}\n\
             Target: {target_label}\n\
             Binary: {packaged_binary_name}\n\n\
             Usage:\n\
             1. Run the packaged binary directly from this directory.\n\
             2. Optional: place this binary on PATH.\n\
             3. Verify with `{packaged_binary_name} --help`.\n",
        ),
    )?;
    fs::write(
        artifact_dir.join("release-metadata.json"),
        serde_json::to_string_pretty(&json!({
            "name": "openyak",
            "version": VERSION,
            "target": target_label,
            "binary": packaged_binary_name,
        }))?,
    )?;

    Ok(ReleasePackageOutput {
        artifact_dir,
        packaged_binary,
    })
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = env::args().skip(1).collect();
    match parse_args(&args)? {
        CliAction::DumpManifests => dump_manifests(),
        CliAction::BootstrapPlan => print_bootstrap_plan(),
        CliAction::Agents { args } => LiveCli::print_agents(args.as_deref())?,
        CliAction::Skills { args } => LiveCli::print_skills(args.as_deref())?,
        CliAction::PrintSystemPrompt { cwd, date } => print_system_prompt(&cwd, &date),
        CliAction::Version => print_version(),
        CliAction::ResumeSession {
            session_path,
            commands,
        } => resume_session(&session_path, &commands),
        CliAction::Prompt {
            prompt,
            model,
            output_format,
            allowed_tools,
            permission_mode,
        } => {
            let cwd = env::current_dir()?;
            let model = resolve_effective_model(model.as_deref(), &cwd)?;
            LiveCli::new(model, true, allowed_tools, permission_mode)?
                .run_turn_with_output(&prompt, output_format)?;
        }
        CliAction::Login => run_login()?,
        CliAction::Logout => run_logout()?,
        CliAction::Init => run_init()?,
        CliAction::Onboard {
            model,
            output_format,
        } => onboard::run_onboard(model.as_deref(), output_format)?,
        CliAction::Doctor => run_doctor()?,
        CliAction::PackageRelease { binary, output_dir } => {
            run_package_release(binary.as_deref(), &output_dir)?;
        }
        CliAction::Server { bind } => run_server(&bind)?,
        CliAction::Repl {
            model,
            allowed_tools,
            permission_mode,
        } => {
            let cwd = env::current_dir()?;
            let model = resolve_effective_model(model.as_deref(), &cwd)?;
            run_repl(model, allowed_tools, permission_mode)?;
        }
        CliAction::Help(topic) => print_help(topic),
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CliAction {
    DumpManifests,
    BootstrapPlan,
    Agents {
        args: Option<String>,
    },
    Skills {
        args: Option<String>,
    },
    PrintSystemPrompt {
        cwd: PathBuf,
        date: String,
    },
    Version,
    ResumeSession {
        session_path: PathBuf,
        commands: Vec<String>,
    },
    Prompt {
        prompt: String,
        model: Option<String>,
        output_format: CliOutputFormat,
        allowed_tools: Option<AllowedToolSet>,
        permission_mode: PermissionMode,
    },
    Login,
    Logout,
    Init,
    Onboard {
        model: Option<String>,
        output_format: CliOutputFormat,
    },
    Doctor,
    PackageRelease {
        binary: Option<PathBuf>,
        output_dir: PathBuf,
    },
    Server {
        bind: String,
    },
    Repl {
        model: Option<String>,
        allowed_tools: Option<AllowedToolSet>,
        permission_mode: PermissionMode,
    },
    // prompt-mode formatting is only supported for non-interactive runs
    Help(HelpTopic),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HelpTopic {
    Root,
    DumpManifests,
    BootstrapPlan,
    SystemPrompt,
    Login,
    Logout,
    Init,
    Onboard,
    Doctor,
    PackageRelease,
    Server,
    Prompt,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CliOutputFormat {
    Text,
    Json,
}

impl CliOutputFormat {
    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "text" => Ok(Self::Text),
            "json" => Ok(Self::Json),
            other => Err(format!(
                "unsupported value for --output-format: {other} (expected text or json)"
            )),
        }
    }
}

#[allow(clippy::too_many_lines)]
fn parse_args(args: &[String]) -> Result<CliAction, String> {
    let mut model = None;
    let mut output_format = CliOutputFormat::Text;
    let mut permission_mode = default_permission_mode();
    let mut wants_version = false;
    let mut allowed_tool_values = Vec::new();
    let mut rest = Vec::new();
    let mut index = 0;

    while index < args.len() {
        if !rest.is_empty() {
            rest.push(args[index].clone());
            index += 1;
            continue;
        }
        match args[index].as_str() {
            "--version" | "-V" => {
                wants_version = true;
                index += 1;
            }
            "--model" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| "missing value for --model".to_string())?;
                model = Some(resolve_model_alias(value).to_string());
                index += 2;
            }
            flag if flag.starts_with("--model=") => {
                model = Some(resolve_model_alias(&flag[8..]).to_string());
                index += 1;
            }
            "--output-format" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| "missing value for --output-format".to_string())?;
                output_format = CliOutputFormat::parse(value)?;
                index += 2;
            }
            "--permission-mode" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| "missing value for --permission-mode".to_string())?;
                permission_mode = parse_permission_mode_arg(value)?;
                index += 2;
            }
            flag if flag.starts_with("--output-format=") => {
                output_format = CliOutputFormat::parse(&flag[16..])?;
                index += 1;
            }
            flag if flag.starts_with("--permission-mode=") => {
                permission_mode = parse_permission_mode_arg(&flag[18..])?;
                index += 1;
            }
            "--dangerously-skip-permissions" => {
                permission_mode = PermissionMode::DangerFullAccess;
                index += 1;
            }
            "-p" => {
                if args
                    .get(index + 1)
                    .is_some_and(|value| matches!(value.as_str(), "--help" | "-h"))
                    && index + 2 == args.len()
                {
                    return Ok(CliAction::Help(HelpTopic::Prompt));
                }
                // openyak compat: -p "prompt" = one-shot prompt
                let prompt = args[index + 1..].join(" ");
                if prompt.trim().is_empty() {
                    return Err("-p requires a prompt string".to_string());
                }
                return Ok(CliAction::Prompt {
                    prompt,
                    model,
                    output_format,
                    allowed_tools: normalize_allowed_tools(&allowed_tool_values)?,
                    permission_mode,
                });
            }
            "--print" => {
                // openyak compat: --print makes output non-interactive
                output_format = CliOutputFormat::Text;
                index += 1;
            }
            "--allowedTools" | "--allowed-tools" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| "missing value for --allowedTools".to_string())?;
                if looks_like_cli_command_token(value) {
                    return Err("missing value for --allowedTools".to_string());
                }
                allowed_tool_values.push(value.clone());
                index += 2;
            }
            flag if flag.starts_with("--allowedTools=") => {
                allowed_tool_values.push(flag[15..].to_string());
                index += 1;
            }
            flag if flag.starts_with("--allowed-tools=") => {
                allowed_tool_values.push(flag[16..].to_string());
                index += 1;
            }
            other => {
                rest.push(other.to_string());
                index += 1;
            }
        }
    }

    if wants_version {
        return Ok(CliAction::Version);
    }

    let allowed_tools = normalize_allowed_tools(&allowed_tool_values)?;

    if rest.is_empty() {
        return Ok(CliAction::Repl {
            model,
            allowed_tools,
            permission_mode,
        });
    }
    if matches!(rest.first().map(String::as_str), Some("--help" | "-h")) {
        return Ok(CliAction::Help(HelpTopic::Root));
    }
    if rest.first().map(String::as_str) == Some("--resume") {
        return parse_resume_args(&rest[1..]);
    }

    match rest[0].as_str() {
        "dump-manifests" if is_help_args(&rest[1..]) => {
            Ok(CliAction::Help(HelpTopic::DumpManifests))
        }
        "dump-manifests" => Ok(CliAction::DumpManifests),
        "bootstrap-plan" if is_help_args(&rest[1..]) => {
            Ok(CliAction::Help(HelpTopic::BootstrapPlan))
        }
        "bootstrap-plan" => Ok(CliAction::BootstrapPlan),
        "agents" => Ok(CliAction::Agents {
            args: join_optional_args(&rest[1..]),
        }),
        "skills" => Ok(CliAction::Skills {
            args: join_optional_args(&rest[1..]),
        }),
        "system-prompt" => parse_system_prompt_args(&rest[1..]),
        "login" if is_help_args(&rest[1..]) => Ok(CliAction::Help(HelpTopic::Login)),
        "login" => Ok(CliAction::Login),
        "logout" if is_help_args(&rest[1..]) => Ok(CliAction::Help(HelpTopic::Logout)),
        "logout" => Ok(CliAction::Logout),
        "init" if is_help_args(&rest[1..]) => Ok(CliAction::Help(HelpTopic::Init)),
        "init" => Ok(CliAction::Init),
        "onboard" => parse_onboard_args(&rest[1..], model, output_format),
        "doctor" => parse_doctor_args(&rest[1..]),
        "package-release" => parse_package_release_args(&rest[1..]),
        "server" => parse_server_args(&rest[1..]),
        "prompt" => {
            if rest
                .get(1)
                .is_some_and(|value| matches!(value.as_str(), "--help" | "-h"))
                && rest.len() == 2
            {
                return Ok(CliAction::Help(HelpTopic::Prompt));
            }
            let prompt = rest[1..].join(" ");
            if prompt.trim().is_empty() {
                return Err("prompt subcommand requires a prompt string".to_string());
            }
            Ok(CliAction::Prompt {
                prompt,
                model,
                output_format,
                allowed_tools,
                permission_mode,
            })
        }
        other if other.starts_with('/') => parse_direct_slash_cli_action(&rest),
        _other => Ok(CliAction::Prompt {
            prompt: rest.join(" "),
            model,
            output_format,
            allowed_tools,
            permission_mode,
        }),
    }
}

fn join_optional_args(args: &[String]) -> Option<String> {
    let joined = args.join(" ");
    let trimmed = joined.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn parse_server_args(args: &[String]) -> Result<CliAction, String> {
    if is_help_args(args) {
        return Ok(CliAction::Help(HelpTopic::Server));
    }

    let mut bind = DEFAULT_SERVER_BIND.to_string();
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--bind" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| "missing value for server --bind".to_string())?;
                bind.clone_from(value);
                index += 2;
            }
            flag if flag.starts_with("--bind=") => {
                bind = flag[7..].to_string();
                index += 1;
            }
            other => return Err(format!("unknown server argument: {other}")),
        }
    }

    if bind.trim().is_empty() {
        return Err("server --bind must not be empty".to_string());
    }

    Ok(CliAction::Server { bind })
}

fn parse_doctor_args(args: &[String]) -> Result<CliAction, String> {
    if args.is_empty() || is_help_args(args) {
        return Ok(if is_help_args(args) {
            CliAction::Help(HelpTopic::Doctor)
        } else {
            CliAction::Doctor
        });
    }

    Err(format!("unknown doctor argument: {}", args[0]))
}

fn parse_onboard_args(
    args: &[String],
    model: Option<String>,
    output_format: CliOutputFormat,
) -> Result<CliAction, String> {
    if args.is_empty() || is_help_args(args) {
        return Ok(if is_help_args(args) {
            CliAction::Help(HelpTopic::Onboard)
        } else {
            CliAction::Onboard {
                model,
                output_format,
            }
        });
    }

    Err(format!("unknown onboard argument: {}", args[0]))
}

fn parse_package_release_args(args: &[String]) -> Result<CliAction, String> {
    if is_help_args(args) {
        return Ok(CliAction::Help(HelpTopic::PackageRelease));
    }

    let mut binary = None;
    let mut output_dir = PathBuf::from(DEFAULT_RELEASE_OUTPUT_DIR);
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--binary" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| "missing value for package-release --binary".to_string())?;
                binary = Some(PathBuf::from(value));
                index += 2;
            }
            flag if flag.starts_with("--binary=") => {
                binary = Some(PathBuf::from(&flag[9..]));
                index += 1;
            }
            "--output-dir" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| "missing value for package-release --output-dir".to_string())?;
                output_dir = PathBuf::from(value);
                index += 2;
            }
            flag if flag.starts_with("--output-dir=") => {
                output_dir = PathBuf::from(&flag[13..]);
                index += 1;
            }
            other => return Err(format!("unknown package-release argument: {other}")),
        }
    }

    Ok(CliAction::PackageRelease { binary, output_dir })
}

fn parse_direct_slash_cli_action(rest: &[String]) -> Result<CliAction, String> {
    let raw = rest.join(" ");
    match SlashCommand::parse(&raw) {
        Some(SlashCommand::Help) => Ok(CliAction::Help(HelpTopic::Root)),
        Some(SlashCommand::Agents { args }) => Ok(CliAction::Agents { args }),
        Some(SlashCommand::Skills { args }) => Ok(CliAction::Skills { args }),
        Some(command) => Err(format_direct_slash_command_error(
            match &command {
                SlashCommand::Unknown(name) => format!("/{name}"),
                _ => rest[0].clone(),
            }
            .as_str(),
            matches!(command, SlashCommand::Unknown(_)),
        )),
        None => Err(format!("unknown subcommand: {}", rest[0])),
    }
}

fn format_direct_slash_command_error(command: &str, is_unknown: bool) -> String {
    let trimmed = command.trim().trim_start_matches('/');
    let mut lines = vec![
        "Direct slash command unavailable".to_string(),
        format!("  Command          /{trimmed}"),
    ];
    if is_unknown {
        append_slash_command_suggestions(&mut lines, trimmed);
    } else {
        lines.push(
            "  Try              Start `openyak` to use interactive slash commands".to_string(),
        );
        lines.push(
            "  Tip              Resume-safe commands also work with `openyak --resume SESSION.json ...`"
                .to_string(),
        );
    }
    lines.join("\n")
}

fn resolve_model_alias(model: &str) -> &str {
    match model {
        "opus" => "claude-opus-4-6",
        "sonnet" => "claude-sonnet-4-6",
        "haiku" => "claude-haiku-4-5-20251213",
        _ => model,
    }
}

fn normalize_allowed_tools(values: &[String]) -> Result<Option<AllowedToolSet>, String> {
    if values.is_empty() {
        return Ok(None);
    }

    match current_tool_registry() {
        Ok(registry) => registry.normalize_allowed_tools(values),
        Err(_) => GlobalToolRegistry::builtin().normalize_allowed_tools(values),
    }
}

fn current_tool_registry() -> Result<GlobalToolRegistry, String> {
    let cwd = env::current_dir().map_err(|error| error.to_string())?;
    let loader = ConfigLoader::default_for(&cwd);
    let runtime_config = loader.load().map_err(|error| error.to_string())?;
    let plugin_manager = build_plugin_manager(&cwd, &loader, &runtime_config);
    let plugin_tools = plugin_manager
        .aggregated_tools()
        .map_err(|error| error.to_string())?;
    GlobalToolRegistry::with_plugin_tools(plugin_tools)
}

fn parse_permission_mode_arg(value: &str) -> Result<PermissionMode, String> {
    normalize_permission_mode(value)
        .ok_or_else(|| {
            format!(
                "unsupported permission mode '{value}'. Use read-only, workspace-write, or danger-full-access."
            )
        })
        .map(permission_mode_from_label)
}

fn permission_mode_from_label(mode: &str) -> PermissionMode {
    match mode {
        "read-only" => PermissionMode::ReadOnly,
        "workspace-write" => PermissionMode::WorkspaceWrite,
        "danger-full-access" => PermissionMode::DangerFullAccess,
        other => panic!("unsupported permission mode label: {other}"),
    }
}

fn default_permission_mode() -> PermissionMode {
    env::var("OPENYAK_PERMISSION_MODE")
        .ok()
        .as_deref()
        .and_then(normalize_permission_mode)
        .map_or(PermissionMode::DangerFullAccess, permission_mode_from_label)
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

fn request_user_input_tools(
    tool_registry: &GlobalToolRegistry,
    allowed_tools: Option<&AllowedToolSet>,
) -> Vec<ToolDefinition> {
    let mut tools = tool_registry.definitions(allowed_tools);
    tools.push(request_user_input_tool_definition());
    tools
}

fn request_user_input_response_value(response: &UserInputResponse) -> serde_json::Value {
    json!({
        "request_id": response.request_id,
        "content": response.content,
        "selected_option": response.selected_option,
    })
}

fn json_string_array_field(
    object: &serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> Result<Vec<String>, RuntimeError> {
    let Some(value) = object.get(key) else {
        return Ok(Vec::new());
    };
    let items = value.as_array().ok_or_else(|| {
        RuntimeError::new(format!("request-user-input field '{key}' must be an array"))
    })?;
    items
        .iter()
        .map(|item| {
            item.as_str().map(ToOwned::to_owned).ok_or_else(|| {
                RuntimeError::new(format!(
                    "request-user-input field '{key}' must contain only strings"
                ))
            })
        })
        .collect()
}

fn parse_request_user_input_request(
    id: &str,
    input: &str,
) -> Result<UserInputRequest, RuntimeError> {
    let value: serde_json::Value = serde_json::from_str(input).map_err(|error| {
        RuntimeError::new(format!("invalid request-user-input payload: {error}"))
    })?;
    let object = value
        .as_object()
        .ok_or_else(|| RuntimeError::new("request-user-input payload must be a JSON object"))?;
    let request_id = object
        .get("request_id")
        .and_then(serde_json::Value::as_str)
        .map_or_else(|| id.to_string(), ToOwned::to_owned);
    let prompt = object
        .get("prompt")
        .and_then(serde_json::Value::as_str)
        .map(ToOwned::to_owned)
        .ok_or_else(|| RuntimeError::new("request-user-input payload missing prompt"))?;
    if prompt.trim().is_empty() {
        return Err(RuntimeError::new(
            "request-user-input payload must include a non-empty prompt",
        ));
    }

    Ok(UserInputRequest {
        request_id,
        prompt,
        options: json_string_array_field(object, "options")?,
        allow_freeform: object
            .get("allow_freeform")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
    })
}

fn filter_tool_specs(
    tool_registry: &GlobalToolRegistry,
    allowed_tools: Option<&AllowedToolSet>,
) -> Vec<ToolDefinition> {
    request_user_input_tools(tool_registry, allowed_tools)
}

fn is_help_args(args: &[String]) -> bool {
    args.len() == 1 && matches!(args[0].as_str(), "--help" | "-h")
}

fn looks_like_cli_command_token(value: &str) -> bool {
    matches!(
        value,
        "dump-manifests"
            | "bootstrap-plan"
            | "agents"
            | "skills"
            | "system-prompt"
            | "login"
            | "logout"
            | "init"
            | "onboard"
            | "prompt"
    ) || value.starts_with('/')
}

fn parse_system_prompt_args(args: &[String]) -> Result<CliAction, String> {
    parse_system_prompt_args_with_default_date(args, current_local_date_string())
}

fn parse_system_prompt_args_with_default_date(
    args: &[String],
    default_date: String,
) -> Result<CliAction, String> {
    if is_help_args(args) {
        return Ok(CliAction::Help(HelpTopic::SystemPrompt));
    }
    let mut cwd = env::current_dir().map_err(|error| error.to_string())?;
    let mut date = default_date;
    let mut index = 0;

    while index < args.len() {
        match args[index].as_str() {
            "--cwd" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| "missing value for --cwd".to_string())?;
                cwd = PathBuf::from(value);
                index += 2;
            }
            "--date" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| "missing value for --date".to_string())?;
                date.clone_from(value);
                index += 2;
            }
            other => return Err(format!("unknown system-prompt option: {other}")),
        }
    }

    Ok(CliAction::PrintSystemPrompt { cwd, date })
}

fn parse_resume_args(args: &[String]) -> Result<CliAction, String> {
    let session_path = args
        .first()
        .ok_or_else(|| "missing session path for --resume".to_string())
        .map(PathBuf::from)?;
    let commands = group_resume_commands(&args[1..])?;
    Ok(CliAction::ResumeSession {
        session_path,
        commands,
    })
}

fn group_resume_commands(args: &[String]) -> Result<Vec<String>, String> {
    if args.is_empty() {
        return Ok(Vec::new());
    }

    let mut commands = Vec::new();
    let mut current = Vec::new();

    for arg in args {
        let starts_with_slash = arg.trim_start().starts_with('/');
        if current.is_empty() {
            if !starts_with_slash {
                return Err("--resume trailing arguments must be slash commands".to_string());
            }
            current.push(arg.clone());
            continue;
        }

        if starts_with_slash
            && (is_known_slash_command_start(arg)
                || !resume_command_allows_slash_prefixed_args(&current[0]))
        {
            commands.push(current.join(" "));
            current.clear();
        }

        current.push(arg.clone());
    }

    if !current.is_empty() {
        commands.push(current.join(" "));
    }

    Ok(commands)
}

fn is_known_slash_command_start(token: &str) -> bool {
    let Some(name) = token.trim().strip_prefix('/').and_then(|value| {
        value
            .split_whitespace()
            .next()
            .filter(|candidate| !candidate.is_empty())
    }) else {
        return false;
    };

    slash_command_specs()
        .iter()
        .any(|spec| spec.name == name || spec.aliases.contains(&name))
}

fn resume_command_allows_slash_prefixed_args(command_head: &str) -> bool {
    matches!(
        SlashCommand::parse(command_head),
        Some(SlashCommand::Export { .. })
    )
}

fn dump_manifests() {
    let workspace_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let paths = UpstreamPaths::from_workspace_dir(&workspace_dir);
    match extract_manifest(&paths) {
        Ok(manifest) => {
            println!("commands: {}", manifest.commands.entries().len());
            println!("tools: {}", manifest.tools.entries().len());
            println!("bootstrap phases: {}", manifest.bootstrap.phases().len());
        }
        Err(error) => {
            eprintln!("warning: upstream manifest extraction unavailable, falling back to local manifests: {error}");
            println!("commands: {}", slash_command_specs().len());
            println!("tools: {}", mvp_tool_specs().len());
            println!(
                "bootstrap phases: {}",
                runtime::BootstrapPlan::openyak_default().phases().len()
            );
        }
    }
}

fn print_bootstrap_plan() {
    for phase in runtime::BootstrapPlan::openyak_default().phases() {
        println!("- {phase:?}");
    }
}

pub(crate) fn configured_oauth_config(
    config: &runtime::RuntimeConfig,
) -> Result<Option<OAuthConfig>, String> {
    if let Some(oauth) = config.oauth() {
        return Ok(Some(oauth.clone()));
    }

    let Some(oauth_override) = config.oauth_override() else {
        return Ok(None);
    };

    let mut missing = Vec::new();
    if oauth_override.client_id.is_none() {
        missing.push("clientId");
    }
    if oauth_override.authorize_url.is_none() {
        missing.push("authorizeUrl");
    }
    if oauth_override.token_url.is_none() {
        missing.push("tokenUrl");
    }

    if missing.is_empty() {
        Ok(oauth_override.resolved())
    } else {
        Err(format!(
            "settings.oauth is incomplete; missing {}. `openyak login` no longer uses a built-in OAuth provider.",
            missing.join(", ")
        ))
    }
}

pub(crate) fn run_login() -> Result<(), Box<dyn std::error::Error>> {
    let cwd = env::current_dir()?;
    let config = ConfigLoader::default_for(&cwd).load()?;
    let oauth = configured_oauth_config(&config)
        .map_err(io::Error::other)?
        .ok_or_else(|| {
            io::Error::other(
                "`openyak login` requires settings.oauth.clientId, authorizeUrl, and tokenUrl; no default OAuth site is configured.",
            )
        })?;
    let callback_port = oauth.callback_port.unwrap_or(DEFAULT_OAUTH_CALLBACK_PORT);
    let redirect_uri = oauth
        .manual_redirect_url
        .clone()
        .unwrap_or_else(|| runtime::loopback_redirect_uri(callback_port));
    let pkce = generate_pkce_pair()?;
    let state = generate_state()?;
    let authorize_url =
        OAuthAuthorizationRequest::from_config(&oauth, redirect_uri.clone(), state.clone(), &pkce)
            .build_url();
    let listener = if oauth.manual_redirect_url.is_some() {
        None
    } else {
        Some(bind_oauth_callback_listener(callback_port)?)
    };

    println!("Starting openyak OAuth login...");
    if listener.is_some() {
        println!("Listening for callback on {redirect_uri}");
    } else {
        println!("Manual redirect URL configured: {redirect_uri}");
    }
    if let Err(error) = open_browser(&authorize_url) {
        eprintln!("warning: failed to open browser automatically: {error}");
        println!("Open this URL manually:\n{authorize_url}");
    }

    let callback = if let Some(listener) = &listener {
        wait_for_oauth_callback(listener)?
    } else {
        wait_for_manual_oauth_callback(&redirect_uri)?
    };
    if let Some(error) = callback.error {
        let description = callback
            .error_description
            .unwrap_or_else(|| "authorization failed".to_string());
        return Err(io::Error::other(format!("{error}: {description}")).into());
    }
    let code = callback.code.ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "callback did not include code")
    })?;
    let returned_state = callback.state.ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "callback did not include state")
    })?;
    if returned_state != state {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "oauth state mismatch").into());
    }

    let client = OpenyakApiClient::from_auth(AuthSource::None).with_base_url(api::read_base_url());
    let exchange_request =
        OAuthTokenExchangeRequest::from_config(&oauth, code, state, pkce.verifier, redirect_uri);
    let runtime = tokio::runtime::Runtime::new()?;
    let token_set = runtime.block_on(client.exchange_oauth_code(&oauth, &exchange_request))?;
    save_oauth_credentials(&runtime::OAuthTokenSet {
        access_token: token_set.access_token,
        refresh_token: token_set.refresh_token,
        expires_at: token_set.expires_at,
        scopes: token_set.scopes,
    })?;
    println!("openyak OAuth login complete.");
    Ok(())
}

fn run_logout() -> Result<(), Box<dyn std::error::Error>> {
    clear_oauth_credentials()?;
    println!("openyak OAuth credentials cleared.");
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DoctorCheckStatus {
    Ok,
    Warning,
    Error,
}

impl DoctorCheckStatus {
    const fn label(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Warning => "warning",
            Self::Error => "error",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DoctorCheck {
    pub(crate) name: &'static str,
    pub(crate) status: DoctorCheckStatus,
    pub(crate) summary: String,
    pub(crate) hint: Option<String>,
}

impl DoctorCheck {
    fn ok(name: &'static str, summary: impl Into<String>, hint: Option<String>) -> Self {
        Self {
            name,
            status: DoctorCheckStatus::Ok,
            summary: summary.into(),
            hint,
        }
    }

    fn warning(name: &'static str, summary: impl Into<String>, hint: Option<String>) -> Self {
        Self {
            name,
            status: DoctorCheckStatus::Warning,
            summary: summary.into(),
            hint,
        }
    }

    fn error(name: &'static str, summary: impl Into<String>, hint: Option<String>) -> Self {
        Self {
            name,
            status: DoctorCheckStatus::Error,
            summary: summary.into(),
            hint,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DoctorReport {
    pub(crate) workspace: PathBuf,
    pub(crate) config_home: PathBuf,
    pub(crate) credentials_path: PathBuf,
    pub(crate) checks: Vec<DoctorCheck>,
}

impl DoctorReport {
    pub(crate) fn has_errors(&self) -> bool {
        self.checks
            .iter()
            .any(|check| matches!(check.status, DoctorCheckStatus::Error))
    }

    pub(crate) fn counts(&self) -> (usize, usize, usize) {
        let mut ok = 0;
        let mut warnings = 0;
        let mut errors = 0;
        for check in &self.checks {
            match check.status {
                DoctorCheckStatus::Ok => ok += 1,
                DoctorCheckStatus::Warning => warnings += 1,
                DoctorCheckStatus::Error => errors += 1,
            }
        }
        (ok, warnings, errors)
    }
}

fn run_doctor() -> Result<(), Box<dyn std::error::Error>> {
    let cwd = env::current_dir()?;
    let loader = ConfigLoader::default_for(&cwd);
    let report = collect_doctor_report_with_loader(&cwd, &loader);
    print!("{}", render_doctor_report(&report));
    if report.has_errors() {
        return Err(io::Error::other("openyak doctor found blocking issues").into());
    }
    Ok(())
}

pub(crate) fn collect_doctor_report(cwd: &Path) -> DoctorReport {
    let loader = ConfigLoader::default_for(cwd);
    collect_doctor_report_with_loader(cwd, &loader)
}

pub(crate) fn collect_doctor_report_with_loader(cwd: &Path, loader: &ConfigLoader) -> DoctorReport {
    let config_home = loader.config_home().to_path_buf();
    let credentials_path =
        credentials_path().unwrap_or_else(|_| config_home.join("credentials.json"));
    let mut checks = Vec::new();

    let config = match loader.load() {
        Ok(config) => {
            if config.loaded_entries().is_empty() {
                checks.push(DoctorCheck::ok(
                    "config",
                    "No config files found; built-in defaults are active.",
                    Some(
                        "Run `openyak init` if you want repo-local starter config files."
                            .to_string(),
                    ),
                ));
            } else {
                checks.push(DoctorCheck::ok(
                    "config",
                    format!("Loaded {} config file(s).", config.loaded_entries().len()),
                    None,
                ));
            }
            Some(config)
        }
        Err(error) => {
            checks.push(DoctorCheck::error(
                "config",
                format!("Runtime config failed to load: {error}"),
                Some(
                    "Fix or remove the reported settings file, then rerun `openyak doctor`."
                        .to_string(),
                ),
            ));
            None
        }
    };

    match config.as_ref() {
        Some(config) => checks.push(doctor_oauth_config_check(config)),
        None => checks.push(doctor_skipped_check("oauth config")),
    }

    checks.push(doctor_saved_oauth_check());

    match config.as_ref() {
        Some(config) => checks.push(doctor_active_model_auth_check(config)),
        None => checks.push(doctor_skipped_check("active model auth")),
    }

    checks.push(doctor_github_cli_check());

    DoctorReport {
        workspace: cwd.to_path_buf(),
        config_home,
        credentials_path,
        checks,
    }
}

fn doctor_oauth_config_check(config: &runtime::RuntimeConfig) -> DoctorCheck {
    match configured_oauth_config(config) {
        Ok(Some(oauth)) => DoctorCheck::ok(
            "oauth config",
            format!("settings.oauth is configured for client `{}`.", oauth.client_id),
            None,
        ),
        Ok(None) => DoctorCheck::ok(
            "oauth config",
            "settings.oauth is not configured. That is fine if you use API keys instead of `openyak login`.",
            Some(
                "Add `settings.oauth.clientId`, `authorizeUrl`, and `tokenUrl` only if you want browser-based `openyak login`.".to_string(),
            ),
        ),
        Err(error) => DoctorCheck::error(
            "oauth config",
            error,
            Some(
                "Fill in `settings.oauth.clientId`, `authorizeUrl`, and `tokenUrl`, or remove the partial override.".to_string(),
            ),
        ),
    }
}

fn doctor_skipped_check(name: &'static str) -> DoctorCheck {
    DoctorCheck::warning(
        name,
        "Skipped because runtime config did not load.",
        Some("Resolve the config error above first.".to_string()),
    )
}

fn doctor_saved_oauth_check() -> DoctorCheck {
    match load_oauth_credentials() {
        Ok(Some(token_set))
            if doctor_token_is_expired(token_set.expires_at) && token_set.refresh_token.is_none() =>
        {
            DoctorCheck::warning(
                "oauth credentials",
                "Saved OAuth credentials are expired and cannot refresh.",
                Some("Run `openyak login` again to replace the expired token.".to_string()),
            )
        }
        Ok(Some(token_set)) if doctor_token_is_expired(token_set.expires_at) => DoctorCheck::warning(
            "oauth credentials",
            "Saved OAuth credentials are expired; runtime will need to refresh them on next use.",
            Some(
                "If refresh fails, rerun `openyak login` after confirming `settings.oauth` is still valid.".to_string(),
            ),
        ),
        Ok(Some(_)) => DoctorCheck::ok(
            "oauth credentials",
            "Saved OAuth credentials are available.",
            None,
        ),
        Ok(None) => DoctorCheck::ok(
            "oauth credentials",
            "No saved OAuth credentials found. That is expected if you use API-key auth.",
            Some(
                "Run `openyak login` only if you want OAuth-backed auth instead of API keys.".to_string(),
            ),
        ),
        Err(error) => DoctorCheck::error(
            "oauth credentials",
            format!("Failed to read saved OAuth credentials: {error}"),
            Some("Clear or repair the credentials store, then rerun `openyak doctor`.".to_string()),
        ),
    }
}

pub(crate) fn doctor_token_is_expired(expires_at: Option<u64>) -> bool {
    expires_at.is_some_and(|timestamp| {
        timestamp
            <= SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock should be after unix epoch")
                .as_secs()
    })
}

fn doctor_active_model_auth_check(config: &runtime::RuntimeConfig) -> DoctorCheck {
    let model = config
        .model()
        .map_or(DEFAULT_MODEL, resolve_model_alias)
        .to_string();
    let provider = api::detect_provider_kind(&model);
    let default_auth = if matches!(provider, api::ProviderKind::OpenyakApi) {
        match resolve_startup_auth_source(|| {
            configured_oauth_config(config).map_err(api::ApiError::Auth)
        }) {
            Ok(auth) => Some(auth),
            Err(error) => {
                return DoctorCheck::warning(
                    "active model auth",
                    format!(
                        "Active model `{model}` ({}) has no usable auth: {error}",
                        doctor_provider_label(provider)
                    ),
                    Some(doctor_auth_hint(provider)),
                )
            }
        }
    } else {
        None
    };

    match ProviderClient::from_model_with_default_auth(&model, default_auth) {
        Ok(_) => DoctorCheck::ok(
            "active model auth",
            format!(
                "Active model `{model}` ({}) passed local auth bootstrap.",
                doctor_provider_label(provider)
            ),
            None,
        ),
        Err(error) => DoctorCheck::warning(
            "active model auth",
            format!(
                "Active model `{model}` ({}) is not ready: {error}",
                doctor_provider_label(provider)
            ),
            Some(doctor_auth_hint(provider)),
        ),
    }
}

fn doctor_github_cli_check() -> DoctorCheck {
    match resolve_command_path("gh") {
        Some(path) => DoctorCheck::ok(
            "github cli",
            format!("GitHub CLI is available at {}.", path.display()),
            None,
        ),
        None => DoctorCheck::warning(
            "github cli",
            "`gh` is not available on PATH.",
            Some(
                "Install GitHub CLI and run `gh auth login --web` if you want `/pr`, `/issue`, or `/commit-push-pr` workflows.".to_string(),
            ),
        ),
    }
}

pub(crate) const fn doctor_provider_label(provider: api::ProviderKind) -> &'static str {
    match provider {
        api::ProviderKind::OpenyakApi => "openyak",
        api::ProviderKind::Xai => "xai",
        api::ProviderKind::OpenAi => "openai-compatible",
    }
}

pub(crate) fn doctor_auth_hint(provider: api::ProviderKind) -> String {
    match provider {
        api::ProviderKind::OpenyakApi => "Set `ANTHROPIC_API_KEY` / `ANTHROPIC_AUTH_TOKEN`, or configure `settings.oauth` and run `openyak login`.".to_string(),
        api::ProviderKind::Xai => {
            "Set `XAI_API_KEY` (and optionally `XAI_BASE_URL`) for the active model.".to_string()
        }
        api::ProviderKind::OpenAi => {
            "Set `OPENAI_API_KEY` (and optionally `OPENAI_BASE_URL`) for the active model.".to_string()
        }
    }
}

pub(crate) fn render_doctor_report(report: &DoctorReport) -> String {
    let mut output = String::new();
    let _ = writeln!(output, "openyak Doctor");
    let _ = writeln!(output, "  Workspace         {}", report.workspace.display());
    let _ = writeln!(
        output,
        "  Config home       {}",
        report.config_home.display()
    );
    let _ = writeln!(
        output,
        "  Credentials path  {}",
        report.credentials_path.display()
    );
    let _ = writeln!(output);
    let _ = writeln!(output, "Checks");
    for check in &report.checks {
        let _ = writeln!(
            output,
            "  [{:<7}] {:<18} {}",
            check.status.label(),
            check.name,
            check.summary
        );
        if let Some(hint) = &check.hint {
            let _ = writeln!(output, "             Fix: {hint}");
        }
    }
    let (ok, warnings, errors) = report.counts();
    let _ = writeln!(output);
    let _ = writeln!(
        output,
        "Summary\n  {ok} ok, {warnings} warning(s), {errors} error(s)"
    );
    output
}

fn open_browser(url: &str) -> io::Result<()> {
    let commands = browser_open_commands(url);
    for (program, args) in commands {
        let Some(resolved_program) = resolve_command_path(program) else {
            continue;
        };
        match Command::new(resolved_program).args(&args).spawn() {
            Ok(_) => return Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "no supported browser opener command found",
    ))
}

fn browser_open_commands(url: &str) -> Vec<(&'static str, Vec<String>)> {
    if cfg!(target_os = "macos") {
        vec![("open", vec![url.to_string()])]
    } else if cfg!(target_os = "windows") {
        vec![
            ("explorer", vec![url.to_string()]),
            (
                "rundll32",
                vec!["url.dll,FileProtocolHandler".to_string(), url.to_string()],
            ),
        ]
    } else {
        vec![("xdg-open", vec![url.to_string()])]
    }
}

fn bind_oauth_callback_listener(port: u16) -> io::Result<TcpListener> {
    TcpListener::bind(("127.0.0.1", port))
}

fn wait_for_oauth_callback(
    listener: &TcpListener,
) -> Result<runtime::OAuthCallbackParams, Box<dyn std::error::Error>> {
    let (mut stream, _) = listener.accept()?;
    let mut buffer = [0_u8; 4096];
    let bytes_read = stream.read(&mut buffer)?;
    let request = String::from_utf8_lossy(&buffer[..bytes_read]);
    let request_line = request.lines().next().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "missing callback request line")
    })?;
    let target = request_line.split_whitespace().nth(1).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "missing callback request target",
        )
    })?;
    let callback = parse_oauth_callback_request_target(target)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let body = if callback.error.is_some() {
        "openyak OAuth login failed. You can close this window."
    } else {
        "openyak OAuth login succeeded. You can close this window."
    };
    let response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/plain; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    stream.write_all(response.as_bytes())?;
    Ok(callback)
}

fn wait_for_manual_oauth_callback(
    redirect_uri: &str,
) -> Result<runtime::OAuthCallbackParams, Box<dyn std::error::Error>> {
    println!("Complete authorization in your browser, then paste the final redirected URL or query string below.");
    println!("Expected redirect base: {redirect_uri}");

    loop {
        print!("OAuth callback> ");
        io::stdout().flush()?;

        let mut input = String::new();
        let bytes_read = io::stdin().read_line(&mut input)?;
        if bytes_read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "stdin closed before OAuth callback was provided",
            )
            .into());
        }

        match parse_manual_oauth_callback_input(&input, redirect_uri) {
            Ok(callback) => return Ok(callback),
            Err(error) => eprintln!("Invalid OAuth callback input: {error}"),
        }
    }
}

fn parse_manual_oauth_callback_input(
    input: &str,
    redirect_uri: &str,
) -> Result<runtime::OAuthCallbackParams, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err("callback input is empty".to_string());
    }

    if trimmed.contains("://") {
        let pasted_base = trimmed.split('?').next().unwrap_or(trimmed);
        let expected_base = redirect_uri.split('?').next().unwrap_or(redirect_uri);
        if pasted_base != expected_base {
            return Err(format!(
                "callback URL must match configured manualRedirectUrl base `{expected_base}`"
            ));
        }
    }

    runtime::parse_oauth_callback_input(trimmed)
}

fn print_system_prompt(cwd: &Path, date: &str) {
    match build_system_prompt_for_cwd_with_date(cwd, None, date) {
        Ok(sections) => println!("{}", sections.join("\n\n")),
        Err(error) => {
            eprintln!("failed to build system prompt: {error}");
            std::process::exit(1);
        }
    }
}

fn print_version() {
    println!("{}", render_version_report());
}

fn resume_session(session_reference: &Path, commands: &[String]) {
    let (handle, session) = match load_session_from_reference(session_reference) {
        Ok(result) => result,
        Err(error) => {
            eprintln!("failed to restore session: {error}");
            std::process::exit(1);
        }
    };

    if commands.is_empty() {
        println!(
            "Restored session from {} ({} messages).",
            handle.path.display(),
            session.messages.len()
        );
        return;
    }

    let mut session = session;
    for raw_command in commands {
        let Some(command) = SlashCommand::parse(raw_command) else {
            eprintln!("unsupported resumed command: {raw_command}");
            std::process::exit(2);
        };
        match run_resume_command(&handle.path, &session, &command) {
            Ok(ResumeCommandOutcome {
                session: next_session,
                message,
            }) => {
                session = next_session;
                if let Some(message) = message {
                    println!("{message}");
                }
            }
            Err(error) => {
                eprintln!("{error}");
                std::process::exit(2);
            }
        }
    }
}

fn load_session_from_reference(
    session_reference: &Path,
) -> Result<(SessionHandle, Session), Box<dyn std::error::Error>> {
    let handle = resolve_session_reference(&session_reference.to_string_lossy())?;
    let session = Session::load_from_path(&handle.path)?;
    Ok((handle, session))
}

#[derive(Debug, Clone)]
struct ResumeCommandOutcome {
    session: Session,
    message: Option<String>,
}

#[derive(Debug, Clone)]
struct StatusContext {
    cwd: PathBuf,
    session_path: Option<PathBuf>,
    loaded_config_files: usize,
    discovered_config_files: usize,
    memory_file_count: usize,
    project_root: Option<PathBuf>,
    git_branch: Option<String>,
    resume_mode: bool,
}

#[derive(Debug, Clone, Copy)]
struct StatusUsage {
    message_count: usize,
    turns: u32,
    latest: TokenUsage,
    cumulative: TokenUsage,
    estimated_tokens: usize,
}

fn format_model_report(model: &str, message_count: usize, turns: u32) -> String {
    format!(
        "Model
  Current          {model}
  Session          {message_count} messages · {turns} turns

Aliases
  opus             claude-opus-4-6
  sonnet           claude-sonnet-4-6
  haiku            claude-haiku-4-5-20251213

Next
  /model           Show the current model
  /model <name>    Switch models for this REPL session"
    )
}

fn format_model_switch_report(previous: &str, next: &str, message_count: usize) -> String {
    format!(
        "Model updated
  Previous         {previous}
  Current          {next}
  Preserved        {message_count} messages
  Tip              Existing conversation context stayed attached"
    )
}

fn format_permissions_report(mode: &str, plan_restore_mode: Option<&str>) -> String {
    let modes = [
        ("read-only", "Read/search tools only", mode == "read-only"),
        (
            "workspace-write",
            "Edit files inside the workspace",
            mode == "workspace-write",
        ),
        (
            "danger-full-access",
            "Unrestricted tool access",
            mode == "danger-full-access",
        ),
    ]
    .into_iter()
    .map(|(name, description, is_current)| {
        let marker = if is_current {
            "● current"
        } else {
            "○ available"
        };
        format!("  {name:<18} {marker:<11} {description}")
    })
    .collect::<Vec<_>>()
    .join(
        "
",
    );

    let effect = match mode {
        "read-only" => "Only read/search tools can run automatically",
        "workspace-write" => "Editing tools can modify files in the workspace",
        "danger-full-access" => "All tools can run without additional sandbox limits",
        _ => "Unknown permission mode",
    };
    let planning = plan_restore_mode.map_or_else(String::new, |restore_mode| {
        format!(
            "

Planning
  State            active
  Restore mode     {restore_mode}
  Exit             /plan exit"
        )
    });
    let next = if plan_restore_mode.is_some() {
        "  /permissions              Show the current mode\n  /plan exit               Leave explicit plan mode first"
    } else {
        "  /permissions              Show the current mode\n  /permissions <mode>       Switch modes for subsequent tool calls"
    };

    format!(
        "Permissions
  Active mode      {mode}
  Effect           {effect}
{planning}

Modes
{modes}

Next
{next}"
    )
}

fn format_permissions_switch_report(previous: &str, next: &str) -> String {
    format!(
        "Permissions updated
  Previous mode    {previous}
  Active mode      {next}
  Applies to       Subsequent tool calls in this REPL
  Tip              Run /permissions to review all available modes"
    )
}

fn format_plan_mode_enabled_report(previous_mode: &str) -> String {
    format!(
        "Plan mode enabled
  Active mode      read-only
  Restore mode     {previous_mode}
  Applies to       Subsequent tool calls in this REPL
  Next             /plan exit to restore {previous_mode}"
    )
}

fn format_plan_mode_already_active_report(restore_mode: &str) -> String {
    format!(
        "Plan mode already active
  Active mode      read-only
  Restore mode     {restore_mode}
  Next             /plan exit to restore {restore_mode}"
    )
}

fn format_plan_mode_disabled_report(restored_mode: &str) -> String {
    format!(
        "Plan mode disabled
  Restored mode    {restored_mode}
  Applies to       Subsequent tool calls in this REPL
  Tip              Run /plan to re-enter explicit planning mode"
    )
}

fn format_plan_mode_not_active_report(mode: &str) -> String {
    format!(
        "Plan mode inactive
  Active mode      {mode}
  Next             Run /plan to enter explicit planning mode"
    )
}

fn format_plan_permissions_blocked_report(active_mode: &str, restore_mode: &str) -> String {
    format!(
        "Plan mode requires an explicit exit
  Active mode      {active_mode}
  Restore mode     {restore_mode}
  Next             Run /plan exit before changing /permissions"
    )
}

fn estimate_cost_for_report(
    usage: TokenUsage,
    model: Option<&str>,
) -> (runtime::UsageCostEstimate, &'static str) {
    if let Some(model_name) = model {
        if let Some(pricing) = pricing_for_model(model_name) {
            return (
                usage.estimate_cost_usd_with_pricing(pricing),
                "model-specific",
            );
        }
    }

    (usage.estimate_cost_usd(), "estimated-default")
}

fn accounting_status_label(status: SessionAccountingStatus) -> &'static str {
    match status {
        SessionAccountingStatus::Complete => "complete",
        SessionAccountingStatus::PartialLegacyCompaction => "partial",
    }
}

fn accounting_status_note(status: SessionAccountingStatus) -> &'static str {
    match status {
        SessionAccountingStatus::Complete => {
            "totals include preserved compacted history when available"
        }
        SessionAccountingStatus::PartialLegacyCompaction => {
            "legacy compacted history predates telemetry; totals may be incomplete"
        }
    }
}

fn compact_accounting_note(status: SessionAccountingStatus) -> &'static str {
    match status {
        SessionAccountingStatus::Complete => {
            "historical accounting was preserved across compaction"
        }
        SessionAccountingStatus::PartialLegacyCompaction => {
            "known history was preserved, but legacy compacted totals remain partial"
        }
    }
}

fn compact_summary_mode_label(mode: CompactionSummaryMode) -> &'static str {
    match mode {
        CompactionSummaryMode::Unchanged => "unchanged",
        CompactionSummaryMode::NewSummary => "new summary",
        CompactionSummaryMode::MergedExisting => "merged existing summary",
    }
}

fn render_cost_report(model: Option<&str>, tracker: &UsageTracker) -> String {
    format_cost_report(
        model,
        tracker.turns(),
        tracker.current_turn_usage(),
        tracker.cumulative_usage(),
        tracker.accounting_status(),
    )
}

fn format_cost_report(
    model: Option<&str>,
    turns: u32,
    latest: TokenUsage,
    cumulative: TokenUsage,
    accounting_status: SessionAccountingStatus,
) -> String {
    let (latest_cost, pricing_label) = estimate_cost_for_report(latest, model);
    let (cumulative_cost, _) = estimate_cost_for_report(cumulative, model);
    format!(
        "Cost
  Model            {}
  Pricing          {}
  Turns            {}
  Accounting       {}
  Note             {}

Latest turn
  Input tokens     {}
  Output tokens    {}
  Cache create     {}
  Cache read       {}
  Total tokens     {}
  Estimated cost   {}

Cumulative
  Input tokens     {}
  Output tokens    {}
  Cache create     {}
  Cache read       {}
  Total tokens     {}
  Estimated cost   {}

Cost breakdown
  Input            {}
  Output           {}
  Cache create     {}
  Cache read       {}

Next
  /status          See session + workspace context
  /compact         Trim local history if the session is getting large",
        model.unwrap_or("restored-session"),
        pricing_label,
        turns,
        accounting_status_label(accounting_status),
        accounting_status_note(accounting_status),
        latest.input_tokens,
        latest.output_tokens,
        latest.cache_creation_input_tokens,
        latest.cache_read_input_tokens,
        latest.total_tokens(),
        format_usd(latest_cost.total_cost_usd()),
        cumulative.input_tokens,
        cumulative.output_tokens,
        cumulative.cache_creation_input_tokens,
        cumulative.cache_read_input_tokens,
        cumulative.total_tokens(),
        format_usd(cumulative_cost.total_cost_usd()),
        format_usd(cumulative_cost.input_cost_usd),
        format_usd(cumulative_cost.output_cost_usd),
        format_usd(cumulative_cost.cache_creation_cost_usd),
        format_usd(cumulative_cost.cache_read_cost_usd),
    )
}

fn format_resume_report(session_path: &str, message_count: usize, turns: u32) -> String {
    format!(
        "Session resumed
  Session file     {session_path}
  History          {message_count} messages · {turns} turns
  Next             /status · /diff · /export"
    )
}

fn format_user_input_report(
    request_id: &str,
    prompt: &str,
    options: &[String],
    allow_freeform: bool,
) -> String {
    let mut lines = vec![
        "Pending input request".to_string(),
        format!("  Request id       {request_id}"),
        format!("  Prompt           {prompt}"),
    ];
    if !options.is_empty() {
        lines.push("  Options".to_string());
        lines.extend(
            options
                .iter()
                .enumerate()
                .map(|(index, option)| format!("    {}. {}", index + 1, option)),
        );
    }
    lines.push(format!(
        "  Freeform         {}",
        if allow_freeform {
            "allowed"
        } else {
            "disabled"
        }
    ));
    lines.push("  Continue         reply at the prompt below to resume the same turn".to_string());
    lines.push("  Cancel           Ctrl+C keeps this request pending".to_string());
    lines.join("\n")
}

fn format_pending_user_input_report(request: &PendingUserInputRequest) -> String {
    format_user_input_report(
        &request.request_id,
        &request.prompt,
        &request.options,
        request.allow_freeform,
    )
}

fn format_user_input_request(request: &UserInputRequest) -> String {
    format_user_input_report(
        &request.request_id,
        &request.prompt,
        &request.options,
        request.allow_freeform,
    )
}

fn parse_user_input_submission(
    request: &UserInputRequest,
    raw: &str,
) -> Result<UserInputResponse, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(
            "Reply required. Enter a value or press Ctrl+C to keep the request pending."
                .to_string(),
        );
    }

    let selected_option = if request.options.is_empty() {
        None
    } else if let Ok(index) = trimmed.parse::<usize>() {
        request
            .options
            .get(index.saturating_sub(1))
            .cloned()
            .ok_or_else(|| {
                format!(
                    "Invalid option '{trimmed}'. Choose 1-{} or an exact label.",
                    request.options.len()
                )
            })?
            .into()
    } else {
        request
            .options
            .iter()
            .find(|option| option.eq_ignore_ascii_case(trimmed))
            .cloned()
    };

    if let Some(selected_option) = selected_option {
        return Ok(UserInputResponse {
            request_id: request.request_id.clone(),
            content: selected_option.clone(),
            selected_option: Some(selected_option),
        });
    }

    if request.allow_freeform || request.options.is_empty() {
        return Ok(UserInputResponse {
            request_id: request.request_id.clone(),
            content: trimmed.to_string(),
            selected_option: None,
        });
    }

    Err(format!(
        "Reply must match one of the listed options: {}.",
        request.options.join(", ")
    ))
}

fn format_compact_report(result: &runtime::CompactionResult) -> String {
    if result.summary_mode == CompactionSummaryMode::Unchanged {
        format!(
            "Compact
  Result           skipped
  Reason           Session is already below the compaction threshold
  Current tokens   {}
  Messages kept    {}",
            result.estimated_tokens_after,
            result.compacted_session.messages.len(),
        )
    } else {
        let token_delta = result
            .estimated_tokens_before
            .saturating_sub(result.estimated_tokens_after);
        format!(
            "Compact
  Result           compacted
  Summary mode     {}
  Messages removed {}
  Messages kept    {}
  Tokens before    {}
  Tokens after     {}
  Token delta      {}
  Accounting       {}
  Note             {}
  Tip              Use /cost to review preserved session accounting",
            compact_summary_mode_label(result.summary_mode),
            result.removed_message_count,
            result.compacted_session.messages.len(),
            result.estimated_tokens_before,
            result.estimated_tokens_after,
            token_delta,
            accounting_status_label(result.accounting_status),
            compact_accounting_note(result.accounting_status),
        )
    }
}

fn parse_git_status_metadata(status: Option<&str>) -> (Option<PathBuf>, Option<String>) {
    let Some(status) = status else {
        return (None, None);
    };
    let branch = status.lines().next().and_then(|line| {
        line.strip_prefix("## ")
            .map(|line| {
                line.split(['.', ' '])
                    .next()
                    .unwrap_or_default()
                    .to_string()
            })
            .filter(|value| !value.is_empty())
    });
    let project_root = find_git_root().ok();
    (project_root, branch)
}

fn find_git_root() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(env::current_dir()?)
        .output()?;
    if !output.status.success() {
        return Err("not a git repository".into());
    }
    let path = String::from_utf8(output.stdout)?.trim().to_string();
    if path.is_empty() {
        return Err("empty git root".into());
    }
    Ok(PathBuf::from(path))
}

#[allow(clippy::too_many_lines)]
fn run_resume_command(
    session_path: &Path,
    session: &Session,
    command: &SlashCommand,
) -> Result<ResumeCommandOutcome, Box<dyn std::error::Error>> {
    match command {
        SlashCommand::Help => Ok(ResumeCommandOutcome {
            session: session.clone(),
            message: Some(render_repl_help()),
        }),
        SlashCommand::Compact => {
            let result = runtime::compact_session(
                session,
                CompactionConfig {
                    max_estimated_tokens: 0,
                    ..CompactionConfig::default()
                },
            );
            let message = format_compact_report(&result);
            result.compacted_session.save_to_path(session_path)?;
            Ok(ResumeCommandOutcome {
                session: result.compacted_session,
                message: Some(message),
            })
        }
        SlashCommand::Clear { confirm } => {
            if !confirm {
                return Ok(ResumeCommandOutcome {
                    session: session.clone(),
                    message: Some(
                        "clear: confirmation required; rerun with /clear --confirm".to_string(),
                    ),
                });
            }
            let cleared = Session::new();
            cleared.save_to_path(session_path)?;
            Ok(ResumeCommandOutcome {
                session: cleared,
                message: Some(format!(
                    "Cleared resumed session file {}.",
                    session_path.display()
                )),
            })
        }
        SlashCommand::Status => {
            let tracker = UsageTracker::from_session(session);
            let usage = tracker.cumulative_usage();
            Ok(ResumeCommandOutcome {
                session: session.clone(),
                message: Some(format_status_report(
                    "restored-session",
                    StatusUsage {
                        message_count: session.messages.len(),
                        turns: tracker.turns(),
                        latest: tracker.current_turn_usage(),
                        cumulative: usage,
                        estimated_tokens: 0,
                    },
                    default_permission_mode().as_str(),
                    None,
                    &status_context_for_mode(Some(session_path), true)?,
                )),
            })
        }
        SlashCommand::Cost => {
            let tracker = UsageTracker::from_session(session);
            Ok(ResumeCommandOutcome {
                session: session.clone(),
                message: Some(render_cost_report(None, &tracker)),
            })
        }
        SlashCommand::Config { section } => Ok(ResumeCommandOutcome {
            session: session.clone(),
            message: Some(render_config_report(section.as_deref())?),
        }),
        SlashCommand::Memory => Ok(ResumeCommandOutcome {
            session: session.clone(),
            message: Some(render_memory_report()?),
        }),
        SlashCommand::Init => Ok(ResumeCommandOutcome {
            session: session.clone(),
            message: Some(init_openyak_md()?),
        }),
        SlashCommand::Diff => Ok(ResumeCommandOutcome {
            session: session.clone(),
            message: Some(render_diff_report()?),
        }),
        SlashCommand::Version => Ok(ResumeCommandOutcome {
            session: session.clone(),
            message: Some(render_version_report()),
        }),
        SlashCommand::Export { path } => {
            let export_path = resolve_export_path(path.as_deref(), session)?;
            fs::write(&export_path, render_export_text(session))?;
            Ok(ResumeCommandOutcome {
                session: session.clone(),
                message: Some(format!(
                    "Export\n  Result           wrote transcript\n  File             {}\n  Messages         {}",
                    export_path.display(),
                    session.messages.len(),
                )),
            })
        }
        SlashCommand::Agents { args } => {
            let cwd = env::current_dir()?;
            Ok(ResumeCommandOutcome {
                session: session.clone(),
                message: Some(handle_agents_slash_command(args.as_deref(), &cwd)?),
            })
        }
        SlashCommand::Skills { args } => {
            let cwd = env::current_dir()?;
            Ok(ResumeCommandOutcome {
                session: session.clone(),
                message: Some(handle_skills_slash_command(args.as_deref(), &cwd)?),
            })
        }
        SlashCommand::Bughunter { .. }
        | SlashCommand::Branch { .. }
        | SlashCommand::Worktree { .. }
        | SlashCommand::CommitPushPr { .. }
        | SlashCommand::Commit
        | SlashCommand::Pr { .. }
        | SlashCommand::Issue { .. }
        | SlashCommand::Ultraplan { .. }
        | SlashCommand::Teleport { .. }
        | SlashCommand::DebugToolCall
        | SlashCommand::Resume { .. }
        | SlashCommand::Model { .. }
        | SlashCommand::Permissions { .. }
        | SlashCommand::Plan { .. }
        | SlashCommand::Session { .. }
        | SlashCommand::Plugins { .. }
        | SlashCommand::Unknown(_) => Err("unsupported resumed slash command".into()),
    }
}

fn run_repl(
    model: String,
    allowed_tools: Option<AllowedToolSet>,
    permission_mode: PermissionMode,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut cli = LiveCli::new(model, true, allowed_tools, permission_mode)?;
    let mut editor = input::LineEditor::new("> ", slash_command_completion_candidates());
    println!("{}", cli.startup_banner());

    loop {
        match editor.read_line()? {
            input::ReadOutcome::Submit(input) => {
                let trimmed = input.trim();
                if trimmed.is_empty() {
                    continue;
                }
                if matches!(trimmed, "/exit" | "/quit") {
                    cli.persist_session()?;
                    break;
                }
                if let Some(command) = SlashCommand::parse(trimmed) {
                    if cli.handle_repl_command(command)? {
                        cli.persist_session()?;
                    }
                    continue;
                }
                editor.push_history(&input);
                cli.run_turn(&input)?;
            }
            input::ReadOutcome::Cancel => {}
            input::ReadOutcome::Exit => {
                cli.persist_session()?;
                break;
            }
        }
    }

    Ok(())
}

#[derive(Debug, Clone)]
struct SessionHandle {
    id: String,
    path: PathBuf,
}

#[derive(Debug, Clone)]
struct ManagedSessionSummary {
    id: String,
    path: PathBuf,
    modified_epoch_secs: u64,
    message_count: usize,
}

struct LiveCli {
    model: String,
    allowed_tools: Option<AllowedToolSet>,
    permission_mode: PermissionMode,
    plan_restore_mode: Option<PermissionMode>,
    system_prompt: Vec<String>,
    runtime: ConversationRuntime<DefaultRuntimeClient, CliToolExecutor>,
    session: SessionHandle,
}

impl LiveCli {
    fn new(
        model: String,
        enable_tools: bool,
        allowed_tools: Option<AllowedToolSet>,
        permission_mode: PermissionMode,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let system_prompt = build_system_prompt(&model)?;
        let session = create_managed_session_handle()?;
        let runtime = build_runtime(
            Session::new(),
            model.clone(),
            system_prompt.clone(),
            enable_tools,
            true,
            allowed_tools.clone(),
            permission_mode,
            None,
        )?;
        let cli = Self {
            model,
            allowed_tools,
            permission_mode,
            plan_restore_mode: None,
            system_prompt,
            runtime,
            session,
        };
        cli.persist_session()?;
        Ok(cli)
    }

    fn startup_banner(&self) -> String {
        let color = io::stdout().is_terminal();
        let cwd = env::current_dir().ok();
        let cwd_display = cwd.as_ref().map_or_else(
            || "<unknown>".to_string(),
            |path| path.display().to_string(),
        );
        let workspace_name = cwd
            .as_ref()
            .and_then(|path| path.file_name())
            .and_then(|name| name.to_str())
            .unwrap_or("workspace");
        let git_branch = status_context(Some(&self.session.path))
            .ok()
            .and_then(|context| context.git_branch);
        let workspace_summary = git_branch.as_deref().map_or_else(
            || workspace_name.to_string(),
            |branch| format!("{workspace_name} · {branch}"),
        );
        let has_openyak_md = cwd
            .as_ref()
            .is_some_and(|path| path.join("OPENYAK.md").is_file());
        let mut lines = vec![
            format!(
                "{} {}",
                if color {
                    "\x1b[1;38;5;45m🦞 openyak\x1b[0m"
                } else {
                    "openyak"
                },
                if color {
                    "\x1b[2m· ready\x1b[0m"
                } else {
                    "· ready"
                }
            ),
            format!("  Workspace        {workspace_summary}"),
            format!("  Directory        {cwd_display}"),
            format!("  Model            {}", self.model),
            format!("  Permissions      {}", self.permission_mode.as_str()),
            format!("  Session          {}", self.session.id),
            format!(
                "  Quick start      {}",
                if has_openyak_md {
                    "/help · /status · ask for a task"
                } else {
                    "/init · /help · /status"
                }
            ),
            "  Editor           Tab completes slash commands · /vim toggles modal editing"
                .to_string(),
            "  Multiline        Shift+Enter or Ctrl+J inserts a newline".to_string(),
        ];
        if !has_openyak_md {
            lines.push(
                "  First run        /init scaffolds OPENYAK.md, .openyak.json, and local session files"
                    .to_string(),
            );
        }
        lines.join("\n")
    }

    fn run_turn(&mut self, input: &str) -> Result<(), Box<dyn std::error::Error>> {
        if self.resolve_pending_user_input()?
            && self
                .runtime
                .session()
                .pending_user_input_request()
                .is_some()
        {
            println!("Pending request remains unresolved; no new turn was started.");
            self.persist_session()?;
            return Ok(());
        }

        let mut spinner = Spinner::new();
        let mut stdout = io::stdout();
        spinner.tick(
            "🦀 Thinking...",
            TerminalRenderer::new().color_theme(),
            &mut stdout,
        )?;
        let mut permission_prompter = CliPermissionPrompter::new(self.permission_mode);
        let mut user_input_prompter = CliUserInputPrompter::interactive();
        let result = self.runtime.run_turn(
            input,
            Some(&mut permission_prompter),
            Some(&mut user_input_prompter),
        );
        match result {
            Ok(_) => {
                spinner.finish(
                    "✨ Done",
                    TerminalRenderer::new().color_theme(),
                    &mut stdout,
                )?;
                println!();
                self.persist_session()?;
                Ok(())
            }
            Err(error) => {
                self.persist_session()?;
                spinner.fail(
                    "❌ Request failed",
                    TerminalRenderer::new().color_theme(),
                    &mut stdout,
                )?;
                Err(Box::new(error))
            }
        }
    }

    fn run_turn_with_output(
        &mut self,
        input: &str,
        output_format: CliOutputFormat,
    ) -> Result<(), Box<dyn std::error::Error>> {
        match output_format {
            CliOutputFormat::Text => self.run_turn(input),
            CliOutputFormat::Json => self.run_prompt_json(input),
        }
    }

    fn run_prompt_json(&mut self, input: &str) -> Result<(), Box<dyn std::error::Error>> {
        let session = self.runtime.session().clone();
        let mut runtime = build_runtime(
            session,
            self.model.clone(),
            self.system_prompt.clone(),
            true,
            false,
            self.allowed_tools.clone(),
            self.permission_mode,
            None,
        )?;
        let mut permission_prompter = CliPermissionPrompter::new(self.permission_mode);
        let mut user_input_prompter = CliUserInputPrompter::unavailable();
        match runtime.run_turn(
            input,
            Some(&mut permission_prompter),
            Some(&mut user_input_prompter),
        ) {
            Ok(summary) => {
                self.runtime = runtime;
                self.persist_session()?;
                println!(
                    "{}",
                    json!({
                        "message": final_assistant_text(&summary),
                        "model": self.model,
                        "iterations": summary.iterations,
                        "tool_uses": collect_tool_uses(&summary),
                        "tool_results": collect_tool_results(&summary),
                        "usage": {
                            "input_tokens": summary.usage.input_tokens,
                            "output_tokens": summary.usage.output_tokens,
                            "cache_creation_input_tokens": summary.usage.cache_creation_input_tokens,
                            "cache_read_input_tokens": summary.usage.cache_read_input_tokens,
                        }
                    })
                );
                Ok(())
            }
            Err(error) => {
                self.runtime = runtime;
                self.persist_session()?;
                Err(Box::new(error))
            }
        }
    }

    #[allow(clippy::too_many_lines)]
    fn handle_repl_command(
        &mut self,
        command: SlashCommand,
    ) -> Result<bool, Box<dyn std::error::Error>> {
        Ok(match command {
            SlashCommand::Help => {
                println!("{}", render_repl_help());
                false
            }
            SlashCommand::Status => {
                self.print_status();
                false
            }
            SlashCommand::Bughunter { scope } => {
                self.run_bughunter(scope.as_deref())?;
                false
            }
            SlashCommand::Commit => {
                self.run_commit()?;
                true
            }
            SlashCommand::Pr { context } => {
                self.run_pr(context.as_deref())?;
                false
            }
            SlashCommand::Issue { context } => {
                self.run_issue(context.as_deref())?;
                false
            }
            SlashCommand::Ultraplan { task } => {
                self.run_ultraplan(task.as_deref())?;
                false
            }
            SlashCommand::Teleport { target } => {
                self.run_teleport(target.as_deref())?;
                false
            }
            SlashCommand::DebugToolCall => {
                self.run_debug_tool_call();
                false
            }
            SlashCommand::Compact => {
                self.compact()?;
                false
            }
            SlashCommand::Model { model } => self.set_model(model)?,
            SlashCommand::Permissions { mode } => self.set_permissions(mode)?,
            SlashCommand::Plan { action } => self.handle_plan_command(action.as_deref())?,
            SlashCommand::Clear { confirm } => self.clear_session(confirm)?,
            SlashCommand::Cost => {
                self.print_cost();
                false
            }
            SlashCommand::Resume { session_path } => self.resume_session(session_path)?,
            SlashCommand::Config { section } => {
                Self::print_config(section.as_deref())?;
                false
            }
            SlashCommand::Memory => {
                Self::print_memory()?;
                false
            }
            SlashCommand::Init => {
                run_init()?;
                false
            }
            SlashCommand::Diff => {
                Self::print_diff()?;
                false
            }
            SlashCommand::Version => {
                Self::print_version();
                false
            }
            SlashCommand::Export { path } => {
                self.export_session(path.as_deref())?;
                false
            }
            SlashCommand::Session { action, target } => {
                self.handle_session_command(action.as_deref(), target.as_deref())?
            }
            SlashCommand::Plugins { action, target } => {
                self.handle_plugins_command(action.as_deref(), target.as_deref())?
            }
            SlashCommand::Agents { args } => {
                Self::print_agents(args.as_deref())?;
                false
            }
            SlashCommand::Skills { args } => {
                Self::print_skills(args.as_deref())?;
                false
            }
            SlashCommand::Branch { .. } => {
                Self::run_branch(command)?;
                false
            }
            SlashCommand::Worktree { .. } => {
                Self::run_worktree(command)?;
                false
            }
            SlashCommand::CommitPushPr { context } => {
                self.run_commit_push_pr(context.as_deref())?;
                false
            }
            SlashCommand::Unknown(name) => {
                eprintln!("{}", render_unknown_repl_command(&name));
                false
            }
        })
    }

    fn persist_session(&self) -> Result<(), Box<dyn std::error::Error>> {
        self.runtime.session().save_to_path(&self.session.path)?;
        Ok(())
    }

    fn resolve_pending_user_input(&mut self) -> Result<bool, Box<dyn std::error::Error>> {
        let Some(request) = self.runtime.session().pending_user_input_request() else {
            return Ok(false);
        };

        println!("{}", format_pending_user_input_report(&request));
        let mut permission_prompter = CliPermissionPrompter::new(self.permission_mode);
        let mut user_input_prompter = CliUserInputPrompter::interactive();
        match self.runtime.resume_pending_turn(
            Some(&mut permission_prompter),
            Some(&mut user_input_prompter),
        ) {
            Ok(_) => {
                self.persist_session()?;
                Ok(true)
            }
            Err(error) => {
                self.persist_session()?;
                if error.pending_user_input_request().is_some() {
                    println!("{error}");
                    Ok(true)
                } else {
                    Err(Box::new(error))
                }
            }
        }
    }

    fn print_status(&self) {
        let cumulative = self.runtime.usage().cumulative_usage();
        let latest = self.runtime.usage().current_turn_usage();
        let (context, warning) = status_context_or_fallback(Some(&self.session.path), false);
        if let Some(warning) = warning {
            eprintln!("warning: failed to load full status context: {warning}");
        }
        println!(
            "{}",
            format_status_report(
                &self.model,
                StatusUsage {
                    message_count: self.runtime.session().messages.len(),
                    turns: self.runtime.usage().turns(),
                    latest,
                    cumulative,
                    estimated_tokens: self.runtime.estimated_tokens(),
                },
                self.permission_mode.as_str(),
                self.plan_restore_mode.map(PermissionMode::as_str),
                &context,
            )
        );
    }

    fn set_model(&mut self, model: Option<String>) -> Result<bool, Box<dyn std::error::Error>> {
        let Some(model) = model else {
            println!(
                "{}",
                format_model_report(
                    &self.model,
                    self.runtime.session().messages.len(),
                    self.runtime.usage().turns(),
                )
            );
            return Ok(false);
        };

        let model = resolve_model_alias(&model).to_string();

        if model == self.model {
            println!(
                "{}",
                format_model_report(
                    &self.model,
                    self.runtime.session().messages.len(),
                    self.runtime.usage().turns(),
                )
            );
            return Ok(false);
        }

        let previous = self.model.clone();
        let session = self.runtime.session().clone();
        let message_count = session.messages.len();
        self.runtime = build_runtime(
            session,
            model.clone(),
            self.system_prompt.clone(),
            true,
            true,
            self.allowed_tools.clone(),
            self.permission_mode,
            None,
        )?;
        self.model.clone_from(&model);
        println!(
            "{}",
            format_model_switch_report(&previous, &model, message_count)
        );
        Ok(true)
    }

    fn set_permissions(
        &mut self,
        mode: Option<String>,
    ) -> Result<bool, Box<dyn std::error::Error>> {
        let Some(mode) = mode else {
            println!(
                "{}",
                format_permissions_report(
                    self.permission_mode.as_str(),
                    self.plan_restore_mode.map(PermissionMode::as_str),
                )
            );
            return Ok(false);
        };

        let normalized = normalize_permission_mode(&mode).ok_or_else(|| {
            format!(
                "unsupported permission mode '{mode}'. Use read-only, workspace-write, or danger-full-access."
            )
        })?;

        if let Some(restore_mode) = self.plan_restore_mode {
            println!(
                "{}",
                format_plan_permissions_blocked_report(
                    self.permission_mode.as_str(),
                    restore_mode.as_str(),
                )
            );
            return Ok(false);
        }

        if normalized == self.permission_mode.as_str() {
            println!("{}", format_permissions_report(normalized, None));
            return Ok(false);
        }

        let previous = self.permission_mode.as_str().to_string();
        self.rebuild_runtime_for_permission_mode(permission_mode_from_label(normalized))?;
        println!(
            "{}",
            format_permissions_switch_report(&previous, normalized)
        );
        Ok(true)
    }

    fn handle_plan_command(
        &mut self,
        action: Option<&str>,
    ) -> Result<bool, Box<dyn std::error::Error>> {
        match action {
            None => self.enter_plan_mode(),
            Some("exit") => self.exit_plan_mode(),
            Some(other) => {
                println!("Unknown /plan action '{other}'. Use /plan or /plan exit.");
                Ok(false)
            }
        }
    }

    fn enter_plan_mode(&mut self) -> Result<bool, Box<dyn std::error::Error>> {
        if let Some(restore_mode) = self.plan_restore_mode {
            println!(
                "{}",
                format_plan_mode_already_active_report(restore_mode.as_str())
            );
            return Ok(false);
        }

        let restore_mode = self.permission_mode;
        if self.permission_mode != PermissionMode::ReadOnly {
            self.rebuild_runtime_for_permission_mode(PermissionMode::ReadOnly)?;
        }
        self.plan_restore_mode = Some(restore_mode);
        println!("{}", format_plan_mode_enabled_report(restore_mode.as_str()));
        Ok(true)
    }

    fn exit_plan_mode(&mut self) -> Result<bool, Box<dyn std::error::Error>> {
        let Some(restore_mode) = self.plan_restore_mode else {
            println!(
                "{}",
                format_plan_mode_not_active_report(self.permission_mode.as_str())
            );
            return Ok(false);
        };

        self.plan_restore_mode = None;
        if self.permission_mode != restore_mode {
            self.rebuild_runtime_for_permission_mode(restore_mode)?;
        }
        println!(
            "{}",
            format_plan_mode_disabled_report(restore_mode.as_str())
        );
        Ok(true)
    }

    fn rebuild_runtime_for_permission_mode(
        &mut self,
        permission_mode: PermissionMode,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let session = self.runtime.session().clone();
        self.permission_mode = permission_mode;
        self.runtime = build_runtime(
            session,
            self.model.clone(),
            self.system_prompt.clone(),
            true,
            true,
            self.allowed_tools.clone(),
            self.permission_mode,
            None,
        )?;
        Ok(())
    }

    fn clear_session(&mut self, confirm: bool) -> Result<bool, Box<dyn std::error::Error>> {
        if !confirm {
            println!(
                "clear: confirmation required; run /clear --confirm to start a fresh session."
            );
            return Ok(false);
        }

        self.session = create_managed_session_handle()?;
        self.runtime = build_runtime(
            Session::new(),
            self.model.clone(),
            self.system_prompt.clone(),
            true,
            true,
            self.allowed_tools.clone(),
            self.permission_mode,
            None,
        )?;
        println!(
            "Session cleared\n  Mode             fresh session\n  Preserved model  {}\n  Permission mode  {}\n  Session          {}",
            self.model,
            self.permission_mode.as_str(),
            self.session.id,
        );
        Ok(true)
    }

    fn print_cost(&self) {
        println!(
            "{}",
            render_cost_report(Some(&self.model), self.runtime.usage())
        );
    }

    fn resume_session(
        &mut self,
        session_path: Option<String>,
    ) -> Result<bool, Box<dyn std::error::Error>> {
        let Some(session_ref) = session_path else {
            println!("Usage: /resume <session-path>");
            return Ok(false);
        };

        let handle = resolve_session_reference(&session_ref)?;
        let session = Session::load_from_path(&handle.path)?;
        let message_count = session.messages.len();
        self.runtime = build_runtime(
            session,
            self.model.clone(),
            self.system_prompt.clone(),
            true,
            true,
            self.allowed_tools.clone(),
            self.permission_mode,
            None,
        )?;
        self.session = handle;
        println!(
            "{}",
            format_resume_report(
                &self.session.path.display().to_string(),
                message_count,
                self.runtime.usage().turns(),
            )
        );
        let _ = self.resolve_pending_user_input()?;
        Ok(true)
    }

    fn print_config(section: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
        println!("{}", render_config_report(section)?);
        Ok(())
    }

    fn print_memory() -> Result<(), Box<dyn std::error::Error>> {
        println!("{}", render_memory_report()?);
        Ok(())
    }

    fn print_agents(args: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
        let cwd = env::current_dir()?;
        println!("{}", handle_agents_slash_command(args, &cwd)?);
        Ok(())
    }

    fn print_skills(args: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
        let cwd = env::current_dir()?;
        println!("{}", handle_skills_slash_command(args, &cwd)?);
        Ok(())
    }

    fn print_diff() -> Result<(), Box<dyn std::error::Error>> {
        println!("{}", render_diff_report()?);
        Ok(())
    }

    fn print_version() {
        println!("{}", render_version_report());
    }

    fn export_session(
        &self,
        requested_path: Option<&str>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let export_path = resolve_export_path(requested_path, self.runtime.session())?;
        fs::write(&export_path, render_export_text(self.runtime.session()))?;
        println!(
            "Export\n  Result           wrote transcript\n  File             {}\n  Messages         {}",
            export_path.display(),
            self.runtime.session().messages.len(),
        );
        Ok(())
    }

    fn handle_session_command(
        &mut self,
        action: Option<&str>,
        target: Option<&str>,
    ) -> Result<bool, Box<dyn std::error::Error>> {
        match action {
            None | Some("list") => {
                println!("{}", render_session_list(&self.session.id)?);
                Ok(false)
            }
            Some("switch") => {
                let Some(target) = target else {
                    println!("Usage: /session switch <session-id>");
                    return Ok(false);
                };
                let handle = resolve_session_reference(target)?;
                let session = Session::load_from_path(&handle.path)?;
                let message_count = session.messages.len();
                self.runtime = build_runtime(
                    session,
                    self.model.clone(),
                    self.system_prompt.clone(),
                    true,
                    true,
                    self.allowed_tools.clone(),
                    self.permission_mode,
                    None,
                )?;
                self.session = handle;
                println!(
                    "Session switched\n  Active session   {}\n  File             {}\n  Messages         {}",
                    self.session.id,
                    self.session.path.display(),
                    message_count,
                );
                let _ = self.resolve_pending_user_input()?;
                Ok(true)
            }
            Some(other) => {
                println!("Unknown /session action '{other}'. Use /session list or /session switch <session-id>.");
                Ok(false)
            }
        }
    }

    fn handle_plugins_command(
        &mut self,
        action: Option<&str>,
        target: Option<&str>,
    ) -> Result<bool, Box<dyn std::error::Error>> {
        let cwd = env::current_dir()?;
        let loader = ConfigLoader::default_for(&cwd);
        let runtime_config = loader.load()?;
        let mut manager = build_plugin_manager(&cwd, &loader, &runtime_config);
        let result = handle_plugins_slash_command(action, target, &mut manager)?;
        println!("{}", result.message);
        if result.reload_runtime {
            self.reload_runtime_features()?;
        }
        Ok(false)
    }

    fn reload_runtime_features(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        self.runtime = build_runtime(
            self.runtime.session().clone(),
            self.model.clone(),
            self.system_prompt.clone(),
            true,
            true,
            self.allowed_tools.clone(),
            self.permission_mode,
            None,
        )?;
        self.persist_session()
    }

    fn compact(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let result = self.runtime.compact(CompactionConfig::default());
        let message = format_compact_report(&result);
        self.runtime = build_runtime(
            result.compacted_session,
            self.model.clone(),
            self.system_prompt.clone(),
            true,
            true,
            self.allowed_tools.clone(),
            self.permission_mode,
            None,
        )?;
        self.persist_session()?;
        println!("{message}");
        Ok(())
    }

    fn run_internal_prompt_text_with_progress(
        &self,
        prompt: &str,
        enable_tools: bool,
        progress: Option<InternalPromptProgressReporter>,
    ) -> Result<String, Box<dyn std::error::Error>> {
        let session = self.runtime.session().clone();
        let mut runtime = build_runtime(
            session,
            self.model.clone(),
            self.system_prompt.clone(),
            enable_tools,
            false,
            self.allowed_tools.clone(),
            self.permission_mode,
            progress,
        )?;
        let mut permission_prompter = CliPermissionPrompter::new(self.permission_mode);
        let mut user_input_prompter = CliUserInputPrompter::unavailable();
        let summary = runtime.run_turn(
            prompt,
            Some(&mut permission_prompter),
            Some(&mut user_input_prompter),
        )?;
        Ok(final_assistant_text(&summary).trim().to_string())
    }

    fn run_internal_prompt_text(
        &self,
        prompt: &str,
        enable_tools: bool,
    ) -> Result<String, Box<dyn std::error::Error>> {
        self.run_internal_prompt_text_with_progress(prompt, enable_tools, None)
    }

    fn run_bughunter(&self, scope: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
        let scope = scope.unwrap_or("the current repository");
        let prompt = format!(
            "You are /bughunter. Inspect {scope} and identify the most likely bugs or correctness issues. Prioritize concrete findings with file paths, severity, and suggested fixes. Use tools if needed."
        );
        println!("{}", self.run_internal_prompt_text(&prompt, true)?);
        Ok(())
    }

    fn run_ultraplan(&self, task: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
        let task = task.unwrap_or("the current repo work");
        let prompt = format!(
            "You are /ultraplan. Produce a deep multi-step execution plan for {task}. Include goals, risks, implementation sequence, verification steps, and rollback considerations. Use tools if needed."
        );
        let mut progress = InternalPromptProgressRun::start_ultraplan(task);
        match self.run_internal_prompt_text_with_progress(&prompt, true, Some(progress.reporter()))
        {
            Ok(plan) => {
                progress.finish_success();
                println!("{plan}");
                Ok(())
            }
            Err(error) => {
                progress.finish_failure(&error.to_string());
                Err(error)
            }
        }
    }

    #[allow(clippy::unused_self)]
    fn run_teleport(&self, target: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
        let Some(target) = target.map(str::trim).filter(|value| !value.is_empty()) else {
            println!("Usage: /teleport <symbol-or-path>");
            return Ok(());
        };

        println!("{}", render_teleport_report(target)?);
        Ok(())
    }

    fn run_debug_tool_call(&self) {
        println!("{}", render_last_tool_debug_report(self.runtime.session()));
    }

    fn run_commit(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        if find_git_root().is_err() {
            println!(
                "{}",
                render_git_command_requires_repo("commit", "git commit automation")
            );
            return Ok(());
        }

        let status = git_output_filtered(&["status", "--short"])?;
        if status.trim().is_empty() {
            println!("Commit\n  Result           skipped\n  Reason           no workspace changes");
            return Ok(());
        }

        let staged_stat = git_output_filtered(&["diff", "--cached", "--stat"])?;
        let prompt = format!(
            "Generate a git commit message in plain text Lore format only. Base it on this staged diff summary:\n\n{}\n\nRecent conversation context:\n{}",
            truncate_for_prompt(&staged_stat, 8_000),
            recent_user_context(self.runtime.session(), 6)
        );
        let message = sanitize_generated_message(&self.run_internal_prompt_text(&prompt, false)?);
        if message.trim().is_empty() {
            return Err("generated commit message was empty".into());
        }

        let cwd = env::current_dir()?;
        println!("{}", handle_commit_slash_command(&message, &cwd)?);
        Ok(())
    }

    fn run_branch(command: SlashCommand) -> Result<(), Box<dyn std::error::Error>> {
        let SlashCommand::Branch { action, target } = command else {
            return Err("expected /branch command".into());
        };
        if find_git_root().is_err() {
            println!(
                "{}",
                render_git_command_requires_repo("branch", "git branch commands")
            );
            return Ok(());
        }

        let cwd = env::current_dir()?;
        println!(
            "{}",
            handle_branch_slash_command(action.as_deref(), target.as_deref(), &cwd)?
        );
        Ok(())
    }

    fn run_worktree(command: SlashCommand) -> Result<(), Box<dyn std::error::Error>> {
        let SlashCommand::Worktree {
            action,
            path,
            branch,
        } = command
        else {
            return Err("expected /worktree command".into());
        };
        if find_git_root().is_err() {
            println!(
                "{}",
                render_git_command_requires_repo("worktree", "git worktree commands")
            );
            return Ok(());
        }

        let cwd = env::current_dir()?;
        println!(
            "{}",
            handle_worktree_slash_command(
                action.as_deref(),
                path.as_deref(),
                branch.as_deref(),
                &cwd,
            )?
        );
        Ok(())
    }

    fn run_commit_push_pr(&self, context: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
        if find_git_root().is_err() {
            println!(
                "{}",
                render_git_command_requires_repo("commit-push-pr", "commit + push + PR automation",)
            );
            return Ok(());
        }

        let cwd = env::current_dir()?;
        let workspace_status = git_output_filtered(&["status", "--short"])?;
        let default_branch = detect_default_branch(&cwd)?;
        let branch_diff = git_output(&["diff", "--stat", &format!("{default_branch}...HEAD")])?;

        if workspace_status.trim().is_empty() && branch_diff.trim().is_empty() {
            println!(
                "Commit/Push/PR\n  Result           skipped\n  Reason           no workspace or branch changes"
            );
            return Ok(());
        }

        if !command_exists("gh") {
            return Err("gh CLI is required for /commit-push-pr".into());
        }

        let workspace_has_changes = !workspace_status.trim().is_empty();
        let diff_summary = git_output_filtered(&["diff", "--stat"])?;
        let prompt = format!(
            "Generate a git commit message plus a GitHub pull request title/body. Return plain text exactly in this format:\nCOMMIT: <one-line commit message or NONE if no commit is needed>\nTITLE: <title>\nBODY:\n<body markdown>\n\nContext hint: {}\n\nWorkspace status:\n{}\n\nWorkspace diff summary:\n{}\n\nCurrent branch diff vs {}:\n{}\n\nRecent conversation context:\n{}",
            context.unwrap_or("none"),
            truncate_for_prompt(&workspace_status, 4_000),
            truncate_for_prompt(&diff_summary, 8_000),
            default_branch,
            truncate_for_prompt(&branch_diff, 8_000),
            recent_user_context(self.runtime.session(), 10)
        );
        let draft = sanitize_generated_message(&self.run_internal_prompt_text(&prompt, false)?);
        let (commit_message, pr_title, pr_body) = parse_commit_push_pr_draft(&draft)
            .ok_or_else(|| "failed to parse generated commit/push/PR response".to_string())?;
        if workspace_has_changes && commit_message.is_none() {
            return Err(
                "generated /commit-push-pr response omitted the required commit message".into(),
            );
        }

        let request = CommitPushPrRequest {
            commit_message,
            pr_title,
            pr_body,
            branch_name_hint: context.unwrap_or_default().to_string(),
        };
        println!("{}", handle_commit_push_pr_slash_command(&request, &cwd)?);
        Ok(())
    }

    fn run_pr(&self, context: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
        if find_git_root().is_err() {
            println!(
                "{}",
                render_git_command_requires_repo("pr", "pull request drafting")
            );
            return Ok(());
        }

        let staged = git_output_filtered(&["diff", "--stat"])?;
        let prompt = format!(
            "Generate a pull request title and body from this conversation and diff summary. Output plain text in this format exactly:\nTITLE: <title>\nBODY:\n<body markdown>\n\nContext hint: {}\n\nDiff summary:\n{}",
            context.unwrap_or("none"),
            truncate_for_prompt(&staged, 10_000)
        );
        let draft = sanitize_generated_message(&self.run_internal_prompt_text(&prompt, false)?);
        let (title, body) = parse_titled_body(&draft)
            .ok_or_else(|| "failed to parse generated PR title/body".to_string())?;

        if let Some(gh_command) = resolve_command_path("gh") {
            let body_path = write_temp_text_file("openyak-pr-body.md", &body)?;
            let output = Command::new(gh_command)
                .args(["pr", "create", "--title", &title, "--body-file"])
                .arg(&body_path)
                .current_dir(env::current_dir()?)
                .output()?;
            if output.status.success() {
                let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
                println!(
                    "PR\n  Result           created\n  Title            {title}\n  URL              {}",
                    if stdout.is_empty() { "<unknown>" } else { &stdout }
                );
                return Ok(());
            }
        }

        println!("PR draft\n  Title            {title}\n\n{body}");
        Ok(())
    }

    fn run_issue(&self, context: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
        let prompt = format!(
            "Generate a GitHub issue title and body from this conversation. Output plain text in this format exactly:\nTITLE: <title>\nBODY:\n<body markdown>\n\nContext hint: {}\n\nConversation context:\n{}",
            context.unwrap_or("none"),
            truncate_for_prompt(&recent_user_context(self.runtime.session(), 10), 10_000)
        );
        let draft = sanitize_generated_message(&self.run_internal_prompt_text(&prompt, false)?);
        let (title, body) = parse_titled_body(&draft)
            .ok_or_else(|| "failed to parse generated issue title/body".to_string())?;

        if let Some(gh_command) = resolve_command_path("gh") {
            let body_path = write_temp_text_file("openyak-issue-body.md", &body)?;
            let output = Command::new(gh_command)
                .args(["issue", "create", "--title", &title, "--body-file"])
                .arg(&body_path)
                .current_dir(env::current_dir()?)
                .output()?;
            if output.status.success() {
                let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
                println!(
                    "Issue\n  Result           created\n  Title            {title}\n  URL              {}",
                    if stdout.is_empty() { "<unknown>" } else { &stdout }
                );
                return Ok(());
            }
        }

        println!("Issue draft\n  Title            {title}\n\n{body}");
        Ok(())
    }
}

fn sessions_dir() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let cwd = env::current_dir()?;
    let path = cwd.join(".openyak").join("sessions");
    fs::create_dir_all(&path)?;
    Ok(path)
}

fn create_managed_session_handle() -> Result<SessionHandle, Box<dyn std::error::Error>> {
    let id = generate_session_id();
    let path = sessions_dir()?.join(format!("{id}.json"));
    Ok(SessionHandle { id, path })
}

fn generate_session_id() -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default();
    format!("session-{millis}")
}

fn resolve_session_reference(reference: &str) -> Result<SessionHandle, Box<dyn std::error::Error>> {
    let direct = PathBuf::from(reference);
    let path = if direct.exists() {
        direct
    } else {
        sessions_dir()?.join(format!("{reference}.json"))
    };
    if !path.exists() {
        return Err(format!("session not found: {reference}").into());
    }
    let id = path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or(reference)
        .to_string();
    Ok(SessionHandle { id, path })
}

fn list_managed_sessions() -> Result<Vec<ManagedSessionSummary>, Box<dyn std::error::Error>> {
    let mut sessions = Vec::new();
    for entry in fs::read_dir(sessions_dir()?)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let metadata = entry.metadata()?;
        let modified_epoch_secs = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map(|duration| duration.as_secs())
            .unwrap_or_default();
        let message_count = Session::load_from_path(&path)
            .map(|session| session.messages.len())
            .unwrap_or_default();
        let id = path
            .file_stem()
            .and_then(|value| value.to_str())
            .unwrap_or("unknown")
            .to_string();
        sessions.push(ManagedSessionSummary {
            id,
            path,
            modified_epoch_secs,
            message_count,
        });
    }
    sessions.sort_by(|left, right| right.modified_epoch_secs.cmp(&left.modified_epoch_secs));
    Ok(sessions)
}

fn format_relative_timestamp(epoch_secs: u64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(epoch_secs);
    let elapsed = now.saturating_sub(epoch_secs);
    match elapsed {
        0..=59 => format!("{elapsed}s ago"),
        60..=3_599 => format!("{}m ago", elapsed / 60),
        3_600..=86_399 => format!("{}h ago", elapsed / 3_600),
        _ => format!("{}d ago", elapsed / 86_400),
    }
}

fn render_session_list(active_session_id: &str) -> Result<String, Box<dyn std::error::Error>> {
    let sessions = list_managed_sessions()?;
    let mut lines = vec![
        "Sessions".to_string(),
        format!("  Directory         {}", sessions_dir()?.display()),
    ];
    if sessions.is_empty() {
        lines.push("  No managed sessions saved yet.".to_string());
        return Ok(lines.join("\n"));
    }
    for session in sessions {
        let marker = if session.id == active_session_id {
            "● current"
        } else {
            "○ saved"
        };
        lines.push(format!(
            "  {id:<20} {marker:<10} {msgs:>3} msgs · updated {modified}",
            id = session.id,
            msgs = session.message_count,
            modified = format_relative_timestamp(session.modified_epoch_secs),
        ));
        lines.push(format!("    {}", session.path.display()));
    }
    Ok(lines.join("\n"))
}

fn render_repl_help() -> String {
    [
        "Interactive REPL".to_string(),
        "  Quick start          Ask a task in plain English or use one of the core commands below."
            .to_string(),
        "  Core commands        /help · /status · /model · /permissions · /plan · /compact"
            .to_string(),
        "  Exit                 /exit or /quit".to_string(),
        "  Vim mode             /vim toggles modal editing".to_string(),
        "  History              Up/Down recalls previous prompts".to_string(),
        "  Completion           Tab cycles slash command matches".to_string(),
        "  Cancel               Ctrl-C clears input (or exits on an empty prompt)".to_string(),
        "  Multiline            Shift+Enter or Ctrl+J inserts a newline".to_string(),
        "  Structured input     The assistant may pause to ask for a reply before the same turn resumes."
            .to_string(),
        String::new(),
        render_slash_command_help(),
    ]
    .join(
        "
",
    )
}

fn append_slash_command_suggestions(lines: &mut Vec<String>, name: &str) {
    let suggestions = suggest_slash_commands(name, 3);
    if suggestions.is_empty() {
        lines.push("  Try              /help shows the full slash command map".to_string());
        return;
    }

    lines.push("  Try              /help shows the full slash command map".to_string());
    lines.push("Suggestions".to_string());
    lines.extend(
        suggestions
            .into_iter()
            .map(|suggestion| format!("  {suggestion}")),
    );
}

fn render_unknown_repl_command(name: &str) -> String {
    let mut lines = vec![
        "Unknown slash command".to_string(),
        format!("  Command          /{name}"),
    ];
    append_repl_command_suggestions(&mut lines, name);
    lines.join("\n")
}

fn append_repl_command_suggestions(lines: &mut Vec<String>, name: &str) {
    let suggestions = suggest_repl_commands(name);
    if suggestions.is_empty() {
        lines.push("  Try              /help shows the full slash command map".to_string());
        return;
    }

    lines.push("  Try              /help shows the full slash command map".to_string());
    lines.push("Suggestions".to_string());
    lines.extend(
        suggestions
            .into_iter()
            .map(|suggestion| format!("  {suggestion}")),
    );
}

fn status_context(
    session_path: Option<&Path>,
) -> Result<StatusContext, Box<dyn std::error::Error>> {
    status_context_for_mode(session_path, false)
}

fn status_context_for_mode(
    session_path: Option<&Path>,
    resume_mode: bool,
) -> Result<StatusContext, Box<dyn std::error::Error>> {
    status_context_with_date(session_path, &current_local_date_string(), resume_mode)
}

fn status_context_with_date(
    session_path: Option<&Path>,
    current_date: &str,
    resume_mode: bool,
) -> Result<StatusContext, Box<dyn std::error::Error>> {
    let cwd = env::current_dir()?;
    status_context_for_cwd_with_date(&cwd, session_path, current_date, resume_mode)
}

fn status_context_for_cwd_with_date(
    cwd: &Path,
    session_path: Option<&Path>,
    current_date: &str,
    resume_mode: bool,
) -> Result<StatusContext, Box<dyn std::error::Error>> {
    let loader = ConfigLoader::default_for(cwd);
    let discovered_config_files = loader.discover().len();
    let runtime_config = loader.load()?;
    let project_context = ProjectContext::discover_with_git(cwd, current_date)?;
    let (project_root, git_branch) =
        parse_git_status_metadata(project_context.git_status.as_deref());
    Ok(StatusContext {
        cwd: cwd.to_path_buf(),
        session_path: session_path.map(Path::to_path_buf),
        loaded_config_files: runtime_config.loaded_entries().len(),
        discovered_config_files,
        memory_file_count: project_context.instruction_files.len(),
        project_root,
        git_branch,
        resume_mode,
    })
}

fn status_context_or_fallback(
    session_path: Option<&Path>,
    resume_mode: bool,
) -> (StatusContext, Option<String>) {
    let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    status_context_or_fallback_for_cwd(
        &cwd,
        session_path,
        &current_local_date_string(),
        resume_mode,
    )
}

fn status_context_or_fallback_for_cwd(
    cwd: &Path,
    session_path: Option<&Path>,
    current_date: &str,
    resume_mode: bool,
) -> (StatusContext, Option<String>) {
    match status_context_for_cwd_with_date(cwd, session_path, current_date, resume_mode) {
        Ok(context) => (context, None),
        Err(error) => {
            let discovered_config_files = ConfigLoader::default_for(cwd).discover().len();
            (
                StatusContext {
                    cwd: cwd.to_path_buf(),
                    session_path: session_path.map(Path::to_path_buf),
                    loaded_config_files: 0,
                    discovered_config_files,
                    memory_file_count: 0,
                    project_root: None,
                    git_branch: None,
                    resume_mode,
                },
                Some(error.to_string()),
            )
        }
    }
}

fn format_status_report(
    model: &str,
    usage: StatusUsage,
    permission_mode: &str,
    plan_restore_mode: Option<&str>,
    context: &StatusContext,
) -> String {
    let next_step = if context.resume_mode {
        "  /help            Browse commands\n  /export [file]   Write the restored transcript\n  /diff            Review current workspace changes"
    } else {
        "  /help            Browse commands\n  /session list    Inspect saved sessions\n  /diff            Review current workspace changes"
    };
    let planning = plan_restore_mode.map_or_else(String::new, |restore_mode| {
        format!("\n  Planning         active · restores {restore_mode} · /plan exit")
    });
    [
        format!(
            "Session
  Model            {model}
  Permissions      {permission_mode}
{planning}
  Activity         {} messages · {} turns
  Tokens           est {} · latest {} · total {}",
            usage.message_count,
            usage.turns,
            usage.estimated_tokens,
            usage.latest.total_tokens(),
            usage.cumulative.total_tokens(),
        ),
        format!(
            "Usage
  Cumulative input {}
  Cumulative output {}
  Cache create     {}
  Cache read       {}",
            usage.cumulative.input_tokens,
            usage.cumulative.output_tokens,
            usage.cumulative.cache_creation_input_tokens,
            usage.cumulative.cache_read_input_tokens,
        ),
        format!(
            "Workspace
  Folder           {}
  Project root     {}
  Git branch       {}
  Session file     {}
  Config files     loaded {}/{}
  Memory files     {}

Next
{}",
            context.cwd.display(),
            context
                .project_root
                .as_ref()
                .map_or_else(|| "unknown".to_string(), |path| path.display().to_string()),
            context.git_branch.as_deref().unwrap_or("unknown"),
            context.session_path.as_ref().map_or_else(
                || "live-repl".to_string(),
                |path| path.display().to_string()
            ),
            context.loaded_config_files,
            context.discovered_config_files,
            context.memory_file_count,
            next_step,
        ),
    ]
    .join(
        "

",
    )
}

fn render_config_report(section: Option<&str>) -> Result<String, Box<dyn std::error::Error>> {
    let cwd = env::current_dir()?;
    let loader = ConfigLoader::default_for(&cwd);
    let discovered = loader.discover();
    let runtime_config = loader.load()?;

    let mut lines = vec![
        format!(
            "Config
  Working directory {}
  Loaded files      {}
  Merged keys       {}",
            cwd.display(),
            runtime_config.loaded_entries().len(),
            runtime_config.merged().len()
        ),
        "Discovered files".to_string(),
    ];
    for entry in discovered {
        let source = match entry.source {
            ConfigSource::User => "user",
            ConfigSource::Project => "project",
            ConfigSource::Local => "local",
        };
        let status = if runtime_config
            .loaded_entries()
            .iter()
            .any(|loaded_entry| loaded_entry.path == entry.path)
        {
            "loaded"
        } else {
            "missing"
        };
        lines.push(format!(
            "  {source:<7} {status:<7} {}",
            entry.path.display()
        ));
    }

    if let Some(section) = section {
        lines.push(format!("Merged section: {section}"));
        let value = match section {
            "env" => runtime_config.get("env"),
            "hooks" => runtime_config.get("hooks"),
            "model" => runtime_config.get("model"),
            "plugins" => runtime_config
                .get("plugins")
                .or_else(|| runtime_config.get("enabledPlugins")),
            other => {
                lines.push(format!(
                    "  Unsupported config section '{other}'. Use env, hooks, model, or plugins."
                ));
                return Ok(lines.join(
                    "
",
                ));
            }
        };
        lines.push(format!(
            "  {}",
            match value {
                Some(value) => value.render(),
                None => "<unset>".to_string(),
            }
        ));
        return Ok(lines.join(
            "
",
        ));
    }

    lines.push("Merged JSON".to_string());
    lines.push(format!("  {}", runtime_config.as_json().render()));
    Ok(lines.join(
        "
",
    ))
}

fn render_memory_report() -> Result<String, Box<dyn std::error::Error>> {
    render_memory_report_with_date(&current_local_date_string())
}

fn render_memory_report_with_date(
    current_date: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let cwd = env::current_dir()?;
    let project_context = ProjectContext::discover(&cwd, current_date)?;
    let mut lines = vec![format!(
        "Memory
  Working directory {}
  Instruction files {}",
        cwd.display(),
        project_context.instruction_files.len()
    )];
    if project_context.instruction_files.is_empty() {
        lines.push("Discovered files".to_string());
        lines.push(
            "  No OPENYAK instruction files discovered in the current directory ancestry."
                .to_string(),
        );
    } else {
        lines.push("Discovered files".to_string());
        for (index, file) in project_context.instruction_files.iter().enumerate() {
            let preview = file.content.lines().next().unwrap_or("").trim();
            let preview = if preview.is_empty() {
                "<empty>"
            } else {
                preview
            };
            lines.push(format!("  {}. {}", index + 1, file.path.display(),));
            lines.push(format!(
                "     lines={} preview={}",
                file.content.lines().count(),
                preview
            ));
        }
    }
    Ok(lines.join(
        "
",
    ))
}

fn init_openyak_md() -> Result<String, Box<dyn std::error::Error>> {
    let cwd = env::current_dir()?;
    Ok(initialize_repo(&cwd)?.render())
}

fn run_init() -> Result<(), Box<dyn std::error::Error>> {
    println!("{}", init_openyak_md()?);
    Ok(())
}

fn normalize_permission_mode(mode: &str) -> Option<&'static str> {
    match mode.trim() {
        "read-only" => Some("read-only"),
        "workspace-write" => Some("workspace-write"),
        "danger-full-access" => Some("danger-full-access"),
        _ => None,
    }
}

fn render_diff_report() -> Result<String, Box<dyn std::error::Error>> {
    if find_git_root().is_err() {
        return Ok("Diff
  Result           unavailable
  Reason           current directory is not inside a git repository"
            .to_string());
    }

    let diff_args = git_args_excluding_local_artifacts(&["diff"]);
    let output = std::process::Command::new("git")
        .args(&diff_args)
        .current_dir(env::current_dir()?)
        .output()?;
    if !output.status.success() {
        return Err(format!(
            "git diff failed: {}",
            summarize_command_stderr(&output.stderr)
        )
        .into());
    }
    let diff = String::from_utf8(output.stdout)?;
    let status = git_output_filtered(&["status", "--short"])?;
    if diff.trim().is_empty() && status.trim().is_empty() {
        return Ok(
            "Diff\n  Result           clean working tree\n  Detail           no current changes"
                .to_string(),
        );
    }

    let mut sections = vec!["Diff".to_string()];
    if !status.trim().is_empty() {
        sections.push(String::new());
        sections.push("Status".to_string());
        sections.push(status.trim_end().to_string());
    }
    if !diff.trim().is_empty() {
        sections.push(String::new());
        sections.push("Patch".to_string());
        sections.push(diff.trim_end().to_string());
    }
    Ok(sections.join("\n"))
}

fn render_git_command_requires_repo(command: &str, feature: &str) -> String {
    format!(
        "Command unavailable
  Command          /{command}
  Feature          {feature}
  Reason           current directory is not inside a git repository
  Tip              Run the command from a git worktree"
    )
}

fn render_teleport_report(target: &str) -> Result<String, Box<dyn std::error::Error>> {
    let cwd = env::current_dir()?;

    let file_list = Command::new("rg")
        .args(["--files"])
        .current_dir(&cwd)
        .output()?;
    let file_matches = if file_list.status.success() {
        String::from_utf8(file_list.stdout)?
            .lines()
            .filter(|line| line.contains(target))
            .take(10)
            .map(ToOwned::to_owned)
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };

    let content_output = Command::new("rg")
        .args(["-n", "-S", "--color", "never", target, "."])
        .current_dir(&cwd)
        .output()?;

    let mut lines = vec![format!("Teleport\n  Target           {target}")];
    if !file_matches.is_empty() {
        lines.push(String::new());
        lines.push("File matches".to_string());
        lines.extend(file_matches.into_iter().map(|path| format!("  {path}")));
    }

    if content_output.status.success() {
        let matches = String::from_utf8(content_output.stdout)?;
        if !matches.trim().is_empty() {
            lines.push(String::new());
            lines.push("Content matches".to_string());
            lines.push(truncate_for_prompt(&matches, 4_000));
        }
    }

    if lines.len() == 1 {
        lines.push("  Result           no matches found".to_string());
    }

    Ok(lines.join("\n"))
}

fn render_last_tool_debug_report(session: &Session) -> String {
    let Some(last_tool_use) = session.messages.iter().rev().find_map(|message| {
        message.blocks.iter().rev().find_map(|block| match block {
            ContentBlock::ToolUse { id, name, input } => {
                Some((id.clone(), name.clone(), input.clone()))
            }
            _ => None,
        })
    }) else {
        return "Debug tool call
  Result           unavailable
  Reason           no prior tool call found in session"
            .to_string();
    };

    let tool_result = session.messages.iter().rev().find_map(|message| {
        message.blocks.iter().rev().find_map(|block| match block {
            ContentBlock::ToolResult {
                tool_use_id,
                tool_name,
                output,
                is_error,
            } if tool_use_id == &last_tool_use.0 => {
                Some((tool_name.clone(), output.clone(), *is_error))
            }
            _ => None,
        })
    });

    let mut lines = vec![
        "Debug tool call".to_string(),
        format!("  Tool id          {}", last_tool_use.0),
        format!("  Tool name        {}", last_tool_use.1),
        "  Input".to_string(),
        indent_block(&last_tool_use.2, 4),
    ];

    match tool_result {
        Some((tool_name, output, is_error)) => {
            lines.push("  Result".to_string());
            lines.push(format!("    name           {tool_name}"));
            lines.push(format!(
                "    status         {}",
                if is_error { "error" } else { "ok" }
            ));
            lines.push(indent_block(&output, 4));
        }
        None => lines.push("  Result           missing tool result".to_string()),
    }

    lines.join("\n")
}

fn indent_block(value: &str, spaces: usize) -> String {
    let indent = " ".repeat(spaces);
    value
        .lines()
        .map(|line| format!("{indent}{line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn git_output(args: &[&str]) -> Result<String, Box<dyn std::error::Error>> {
    let output = Command::new("git")
        .args(args)
        .current_dir(env::current_dir()?)
        .output()?;
    if !output.status.success() {
        return Err(format!(
            "git {} failed: {}",
            args.join(" "),
            summarize_command_stderr(&output.stderr)
        )
        .into());
    }
    Ok(String::from_utf8(output.stdout)?)
}

fn git_output_filtered(args: &[&str]) -> Result<String, Box<dyn std::error::Error>> {
    let filtered_args = git_args_excluding_local_artifacts(args);
    let output = Command::new("git")
        .args(&filtered_args)
        .current_dir(env::current_dir()?)
        .output()?;
    if !output.status.success() {
        return Err(format!(
            "git {} failed: {}",
            filtered_args.join(" "),
            summarize_command_stderr(&output.stderr)
        )
        .into());
    }
    Ok(String::from_utf8(output.stdout)?)
}

fn git_args_excluding_local_artifacts<'a>(args: &[&'a str]) -> Vec<&'a str> {
    let mut filtered = Vec::with_capacity(args.len() + 5);
    filtered.extend_from_slice(args);
    filtered.push("--");
    filtered.push(".");
    filtered.push(":(exclude).omx");
    filtered.push(":(exclude).openyak/settings.local.json");
    filtered.push(":(exclude).openyak/sessions");
    filtered
}

fn summarize_command_stderr(stderr: &[u8]) -> String {
    let summary = String::from_utf8_lossy(stderr);
    summary
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("command failed")
        .to_string()
}

fn write_temp_text_file(
    filename: &str,
    contents: &str,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let path = env::temp_dir().join(filename);
    fs::write(&path, contents)?;
    Ok(path)
}

fn recent_user_context(session: &Session, limit: usize) -> String {
    let requests = session
        .messages
        .iter()
        .filter(|message| message.role == MessageRole::User)
        .filter_map(|message| {
            message.blocks.iter().find_map(|block| match block {
                ContentBlock::Text { text } => Some(text.trim().to_string()),
                _ => None,
            })
        })
        .rev()
        .take(limit)
        .collect::<Vec<_>>();

    if requests.is_empty() {
        "<no prior user messages>".to_string()
    } else {
        requests
            .into_iter()
            .rev()
            .enumerate()
            .map(|(index, text)| format!("{}. {}", index + 1, text))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

fn truncate_for_prompt(value: &str, limit: usize) -> String {
    if value.chars().count() <= limit {
        value.trim().to_string()
    } else {
        let truncated = value.chars().take(limit).collect::<String>();
        format!("{}\n…[truncated]", truncated.trim_end())
    }
}

fn sanitize_generated_message(value: &str) -> String {
    value.trim().trim_matches('`').trim().replace("\r\n", "\n")
}

fn parse_titled_body(value: &str) -> Option<(String, String)> {
    let normalized = sanitize_generated_message(value);
    let title = normalized
        .lines()
        .find_map(|line| line.strip_prefix("TITLE:").map(str::trim))?;
    let body_start = normalized.find("BODY:")?;
    let body = normalized[body_start + "BODY:".len()..].trim();
    Some((title.to_string(), body.to_string()))
}

fn parse_commit_push_pr_draft(value: &str) -> Option<(Option<String>, String, String)> {
    let normalized = sanitize_generated_message(value);
    let commit = normalized
        .lines()
        .find_map(|line| line.strip_prefix("COMMIT:").map(str::trim))?;
    let commit = match commit {
        "" | "NONE" => None,
        value => Some(value.to_string()),
    };
    let (title, body) = parse_titled_body(&normalized)?;
    Some((commit, title, body))
}

fn render_version_report() -> String {
    let git_sha = GIT_SHA.unwrap_or("unknown");
    let target = BUILD_TARGET.unwrap_or("unknown");
    let build_date = BUILD_DATE.unwrap_or("unknown");
    format!(
        "openyak\n  Version          {VERSION}\n  Git SHA          {git_sha}\n  Target           {target}\n  Build date       {build_date}\n\nSupport\n  Help             openyak --help\n  REPL             /help"
    )
}

fn render_export_text(session: &Session) -> String {
    let mut lines = vec!["# Conversation Export".to_string(), String::new()];
    for (index, message) in session.messages.iter().enumerate() {
        let role = match message.role {
            MessageRole::System => "system",
            MessageRole::User => "user",
            MessageRole::Assistant => "assistant",
            MessageRole::Tool => "tool",
        };
        lines.push(format!("## {}. {role}", index + 1));
        for block in &message.blocks {
            match block {
                ContentBlock::Text { text } => lines.push(text.clone()),
                ContentBlock::ToolUse { id, name, input } => {
                    lines.push(format!("[tool_use id={id} name={name}] {input}"));
                }
                ContentBlock::ToolResult {
                    tool_use_id,
                    tool_name,
                    output,
                    is_error,
                } => {
                    lines.push(format!(
                        "[tool_result id={tool_use_id} name={tool_name} error={is_error}] {output}"
                    ));
                }
                ContentBlock::UserInputRequest {
                    request_id,
                    prompt,
                    options,
                    allow_freeform,
                } => lines.push(format!(
                    "[user_input_request id={request_id} freeform={allow_freeform} options={}] {prompt}",
                    options.join("|")
                )),
                ContentBlock::UserInputResponse {
                    request_id,
                    content,
                    selected_option,
                } => lines.push(format!(
                    "[user_input_response id={request_id} selected={}] {content}",
                    selected_option.as_deref().unwrap_or("-")
                )),
            }
        }
        lines.push(String::new());
    }
    lines.join("\n")
}

fn default_export_filename(session: &Session) -> String {
    let stem = session
        .messages
        .iter()
        .find_map(|message| match message.role {
            MessageRole::User => message.blocks.iter().find_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            }),
            _ => None,
        })
        .map_or("conversation", |text| {
            text.lines().next().unwrap_or("conversation")
        })
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .split('-')
        .filter(|part| !part.is_empty())
        .take(8)
        .collect::<Vec<_>>()
        .join("-");
    let fallback = if stem.is_empty() {
        "conversation"
    } else {
        &stem
    };
    format!("{fallback}.txt")
}

fn resolve_export_path(
    requested_path: Option<&str>,
    session: &Session,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let cwd = env::current_dir()?;
    let file_name =
        requested_path.map_or_else(|| default_export_filename(session), ToOwned::to_owned);
    let final_name = if Path::new(&file_name)
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("txt"))
    {
        file_name
    } else {
        format!("{file_name}.txt")
    };
    Ok(cwd.join(final_name))
}

fn build_system_prompt(model: &str) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    build_system_prompt_with_date(model, &current_local_date_string())
}

fn build_system_prompt_with_date(
    model: &str,
    current_date: &str,
) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let cwd = env::current_dir()?;
    build_system_prompt_for_cwd_with_date(&cwd, Some(model), current_date)
}

fn build_system_prompt_for_cwd_with_date(
    cwd: &Path,
    requested_model: Option<&str>,
    current_date: &str,
) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let model = resolve_effective_model(requested_model, cwd)?;
    Ok(load_system_prompt(
        cwd.to_path_buf(),
        current_date,
        &model,
        env::consts::OS,
        "unknown",
    )?)
}

pub(crate) fn resolve_effective_model(
    requested_model: Option<&str>,
    cwd: &Path,
) -> Result<String, Box<dyn std::error::Error>> {
    if let Some(model) = requested_model {
        return Ok(resolve_model_alias(model).to_string());
    }

    let config = ConfigLoader::default_for(cwd).load()?;
    Ok(config
        .model()
        .map_or(DEFAULT_MODEL, resolve_model_alias)
        .to_string())
}

fn build_runtime_plugin_state(
) -> Result<(runtime::RuntimeFeatureConfig, GlobalToolRegistry), Box<dyn std::error::Error>> {
    let cwd = env::current_dir()?;
    let loader = ConfigLoader::default_for(&cwd);
    let runtime_config = loader.load()?;
    let plugin_manager = build_plugin_manager(&cwd, &loader, &runtime_config);
    let feature_config = merge_plugin_hooks(
        runtime_config.feature_config().clone(),
        plugin_manager.aggregated_hooks()?,
    );
    let tool_registry = GlobalToolRegistry::with_plugin_tools(plugin_manager.aggregated_tools()?)?;
    Ok((feature_config, tool_registry))
}

fn merge_plugin_hooks(
    feature_config: runtime::RuntimeFeatureConfig,
    plugin_hooks: PluginHooks,
) -> runtime::RuntimeFeatureConfig {
    if plugin_hooks.is_empty() {
        return feature_config;
    }

    let plugin_hook_config =
        runtime::RuntimeHookConfig::new(plugin_hooks.pre_tool_use, plugin_hooks.post_tool_use);
    let merged_hooks = feature_config.hooks().merged(&plugin_hook_config);
    feature_config.with_hooks(merged_hooks)
}

fn build_plugin_manager(
    cwd: &Path,
    loader: &ConfigLoader,
    runtime_config: &runtime::RuntimeConfig,
) -> PluginManager {
    let plugin_settings = runtime_config.plugins();
    let mut plugin_config = PluginManagerConfig::new(loader.config_home().to_path_buf());
    plugin_config.enabled_plugins = plugin_settings.enabled_plugins().clone();
    plugin_config.external_dirs = plugin_settings
        .external_directories()
        .iter()
        .map(|path| resolve_plugin_path(cwd, loader.config_home(), path))
        .collect();
    plugin_config.install_root = plugin_settings
        .install_root()
        .map(|path| resolve_plugin_path(cwd, loader.config_home(), path));
    plugin_config.registry_path = plugin_settings
        .registry_path()
        .map(|path| resolve_plugin_path(cwd, loader.config_home(), path));
    plugin_config.bundled_root = plugin_settings
        .bundled_root()
        .map(|path| resolve_plugin_path(cwd, loader.config_home(), path));
    PluginManager::new(plugin_config)
}

fn resolve_plugin_path(cwd: &Path, config_home: &Path, value: &str) -> PathBuf {
    let path = PathBuf::from(value);
    if path.is_absolute() {
        path
    } else if value.starts_with('.') {
        cwd.join(path)
    } else {
        config_home.join(path)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct InternalPromptProgressState {
    command_label: &'static str,
    task_label: String,
    step: usize,
    phase: String,
    detail: Option<String>,
    saw_final_text: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InternalPromptProgressEvent {
    Started,
    Update,
    Heartbeat,
    Complete,
    Failed,
}

#[derive(Debug)]
struct InternalPromptProgressShared {
    state: Mutex<InternalPromptProgressState>,
    output_lock: Mutex<()>,
    started_at: Instant,
}

#[derive(Debug, Clone)]
struct InternalPromptProgressReporter {
    shared: Arc<InternalPromptProgressShared>,
}

#[derive(Debug)]
struct InternalPromptProgressRun {
    reporter: InternalPromptProgressReporter,
    heartbeat_stop: Option<mpsc::Sender<()>>,
    heartbeat_handle: Option<thread::JoinHandle<()>>,
}

impl InternalPromptProgressReporter {
    fn ultraplan(task: &str) -> Self {
        Self {
            shared: Arc::new(InternalPromptProgressShared {
                state: Mutex::new(InternalPromptProgressState {
                    command_label: "Ultraplan",
                    task_label: task.to_string(),
                    step: 0,
                    phase: "planning started".to_string(),
                    detail: Some(format!("task: {task}")),
                    saw_final_text: false,
                }),
                output_lock: Mutex::new(()),
                started_at: Instant::now(),
            }),
        }
    }

    fn emit(&self, event: InternalPromptProgressEvent, error: Option<&str>) {
        let snapshot = self.snapshot();
        let line = format_internal_prompt_progress_line(event, &snapshot, self.elapsed(), error);
        self.write_line(&line);
    }

    fn mark_model_phase(&self) {
        let snapshot = {
            let mut state = self
                .shared
                .state
                .lock()
                .expect("internal prompt progress state poisoned");
            state.step += 1;
            state.phase = if state.step == 1 {
                "analyzing request".to_string()
            } else {
                "reviewing findings".to_string()
            };
            state.detail = Some(format!("task: {}", state.task_label));
            state.clone()
        };
        self.write_line(&format_internal_prompt_progress_line(
            InternalPromptProgressEvent::Update,
            &snapshot,
            self.elapsed(),
            None,
        ));
    }

    fn mark_tool_phase(&self, name: &str, input: &str) {
        let detail = describe_tool_progress(name, input);
        let snapshot = {
            let mut state = self
                .shared
                .state
                .lock()
                .expect("internal prompt progress state poisoned");
            state.step += 1;
            state.phase = format!("running {name}");
            state.detail = Some(detail);
            state.clone()
        };
        self.write_line(&format_internal_prompt_progress_line(
            InternalPromptProgressEvent::Update,
            &snapshot,
            self.elapsed(),
            None,
        ));
    }

    fn mark_text_phase(&self, text: &str) {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return;
        }
        let detail = truncate_for_summary(first_visible_line(trimmed), 120);
        let snapshot = {
            let mut state = self
                .shared
                .state
                .lock()
                .expect("internal prompt progress state poisoned");
            if state.saw_final_text {
                return;
            }
            state.saw_final_text = true;
            state.step += 1;
            state.phase = "drafting final plan".to_string();
            state.detail = (!detail.is_empty()).then_some(detail);
            state.clone()
        };
        self.write_line(&format_internal_prompt_progress_line(
            InternalPromptProgressEvent::Update,
            &snapshot,
            self.elapsed(),
            None,
        ));
    }

    fn emit_heartbeat(&self) {
        let snapshot = self.snapshot();
        self.write_line(&format_internal_prompt_progress_line(
            InternalPromptProgressEvent::Heartbeat,
            &snapshot,
            self.elapsed(),
            None,
        ));
    }

    fn snapshot(&self) -> InternalPromptProgressState {
        self.shared
            .state
            .lock()
            .expect("internal prompt progress state poisoned")
            .clone()
    }

    fn elapsed(&self) -> Duration {
        self.shared.started_at.elapsed()
    }

    fn write_line(&self, line: &str) {
        let _guard = self
            .shared
            .output_lock
            .lock()
            .expect("internal prompt progress output lock poisoned");
        let mut stdout = io::stdout();
        let _ = writeln!(stdout, "{line}");
        let _ = stdout.flush();
    }
}

impl InternalPromptProgressRun {
    fn start_ultraplan(task: &str) -> Self {
        let reporter = InternalPromptProgressReporter::ultraplan(task);
        reporter.emit(InternalPromptProgressEvent::Started, None);

        let (heartbeat_stop, heartbeat_rx) = mpsc::channel();
        let heartbeat_reporter = reporter.clone();
        let heartbeat_handle = thread::spawn(move || loop {
            match heartbeat_rx.recv_timeout(INTERNAL_PROGRESS_HEARTBEAT_INTERVAL) {
                Ok(()) | Err(RecvTimeoutError::Disconnected) => break,
                Err(RecvTimeoutError::Timeout) => heartbeat_reporter.emit_heartbeat(),
            }
        });

        Self {
            reporter,
            heartbeat_stop: Some(heartbeat_stop),
            heartbeat_handle: Some(heartbeat_handle),
        }
    }

    fn reporter(&self) -> InternalPromptProgressReporter {
        self.reporter.clone()
    }

    fn finish_success(&mut self) {
        self.stop_heartbeat();
        self.reporter
            .emit(InternalPromptProgressEvent::Complete, None);
    }

    fn finish_failure(&mut self, error: &str) {
        self.stop_heartbeat();
        self.reporter
            .emit(InternalPromptProgressEvent::Failed, Some(error));
    }

    fn stop_heartbeat(&mut self) {
        if let Some(sender) = self.heartbeat_stop.take() {
            let _ = sender.send(());
        }
        if let Some(handle) = self.heartbeat_handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for InternalPromptProgressRun {
    fn drop(&mut self) {
        self.stop_heartbeat();
    }
}

fn format_internal_prompt_progress_line(
    event: InternalPromptProgressEvent,
    snapshot: &InternalPromptProgressState,
    elapsed: Duration,
    error: Option<&str>,
) -> String {
    let elapsed_seconds = elapsed.as_secs();
    let step_label = if snapshot.step == 0 {
        "current step pending".to_string()
    } else {
        format!("current step {}", snapshot.step)
    };
    let mut status_bits = vec![step_label, format!("phase {}", snapshot.phase)];
    if let Some(detail) = snapshot
        .detail
        .as_deref()
        .filter(|detail| !detail.is_empty())
    {
        status_bits.push(detail.to_string());
    }
    let status = status_bits.join(" · ");
    match event {
        InternalPromptProgressEvent::Started => {
            format!(
                "🧭 {} status · planning started · {status}",
                snapshot.command_label
            )
        }
        InternalPromptProgressEvent::Update => {
            format!("… {} status · {status}", snapshot.command_label)
        }
        InternalPromptProgressEvent::Heartbeat => format!(
            "… {} heartbeat · {elapsed_seconds}s elapsed · {status}",
            snapshot.command_label
        ),
        InternalPromptProgressEvent::Complete => format!(
            "✔ {} status · completed · {elapsed_seconds}s elapsed · {} steps total",
            snapshot.command_label, snapshot.step
        ),
        InternalPromptProgressEvent::Failed => format!(
            "✘ {} status · failed · {elapsed_seconds}s elapsed · {}",
            snapshot.command_label,
            error.unwrap_or("unknown error")
        ),
    }
}

fn describe_tool_progress(name: &str, input: &str) -> String {
    let parsed: serde_json::Value =
        serde_json::from_str(input).unwrap_or(serde_json::Value::String(input.to_string()));
    match name {
        "bash" | "Bash" => {
            let command = parsed
                .get("command")
                .and_then(|value| value.as_str())
                .unwrap_or_default();
            if command.is_empty() {
                "running shell command".to_string()
            } else {
                format!("command {}", truncate_for_summary(command.trim(), 100))
            }
        }
        "read_file" | "Read" => format!("reading {}", extract_tool_path(&parsed)),
        "write_file" | "Write" => format!("writing {}", extract_tool_path(&parsed)),
        "edit_file" | "Edit" => format!("editing {}", extract_tool_path(&parsed)),
        "glob_search" | "Glob" => {
            let pattern = parsed
                .get("pattern")
                .and_then(|value| value.as_str())
                .unwrap_or("?");
            let scope = parsed
                .get("path")
                .and_then(|value| value.as_str())
                .unwrap_or(".");
            format!("glob `{pattern}` in {scope}")
        }
        "grep_search" | "Grep" => {
            let pattern = parsed
                .get("pattern")
                .and_then(|value| value.as_str())
                .unwrap_or("?");
            let scope = parsed
                .get("path")
                .and_then(|value| value.as_str())
                .unwrap_or(".");
            format!("grep `{pattern}` in {scope}")
        }
        "web_search" | "WebSearch" => parsed
            .get("query")
            .and_then(|value| value.as_str())
            .map_or_else(
                || "running web search".to_string(),
                |query| format!("query {}", truncate_for_summary(query, 100)),
            ),
        _ => {
            let summary = summarize_tool_payload(input);
            if summary.is_empty() {
                format!("running {name}")
            } else {
                format!("{name}: {summary}")
            }
        }
    }
}

#[allow(clippy::needless_pass_by_value)]
#[allow(clippy::too_many_arguments)]
fn build_runtime(
    session: Session,
    model: String,
    system_prompt: Vec<String>,
    enable_tools: bool,
    emit_output: bool,
    allowed_tools: Option<AllowedToolSet>,
    permission_mode: PermissionMode,
    progress_reporter: Option<InternalPromptProgressReporter>,
) -> Result<ConversationRuntime<DefaultRuntimeClient, CliToolExecutor>, Box<dyn std::error::Error>>
{
    let (feature_config, mut tool_registry) = build_runtime_plugin_state()?;
    let policy = permission_policy(permission_mode, &tool_registry);
    tool_registry = tool_registry.with_enforcer(runtime::PermissionEnforcer::new(policy.clone()));
    Ok(ConversationRuntime::new_with_features(
        session,
        DefaultRuntimeClient::new(
            model,
            enable_tools,
            emit_output,
            allowed_tools.clone(),
            tool_registry.clone(),
            progress_reporter,
        )?,
        CliToolExecutor::new(allowed_tools.clone(), emit_output, tool_registry.clone()),
        policy,
        system_prompt,
        &feature_config,
    ))
}

struct CliPermissionPrompter {
    current_mode: PermissionMode,
}

impl CliPermissionPrompter {
    fn new(current_mode: PermissionMode) -> Self {
        Self { current_mode }
    }
}

impl runtime::PermissionPrompter for CliPermissionPrompter {
    fn decide(
        &mut self,
        request: &runtime::PermissionRequest,
    ) -> runtime::PermissionPromptDecision {
        println!();
        println!("Permission approval required");
        println!("  Tool             {}", request.tool_name);
        println!("  Current mode     {}", self.current_mode.as_str());
        println!("  Required mode    {}", request.required_mode.as_str());
        println!("  Input            {}", request.input);
        print!("Approve this tool call? [y/N]: ");
        let _ = io::stdout().flush();

        let mut response = String::new();
        match io::stdin().read_line(&mut response) {
            Ok(_) => {
                let normalized = response.trim().to_ascii_lowercase();
                if matches!(normalized.as_str(), "y" | "yes") {
                    runtime::PermissionPromptDecision::Allow
                } else {
                    runtime::PermissionPromptDecision::Deny {
                        reason: format!(
                            "tool '{}' denied by user approval prompt",
                            request.tool_name
                        ),
                    }
                }
            }
            Err(error) => runtime::PermissionPromptDecision::Deny {
                reason: format!("permission approval failed: {error}"),
            },
        }
    }
}

struct CliUserInputPrompter {
    interactive: bool,
}

impl CliUserInputPrompter {
    fn interactive() -> Self {
        Self {
            interactive: io::stdin().is_terminal() && io::stdout().is_terminal(),
        }
    }

    fn unavailable() -> Self {
        Self { interactive: false }
    }
}

impl UserInputPrompter for CliUserInputPrompter {
    fn prompt(&mut self, request: &UserInputRequest) -> UserInputOutcome {
        if !self.interactive {
            return UserInputOutcome::Unavailable {
                reason: "interactive CLI input is unavailable in this mode".to_string(),
            };
        }

        println!();
        println!("{}", format_user_input_request(request));

        let mut editor = input::LineEditor::new(REQUEST_USER_INPUT_PROMPT, request.options.clone());
        loop {
            match editor.read_line() {
                Ok(input::ReadOutcome::Submit(line)) => {
                    match parse_user_input_submission(request, &line) {
                        Ok(response) => return UserInputOutcome::Submitted(response),
                        Err(problem) => {
                            println!("{problem}");
                        }
                    }
                }
                Ok(input::ReadOutcome::Cancel) => return UserInputOutcome::Cancelled,
                Ok(input::ReadOutcome::Exit) => {
                    return UserInputOutcome::Unavailable {
                        reason: "stdin closed while waiting for request-user-input".to_string(),
                    }
                }
                Err(error) => {
                    return UserInputOutcome::Unavailable {
                        reason: format!("failed to read request-user-input reply: {error}"),
                    }
                }
            }
        }
    }
}

struct DefaultRuntimeClient {
    runtime: tokio::runtime::Runtime,
    client: Option<ProviderClient>,
    model: String,
    enable_tools: bool,
    emit_output: bool,
    allowed_tools: Option<AllowedToolSet>,
    tool_registry: GlobalToolRegistry,
    progress_reporter: Option<InternalPromptProgressReporter>,
}

impl DefaultRuntimeClient {
    fn new(
        model: String,
        enable_tools: bool,
        emit_output: bool,
        allowed_tools: Option<AllowedToolSet>,
        tool_registry: GlobalToolRegistry,
        progress_reporter: Option<InternalPromptProgressReporter>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        Ok(Self {
            runtime: tokio::runtime::Runtime::new()?,
            client: None,
            model,
            enable_tools,
            emit_output,
            allowed_tools,
            tool_registry,
            progress_reporter,
        })
    }

    fn ensure_api_auth(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        if self.client.is_none() {
            let default_auth = if matches!(
                api::detect_provider_kind(&self.model),
                api::ProviderKind::OpenyakApi
            ) {
                Some(resolve_cli_auth_source()?)
            } else {
                None
            };
            self.client = Some(ProviderClient::from_model_with_default_auth(
                &self.model,
                default_auth,
            )?);
        }
        Ok(())
    }
}

fn resolve_cli_auth_source() -> Result<AuthSource, Box<dyn std::error::Error>> {
    Ok(resolve_startup_auth_source(|| {
        let cwd = env::current_dir().map_err(api::ApiError::from)?;
        let config = ConfigLoader::default_for(&cwd).load().map_err(|error| {
            api::ApiError::Auth(format!("failed to load runtime OAuth config: {error}"))
        })?;
        configured_oauth_config(&config).map_err(api::ApiError::Auth)
    })?)
}

impl ApiClient for DefaultRuntimeClient {
    #[allow(clippy::too_many_lines)]
    fn stream(&mut self, request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
        self.ensure_api_auth()
            .map_err(|error| RuntimeError::new(error.to_string()))?;
        let client = self.client.as_ref().expect("api client should initialize");
        if let Some(progress_reporter) = &self.progress_reporter {
            progress_reporter.mark_model_phase();
        }
        let message_request = MessageRequest {
            model: self.model.clone(),
            max_tokens: max_tokens_for_model(&self.model),
            messages: convert_messages(&request.messages),
            system: (!request.system_prompt.is_empty()).then(|| request.system_prompt.join("\n\n")),
            tools: Some(if self.enable_tools {
                filter_tool_specs(&self.tool_registry, self.allowed_tools.as_ref())
            } else {
                vec![request_user_input_tool_definition()]
            }),
            tool_choice: Some(ToolChoice::Auto),
            stream: true,
        };

        self.runtime.block_on(async {
            let mut stream = client
                .stream_message(&message_request)
                .await
                .map_err(|error| RuntimeError::new(error.to_string()))?;
            let mut stdout = io::stdout();
            let mut sink = io::sink();
            let out: &mut dyn Write = if self.emit_output {
                &mut stdout
            } else {
                &mut sink
            };
            let renderer = TerminalRenderer::new();
            let mut markdown_stream = MarkdownStreamState::default();
            let mut events = Vec::new();
            let mut pending_tool: Option<(String, String, String)> = None;
            let mut saw_stop = false;

            while let Some(event) = stream
                .next_event()
                .await
                .map_err(|error| RuntimeError::new(error.to_string()))?
            {
                match event {
                    ApiStreamEvent::MessageStart(start) => {
                        for block in start.message.content {
                            push_output_block(block, out, &mut events, &mut pending_tool, true)?;
                        }
                    }
                    ApiStreamEvent::ContentBlockStart(start) => {
                        push_output_block(
                            start.content_block,
                            out,
                            &mut events,
                            &mut pending_tool,
                            true,
                        )?;
                    }
                    ApiStreamEvent::ContentBlockDelta(delta) => match delta.delta {
                        ContentBlockDelta::TextDelta { text } => {
                            if !text.is_empty() {
                                if let Some(progress_reporter) = &self.progress_reporter {
                                    progress_reporter.mark_text_phase(&text);
                                }
                                if let Some(rendered) = markdown_stream.push(&renderer, &text) {
                                    write!(out, "{rendered}")
                                        .and_then(|()| out.flush())
                                        .map_err(|error| RuntimeError::new(error.to_string()))?;
                                }
                                events.push(AssistantEvent::TextDelta(text));
                            }
                        }
                        ContentBlockDelta::InputJsonDelta { partial_json } => {
                            if let Some((_, _, input)) = &mut pending_tool {
                                input.push_str(&partial_json);
                            }
                        }
                        ContentBlockDelta::ThinkingDelta { .. }
                        | ContentBlockDelta::SignatureDelta { .. } => {}
                    },
                    ApiStreamEvent::ContentBlockStop(_) => {
                        if let Some(rendered) = markdown_stream.flush(&renderer) {
                            write!(out, "{rendered}")
                                .and_then(|()| out.flush())
                                .map_err(|error| RuntimeError::new(error.to_string()))?;
                        }
                        if let Some((id, name, input)) = pending_tool.take() {
                            finalize_pending_output_block(
                                id,
                                name,
                                input,
                                out,
                                &mut events,
                                self.progress_reporter.as_ref(),
                            )?;
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
                        if let Some(rendered) = markdown_stream.flush(&renderer) {
                            write!(out, "{rendered}")
                                .and_then(|()| out.flush())
                                .map_err(|error| RuntimeError::new(error.to_string()))?;
                        }
                        events.push(AssistantEvent::MessageStop);
                    }
                }
            }

            if !saw_stop
                && events.iter().any(|event| {
                    matches!(event, AssistantEvent::TextDelta(text) if !text.is_empty())
                        || matches!(event, AssistantEvent::ToolUse { .. })
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
                .as_ref()
                .expect("api client should initialize")
                .send_message(&MessageRequest {
                    stream: false,
                    ..message_request.clone()
                })
                .await
                .map_err(|error| RuntimeError::new(error.to_string()))?;
            response_to_events(response, out)
        })
    }
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

fn collect_tool_uses(summary: &runtime::TurnSummary) -> Vec<serde_json::Value> {
    summary
        .assistant_messages
        .iter()
        .flat_map(|message| message.blocks.iter())
        .filter_map(|block| match block {
            ContentBlock::ToolUse { id, name, input } => Some(json!({
                "id": id,
                "name": name,
                "input": input,
            })),
            _ => None,
        })
        .collect()
}

fn collect_tool_results(summary: &runtime::TurnSummary) -> Vec<serde_json::Value> {
    summary
        .tool_results
        .iter()
        .flat_map(|message| message.blocks.iter())
        .filter_map(|block| match block {
            ContentBlock::ToolResult {
                tool_use_id,
                tool_name,
                output,
                is_error,
            } => Some(json!({
                "tool_use_id": tool_use_id,
                "tool_name": tool_name,
                "output": output,
                "is_error": is_error,
            })),
            _ => None,
        })
        .collect()
}

fn slash_command_completion_candidates() -> Vec<String> {
    let mut candidates = slash_command_specs()
        .iter()
        .flat_map(|spec| {
            std::iter::once(spec.name)
                .chain(spec.aliases.iter().copied())
                .map(|name| format!("/{name}"))
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    candidates.extend([
        String::from("/vim"),
        String::from("/exit"),
        String::from("/quit"),
    ]);
    candidates.sort();
    candidates.dedup();
    candidates
}

fn suggest_repl_commands(name: &str) -> Vec<String> {
    let normalized = name.trim().trim_start_matches('/').to_ascii_lowercase();
    if normalized.is_empty() {
        return Vec::new();
    }

    let mut ranked = slash_command_completion_candidates()
        .into_iter()
        .filter_map(|candidate| {
            let raw = candidate.trim_start_matches('/').to_ascii_lowercase();
            let distance = edit_distance(&normalized, &raw);
            let prefix_match = raw.starts_with(&normalized) || normalized.starts_with(&raw);
            let near_match = distance <= 2;
            (prefix_match || near_match).then_some((distance, candidate))
        })
        .collect::<Vec<_>>();
    ranked.sort();
    ranked.dedup_by(|left, right| left.1 == right.1);
    ranked
        .into_iter()
        .map(|(_, candidate)| candidate)
        .take(3)
        .collect()
}

fn edit_distance(left: &str, right: &str) -> usize {
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
            let substitution_cost = usize::from(left_char != *right_char);
            current[right_index + 1] = (previous[right_index + 1] + 1)
                .min(current[right_index] + 1)
                .min(previous[right_index] + substitution_cost);
        }
        std::mem::swap(&mut previous, &mut current);
    }

    previous[right_chars.len()]
}

fn format_tool_call_start(name: &str, input: &str) -> String {
    let parsed: serde_json::Value =
        serde_json::from_str(input).unwrap_or(serde_json::Value::String(input.to_string()));

    let detail = match name {
        "bash" | "Bash" => format_bash_call(&parsed),
        "read_file" | "Read" => {
            let path = extract_tool_path(&parsed);
            format!("\x1b[2m📄 Reading {path}…\x1b[0m")
        }
        "write_file" | "Write" => {
            let path = extract_tool_path(&parsed);
            let lines = parsed
                .get("content")
                .and_then(|value| value.as_str())
                .map_or(0, |content| content.lines().count());
            format!("\x1b[1;32m✏️ Writing {path}\x1b[0m \x1b[2m({lines} lines)\x1b[0m")
        }
        "edit_file" | "Edit" => {
            let path = extract_tool_path(&parsed);
            let old_value = parsed
                .get("old_string")
                .or_else(|| parsed.get("oldString"))
                .and_then(|value| value.as_str())
                .unwrap_or_default();
            let new_value = parsed
                .get("new_string")
                .or_else(|| parsed.get("newString"))
                .and_then(|value| value.as_str())
                .unwrap_or_default();
            format!(
                "\x1b[1;33m📝 Editing {path}\x1b[0m{}",
                format_patch_preview(old_value, new_value)
                    .map(|preview| format!("\n{preview}"))
                    .unwrap_or_default()
            )
        }
        "glob_search" | "Glob" => format_search_start("🔎 Glob", &parsed),
        "grep_search" | "Grep" => format_search_start("🔎 Grep", &parsed),
        "web_search" | "WebSearch" => parsed
            .get("query")
            .and_then(|value| value.as_str())
            .unwrap_or("?")
            .to_string(),
        _ => summarize_tool_payload(input),
    };

    let border = "─".repeat(name.len() + 8);
    format!(
        "\x1b[38;5;245m╭─ \x1b[1;36m{name}\x1b[0;38;5;245m ─╮\x1b[0m\n\x1b[38;5;245m│\x1b[0m {detail}\n\x1b[38;5;245m╰{border}╯\x1b[0m"
    )
}

fn format_tool_result(name: &str, output: &str, is_error: bool) -> String {
    let icon = if is_error {
        "\x1b[1;31m✗\x1b[0m"
    } else {
        "\x1b[1;32m✓\x1b[0m"
    };
    if is_error {
        let summary = truncate_for_summary(output.trim(), 160);
        return if summary.is_empty() {
            format!("{icon} \x1b[38;5;245m{name}\x1b[0m")
        } else {
            format!("{icon} \x1b[38;5;245m{name}\x1b[0m\n\x1b[38;5;203m{summary}\x1b[0m")
        };
    }

    let parsed: serde_json::Value =
        serde_json::from_str(output).unwrap_or(serde_json::Value::String(output.to_string()));
    match name {
        "bash" | "Bash" => format_bash_result(icon, &parsed),
        "read_file" | "Read" => format_read_result(icon, &parsed),
        "write_file" | "Write" => format_write_result(icon, &parsed),
        "edit_file" | "Edit" => format_edit_result(icon, &parsed),
        "glob_search" | "Glob" => format_glob_result(icon, &parsed),
        "grep_search" | "Grep" => format_grep_result(icon, &parsed),
        _ => format_generic_tool_result(icon, name, &parsed),
    }
}

const DISPLAY_TRUNCATION_NOTICE: &str =
    "\x1b[2m… output truncated for display; full result preserved in session.\x1b[0m";
const READ_DISPLAY_MAX_LINES: usize = 80;
const READ_DISPLAY_MAX_CHARS: usize = 6_000;
const TOOL_OUTPUT_DISPLAY_MAX_LINES: usize = 60;
const TOOL_OUTPUT_DISPLAY_MAX_CHARS: usize = 4_000;

fn extract_tool_path(parsed: &serde_json::Value) -> String {
    parsed
        .get("file_path")
        .or_else(|| parsed.get("filePath"))
        .or_else(|| parsed.get("path"))
        .and_then(|value| value.as_str())
        .unwrap_or("?")
        .to_string()
}

fn format_search_start(label: &str, parsed: &serde_json::Value) -> String {
    let pattern = parsed
        .get("pattern")
        .and_then(|value| value.as_str())
        .unwrap_or("?");
    let scope = parsed
        .get("path")
        .and_then(|value| value.as_str())
        .unwrap_or(".");
    format!("{label} {pattern}\n\x1b[2min {scope}\x1b[0m")
}

fn format_patch_preview(old_value: &str, new_value: &str) -> Option<String> {
    if old_value.is_empty() && new_value.is_empty() {
        return None;
    }
    Some(format!(
        "\x1b[38;5;203m- {}\x1b[0m\n\x1b[38;5;70m+ {}\x1b[0m",
        truncate_for_summary(first_visible_line(old_value), 72),
        truncate_for_summary(first_visible_line(new_value), 72)
    ))
}

fn format_bash_call(parsed: &serde_json::Value) -> String {
    let command = parsed
        .get("command")
        .and_then(|value| value.as_str())
        .unwrap_or_default();
    if command.is_empty() {
        String::new()
    } else {
        format!(
            "\x1b[48;5;236;38;5;255m $ {} \x1b[0m",
            truncate_for_summary(command, 160)
        )
    }
}

fn first_visible_line(text: &str) -> &str {
    text.lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or(text)
}

fn format_bash_result(icon: &str, parsed: &serde_json::Value) -> String {
    let mut lines = vec![format!("{icon} \x1b[38;5;245mbash\x1b[0m")];
    if let Some(task_id) = parsed
        .get("backgroundTaskId")
        .and_then(|value| value.as_str())
    {
        write!(&mut lines[0], " backgrounded ({task_id})").expect("write to string");
    } else if let Some(status) = parsed
        .get("returnCodeInterpretation")
        .and_then(|value| value.as_str())
        .filter(|status| !status.is_empty())
    {
        write!(&mut lines[0], " {status}").expect("write to string");
    }

    if let Some(stdout) = parsed.get("stdout").and_then(|value| value.as_str()) {
        if !stdout.trim().is_empty() {
            lines.push(truncate_output_for_display(
                stdout,
                TOOL_OUTPUT_DISPLAY_MAX_LINES,
                TOOL_OUTPUT_DISPLAY_MAX_CHARS,
            ));
        }
    }
    if let Some(stderr) = parsed.get("stderr").and_then(|value| value.as_str()) {
        if !stderr.trim().is_empty() {
            lines.push(format!(
                "\x1b[38;5;203m{}\x1b[0m",
                truncate_output_for_display(
                    stderr,
                    TOOL_OUTPUT_DISPLAY_MAX_LINES,
                    TOOL_OUTPUT_DISPLAY_MAX_CHARS,
                )
            ));
        }
    }

    lines.join("\n\n")
}

fn format_read_result(icon: &str, parsed: &serde_json::Value) -> String {
    let file = parsed.get("file").unwrap_or(parsed);
    let path = extract_tool_path(file);
    let start_line = file
        .get("startLine")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(1);
    let num_lines = file
        .get("numLines")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let total_lines = file
        .get("totalLines")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(num_lines);
    let content = file
        .get("content")
        .and_then(|value| value.as_str())
        .unwrap_or_default();
    let end_line = start_line.saturating_add(num_lines.saturating_sub(1));

    format!(
        "{icon} \x1b[2m📄 Read {path} (lines {}-{} of {})\x1b[0m\n{}",
        start_line,
        end_line.max(start_line),
        total_lines,
        truncate_output_for_display(content, READ_DISPLAY_MAX_LINES, READ_DISPLAY_MAX_CHARS)
    )
}

fn format_write_result(icon: &str, parsed: &serde_json::Value) -> String {
    let path = extract_tool_path(parsed);
    let kind = parsed
        .get("type")
        .and_then(|value| value.as_str())
        .unwrap_or("write");
    let line_count = parsed
        .get("content")
        .and_then(|value| value.as_str())
        .map_or(0, |content| content.lines().count());
    format!(
        "{icon} \x1b[1;32m✏️ {} {path}\x1b[0m \x1b[2m({line_count} lines)\x1b[0m",
        if kind == "create" { "Wrote" } else { "Updated" },
    )
}

fn format_structured_patch_preview(parsed: &serde_json::Value) -> Option<String> {
    let hunks = parsed.get("structuredPatch")?.as_array()?;
    let mut preview = Vec::new();
    for hunk in hunks.iter().take(2) {
        let lines = hunk.get("lines")?.as_array()?;
        for line in lines.iter().filter_map(|value| value.as_str()).take(6) {
            match line.chars().next() {
                Some('+') => preview.push(format!("\x1b[38;5;70m{line}\x1b[0m")),
                Some('-') => preview.push(format!("\x1b[38;5;203m{line}\x1b[0m")),
                _ => preview.push(line.to_string()),
            }
        }
    }
    if preview.is_empty() {
        None
    } else {
        Some(preview.join("\n"))
    }
}

fn format_edit_result(icon: &str, parsed: &serde_json::Value) -> String {
    let path = extract_tool_path(parsed);
    let suffix = if parsed
        .get("replaceAll")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
    {
        " (replace all)"
    } else {
        ""
    };
    let preview = format_structured_patch_preview(parsed).or_else(|| {
        let old_value = parsed
            .get("oldString")
            .and_then(|value| value.as_str())
            .unwrap_or_default();
        let new_value = parsed
            .get("newString")
            .and_then(|value| value.as_str())
            .unwrap_or_default();
        format_patch_preview(old_value, new_value)
    });

    match preview {
        Some(preview) => format!("{icon} \x1b[1;33m📝 Edited {path}{suffix}\x1b[0m\n{preview}"),
        None => format!("{icon} \x1b[1;33m📝 Edited {path}{suffix}\x1b[0m"),
    }
}

fn format_glob_result(icon: &str, parsed: &serde_json::Value) -> String {
    let num_files = parsed
        .get("numFiles")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let filenames = parsed
        .get("filenames")
        .and_then(|value| value.as_array())
        .map(|files| {
            files
                .iter()
                .filter_map(|value| value.as_str())
                .take(8)
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();
    if filenames.is_empty() {
        format!("{icon} \x1b[38;5;245mglob_search\x1b[0m matched {num_files} files")
    } else {
        format!("{icon} \x1b[38;5;245mglob_search\x1b[0m matched {num_files} files\n{filenames}")
    }
}

fn format_grep_result(icon: &str, parsed: &serde_json::Value) -> String {
    let num_matches = parsed
        .get("numMatches")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let num_files = parsed
        .get("numFiles")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let content = parsed
        .get("content")
        .and_then(|value| value.as_str())
        .unwrap_or_default();
    let filenames = parsed
        .get("filenames")
        .and_then(|value| value.as_array())
        .map(|files| {
            files
                .iter()
                .filter_map(|value| value.as_str())
                .take(8)
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();
    let summary = format!(
        "{icon} \x1b[38;5;245mgrep_search\x1b[0m {num_matches} matches across {num_files} files"
    );
    if !content.trim().is_empty() {
        format!(
            "{summary}\n{}",
            truncate_output_for_display(
                content,
                TOOL_OUTPUT_DISPLAY_MAX_LINES,
                TOOL_OUTPUT_DISPLAY_MAX_CHARS,
            )
        )
    } else if !filenames.is_empty() {
        format!("{summary}\n{filenames}")
    } else {
        summary
    }
}

fn format_generic_tool_result(icon: &str, name: &str, parsed: &serde_json::Value) -> String {
    let rendered_output = match parsed {
        serde_json::Value::String(text) => text.clone(),
        serde_json::Value::Null => String::new(),
        serde_json::Value::Object(_) | serde_json::Value::Array(_) => {
            serde_json::to_string_pretty(parsed).unwrap_or_else(|_| parsed.to_string())
        }
        _ => parsed.to_string(),
    };
    let preview = truncate_output_for_display(
        &rendered_output,
        TOOL_OUTPUT_DISPLAY_MAX_LINES,
        TOOL_OUTPUT_DISPLAY_MAX_CHARS,
    );

    if preview.is_empty() {
        format!("{icon} \x1b[38;5;245m{name}\x1b[0m")
    } else if preview.contains('\n') {
        format!("{icon} \x1b[38;5;245m{name}\x1b[0m\n{preview}")
    } else {
        format!("{icon} \x1b[38;5;245m{name}:\x1b[0m {preview}")
    }
}

fn summarize_tool_payload(payload: &str) -> String {
    let compact = match serde_json::from_str::<serde_json::Value>(payload) {
        Ok(value) => value.to_string(),
        Err(_) => payload.trim().to_string(),
    };
    truncate_for_summary(&compact, 96)
}

fn truncate_for_summary(value: &str, limit: usize) -> String {
    let mut chars = value.chars();
    let truncated = chars.by_ref().take(limit).collect::<String>();
    if chars.next().is_some() {
        format!("{truncated}…")
    } else {
        truncated
    }
}

fn truncate_output_for_display(content: &str, max_lines: usize, max_chars: usize) -> String {
    let original = content.trim_end_matches('\n');
    if original.is_empty() {
        return String::new();
    }

    let mut preview_lines = Vec::new();
    let mut used_chars = 0usize;
    let mut truncated = false;

    for (index, line) in original.lines().enumerate() {
        if index >= max_lines {
            truncated = true;
            break;
        }

        let newline_cost = usize::from(!preview_lines.is_empty());
        let available = max_chars.saturating_sub(used_chars + newline_cost);
        if available == 0 {
            truncated = true;
            break;
        }

        let line_chars = line.chars().count();
        if line_chars > available {
            preview_lines.push(line.chars().take(available).collect::<String>());
            truncated = true;
            break;
        }

        preview_lines.push(line.to_string());
        used_chars += newline_cost + line_chars;
    }

    let mut preview = preview_lines.join("\n");
    if truncated {
        if !preview.is_empty() {
            preview.push('\n');
        }
        preview.push_str(DISPLAY_TRUNCATION_NOTICE);
    }
    preview
}

fn push_output_block(
    block: OutputContentBlock,
    out: &mut (impl Write + ?Sized),
    events: &mut Vec<AssistantEvent>,
    pending_tool: &mut Option<(String, String, String)>,
    streaming_tool_input: bool,
) -> Result<(), RuntimeError> {
    match block {
        OutputContentBlock::Text { text } => {
            if !text.is_empty() {
                let rendered = TerminalRenderer::new().markdown_to_ansi(&text);
                write!(out, "{rendered}")
                    .and_then(|()| out.flush())
                    .map_err(|error| RuntimeError::new(error.to_string()))?;
                events.push(AssistantEvent::TextDelta(text));
            }
        }
        OutputContentBlock::ToolUse { id, name, input } => {
            // During streaming, the initial content_block_start has an empty input ({}).
            // The real input arrives via input_json_delta events. In
            // non-streaming responses, preserve a legitimate empty object.
            let initial_input = if streaming_tool_input
                && input.is_object()
                && input.as_object().is_some_and(serde_json::Map::is_empty)
            {
                String::new()
            } else {
                input.to_string()
            };
            *pending_tool = Some((id, name, initial_input));
        }
        OutputContentBlock::Thinking { .. } | OutputContentBlock::RedactedThinking { .. } => {}
    }
    Ok(())
}

fn finalize_pending_output_block(
    id: String,
    name: String,
    input: String,
    out: &mut (impl Write + ?Sized),
    events: &mut Vec<AssistantEvent>,
    progress_reporter: Option<&InternalPromptProgressReporter>,
) -> Result<(), RuntimeError> {
    if name == REQUEST_USER_INPUT_TOOL_NAME {
        events.push(AssistantEvent::RequestUserInput(
            parse_request_user_input_request(&id, &input)?,
        ));
        return Ok(());
    }

    if let Some(progress_reporter) = progress_reporter {
        progress_reporter.mark_tool_phase(&name, &input);
    }
    writeln!(out, "\n{}", format_tool_call_start(&name, &input))
        .and_then(|()| out.flush())
        .map_err(|error| RuntimeError::new(error.to_string()))?;
    events.push(AssistantEvent::ToolUse { id, name, input });
    Ok(())
}

fn response_to_events(
    response: MessageResponse,
    out: &mut (impl Write + ?Sized),
) -> Result<Vec<AssistantEvent>, RuntimeError> {
    let mut events = Vec::new();
    let mut pending_tool = None;

    for block in response.content {
        push_output_block(block, out, &mut events, &mut pending_tool, false)?;
        if let Some((id, name, input)) = pending_tool.take() {
            finalize_pending_output_block(id, name, input, out, &mut events, None)?;
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

struct CliToolExecutor {
    renderer: TerminalRenderer,
    emit_output: bool,
    allowed_tools: Option<AllowedToolSet>,
    tool_registry: GlobalToolRegistry,
}

impl CliToolExecutor {
    fn new(
        allowed_tools: Option<AllowedToolSet>,
        emit_output: bool,
        tool_registry: GlobalToolRegistry,
    ) -> Self {
        Self {
            renderer: TerminalRenderer::new(),
            emit_output,
            allowed_tools,
            tool_registry,
        }
    }
}

impl ToolExecutor for CliToolExecutor {
    fn execute(&mut self, tool_name: &str, input: &str) -> Result<String, ToolError> {
        if self
            .allowed_tools
            .as_ref()
            .is_some_and(|allowed| !allowed.contains(tool_name))
        {
            return Err(ToolError::new(format!(
                "tool `{tool_name}` is not enabled by the current --allowedTools setting"
            )));
        }
        let value = serde_json::from_str(input)
            .map_err(|error| ToolError::new(format!("invalid tool input JSON: {error}")))?;
        match self.tool_registry.execute(tool_name, &value) {
            Ok(output) => {
                if self.emit_output {
                    let markdown = format_tool_result(tool_name, &output, false);
                    self.renderer
                        .stream_markdown(&markdown, &mut io::stdout())
                        .map_err(|error| ToolError::new(error.to_string()))?;
                }
                Ok(output)
            }
            Err(error) => {
                if self.emit_output {
                    let markdown = format_tool_result(tool_name, &error, true);
                    self.renderer
                        .stream_markdown(&markdown, &mut io::stdout())
                        .map_err(|stream_error| ToolError::new(stream_error.to_string()))?;
                }
                Err(ToolError::new(error))
            }
        }
    }
}

fn permission_policy(mode: PermissionMode, tool_registry: &GlobalToolRegistry) -> PermissionPolicy {
    tool_registry.permission_specs(None).into_iter().fold(
        PermissionPolicy::new(mode),
        |policy, (name, required_permission)| {
            policy.with_tool_requirement(name, required_permission)
        },
    )
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
                            value: request_user_input_response_value(&UserInputResponse {
                                request_id: request_id.clone(),
                                content: content.clone(),
                                selected_option: selected_option.clone(),
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

#[allow(clippy::too_many_lines)]
fn print_help_to(out: &mut impl Write) -> io::Result<()> {
    writeln!(out, "openyak CLI v{VERSION}")?;
    writeln!(
        out,
        "  Interactive coding assistant for the current workspace."
    )?;
    writeln!(out)?;
    writeln!(out, "Quick start")?;
    writeln!(
        out,
        "  openyak                                  Start the interactive REPL"
    )?;
    writeln!(
        out,
        "  openyak \"summarize this repo\"            Run one prompt and exit"
    )?;
    writeln!(
        out,
        "  openyak prompt \"explain src/main.rs\"     Explicit one-shot prompt"
    )?;
    writeln!(
        out,
        "  openyak --resume SESSION.json /status    Inspect a saved session"
    )?;
    writeln!(out)?;
    writeln!(out, "Interactive essentials")?;
    writeln!(
        out,
        "  /help                                 Browse the full slash command map"
    )?;
    writeln!(
        out,
        "  /status                               Inspect session + workspace state"
    )?;
    writeln!(
        out,
        "  /model <name>                         Switch models mid-session"
    )?;
    writeln!(
        out,
        "  /permissions <mode>                   Adjust tool access"
    )?;
    writeln!(
        out,
        "  /plan [exit]                         Enter or leave explicit plan mode"
    )?;
    writeln!(
        out,
        "  Tab                                   Complete slash commands"
    )?;
    writeln!(
        out,
        "  /vim                                  Toggle modal editing"
    )?;
    writeln!(
        out,
        "  Shift+Enter / Ctrl+J                  Insert a newline"
    )?;
    writeln!(out)?;
    writeln!(out, "Commands")?;
    writeln!(
        out,
        "  openyak dump-manifests                   Print manifest counts, preferring upstream TS sources when available"
    )?;
    writeln!(
        out,
        "  openyak bootstrap-plan                   Print the bootstrap phase skeleton"
    )?;
    writeln!(
        out,
        "  openyak agents                           List configured agents"
    )?;
    writeln!(
        out,
        "  openyak skills [subcommand]              List or manage local skills"
    )?;
    writeln!(
        out,
        "  openyak system-prompt [--cwd PATH] [--date YYYY-MM-DD]"
    )?;
    writeln!(
        out,
        "  openyak login                            Start the configured OAuth login flow"
    )?;
    writeln!(
        out,
        "  openyak logout                           Clear saved OAuth credentials"
    )?;
    writeln!(
        out,
        "  openyak init                             Scaffold OPENYAK.md + local files"
    )?;
    writeln!(
        out,
        "  openyak onboard                          Run the explicit local onboarding wizard"
    )?;
    writeln!(
        out,
        "  openyak doctor                           Check local config, auth, and runtime health"
    )?;
    writeln!(
        out,
        "  openyak package-release [--output-dir PATH] [--binary PATH]  Stage a release artifact directory"
    )?;
    writeln!(
        out,
        "  openyak server [--bind HOST:PORT]        Run the local HTTP/SSE thread server"
    )?;
    writeln!(out)?;
    writeln!(out, "Flags")?;
    writeln!(
        out,
        "  --model MODEL                         Override the active model"
    )?;
    writeln!(
        out,
        "  --output-format FORMAT                Non-interactive output: text or json"
    )?;
    writeln!(
        out,
        "  --permission-mode MODE                Set read-only, workspace-write, or danger-full-access"
    )?;
    writeln!(
        out,
        "  --dangerously-skip-permissions        Skip all permission checks"
    )?;
    writeln!(
        out,
        "  --allowedTools TOOLS                  Restrict enabled tools (repeatable; comma-separated aliases supported)"
    )?;
    writeln!(
        out,
        "  --version, -V                         Print version and build information"
    )?;
    writeln!(out)?;
    writeln!(out, "Slash command reference")?;
    writeln!(out, "{}", render_slash_command_help())?;
    writeln!(out)?;
    let resume_commands = resume_supported_slash_commands()
        .into_iter()
        .map(|spec| match spec.argument_hint {
            Some(argument_hint) => format!("/{} {}", spec.name, argument_hint),
            None => format!("/{}", spec.name),
        })
        .collect::<Vec<_>>()
        .join(", ");
    writeln!(out, "Resume-safe commands: {resume_commands}")?;
    writeln!(out, "Examples")?;
    writeln!(out, "  openyak --model opus \"summarize this repo\"")?;
    writeln!(
        out,
        "  openyak --output-format json prompt \"explain src/main.rs\""
    )?;
    writeln!(
        out,
        "  openyak --allowedTools read,glob \"summarize Cargo.toml\""
    )?;
    writeln!(
        out,
        "  openyak --resume session.json /status /diff /export notes.txt"
    )?;
    writeln!(out, "  openyak agents")?;
    writeln!(out, "  openyak /skills")?;
    writeln!(out, "  openyak login")?;
    writeln!(out, "  openyak init")?;
    writeln!(out, "  openyak onboard")?;
    writeln!(out, "  openyak doctor")?;
    writeln!(out, "  openyak package-release --output-dir dist")?;
    writeln!(out, "  openyak server --bind 127.0.0.1:0")?;
    Ok(())
}

fn print_help(topic: HelpTopic) {
    let _ = match topic {
        HelpTopic::Root => print_help_to(&mut io::stdout()),
        _ => writeln!(io::stdout(), "{}", render_help_topic(topic)),
    };
}

fn render_help_topic(topic: HelpTopic) -> &'static str {
    match topic {
        HelpTopic::Root => "openyak CLI help",
        HelpTopic::DumpManifests => {
            "Usage: openyak dump-manifests\n\nPrint manifest counts, preferring upstream TypeScript sources when available."
        }
        HelpTopic::BootstrapPlan => {
            "Usage: openyak bootstrap-plan\n\nPrint the bootstrap phase skeleton."
        }
        HelpTopic::SystemPrompt => {
            "Usage: openyak system-prompt [--cwd PATH] [--date YYYY-MM-DD]\n\nRender the assembled system prompt for a workspace."
        }
        HelpTopic::Login => {
            "Usage: openyak login\n\nStart the configured OAuth login flow using settings.oauth."
        }
        HelpTopic::Logout => {
            "Usage: openyak logout\n\nClear saved OAuth credentials from the configured storage backends."
        }
        HelpTopic::Init => {
            "Usage: openyak init\n\nScaffold OPENYAK.md, .openyak.json, .openyak/, and recommended local gitignore entries."
        }
        HelpTopic::Onboard => {
            "Usage: openyak onboard\n\nRun the explicit interactive onboarding wizard.\nThe flow is local-only, reuses `openyak init`, persisted user-model setup, provider-aware auth guidance, and `openyak doctor`, and exits safely without writes in non-interactive terminals."
        }
        HelpTopic::Doctor => {
            "Usage: openyak doctor\n\nRun local read-only health checks for config loading, OAuth setup, active model auth bootstrap, and GitHub CLI availability."
        }
        HelpTopic::PackageRelease => {
            "Usage: openyak package-release [--output-dir PATH] [--binary PATH]\n\nStage a release artifact directory containing the packaged openyak binary plus generated install metadata.\nBy default the command packages the currently running openyak executable into ./dist."
        }
        HelpTopic::Server => {
            "Usage: openyak server [--bind HOST:PORT]\n\nRun the local HTTP/SSE thread server backed by the `server` crate.\nThe server exposes the local `/v1/threads` protocol plus legacy `/sessions` compatibility routes, persists thread state in the workspace `.openyak/state.sqlite3` SQLite store, prints the actual bound address on startup, and keeps serving until interrupted."
        }
        HelpTopic::Prompt => {
            "Usage: openyak prompt <text>\n       openyak -p <text>\n\nRun one non-interactive prompt and exit.\nIf the model requests structured follow-up input, this mode fails explicitly instead of guessing a reply."
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        browser_open_commands, build_system_prompt_for_cwd_with_date, configured_oauth_config,
        describe_tool_progress, filter_tool_specs, format_compact_report, format_cost_report,
        format_internal_prompt_progress_line, format_model_report, format_model_switch_report,
        format_permissions_report, format_permissions_switch_report,
        format_plan_mode_already_active_report, format_plan_mode_disabled_report,
        format_plan_mode_enabled_report, format_plan_mode_not_active_report,
        format_plan_permissions_blocked_report, format_resume_report, format_status_report,
        format_tool_call_start, format_tool_result, git_args_excluding_local_artifacts,
        initialize_repo, load_session_from_reference, normalize_permission_mode, parse_args,
        parse_commit_push_pr_draft, parse_git_status_metadata, parse_manual_oauth_callback_input,
        parse_user_input_submission, permission_policy, print_help_to, push_output_block,
        render_config_report, render_diff_report, render_git_command_requires_repo,
        render_help_topic, render_last_tool_debug_report, render_memory_report, render_repl_help,
        render_unknown_repl_command, resolve_effective_model, resolve_model_alias,
        response_to_events, resume_supported_slash_commands, run_resume_command, sessions_dir,
        slash_command_completion_candidates, stage_release_artifact, status_context,
        status_context_or_fallback_for_cwd, summarize_command_stderr, CliAction, CliOutputFormat,
        CliUserInputPrompter, DefaultRuntimeClient, DoctorCheckStatus, HelpTopic,
        InternalPromptProgressEvent, InternalPromptProgressState, SlashCommand, StatusUsage,
        ThreadServerInfoGuard, DEFAULT_MODEL, DEFAULT_RELEASE_OUTPUT_DIR, DEFAULT_SERVER_BIND,
        REQUEST_USER_INPUT_TOOL_NAME, THREAD_SERVER_INFO_FILENAME, VERSION,
    };
    use api::{InputContentBlock, MessageResponse, OutputContentBlock, ProviderKind, Usage};
    use plugins::{PluginHooks, PluginTool, PluginToolDefinition, PluginToolPermission};
    use runtime::{
        resolve_command_path, AssistantEvent, CompactionSummaryMode, ContentBlock,
        ConversationMessage, MessageRole, PermissionMode, Session, SessionAccountingStatus,
        SessionTelemetry, UserInputOutcome, UserInputPrompter, UserInputRequest,
    };
    use serde_json::json;
    use std::ffi::OsString;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::time::Duration;
    use tools::GlobalToolRegistry;

    struct CurrentDirGuard {
        original: PathBuf,
    }

    impl CurrentDirGuard {
        fn set(path: &std::path::Path) -> Self {
            let original = std::env::current_dir().expect("current dir");
            std::env::set_current_dir(path).expect("set current dir");
            Self { original }
        }
    }

    impl Drop for CurrentDirGuard {
        fn drop(&mut self) {
            let _ = std::env::set_current_dir(&self.original);
        }
    }

    fn registry_with_plugin_tool() -> GlobalToolRegistry {
        GlobalToolRegistry::with_plugin_tools(vec![PluginTool::new(
            "plugin-demo@external",
            "plugin-demo",
            PluginToolDefinition {
                name: "plugin_echo".to_string(),
                description: Some("Echo plugin payload".to_string()),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "message": { "type": "string" }
                    },
                    "required": ["message"],
                    "additionalProperties": false
                }),
            },
            "echo".to_string(),
            Vec::new(),
            PluginToolPermission::WorkspaceWrite,
            None,
        )])
        .expect("plugin tool registry should build")
    }

    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        crate::test_env_lock()
    }

    struct EnvVarGuard {
        key: &'static str,
        original: Option<OsString>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: Option<&str>) -> Self {
            let original = std::env::var_os(key);
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
            Self { key, original }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            match &self.original {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
    }

    fn unique_temp_dir(prefix: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "{prefix}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock should be after unix epoch")
                .as_nanos()
        ))
    }

    fn write_fake_command(dir: &Path, name: &str) -> PathBuf {
        let path = if cfg!(windows) {
            dir.join(format!("{name}.cmd"))
        } else {
            dir.join(name)
        };
        fs::write(&path, "@echo off\r\n").expect("fake command should write");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let mut permissions = fs::metadata(&path)
                .expect("fake command metadata should load")
                .permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&path, permissions)
                .expect("fake command permissions should update");
        }
        path
    }

    fn doctor_check<'a>(report: &'a super::DoctorReport, name: &str) -> &'a super::DoctorCheck {
        report
            .checks
            .iter()
            .find(|check| check.name == name)
            .unwrap_or_else(|| panic!("doctor check `{name}` should exist"))
    }

    fn test_live_cli(permission_mode: PermissionMode) -> super::LiveCli {
        super::LiveCli::new("sonnet".to_string(), false, None, permission_mode)
            .expect("test live cli should initialize")
    }

    fn write_saved_session(name: &str) -> PathBuf {
        let path = sessions_dir()
            .expect("sessions dir should resolve")
            .join(format!("{name}.json"));
        Session::new()
            .save_to_path(&path)
            .expect("session file should save");
        path
    }

    #[test]
    fn defaults_to_repl_when_no_args() {
        assert_eq!(
            parse_args(&[]).expect("args should parse"),
            CliAction::Repl {
                model: None,
                allowed_tools: None,
                permission_mode: PermissionMode::DangerFullAccess,
            }
        );
    }

    #[test]
    fn parses_prompt_subcommand() {
        let args = vec![
            "prompt".to_string(),
            "hello".to_string(),
            "world".to_string(),
        ];
        assert_eq!(
            parse_args(&args).expect("args should parse"),
            CliAction::Prompt {
                prompt: "hello world".to_string(),
                model: None,
                output_format: CliOutputFormat::Text,
                allowed_tools: None,
                permission_mode: PermissionMode::DangerFullAccess,
            }
        );
    }

    #[test]
    fn parses_bare_prompt_and_json_output_flag() {
        let args = vec![
            "--output-format=json".to_string(),
            "--model".to_string(),
            "custom-opus".to_string(),
            "explain".to_string(),
            "this".to_string(),
        ];
        assert_eq!(
            parse_args(&args).expect("args should parse"),
            CliAction::Prompt {
                prompt: "explain this".to_string(),
                model: Some("custom-opus".to_string()),
                output_format: CliOutputFormat::Json,
                allowed_tools: None,
                permission_mode: PermissionMode::DangerFullAccess,
            }
        );
    }

    #[test]
    fn resolves_model_aliases_in_args() {
        let args = vec![
            "--model".to_string(),
            "opus".to_string(),
            "explain".to_string(),
            "this".to_string(),
        ];
        assert_eq!(
            parse_args(&args).expect("args should parse"),
            CliAction::Prompt {
                prompt: "explain this".to_string(),
                model: Some("claude-opus-4-6".to_string()),
                output_format: CliOutputFormat::Text,
                allowed_tools: None,
                permission_mode: PermissionMode::DangerFullAccess,
            }
        );
    }

    #[test]
    fn resolves_known_model_aliases() {
        assert_eq!(resolve_model_alias("opus"), "claude-opus-4-6");
        assert_eq!(resolve_model_alias("sonnet"), "claude-sonnet-4-6");
        assert_eq!(resolve_model_alias("haiku"), "claude-haiku-4-5-20251213");
        assert_eq!(resolve_model_alias("custom-opus"), "custom-opus");
    }

    #[test]
    fn default_runtime_client_uses_openai_provider_for_gpt_models() {
        let _lock = env_lock();
        let temp_dir = std::env::temp_dir().join(format!(
            "openyak-cli-openai-provider-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock should be after unix epoch")
                .as_nanos()
        ));
        fs::create_dir_all(&temp_dir).expect("temp config dir should be created");

        let _openyak_config_home = EnvVarGuard::set(
            "OPENYAK_CONFIG_HOME",
            Some(temp_dir.to_string_lossy().as_ref()),
        );
        let _openai_api_key = EnvVarGuard::set("OPENAI_API_KEY", Some("openai-test-key"));
        let _openai_base_url =
            EnvVarGuard::set("OPENAI_BASE_URL", Some("https://example.openai.test/v1"));
        let _anthropic_api_key = EnvVarGuard::set("ANTHROPIC_API_KEY", None);
        let _anthropic_auth_token = EnvVarGuard::set("ANTHROPIC_AUTH_TOKEN", None);

        let mut client = DefaultRuntimeClient::new(
            "gpt-5.3".to_string(),
            false,
            false,
            None,
            GlobalToolRegistry::builtin(),
            None,
        )
        .expect("runtime client should initialize");

        client.ensure_api_auth().expect("provider should resolve");

        assert_eq!(
            client
                .client
                .as_ref()
                .expect("api client should initialize")
                .provider_kind(),
            ProviderKind::OpenAi
        );

        fs::remove_dir_all(temp_dir).expect("temp config dir should be removable");
    }

    #[test]
    fn configured_oauth_config_requires_explicit_provider_fields() {
        let _env_lock = env_lock();
        let root = unique_temp_dir("openyak-cli-oauth-override");
        fs::create_dir_all(root.join(".openyak")).expect("config dir");
        let isolated_openyak_home = root.join("isolated-openyak-home");
        fs::create_dir_all(&isolated_openyak_home).expect("isolated openyak home");
        let isolated_openyak_home_env = isolated_openyak_home.to_string_lossy().into_owned();
        let _openyak_home =
            EnvVarGuard::set("OPENYAK_CONFIG_HOME", Some(&isolated_openyak_home_env));
        let _codex_home = EnvVarGuard::set("CODEX_HOME", None);
        fs::write(
            root.join(".openyak").join("settings.json"),
            "{\n  \"oauth\": {\n    \"callbackPort\": 4557\n  }\n}\n",
        )
        .expect("write settings");

        let config = runtime::ConfigLoader::default_for(&root)
            .load()
            .expect("config should load");
        let error =
            configured_oauth_config(&config).expect_err("partial oauth config should be rejected");

        assert!(error.contains("missing clientId, authorizeUrl, tokenUrl"));

        for _ in 0..5 {
            match fs::remove_dir_all(&root) {
                Ok(()) => return,
                Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                Err(error) => panic!("cleanup temp dir: {error}"),
            }
        }
        crate::cleanup_temp_dir(&root);
    }

    #[test]
    fn configured_oauth_config_accepts_complete_oauth_config() {
        let _env_lock = env_lock();
        let root = unique_temp_dir("openyak-cli-oauth-complete");
        fs::create_dir_all(root.join(".openyak")).expect("config dir");
        let isolated_openyak_home = root.join("isolated-openyak-home");
        fs::create_dir_all(&isolated_openyak_home).expect("isolated openyak home");
        let isolated_openyak_home_env = isolated_openyak_home.to_string_lossy().into_owned();
        let _openyak_home =
            EnvVarGuard::set("OPENYAK_CONFIG_HOME", Some(&isolated_openyak_home_env));
        let _codex_home = EnvVarGuard::set("CODEX_HOME", None);
        fs::write(
            root.join(".openyak").join("settings.json"),
            "{\n  \"oauth\": {\n    \"clientId\": \"runtime-client\",\n    \"authorizeUrl\": \"https://oauth.example.test/authorize\",\n    \"tokenUrl\": \"https://oauth.example.test/token\",\n    \"manualRedirectUrl\": \"https://oauth.example.test/callback\",\n    \"callbackPort\": 4557,\n    \"scopes\": [\"scope:a\"]\n  }\n}\n",
        )
        .expect("write settings");

        let config = runtime::ConfigLoader::default_for(&root)
            .load()
            .expect("config should load");
        let oauth = configured_oauth_config(&config)
            .expect("complete oauth config should parse")
            .expect("oauth config should exist");

        assert_eq!(oauth.client_id, "runtime-client");
        assert_eq!(oauth.authorize_url, "https://oauth.example.test/authorize");
        assert_eq!(oauth.token_url, "https://oauth.example.test/token");
        assert_eq!(
            oauth.manual_redirect_url.as_deref(),
            Some("https://oauth.example.test/callback")
        );
        assert_eq!(oauth.callback_port, Some(4557));
        assert_eq!(oauth.scopes, vec!["scope:a".to_string()]);

        crate::cleanup_temp_dir(&root);
    }

    #[test]
    fn manual_oauth_callback_parser_accepts_query_and_full_url() {
        let params = parse_manual_oauth_callback_input(
            "https://oauth.example.test/callback?code=abc&state=xyz",
            "https://oauth.example.test/callback",
        )
        .expect("full callback url should parse");
        assert_eq!(params.code.as_deref(), Some("abc"));
        assert_eq!(params.state.as_deref(), Some("xyz"));

        let params = parse_manual_oauth_callback_input(
            "code=def&state=uvw",
            "https://oauth.example.test/callback",
        )
        .expect("query string should parse");
        assert_eq!(params.code.as_deref(), Some("def"));
        assert_eq!(params.state.as_deref(), Some("uvw"));
    }

    #[test]
    fn manual_oauth_callback_parser_rejects_mismatched_redirect_base() {
        let error = parse_manual_oauth_callback_input(
            "https://evil.example.test/callback?code=abc&state=xyz",
            "https://oauth.example.test/callback",
        )
        .expect_err("mismatched callback url should fail");
        assert!(error.contains("manualRedirectUrl"));
    }

    #[test]
    fn parses_version_flags_without_initializing_prompt_mode() {
        assert_eq!(
            parse_args(&["--version".to_string()]).expect("args should parse"),
            CliAction::Version
        );
        assert_eq!(
            parse_args(&["-V".to_string()]).expect("args should parse"),
            CliAction::Version
        );
    }

    #[test]
    fn parses_permission_mode_flag() {
        let args = vec!["--permission-mode=read-only".to_string()];
        assert_eq!(
            parse_args(&args).expect("args should parse"),
            CliAction::Repl {
                model: None,
                allowed_tools: None,
                permission_mode: PermissionMode::ReadOnly,
            }
        );
    }

    #[test]
    fn parses_allowed_tools_flags_with_aliases_and_lists() {
        let args = vec![
            "--allowedTools".to_string(),
            "read,glob".to_string(),
            "--allowed-tools=write_file".to_string(),
        ];
        assert_eq!(
            parse_args(&args).expect("args should parse"),
            CliAction::Repl {
                model: None,
                allowed_tools: Some(
                    ["glob_search", "read_file", "write_file"]
                        .into_iter()
                        .map(str::to_string)
                        .collect()
                ),
                permission_mode: PermissionMode::DangerFullAccess,
            }
        );
    }

    #[test]
    fn prompt_help_flag_routes_to_help() {
        assert_eq!(
            parse_args(&["prompt".to_string(), "--help".to_string()]).expect("args should parse"),
            CliAction::Help(HelpTopic::Prompt)
        );
        assert_eq!(
            parse_args(&["-p".to_string(), "--help".to_string()]).expect("args should parse"),
            CliAction::Help(HelpTopic::Prompt)
        );
    }

    #[test]
    fn rejects_unknown_allowed_tools() {
        let error = parse_args(&["--allowedTools".to_string(), "teleport".to_string()])
            .expect_err("tool should be rejected");
        assert!(error.contains("unsupported tool in --allowedTools: teleport"));
    }

    #[test]
    fn rejects_missing_allowed_tools_value_when_next_token_is_cli_command() {
        let error = parse_args(&["--allowedTools".to_string(), "prompt".to_string()])
            .expect_err("missing allowedTools value should be rejected");
        assert!(error.contains("missing value for --allowedTools"));
    }

    #[test]
    fn parses_system_prompt_options() {
        let args = vec![
            "system-prompt".to_string(),
            "--cwd".to_string(),
            "/tmp/project".to_string(),
            "--date".to_string(),
            "2026-04-01".to_string(),
        ];
        assert_eq!(
            parse_args(&args).expect("args should parse"),
            CliAction::PrintSystemPrompt {
                cwd: PathBuf::from("/tmp/project"),
                date: "2026-04-01".to_string(),
            }
        );
    }

    #[test]
    fn parses_system_prompt_help_flag() {
        assert_eq!(
            parse_args(&["system-prompt".to_string(), "--help".to_string()])
                .expect("args should parse"),
            CliAction::Help(HelpTopic::SystemPrompt)
        );
    }

    #[test]
    fn parses_subcommand_help_flags() {
        assert_eq!(
            parse_args(&["dump-manifests".to_string(), "--help".to_string()])
                .expect("dump-manifests help should parse"),
            CliAction::Help(HelpTopic::DumpManifests)
        );
        assert_eq!(
            parse_args(&["bootstrap-plan".to_string(), "--help".to_string()])
                .expect("bootstrap-plan help should parse"),
            CliAction::Help(HelpTopic::BootstrapPlan)
        );
        assert_eq!(
            parse_args(&["login".to_string(), "--help".to_string()])
                .expect("login help should parse"),
            CliAction::Help(HelpTopic::Login)
        );
        assert_eq!(
            parse_args(&["logout".to_string(), "--help".to_string()])
                .expect("logout help should parse"),
            CliAction::Help(HelpTopic::Logout)
        );
        assert_eq!(
            parse_args(&["init".to_string(), "--help".to_string()])
                .expect("init help should parse"),
            CliAction::Help(HelpTopic::Init)
        );
        assert_eq!(
            parse_args(&["onboard".to_string(), "--help".to_string()])
                .expect("onboard help should parse"),
            CliAction::Help(HelpTopic::Onboard)
        );
        assert_eq!(
            parse_args(&["doctor".to_string(), "--help".to_string()])
                .expect("doctor help should parse"),
            CliAction::Help(HelpTopic::Doctor)
        );
        assert_eq!(
            parse_args(&["package-release".to_string(), "--help".to_string()])
                .expect("package-release help should parse"),
            CliAction::Help(HelpTopic::PackageRelease)
        );
        assert_eq!(
            parse_args(&["server".to_string(), "--help".to_string()])
                .expect("server help should parse"),
            CliAction::Help(HelpTopic::Server)
        );
    }

    #[test]
    fn help_topics_render_targeted_usage() {
        assert!(render_help_topic(HelpTopic::Prompt).contains("Usage: openyak prompt <text>"));
        assert!(render_help_topic(HelpTopic::Prompt).contains("fails explicitly"));
        assert!(render_help_topic(HelpTopic::Login).contains("Usage: openyak login"));
        assert!(
            render_help_topic(HelpTopic::DumpManifests).contains("Usage: openyak dump-manifests")
        );
        assert!(render_help_topic(HelpTopic::Onboard).contains("Usage: openyak onboard"));
        assert!(render_help_topic(HelpTopic::Onboard).contains("interactive onboarding wizard"));
        assert!(render_help_topic(HelpTopic::Doctor).contains("Usage: openyak doctor"));
        assert!(
            render_help_topic(HelpTopic::PackageRelease).contains("Usage: openyak package-release")
        );
        assert!(render_help_topic(HelpTopic::Server).contains("Usage: openyak server"));
    }

    #[test]
    fn system_prompt_args_default_to_injected_runtime_date() {
        let action =
            super::parse_system_prompt_args_with_default_date(&[], "2030-02-03".to_string())
                .expect("args should parse");
        match action {
            CliAction::PrintSystemPrompt { date, .. } => assert_eq!(date, "2030-02-03"),
            other => panic!("unexpected action: {other:?}"),
        }
    }

    #[test]
    fn parses_login_and_logout_subcommands() {
        assert_eq!(
            parse_args(&["login".to_string()]).expect("login should parse"),
            CliAction::Login
        );
        assert_eq!(
            parse_args(&["logout".to_string()]).expect("logout should parse"),
            CliAction::Logout
        );
        assert_eq!(
            parse_args(&["init".to_string()]).expect("init should parse"),
            CliAction::Init
        );
        assert_eq!(
            parse_args(&["onboard".to_string()]).expect("onboard should parse"),
            CliAction::Onboard {
                model: None,
                output_format: CliOutputFormat::Text,
            }
        );
        assert_eq!(
            parse_args(&["doctor".to_string()]).expect("doctor should parse"),
            CliAction::Doctor
        );
        assert_eq!(
            parse_args(&["agents".to_string()]).expect("agents should parse"),
            CliAction::Agents { args: None }
        );
        assert_eq!(
            parse_args(&["skills".to_string()]).expect("skills should parse"),
            CliAction::Skills { args: None }
        );
        assert_eq!(
            parse_args(&["agents".to_string(), "--help".to_string()])
                .expect("agents help should parse"),
            CliAction::Agents {
                args: Some("--help".to_string())
            }
        );
        assert_eq!(
            parse_args(&["package-release".to_string()]).expect("package-release should parse"),
            CliAction::PackageRelease {
                binary: None,
                output_dir: PathBuf::from(DEFAULT_RELEASE_OUTPUT_DIR),
            }
        );
        assert_eq!(
            parse_args(&[
                "package-release".to_string(),
                "--binary".to_string(),
                "target/release/openyak.exe".to_string(),
                "--output-dir".to_string(),
                "artifacts".to_string(),
            ])
            .expect("package-release with args should parse"),
            CliAction::PackageRelease {
                binary: Some(PathBuf::from("target/release/openyak.exe")),
                output_dir: PathBuf::from("artifacts"),
            }
        );
        assert_eq!(
            parse_args(&["server".to_string()]).expect("server should parse"),
            CliAction::Server {
                bind: DEFAULT_SERVER_BIND.to_string(),
            }
        );
        assert_eq!(
            parse_args(&["server".to_string(), "--bind=127.0.0.1:0".to_string()])
                .expect("server with bind should parse"),
            CliAction::Server {
                bind: "127.0.0.1:0".to_string(),
            }
        );
    }

    #[test]
    fn parses_direct_agents_and_skills_slash_commands() {
        assert_eq!(
            parse_args(&["/agents".to_string()]).expect("/agents should parse"),
            CliAction::Agents { args: None }
        );
        assert_eq!(
            parse_args(&["/skills".to_string()]).expect("/skills should parse"),
            CliAction::Skills { args: None }
        );
        assert_eq!(
            parse_args(&["/skills".to_string(), "help".to_string()])
                .expect("/skills help should parse"),
            CliAction::Skills {
                args: Some("help".to_string())
            }
        );
        let error = parse_args(&["/status".to_string()])
            .expect_err("/status should remain REPL-only when invoked directly");
        assert!(error.contains("Direct slash command unavailable"));
        assert!(error.contains("/status"));
    }

    #[test]
    fn preserves_subcommand_flags_inside_skills_arguments() {
        assert_eq!(
            parse_args(&[
                "skills".to_string(),
                "install".to_string(),
                "release-checklist".to_string(),
                "--version".to_string(),
                "1.0.0".to_string(),
            ])
            .expect("skills install should parse"),
            CliAction::Skills {
                args: Some("install release-checklist --version 1.0.0".to_string())
            }
        );
    }

    #[test]
    fn parses_resume_flag_with_slash_command() {
        let args = vec![
            "--resume".to_string(),
            "session.json".to_string(),
            "/compact".to_string(),
        ];
        assert_eq!(
            parse_args(&args).expect("args should parse"),
            CliAction::ResumeSession {
                session_path: PathBuf::from("session.json"),
                commands: vec!["/compact".to_string()],
            }
        );
    }

    #[test]
    fn parses_resume_flag_with_multiple_slash_commands() {
        let args = vec![
            "--resume".to_string(),
            "session.json".to_string(),
            "/status".to_string(),
            "/compact".to_string(),
            "/cost".to_string(),
        ];
        assert_eq!(
            parse_args(&args).expect("args should parse"),
            CliAction::ResumeSession {
                session_path: PathBuf::from("session.json"),
                commands: vec![
                    "/status".to_string(),
                    "/compact".to_string(),
                    "/cost".to_string(),
                ],
            }
        );
    }

    #[test]
    fn parses_resume_flag_with_slash_command_arguments() {
        let args = vec![
            "--resume".to_string(),
            "session.json".to_string(),
            "/status".to_string(),
            "/config".to_string(),
            "env".to_string(),
            "/export".to_string(),
            "notes.txt".to_string(),
            "/clear".to_string(),
            "--confirm".to_string(),
        ];
        assert_eq!(
            parse_args(&args).expect("args should parse"),
            CliAction::ResumeSession {
                session_path: PathBuf::from("session.json"),
                commands: vec![
                    "/status".to_string(),
                    "/config env".to_string(),
                    "/export notes.txt".to_string(),
                    "/clear --confirm".to_string(),
                ],
            }
        );
    }

    #[test]
    fn parses_resume_export_with_absolute_posix_path() {
        let args = vec![
            "--resume".to_string(),
            "session.json".to_string(),
            "/export".to_string(),
            "/tmp/notes.txt".to_string(),
        ];
        assert_eq!(
            parse_args(&args).expect("args should parse"),
            CliAction::ResumeSession {
                session_path: PathBuf::from("session.json"),
                commands: vec!["/export /tmp/notes.txt".to_string()],
            }
        );
    }

    #[test]
    fn loads_resumed_session_from_managed_session_id() {
        let _lock = env_lock();
        let root = unique_temp_dir("openyak-cli-resume-managed-session");
        fs::create_dir_all(&root).expect("create root");
        {
            let _cwd = CurrentDirGuard::set(&root);
            let id = format!("resume-test-{}", super::generate_session_id());
            let path = sessions_dir()
                .expect("sessions dir should resolve")
                .join(format!("{id}.json"));
            Session::new()
                .save_to_path(&path)
                .expect("session file should save");

            let (handle, session) = load_session_from_reference(std::path::Path::new(&id))
                .expect("session should load");
            assert_eq!(handle.id, id);
            assert_eq!(handle.path, path);
            assert!(session.messages.is_empty());

            fs::remove_file(&path).expect("test session file should clean up");
        }
        crate::cleanup_temp_dir(&root);
    }

    #[test]
    fn summarizes_command_stderr_to_first_non_empty_line() {
        let stderr = b"\nwarning: Not a git repository. Use --no-index to compare two paths outside a working tree\nusage: git diff --no-index [<options>] <path> <path>\n";
        assert_eq!(
            summarize_command_stderr(stderr),
            "warning: Not a git repository. Use --no-index to compare two paths outside a working tree"
        );
    }

    #[test]
    fn windows_browser_launcher_avoids_cmd_start() {
        if !cfg!(target_os = "windows") {
            return;
        }

        let commands = browser_open_commands("https://example.com/oauth?foo=1&bar=2");
        assert_eq!(commands[0].0, "explorer");
        assert_eq!(
            commands[0].1,
            vec!["https://example.com/oauth?foo=1&bar=2".to_string()]
        );
        assert!(commands.iter().all(|(program, _)| *program != "cmd"));
    }

    #[test]
    fn runtime_client_starts_without_preloading_auth() {
        let client = DefaultRuntimeClient::new(
            DEFAULT_MODEL.to_string(),
            true,
            false,
            None,
            GlobalToolRegistry::builtin(),
            None,
        )
        .expect("runtime client should initialize without credentials");
        assert!(client.client.is_none());
    }

    #[test]
    fn filtered_tool_specs_respect_allowlist() {
        let allowed = ["read_file", "grep_search"]
            .into_iter()
            .map(str::to_string)
            .collect();
        let filtered = filter_tool_specs(&GlobalToolRegistry::builtin(), Some(&allowed));
        let names = filtered
            .into_iter()
            .map(|spec| spec.name)
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            vec!["read_file", "grep_search", REQUEST_USER_INPUT_TOOL_NAME]
        );
    }

    #[test]
    fn filtered_tool_specs_include_plugin_tools() {
        let filtered = filter_tool_specs(&registry_with_plugin_tool(), None);
        let names = filtered
            .into_iter()
            .map(|definition| definition.name)
            .collect::<Vec<_>>();
        assert!(names.contains(&"bash".to_string()));
        assert!(names.contains(&"plugin_echo".to_string()));
    }

    #[test]
    fn permission_policy_uses_plugin_tool_permissions() {
        let policy = permission_policy(PermissionMode::ReadOnly, &registry_with_plugin_tool());
        let required = policy.required_mode_for("plugin_echo");
        assert_eq!(required, PermissionMode::WorkspaceWrite);
    }

    #[test]
    fn merge_plugin_hooks_preserves_base_hooks_and_adds_plugin_hooks() {
        let base =
            runtime::RuntimeFeatureConfig::default().with_hooks(runtime::RuntimeHookConfig::new(
                vec!["base-pre".to_string()],
                vec!["base-post".to_string()],
            ));

        let merged = super::merge_plugin_hooks(
            base,
            PluginHooks {
                pre_tool_use: vec!["base-pre".to_string(), "plugin-pre".to_string()],
                post_tool_use: vec!["plugin-post".to_string()],
            },
        );

        assert_eq!(
            merged.hooks().pre_tool_use(),
            &["base-pre".to_string(), "plugin-pre".to_string()]
        );
        assert_eq!(
            merged.hooks().post_tool_use(),
            &["base-post".to_string(), "plugin-post".to_string()]
        );
    }

    #[test]
    fn shared_help_uses_resume_annotation_copy() {
        let help = commands::render_slash_command_help();
        assert!(help.contains("Slash commands"));
        assert!(help.contains("Tab completes commands inside the REPL."));
        assert!(help.contains("available via openyak --resume SESSION.json"));
    }

    #[test]
    fn repl_help_includes_shared_commands_and_exit() {
        let help = render_repl_help();
        assert!(help.contains("Interactive REPL"));
        assert!(help.contains("/help"));
        assert!(help.contains("/status"));
        assert!(help.contains("/model [model]"));
        assert!(help.contains("/permissions [read-only|workspace-write|danger-full-access]"));
        assert!(help.contains("/plan [exit]"));
        assert!(help.contains("/clear [--confirm]"));
        assert!(help.contains("/cost"));
        assert!(help.contains("/resume <session-path>"));
        assert!(help.contains("/config [env|hooks|model|plugins]"));
        assert!(help.contains("/memory"));
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
        assert!(help.contains("/skills"));
        assert!(help.contains("/exit"));
        assert!(help.contains("Tab cycles slash command matches"));
    }

    #[test]
    fn completion_candidates_include_repl_only_exit_commands() {
        let candidates = slash_command_completion_candidates();
        assert!(candidates.contains(&"/help".to_string()));
        assert!(candidates.contains(&"/plan".to_string()));
        assert!(candidates.contains(&"/vim".to_string()));
        assert!(candidates.contains(&"/exit".to_string()));
        assert!(candidates.contains(&"/quit".to_string()));
    }

    #[test]
    fn unknown_repl_command_suggestions_include_repl_shortcuts() {
        let rendered = render_unknown_repl_command("exi");
        assert!(rendered.contains("Unknown slash command"));
        assert!(rendered.contains("/exit"));
        assert!(rendered.contains("/help"));
    }

    #[test]
    fn resume_supported_command_list_matches_expected_surface() {
        let names = resume_supported_slash_commands()
            .into_iter()
            .map(|spec| spec.name)
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            vec![
                "help", "status", "compact", "clear", "cost", "config", "memory", "init", "diff",
                "version", "export", "agents", "skills",
            ]
        );
    }

    #[test]
    fn resume_report_uses_sectioned_layout() {
        let report = format_resume_report("session.json", 14, 6);
        assert!(report.contains("Session resumed"));
        assert!(report.contains("Session file     session.json"));
        assert!(report.contains("History          14 messages · 6 turns"));
        assert!(report.contains("/status · /diff · /export"));
    }

    #[test]
    fn compact_report_uses_structured_output() {
        let compacted = format_compact_report(&runtime::CompactionResult {
            summary: "summary".to_string(),
            formatted_summary: "Summary:\nsummary".to_string(),
            compacted_session: Session {
                version: 1,
                messages: vec![ConversationMessage::assistant(vec![ContentBlock::Text {
                    text: "recent".to_string(),
                }])],
                telemetry: Some(SessionTelemetry {
                    compacted_usage: runtime::TokenUsage::default(),
                    compacted_turns: 0,
                    accounting_status: SessionAccountingStatus::Complete,
                }),
            },
            removed_message_count: 8,
            summary_mode: CompactionSummaryMode::NewSummary,
            estimated_tokens_before: 120,
            estimated_tokens_after: 45,
            accounting_status: SessionAccountingStatus::Complete,
        });
        assert!(compacted.contains("Compact"));
        assert!(compacted.contains("Result           compacted"));
        assert!(compacted.contains("Messages removed 8"));
        assert!(compacted.contains("Summary mode     new summary"));
        assert!(compacted.contains("Token delta      75"));
        assert!(compacted.contains("Use /cost"));
        let skipped = format_compact_report(&runtime::CompactionResult {
            summary: String::new(),
            formatted_summary: String::new(),
            compacted_session: Session {
                version: 1,
                messages: vec![
                    ConversationMessage::user_text("hi"),
                    ConversationMessage::assistant(vec![ContentBlock::Text {
                        text: "ok".to_string(),
                    }]),
                    ConversationMessage::tool_result("1", "bash", "done", false),
                ],
                telemetry: None,
            },
            removed_message_count: 0,
            summary_mode: CompactionSummaryMode::Unchanged,
            estimated_tokens_before: 33,
            estimated_tokens_after: 33,
            accounting_status: SessionAccountingStatus::Complete,
        });
        assert!(skipped.contains("Result           skipped"));
        assert!(skipped.contains("Current tokens   33"));
    }

    #[test]
    fn cost_report_uses_sectioned_layout() {
        let report = format_cost_report(
            Some("claude-sonnet-4-6"),
            3,
            runtime::TokenUsage {
                input_tokens: 6,
                output_tokens: 2,
                cache_creation_input_tokens: 1,
                cache_read_input_tokens: 0,
            },
            runtime::TokenUsage {
                input_tokens: 20,
                output_tokens: 8,
                cache_creation_input_tokens: 3,
                cache_read_input_tokens: 1,
            },
            SessionAccountingStatus::Complete,
        );
        assert!(report.contains("Cost"));
        assert!(report.contains("Model            claude-sonnet-4-6"));
        assert!(report.contains("Turns            3"));
        assert!(report.contains("Accounting       complete"));
        assert!(report.contains("Latest turn"));
        assert!(report.contains("Input tokens     20"));
        assert!(report.contains("Output tokens    8"));
        assert!(report.contains("Cache create     3"));
        assert!(report.contains("Cache read       1"));
        assert!(report.contains("Total tokens     32"));
        assert!(report.contains("Cost breakdown"));
        assert!(report.contains("/compact"));
    }

    #[test]
    fn cost_report_marks_partial_legacy_accounting() {
        let report = format_cost_report(
            None,
            1,
            runtime::TokenUsage::default(),
            runtime::TokenUsage {
                input_tokens: 5,
                output_tokens: 3,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 1,
            },
            SessionAccountingStatus::PartialLegacyCompaction,
        );

        assert!(report.contains("Model            restored-session"));
        assert!(report.contains("Accounting       partial"));
        assert!(report.contains("legacy compacted history predates telemetry"));
    }

    #[test]
    fn resume_cost_report_preserves_compacted_session_accounting() {
        let root = unique_temp_dir("openyak-cli-resume-cost");
        fs::create_dir_all(&root).expect("temp dir should exist");
        let path = root.join("session.json");
        Session {
            version: 1,
            messages: vec![ConversationMessage::assistant_with_usage(
                vec![ContentBlock::Text {
                    text: "recent".to_string(),
                }],
                Some(runtime::TokenUsage {
                    input_tokens: 4,
                    output_tokens: 2,
                    cache_creation_input_tokens: 0,
                    cache_read_input_tokens: 1,
                }),
            )],
            telemetry: Some(SessionTelemetry {
                compacted_usage: runtime::TokenUsage {
                    input_tokens: 10,
                    output_tokens: 5,
                    cache_creation_input_tokens: 2,
                    cache_read_input_tokens: 1,
                },
                compacted_turns: 2,
                accounting_status: SessionAccountingStatus::Complete,
            }),
        }
        .save_to_path(&path)
        .expect("session should save");

        let session = Session::load_from_path(&path).expect("session should load");
        let outcome = run_resume_command(&path, &session, &SlashCommand::Cost)
            .expect("resume /cost should succeed");
        let message = outcome.message.expect("cost report");

        assert!(message.contains("Turns            3"));
        assert!(message.contains("Input tokens     14"));
        assert!(message.contains("Output tokens    7"));
        assert!(message.contains("Cache create     2"));
        assert!(message.contains("Cache read       2"));

        fs::remove_file(&path).expect("session file should clean up");
        crate::cleanup_temp_dir(&root);
    }

    #[test]
    fn permissions_report_uses_sectioned_layout() {
        let report = format_permissions_report("workspace-write", None);
        assert!(report.contains("Permissions"));
        assert!(report.contains("Active mode      workspace-write"));
        assert!(report.contains("Effect           Editing tools can modify files in the workspace"));
        assert!(report.contains("Modes"));
        assert!(report.contains("read-only          ○ available Read/search tools only"));
        assert!(report.contains("workspace-write    ● current   Edit files inside the workspace"));
        assert!(report.contains("danger-full-access ○ available Unrestricted tool access"));
    }

    #[test]
    fn permissions_report_surfaces_active_plan_mode() {
        let report = format_permissions_report("read-only", Some("workspace-write"));
        assert!(report.contains("Planning"));
        assert!(report.contains("State            active"));
        assert!(report.contains("Restore mode     workspace-write"));
        assert!(report.contains("Exit             /plan exit"));
        assert!(report.contains("/plan exit               Leave explicit plan mode first"));
        assert!(
            !report.contains("/permissions <mode>       Switch modes for subsequent tool calls")
        );
    }

    #[test]
    fn permissions_switch_report_is_structured() {
        let report = format_permissions_switch_report("read-only", "workspace-write");
        assert!(report.contains("Permissions updated"));
        assert!(report.contains("Previous mode    read-only"));
        assert!(report.contains("Active mode      workspace-write"));
        assert!(report.contains("Applies to       Subsequent tool calls in this REPL"));
    }

    #[test]
    fn plan_mode_reports_are_structured() {
        let enabled = format_plan_mode_enabled_report("workspace-write");
        assert!(enabled.contains("Plan mode enabled"));
        assert!(enabled.contains("Active mode      read-only"));
        assert!(enabled.contains("Restore mode     workspace-write"));
        assert!(enabled.contains("/plan exit to restore workspace-write"));

        let already_active = format_plan_mode_already_active_report("workspace-write");
        assert!(already_active.contains("Plan mode already active"));
        assert!(already_active.contains("Restore mode     workspace-write"));

        let disabled = format_plan_mode_disabled_report("workspace-write");
        assert!(disabled.contains("Plan mode disabled"));
        assert!(disabled.contains("Restored mode    workspace-write"));

        let inactive = format_plan_mode_not_active_report("workspace-write");
        assert!(inactive.contains("Plan mode inactive"));
        assert!(inactive.contains("Run /plan to enter explicit planning mode"));

        let blocked = format_plan_permissions_blocked_report("read-only", "workspace-write");
        assert!(blocked.contains("Plan mode requires an explicit exit"));
        assert!(blocked.contains("Active mode      read-only"));
        assert!(blocked.contains("Restore mode     workspace-write"));
        assert!(blocked.contains("Run /plan exit before changing /permissions"));
    }

    #[test]
    fn entering_plan_mode_switches_to_read_only_and_stays_out_of_session_payloads() {
        let _lock = env_lock();
        let root = unique_temp_dir("openyak-cli-plan-enter");
        fs::create_dir_all(&root).expect("root should exist");
        {
            let _cwd = CurrentDirGuard::set(&root);

            let mut cli = test_live_cli(PermissionMode::WorkspaceWrite);
            assert!(cli
                .enter_plan_mode()
                .expect("enter plan mode should succeed"));
            assert_eq!(cli.permission_mode, PermissionMode::ReadOnly);
            assert_eq!(cli.plan_restore_mode, Some(PermissionMode::WorkspaceWrite));

            cli.persist_session()
                .expect("plan-mode session should persist without extra fields");
            let saved = fs::read_to_string(&cli.session.path).expect("session file should read");
            assert!(!saved.contains("plan_mode"));
            assert!(!saved.contains("restore_mode"));
        }

        crate::cleanup_temp_dir(&root);
    }

    #[test]
    fn entering_plan_mode_is_idempotent_and_preserves_restore_target() {
        let _lock = env_lock();
        let root = unique_temp_dir("openyak-cli-plan-idempotent");
        fs::create_dir_all(&root).expect("root should exist");
        {
            let _cwd = CurrentDirGuard::set(&root);

            let mut cli = test_live_cli(PermissionMode::DangerFullAccess);
            assert!(cli.enter_plan_mode().expect("first enter should succeed"));
            assert!(!cli
                .enter_plan_mode()
                .expect("second enter should be a no-op"));
            assert_eq!(cli.permission_mode, PermissionMode::ReadOnly);
            assert_eq!(
                cli.plan_restore_mode,
                Some(PermissionMode::DangerFullAccess)
            );
        }

        crate::cleanup_temp_dir(&root);
    }

    #[test]
    fn plan_mode_blocks_raw_permission_switches_until_exit() {
        let _lock = env_lock();
        let root = unique_temp_dir("openyak-cli-plan-permissions");
        fs::create_dir_all(&root).expect("root should exist");
        {
            let _cwd = CurrentDirGuard::set(&root);

            let mut cli = test_live_cli(PermissionMode::WorkspaceWrite);
            assert!(cli
                .enter_plan_mode()
                .expect("enter plan mode should succeed"));
            assert!(!cli
                .set_permissions(Some("danger-full-access".to_string()))
                .expect("raw permission change should be rejected while plan mode is active"));
            assert_eq!(cli.permission_mode, PermissionMode::ReadOnly);
            assert_eq!(cli.plan_restore_mode, Some(PermissionMode::WorkspaceWrite));

            assert!(cli.exit_plan_mode().expect("exit plan mode should succeed"));
            assert_eq!(cli.permission_mode, PermissionMode::WorkspaceWrite);
            assert!(cli.plan_restore_mode.is_none());

            assert!(cli
                .set_permissions(Some("danger-full-access".to_string()))
                .expect("raw permission change should work again after plan mode exits"));
            assert_eq!(cli.permission_mode, PermissionMode::DangerFullAccess);
        }

        crate::cleanup_temp_dir(&root);
    }

    #[test]
    fn invalid_plan_action_is_non_mutating() {
        let _lock = env_lock();
        let root = unique_temp_dir("openyak-cli-plan-invalid-action");
        fs::create_dir_all(&root).expect("root should exist");
        {
            let _cwd = CurrentDirGuard::set(&root);

            let mut cli = test_live_cli(PermissionMode::WorkspaceWrite);
            assert!(!cli
                .handle_plan_command(Some("nope"))
                .expect("invalid /plan action should be a no-op"));
            assert_eq!(cli.permission_mode, PermissionMode::WorkspaceWrite);
            assert!(cli.plan_restore_mode.is_none());
        }

        crate::cleanup_temp_dir(&root);
    }

    #[test]
    fn permissions_report_command_keeps_plan_mode_state() {
        let _lock = env_lock();
        let root = unique_temp_dir("openyak-cli-plan-permissions-report");
        fs::create_dir_all(&root).expect("root should exist");
        {
            let _cwd = CurrentDirGuard::set(&root);

            let mut cli = test_live_cli(PermissionMode::WorkspaceWrite);
            assert!(cli
                .enter_plan_mode()
                .expect("enter plan mode should succeed"));
            assert!(!cli
                .set_permissions(None)
                .expect("/permissions report should not mutate state"));
            assert_eq!(cli.permission_mode, PermissionMode::ReadOnly);
            assert_eq!(cli.plan_restore_mode, Some(PermissionMode::WorkspaceWrite));
        }

        crate::cleanup_temp_dir(&root);
    }

    #[test]
    fn resume_session_preserves_plan_mode_state_within_same_repl() {
        let _lock = env_lock();
        let root = unique_temp_dir("openyak-cli-plan-resume");
        fs::create_dir_all(&root).expect("root should exist");
        {
            let _cwd = CurrentDirGuard::set(&root);

            let saved_session = root.join("resume-target.json");
            Session::new()
                .save_to_path(&saved_session)
                .expect("saved session should write");

            let mut cli = test_live_cli(PermissionMode::WorkspaceWrite);
            assert!(cli
                .enter_plan_mode()
                .expect("enter plan mode should succeed"));
            assert!(cli
                .resume_session(Some(saved_session.display().to_string()))
                .expect("resume should succeed"));
            assert_eq!(cli.permission_mode, PermissionMode::ReadOnly);
            assert_eq!(cli.plan_restore_mode, Some(PermissionMode::WorkspaceWrite));
        }

        crate::cleanup_temp_dir(&root);
    }

    #[test]
    fn session_switch_preserves_plan_mode_state_within_same_repl() {
        let _lock = env_lock();
        let root = unique_temp_dir("openyak-cli-plan-switch");
        fs::create_dir_all(&root).expect("root should exist");
        {
            let _cwd = CurrentDirGuard::set(&root);

            let target_path = write_saved_session("switch-target");
            assert!(target_path.is_file());

            let mut cli = test_live_cli(PermissionMode::WorkspaceWrite);
            assert!(cli
                .enter_plan_mode()
                .expect("enter plan mode should succeed"));
            assert!(cli
                .handle_session_command(Some("switch"), Some("switch-target"))
                .expect("session switch should succeed"));
            assert_eq!(cli.permission_mode, PermissionMode::ReadOnly);
            assert_eq!(cli.plan_restore_mode, Some(PermissionMode::WorkspaceWrite));
            assert_eq!(cli.session.id, "switch-target");
        }

        crate::cleanup_temp_dir(&root);
    }

    #[test]
    fn init_help_mentions_direct_subcommand() {
        let mut help = Vec::new();
        print_help_to(&mut help).expect("help should render");
        let help = String::from_utf8(help).expect("help should be utf8");
        assert!(help.contains("openyak init"));
        assert!(help.contains("openyak onboard"));
        assert!(help.contains("openyak package-release"));
        assert!(help.contains("openyak server"));
        assert!(help.contains("openyak agents"));
        assert!(help.contains("openyak skills"));
        assert!(help.contains("openyak /skills"));
        assert!(help.contains("/plan [exit]"));
    }

    #[test]
    fn stage_release_artifact_copies_binary_and_metadata() {
        let root = unique_temp_dir("openyak-cli-package-release");
        let binary_path = root.join(if cfg!(windows) {
            "openyak.exe"
        } else {
            "openyak"
        });
        let output_dir = root.join("dist");
        fs::create_dir_all(&root).expect("root should exist");
        fs::write(&binary_path, b"stub binary").expect("binary should be written");

        let package = stage_release_artifact(&binary_path, &output_dir)
            .expect("release artifact should stage");

        assert!(package.artifact_dir.starts_with(&output_dir));
        assert!(package.packaged_binary.is_file());
        assert!(package.artifact_dir.join("INSTALL.txt").is_file());
        assert!(package.artifact_dir.join("release-metadata.json").is_file());

        let metadata = fs::read_to_string(package.artifact_dir.join("release-metadata.json"))
            .expect("metadata should read");
        assert!(metadata.contains("\"name\": \"openyak\""));
        assert!(metadata.contains(&format!("\"version\": \"{VERSION}\"")));

        crate::cleanup_temp_dir(&root);
    }

    #[test]
    fn stage_release_artifact_rejects_binary_inside_existing_destination_dir() {
        let root = unique_temp_dir("openyak-cli-package-release-nested-source");
        let output_dir = root.join("dist");
        let target_label = crate::BUILD_TARGET.map_or_else(
            || format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS),
            str::to_string,
        );
        let artifact_dir = output_dir.join(format!("openyak-{VERSION}-{target_label}"));
        let binary_path = artifact_dir.join(if cfg!(windows) {
            "openyak.exe"
        } else {
            "openyak"
        });
        fs::create_dir_all(&artifact_dir).expect("artifact dir should exist");
        fs::write(&binary_path, b"stub binary").expect("binary should be written");

        let error = stage_release_artifact(&binary_path, &output_dir)
            .expect_err("packaging should reject a source binary inside the destination dir");
        assert!(error.to_string().contains("destination artifact directory"));

        crate::cleanup_temp_dir(&root);
    }

    #[test]
    fn thread_server_info_guard_keeps_newer_server_registration() {
        let root = unique_temp_dir("openyak-cli-thread-server-info");
        let path = root.join(".openyak").join(THREAD_SERVER_INFO_FILENAME);
        fs::create_dir_all(path.parent().expect("parent")).expect("openyak dir should exist");
        fs::write(
            &path,
            serde_json::to_string_pretty(&json!({
                "baseUrl": "http://127.0.0.1:4100",
                "pid": 100_u32,
            }))
            .expect("server info json"),
        )
        .expect("original server info should write");

        let guard = ThreadServerInfoGuard {
            path: path.clone(),
            pid: 100,
        };
        fs::write(
            &path,
            serde_json::to_string_pretty(&json!({
                "baseUrl": "http://127.0.0.1:4200",
                "pid": 200_u32,
            }))
            .expect("replacement server info json"),
        )
        .expect("replacement server info should write");
        drop(guard);

        let contents = fs::read_to_string(&path).expect("newer server info should remain");
        assert!(contents.contains("\"pid\": 200"));
        assert!(contents.contains("127.0.0.1:4200"));

        crate::cleanup_temp_dir(&root);
    }

    #[test]
    fn doctor_report_passes_for_healthy_api_key_fixture() {
        let _lock = env_lock();
        let root = unique_temp_dir("openyak-cli-doctor-healthy");
        let cwd = root.join("workspace");
        let config_home = root.join("openyak-home");
        let bin_dir = root.join("bin");
        fs::create_dir_all(&cwd).expect("workspace should exist");
        fs::create_dir_all(&config_home).expect("config home should exist");
        fs::create_dir_all(&bin_dir).expect("bin dir should exist");
        write_fake_command(&bin_dir, "gh");
        let config_home_env = config_home.to_string_lossy().to_string();
        let path_env = std::env::join_paths([bin_dir.as_path()])
            .expect("path should join")
            .to_string_lossy()
            .to_string();
        let _openyak_home = EnvVarGuard::set("OPENYAK_CONFIG_HOME", Some(&config_home_env));
        let _path = EnvVarGuard::set("PATH", Some(&path_env));
        let _api_key = EnvVarGuard::set("ANTHROPIC_API_KEY", Some("doctor-test-key"));
        let _auth_token = EnvVarGuard::set("ANTHROPIC_AUTH_TOKEN", None);

        let report = super::collect_doctor_report(&cwd);

        assert!(
            !report.has_errors(),
            "healthy doctor report should not error"
        );
        assert_eq!(
            doctor_check(&report, "config").status,
            DoctorCheckStatus::Ok
        );
        assert_eq!(
            doctor_check(&report, "oauth config").status,
            DoctorCheckStatus::Ok
        );
        assert_eq!(
            doctor_check(&report, "oauth credentials").status,
            DoctorCheckStatus::Ok
        );
        assert_eq!(
            doctor_check(&report, "active model auth").status,
            DoctorCheckStatus::Ok
        );
        assert_eq!(
            doctor_check(&report, "github cli").status,
            DoctorCheckStatus::Ok
        );

        crate::cleanup_temp_dir(&root);
    }

    #[test]
    fn doctor_report_flags_incomplete_oauth_config() {
        let _lock = env_lock();
        let root = unique_temp_dir("openyak-cli-doctor-partial-oauth");
        let cwd = root.join("workspace");
        let config_home = root.join("openyak-home");
        let bin_dir = root.join("bin");
        fs::create_dir_all(&cwd).expect("workspace should exist");
        fs::create_dir_all(&config_home).expect("config home should exist");
        fs::create_dir_all(&bin_dir).expect("bin dir should exist");
        fs::write(
            config_home.join("settings.json"),
            "{\n  \"oauth\": {\n    \"callbackPort\": 4557\n  }\n}\n",
        )
        .expect("settings should write");
        write_fake_command(&bin_dir, "gh");
        let config_home_env = config_home.to_string_lossy().to_string();
        let path_env = std::env::join_paths([bin_dir.as_path()])
            .expect("path should join")
            .to_string_lossy()
            .to_string();
        let _openyak_home = EnvVarGuard::set("OPENYAK_CONFIG_HOME", Some(&config_home_env));
        let _path = EnvVarGuard::set("PATH", Some(&path_env));
        let _api_key = EnvVarGuard::set("ANTHROPIC_API_KEY", Some("doctor-test-key"));

        let report = super::collect_doctor_report(&cwd);
        let oauth_check = doctor_check(&report, "oauth config");

        assert!(report.has_errors(), "partial oauth config should error");
        assert_eq!(oauth_check.status, DoctorCheckStatus::Error);
        assert!(oauth_check.summary.contains("settings.oauth is incomplete"));

        crate::cleanup_temp_dir(&root);
    }

    #[test]
    fn doctor_report_flags_malformed_config_path() {
        let _lock = env_lock();
        let root = unique_temp_dir("openyak-cli-doctor-bad-config");
        let cwd = root.join("workspace");
        let config_home = root.join("openyak-home");
        fs::create_dir_all(&cwd).expect("workspace should exist");
        fs::create_dir_all(&config_home).expect("config home should exist");
        let settings_path = config_home.join("settings.json");
        fs::write(&settings_path, "{ invalid json").expect("bad settings should write");
        let config_home_env = config_home.to_string_lossy().to_string();
        let _openyak_home = EnvVarGuard::set("OPENYAK_CONFIG_HOME", Some(&config_home_env));

        let report = super::collect_doctor_report(&cwd);
        let config_check = doctor_check(&report, "config");

        assert!(report.has_errors(), "bad config should error");
        assert_eq!(config_check.status, DoctorCheckStatus::Error);
        assert!(config_check
            .summary
            .contains(&settings_path.display().to_string()));

        crate::cleanup_temp_dir(&root);
    }

    #[test]
    fn model_report_uses_sectioned_layout() {
        let report = format_model_report("sonnet", 12, 4);
        assert!(report.contains("Model"));
        assert!(report.contains("Current          sonnet"));
        assert!(report.contains("Session          12 messages · 4 turns"));
        assert!(report.contains("Aliases"));
        assert!(report.contains("/model <name>    Switch models for this REPL session"));
    }

    #[test]
    fn model_switch_report_preserves_context_summary() {
        let report = format_model_switch_report("sonnet", "opus", 9);
        assert!(report.contains("Model updated"));
        assert!(report.contains("Previous         sonnet"));
        assert!(report.contains("Current          opus"));
        assert!(report.contains("Preserved        9 messages"));
    }

    #[test]
    fn status_line_reports_model_and_token_totals() {
        let status = format_status_report(
            "sonnet",
            StatusUsage {
                message_count: 7,
                turns: 3,
                latest: runtime::TokenUsage {
                    input_tokens: 5,
                    output_tokens: 4,
                    cache_creation_input_tokens: 1,
                    cache_read_input_tokens: 0,
                },
                cumulative: runtime::TokenUsage {
                    input_tokens: 20,
                    output_tokens: 8,
                    cache_creation_input_tokens: 2,
                    cache_read_input_tokens: 1,
                },
                estimated_tokens: 128,
            },
            "workspace-write",
            None,
            &super::StatusContext {
                cwd: PathBuf::from("/tmp/project"),
                session_path: Some(PathBuf::from("session.json")),
                loaded_config_files: 2,
                discovered_config_files: 3,
                memory_file_count: 4,
                project_root: Some(PathBuf::from("/tmp")),
                git_branch: Some("main".to_string()),
                resume_mode: false,
            },
        );
        assert!(status.contains("Session"));
        assert!(status.contains("Model            sonnet"));
        assert!(status.contains("Permissions      workspace-write"));
        assert!(status.contains("Activity         7 messages · 3 turns"));
        assert!(status.contains("Tokens           est 128 · latest 10 · total 31"));
        assert!(status.contains("Folder           /tmp/project"));
        assert!(status.contains("Project root     /tmp"));
        assert!(status.contains("Git branch       main"));
        assert!(status.contains("Session file     session.json"));
        assert!(status.contains("Config files     loaded 2/3"));
        assert!(status.contains("Memory files     4"));
        assert!(status.contains("/session list"));
    }

    #[test]
    fn status_line_surfaces_active_plan_mode() {
        let status = format_status_report(
            "sonnet",
            StatusUsage {
                message_count: 7,
                turns: 3,
                latest: runtime::TokenUsage::default(),
                cumulative: runtime::TokenUsage::default(),
                estimated_tokens: 128,
            },
            "read-only",
            Some("workspace-write"),
            &super::StatusContext {
                cwd: PathBuf::from("/tmp/project"),
                session_path: Some(PathBuf::from("session.json")),
                loaded_config_files: 2,
                discovered_config_files: 3,
                memory_file_count: 4,
                project_root: Some(PathBuf::from("/tmp")),
                git_branch: Some("main".to_string()),
                resume_mode: false,
            },
        );
        assert!(status.contains("Permissions      read-only"));
        assert!(status.contains("Planning         active · restores workspace-write · /plan exit"));
    }

    #[test]
    fn resumed_status_line_only_suggests_resume_safe_commands() {
        let status = format_status_report(
            "restored-session",
            StatusUsage {
                message_count: 0,
                turns: 0,
                latest: runtime::TokenUsage::default(),
                cumulative: runtime::TokenUsage::default(),
                estimated_tokens: 0,
            },
            "danger-full-access",
            None,
            &super::StatusContext {
                cwd: PathBuf::from("/tmp/project"),
                session_path: Some(PathBuf::from("session.json")),
                loaded_config_files: 0,
                discovered_config_files: 0,
                memory_file_count: 0,
                project_root: None,
                git_branch: None,
                resume_mode: true,
            },
        );
        assert!(status.contains("/export [file]"));
        assert!(!status.contains("/session list"));
    }

    #[test]
    fn config_report_supports_section_views() {
        let report = render_config_report(Some("env")).expect("config report should render");
        assert!(report.contains("Merged section: env"));
        let plugins_report =
            render_config_report(Some("plugins")).expect("plugins config report should render");
        assert!(plugins_report.contains("Merged section: plugins"));
    }

    #[test]
    fn memory_report_uses_sectioned_layout() {
        let report = render_memory_report().expect("memory report should render");
        assert!(report.contains("Memory"));
        assert!(report.contains("Working directory"));
        assert!(report.contains("Instruction files"));
        assert!(report.contains("Discovered files"));
    }

    #[test]
    fn system_prompt_builder_uses_injected_date() {
        let prompt = super::build_system_prompt_with_date("gpt-5.3-codex", "2030-02-03")
            .expect("system prompt should render");
        let rendered = prompt.join("\n");
        assert!(rendered.contains("Today's date is 2030-02-03."));
        assert!(rendered.contains("Model family: gpt-5.3-codex"));
    }

    #[test]
    fn resolve_effective_model_uses_configured_model_by_default() {
        let _lock = env_lock();
        let _openyak_home = EnvVarGuard::set("OPENYAK_CONFIG_HOME", None);
        let _codex_home = EnvVarGuard::set("CODEX_HOME", None);
        let root = unique_temp_dir("openyak-cli-configured-model");
        fs::create_dir_all(root.join(".openyak")).expect("config dir");
        fs::write(
            root.join(".openyak").join("settings.json"),
            "{\n  \"model\": \"gpt-5.3-codex\"\n}\n",
        )
        .expect("write settings");

        let model = resolve_effective_model(None, &root).expect("model should resolve");
        assert_eq!(model, "gpt-5.3-codex");

        crate::cleanup_temp_dir(&root);
    }

    #[test]
    fn explicit_model_overrides_configured_model() {
        let _lock = env_lock();
        let _openyak_home = EnvVarGuard::set("OPENYAK_CONFIG_HOME", None);
        let _codex_home = EnvVarGuard::set("CODEX_HOME", None);
        let root = unique_temp_dir("openyak-cli-explicit-model");
        fs::create_dir_all(root.join(".openyak")).expect("config dir");
        fs::write(
            root.join(".openyak").join("settings.json"),
            "{\n  \"model\": \"claude-sonnet-4-6\"\n}\n",
        )
        .expect("write settings");

        let model =
            resolve_effective_model(Some("gpt-5.3-codex"), &root).expect("model should resolve");
        assert_eq!(model, "gpt-5.3-codex");

        crate::cleanup_temp_dir(&root);
    }

    #[test]
    fn system_prompt_uses_configured_model_when_no_cli_model_is_supplied() {
        let root = unique_temp_dir("openyak-cli-system-prompt-model");
        fs::create_dir_all(root.join(".openyak")).expect("config dir");
        fs::write(
            root.join(".openyak").join("settings.json"),
            "{\n  \"model\": \"gpt-5.3-codex\"\n}\n",
        )
        .expect("write settings");

        let prompt = build_system_prompt_for_cwd_with_date(&root, None, "2030-02-03")
            .expect("system prompt should render");
        let rendered = prompt.join("\n");
        assert!(rendered.contains("Model family: gpt-5.3-codex"));

        crate::cleanup_temp_dir(&root);
    }

    #[test]
    fn config_report_uses_sectioned_layout() {
        let report = render_config_report(None).expect("config report should render");
        assert!(report.contains("Config"));
        assert!(report.contains("Discovered files"));
        assert!(report.contains("Merged JSON"));
    }

    #[test]
    fn parses_git_status_metadata() {
        let (root, branch) = parse_git_status_metadata(Some(
            "## rcc/cli...origin/rcc/cli
 M src/main.rs",
        ));
        assert_eq!(branch.as_deref(), Some("rcc/cli"));
        let _ = root;
    }

    #[test]
    fn status_context_reads_real_workspace_metadata() {
        let context = status_context(None).expect("status context should load");
        assert!(context.cwd.is_absolute());
        assert_eq!(context.discovered_config_files, 5);
        assert!(context.loaded_config_files <= context.discovered_config_files);
    }

    #[test]
    fn status_context_falls_back_when_config_is_invalid() {
        let root = unique_temp_dir("openyak-cli-status-fallback");
        let session_path = root.join("session.json");
        fs::create_dir_all(root.join(".openyak")).expect("config dir");
        fs::write(
            root.join(".openyak").join("settings.json"),
            "{ invalid json",
        )
        .expect("write invalid settings");

        let (context, warning) =
            status_context_or_fallback_for_cwd(&root, Some(&session_path), "2030-02-03", false);

        assert_eq!(context.cwd, root);
        assert_eq!(
            context.session_path.as_deref(),
            Some(session_path.as_path())
        );
        assert_eq!(context.loaded_config_files, 0);
        assert!(context.discovered_config_files >= 1);
        assert_eq!(context.memory_file_count, 0);
        assert!(context.project_root.is_none());
        assert!(context.git_branch.is_none());
        assert!(warning
            .as_deref()
            .is_some_and(|message| !message.is_empty()));

        crate::cleanup_temp_dir(&root);
    }

    #[test]
    fn normalizes_supported_permission_modes() {
        assert_eq!(normalize_permission_mode("read-only"), Some("read-only"));
        assert_eq!(
            normalize_permission_mode("workspace-write"),
            Some("workspace-write")
        );
        assert_eq!(
            normalize_permission_mode("danger-full-access"),
            Some("danger-full-access")
        );
        assert_eq!(normalize_permission_mode("unknown"), None);
    }

    #[test]
    fn clear_command_requires_explicit_confirmation_flag() {
        assert_eq!(
            SlashCommand::parse("/clear"),
            Some(SlashCommand::Clear { confirm: false })
        );
        assert_eq!(
            SlashCommand::parse("/clear --confirm"),
            Some(SlashCommand::Clear { confirm: true })
        );
    }

    #[test]
    fn parses_resume_and_config_slash_commands() {
        assert_eq!(
            SlashCommand::parse("/resume saved-session.json"),
            Some(SlashCommand::Resume {
                session_path: Some("saved-session.json".to_string())
            })
        );
        assert_eq!(
            SlashCommand::parse("/clear --confirm"),
            Some(SlashCommand::Clear { confirm: true })
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
        assert_eq!(SlashCommand::parse("/init"), Some(SlashCommand::Init));
    }

    #[test]
    fn init_template_mentions_detected_rust_workspace() {
        let rendered = crate::init::render_init_openyak_md(std::path::Path::new("."));
        assert!(rendered.contains("# OPENYAK.md"));
        assert!(rendered.contains("cargo clippy --workspace --all-targets -- -D warnings"));
    }

    #[test]
    fn converts_tool_roundtrip_messages() {
        let messages = vec![
            ConversationMessage::user_text("hello"),
            ConversationMessage::assistant(vec![ContentBlock::ToolUse {
                id: "tool-1".to_string(),
                name: "bash".to_string(),
                input: "{\"command\":\"pwd\"}".to_string(),
            }]),
            ConversationMessage {
                role: MessageRole::Tool,
                blocks: vec![ContentBlock::ToolResult {
                    tool_use_id: "tool-1".to_string(),
                    tool_name: "bash".to_string(),
                    output: "ok".to_string(),
                    is_error: false,
                }],
                usage: None,
            },
        ];

        let converted = super::convert_messages(&messages);
        assert_eq!(converted.len(), 3);
        assert_eq!(converted[1].role, "assistant");
        assert_eq!(converted[2].role, "user");
    }

    #[test]
    fn converts_request_user_input_messages_into_reserved_tool_round_trip() {
        let messages = vec![
            ConversationMessage::assistant(vec![ContentBlock::UserInputRequest {
                request_id: "req-1".to_string(),
                prompt: "Which branch?".to_string(),
                options: vec!["main".to_string(), "feature".to_string()],
                allow_freeform: false,
            }]),
            ConversationMessage::user_input_response(
                "req-1",
                "feature",
                Some("feature".to_string()),
            ),
        ];

        let converted = super::convert_messages(&messages);
        assert_eq!(converted.len(), 2);
        assert!(matches!(
            &converted[0].content[0],
            InputContentBlock::ToolUse { name, .. } if name == REQUEST_USER_INPUT_TOOL_NAME
        ));
        assert!(matches!(
            &converted[1].content[0],
            InputContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == "req-1"
        ));
    }

    #[test]
    fn parses_option_and_freeform_user_input_replies() {
        let request = UserInputRequest {
            request_id: "req-1".to_string(),
            prompt: "Choose".to_string(),
            options: vec!["main".to_string(), "feature".to_string()],
            allow_freeform: true,
        };

        let option = parse_user_input_submission(&request, "2").expect("option reply");
        assert_eq!(option.selected_option.as_deref(), Some("feature"));
        assert_eq!(option.content, "feature");

        let freeform = parse_user_input_submission(&request, "ship it").expect("freeform reply");
        assert_eq!(freeform.selected_option, None);
        assert_eq!(freeform.content, "ship it");
    }

    #[test]
    fn non_interactive_user_input_prompter_fails_explicitly() {
        let mut prompter = CliUserInputPrompter::unavailable();
        let outcome = prompter.prompt(&UserInputRequest {
            request_id: "req-1".to_string(),
            prompt: "Choose".to_string(),
            options: vec!["main".to_string()],
            allow_freeform: false,
        });

        assert!(matches!(
            outcome,
            UserInputOutcome::Unavailable { reason }
                if reason.contains("interactive CLI input is unavailable")
        ));
    }
    #[test]
    fn repl_help_mentions_history_completion_and_multiline() {
        let help = render_repl_help();
        assert!(help.contains("Up/Down"));
        assert!(help.contains("Tab cycles"));
        assert!(help.contains("Shift+Enter or Ctrl+J"));
        assert!(help.contains("Structured input"));
    }

    #[test]
    fn tool_rendering_helpers_compact_output() {
        let start = format_tool_call_start("read_file", r#"{"path":"src/main.rs"}"#);
        assert!(start.contains("read_file"));
        assert!(start.contains("src/main.rs"));

        let done = format_tool_result(
            "read_file",
            r#"{"file":{"filePath":"src/main.rs","content":"hello","numLines":1,"startLine":1,"totalLines":1}}"#,
            false,
        );
        assert!(done.contains("📄 Read src/main.rs"));
        assert!(done.contains("hello"));
    }

    #[test]
    fn tool_rendering_truncates_large_read_output_for_display_only() {
        let content = (0..200)
            .map(|index| format!("line {index:03}"))
            .collect::<Vec<_>>()
            .join("\n");
        let output = json!({
            "file": {
                "filePath": "src/main.rs",
                "content": content,
                "numLines": 200,
                "startLine": 1,
                "totalLines": 200
            }
        })
        .to_string();

        let rendered = format_tool_result("read_file", &output, false);

        assert!(rendered.contains("line 000"));
        assert!(rendered.contains("line 079"));
        assert!(!rendered.contains("line 199"));
        assert!(rendered.contains("full result preserved in session"));
        assert!(output.contains("line 199"));
    }

    #[test]
    fn tool_rendering_truncates_large_bash_output_for_display_only() {
        let stdout = (0..120)
            .map(|index| format!("stdout {index:03}"))
            .collect::<Vec<_>>()
            .join("\n");
        let output = json!({
            "stdout": stdout,
            "stderr": "",
            "returnCodeInterpretation": "completed successfully"
        })
        .to_string();

        let rendered = format_tool_result("bash", &output, false);

        assert!(rendered.contains("stdout 000"));
        assert!(rendered.contains("stdout 059"));
        assert!(!rendered.contains("stdout 119"));
        assert!(rendered.contains("full result preserved in session"));
        assert!(output.contains("stdout 119"));
    }

    #[test]
    fn tool_rendering_truncates_generic_long_output_for_display_only() {
        let items = (0..120)
            .map(|index| format!("payload {index:03}"))
            .collect::<Vec<_>>();
        let output = json!({
            "summary": "plugin payload",
            "items": items,
        })
        .to_string();

        let rendered = format_tool_result("plugin_echo", &output, false);

        assert!(rendered.contains("plugin_echo"));
        assert!(rendered.contains("payload 000"));
        assert!(rendered.contains("payload 040"));
        assert!(!rendered.contains("payload 080"));
        assert!(!rendered.contains("payload 119"));
        assert!(rendered.contains("full result preserved in session"));
        assert!(output.contains("payload 119"));
    }

    #[test]
    fn tool_rendering_truncates_raw_generic_output_for_display_only() {
        let output = (0..120)
            .map(|index| format!("raw {index:03}"))
            .collect::<Vec<_>>()
            .join("\n");

        let rendered = format_tool_result("plugin_echo", &output, false);

        assert!(rendered.contains("plugin_echo"));
        assert!(rendered.contains("raw 000"));
        assert!(rendered.contains("raw 059"));
        assert!(!rendered.contains("raw 119"));
        assert!(rendered.contains("full result preserved in session"));
        assert!(output.contains("raw 119"));
    }

    #[test]
    fn ultraplan_progress_lines_include_phase_step_and_elapsed_status() {
        let snapshot = InternalPromptProgressState {
            command_label: "Ultraplan",
            task_label: "ship plugin progress".to_string(),
            step: 3,
            phase: "running read_file".to_string(),
            detail: Some("reading rust/crates/openyak-cli/src/main.rs".to_string()),
            saw_final_text: false,
        };

        let started = format_internal_prompt_progress_line(
            InternalPromptProgressEvent::Started,
            &snapshot,
            Duration::from_secs(0),
            None,
        );
        let heartbeat = format_internal_prompt_progress_line(
            InternalPromptProgressEvent::Heartbeat,
            &snapshot,
            Duration::from_secs(9),
            None,
        );
        let completed = format_internal_prompt_progress_line(
            InternalPromptProgressEvent::Complete,
            &snapshot,
            Duration::from_secs(12),
            None,
        );
        let failed = format_internal_prompt_progress_line(
            InternalPromptProgressEvent::Failed,
            &snapshot,
            Duration::from_secs(12),
            Some("network timeout"),
        );

        assert!(started.contains("planning started"));
        assert!(started.contains("current step 3"));
        assert!(heartbeat.contains("heartbeat"));
        assert!(heartbeat.contains("9s elapsed"));
        assert!(heartbeat.contains("phase running read_file"));
        assert!(completed.contains("completed"));
        assert!(completed.contains("3 steps total"));
        assert!(failed.contains("failed"));
        assert!(failed.contains("network timeout"));
    }

    #[test]
    fn describe_tool_progress_summarizes_known_tools() {
        assert_eq!(
            describe_tool_progress("read_file", r#"{"path":"src/main.rs"}"#),
            "reading src/main.rs"
        );
        assert!(
            describe_tool_progress("bash", r#"{"command":"cargo test -p openyak-cli"}"#)
                .contains("cargo test -p openyak-cli")
        );
        assert_eq!(
            describe_tool_progress("grep_search", r#"{"pattern":"ultraplan","path":"rust"}"#),
            "grep `ultraplan` in rust"
        );
    }

    #[test]
    fn push_output_block_renders_markdown_text() {
        let mut out = Vec::new();
        let mut events = Vec::new();
        let mut pending_tool = None;

        push_output_block(
            OutputContentBlock::Text {
                text: "# Heading".to_string(),
            },
            &mut out,
            &mut events,
            &mut pending_tool,
            false,
        )
        .expect("text block should render");

        let rendered = String::from_utf8(out).expect("utf8");
        assert!(rendered.contains("Heading"));
        assert!(rendered.contains('\u{1b}'));
    }

    #[test]
    fn push_output_block_skips_empty_object_prefix_for_tool_streams() {
        let mut out = Vec::new();
        let mut events = Vec::new();
        let mut pending_tool = None;

        push_output_block(
            OutputContentBlock::ToolUse {
                id: "tool-1".to_string(),
                name: "read_file".to_string(),
                input: json!({}),
            },
            &mut out,
            &mut events,
            &mut pending_tool,
            true,
        )
        .expect("tool block should accumulate");

        assert!(events.is_empty());
        assert_eq!(
            pending_tool,
            Some(("tool-1".to_string(), "read_file".to_string(), String::new(),))
        );
    }

    #[test]
    fn response_to_events_preserves_empty_object_json_input_outside_streaming() {
        let mut out = Vec::new();
        let events = response_to_events(
            MessageResponse {
                id: "msg-1".to_string(),
                kind: "message".to_string(),
                model: "claude-opus-4-6".to_string(),
                role: "assistant".to_string(),
                content: vec![OutputContentBlock::ToolUse {
                    id: "tool-1".to_string(),
                    name: "read_file".to_string(),
                    input: json!({}),
                }],
                stop_reason: Some("tool_use".to_string()),
                stop_sequence: None,
                usage: Usage {
                    input_tokens: 1,
                    output_tokens: 1,
                    cache_creation_input_tokens: 0,
                    cache_read_input_tokens: 0,
                },
                request_id: None,
            },
            &mut out,
        )
        .expect("response conversion should succeed");

        assert!(matches!(
            &events[0],
            AssistantEvent::ToolUse { name, input, .. }
                if name == "read_file" && input == "{}"
        ));
    }

    #[test]
    fn response_to_events_preserves_non_empty_json_input_outside_streaming() {
        let mut out = Vec::new();
        let events = response_to_events(
            MessageResponse {
                id: "msg-2".to_string(),
                kind: "message".to_string(),
                model: "claude-opus-4-6".to_string(),
                role: "assistant".to_string(),
                content: vec![OutputContentBlock::ToolUse {
                    id: "tool-2".to_string(),
                    name: "read_file".to_string(),
                    input: json!({ "path": "rust/Cargo.toml" }),
                }],
                stop_reason: Some("tool_use".to_string()),
                stop_sequence: None,
                usage: Usage {
                    input_tokens: 1,
                    output_tokens: 1,
                    cache_creation_input_tokens: 0,
                    cache_read_input_tokens: 0,
                },
                request_id: None,
            },
            &mut out,
        )
        .expect("response conversion should succeed");

        assert!(matches!(
            &events[0],
            AssistantEvent::ToolUse { name, input, .. }
                if name == "read_file" && input == "{\"path\":\"rust/Cargo.toml\"}"
        ));
    }

    #[test]
    fn response_to_events_maps_reserved_tool_into_request_user_input_event() {
        let mut out = Vec::new();
        let events = response_to_events(
            MessageResponse {
                id: "msg-user-input".to_string(),
                kind: "message".to_string(),
                model: "claude-opus-4-6".to_string(),
                role: "assistant".to_string(),
                content: vec![OutputContentBlock::ToolUse {
                    id: "req-1".to_string(),
                    name: REQUEST_USER_INPUT_TOOL_NAME.to_string(),
                    input: json!({
                        "request_id": "req-1",
                        "prompt": "Which branch?",
                        "options": ["main", "feature"],
                        "allow_freeform": false
                    }),
                }],
                stop_reason: Some("tool_use".to_string()),
                stop_sequence: None,
                usage: Usage {
                    input_tokens: 1,
                    output_tokens: 1,
                    cache_creation_input_tokens: 0,
                    cache_read_input_tokens: 0,
                },
                request_id: None,
            },
            &mut out,
        )
        .expect("response conversion should succeed");

        assert!(matches!(
            &events[0],
            AssistantEvent::RequestUserInput(request)
                if request.request_id == "req-1"
                    && request.prompt == "Which branch?"
                    && request.options == vec!["main".to_string(), "feature".to_string()]
                    && !request.allow_freeform
        ));
    }

    #[test]
    fn response_to_events_ignores_thinking_blocks() {
        let mut out = Vec::new();
        let events = response_to_events(
            MessageResponse {
                id: "msg-3".to_string(),
                kind: "message".to_string(),
                model: "claude-opus-4-6".to_string(),
                role: "assistant".to_string(),
                content: vec![
                    OutputContentBlock::Thinking {
                        thinking: "step 1".to_string(),
                        signature: Some("sig_123".to_string()),
                    },
                    OutputContentBlock::Text {
                        text: "Final answer".to_string(),
                    },
                ],
                stop_reason: Some("end_turn".to_string()),
                stop_sequence: None,
                usage: Usage {
                    input_tokens: 1,
                    output_tokens: 1,
                    cache_creation_input_tokens: 0,
                    cache_read_input_tokens: 0,
                },
                request_id: None,
            },
            &mut out,
        )
        .expect("response conversion should succeed");

        assert!(matches!(
            &events[0],
            AssistantEvent::TextDelta(text) if text == "Final answer"
        ));
        assert!(!String::from_utf8(out).expect("utf8").contains("step 1"));
    }

    #[test]
    fn commit_push_pr_parser_accepts_optional_commit_message() {
        let parsed =
            parse_commit_push_pr_draft("COMMIT: NONE\nTITLE: chore: sync docs\nBODY:\nSummary\n")
                .expect("draft should parse");
        assert_eq!(parsed.0, None);
        assert_eq!(parsed.1, "chore: sync docs");
        assert_eq!(parsed.2, "Summary");

        let parsed = parse_commit_push_pr_draft(
            "COMMIT: feat: add branch command\nTITLE: feat: add branch command\nBODY:\nDetails\n",
        )
        .expect("draft should parse");
        assert_eq!(parsed.0.as_deref(), Some("feat: add branch command"));
    }

    #[test]
    fn git_args_excluding_local_artifacts_filters_openyak_state() {
        let args = git_args_excluding_local_artifacts(&["status", "--short"]);
        assert_eq!(args[0], "status");
        assert_eq!(args[1], "--short");
        assert!(args.contains(&":(exclude).omx"));
        assert!(args.contains(&":(exclude).openyak/settings.local.json"));
        assert!(args.contains(&":(exclude).openyak/sessions"));
    }

    #[test]
    fn render_diff_report_includes_untracked_files() {
        let _lock = env_lock();
        let Some(git_path) = resolve_command_path("git") else {
            return;
        };
        let root = unique_temp_dir("openyak-cli-diff-untracked");
        fs::create_dir_all(&root).expect("create root");
        {
            let _cwd = CurrentDirGuard::set(&root);

            let output = Command::new(git_path)
                .args(["init", "-b", "main"])
                .current_dir(&root)
                .output()
                .expect("git init should run");
            assert!(output.status.success(), "git init should succeed");

            initialize_repo(&root).expect("init should succeed");

            let report = render_diff_report().expect("diff report should render");

            assert!(report.contains("Status"));
            assert!(report.contains("?? .openyak.json"));
            assert!(report.contains("?? .gitignore"));
            assert!(report.contains("?? OPENYAK.md"));
        }

        crate::cleanup_temp_dir(&root);
    }

    #[test]
    fn debug_tool_call_report_is_non_fatal_when_no_tool_calls_exist() {
        let report = render_last_tool_debug_report(&Session::new());
        assert!(report.contains("Result           unavailable"));
        assert!(report.contains("no prior tool call found in session"));
    }

    #[test]
    fn git_command_repo_requirement_report_is_structured() {
        let report = render_git_command_requires_repo("pr", "pull request drafting");
        assert!(report.contains("Command unavailable"));
        assert!(report.contains("Command          /pr"));
        assert!(report.contains("current directory is not inside a git repository"));
    }
}

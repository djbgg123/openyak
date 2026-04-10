use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;

mod common;

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[test]
#[allow(clippy::too_many_lines)]
fn openyak_root_and_subcommand_help_cover_verified_surface() {
    let _guard = env_lock();
    let sandbox = CliSandbox::new("openyak-help-surface");

    let root_help = sandbox.run_success(&["--help"]);
    for marker in [
        "openyak dump-manifests",
        "openyak bootstrap-plan",
        "openyak agents",
        "openyak skills",
        "openyak system-prompt",
        "openyak login",
        "openyak logout",
        "openyak init",
        "openyak onboard",
        "openyak doctor",
        "openyak foundations",
        "openyak package-release",
        "openyak server",
        "openyak server start --detach",
        "openyak server status",
        "openyak server stop",
        "--tool-profile NAME",
    ] {
        assert!(
            root_help.contains(marker),
            "root help should list {marker}: {root_help}"
        );
    }

    for (args, expected) in [
        (
            vec!["prompt", "--help"],
            "Usage: openyak prompt [--tool-profile NAME] <text>",
        ),
        (
            vec!["dump-manifests", "--help"],
            "Usage: openyak dump-manifests",
        ),
        (
            vec!["bootstrap-plan", "--help"],
            "Usage: openyak bootstrap-plan",
        ),
        (vec!["agents", "--help"], "Usage            /agents"),
        (vec!["skills", "--help"], "Usage            /skills"),
        (
            vec!["system-prompt", "--help"],
            "Usage: openyak system-prompt",
        ),
        (vec!["login", "--help"], "Usage: openyak login"),
        (vec!["logout", "--help"], "Usage: openyak logout"),
        (vec!["init", "--help"], "Usage: openyak init"),
        (vec!["onboard", "--help"], "Usage: openyak onboard"),
        (vec!["doctor", "--help"], "Usage: openyak doctor"),
        (
            vec!["foundations", "--help"],
            "Usage: openyak foundations [task|team|cron|lsp|mcp]",
        ),
        (
            vec!["package-release", "--help"],
            "Usage: openyak package-release",
        ),
        (vec!["server", "--help"], "Usage: openyak server"),
    ] {
        let output = sandbox.run_success(&args);
        assert!(
            output.contains(expected),
            "expected `{expected}` in help output, got:\n{output}"
        );
    }
    assert!(
        root_help.contains(
            "hidden BrowserObserve/BrowserInteract still require browserControl.enabled=true"
        ),
        "{root_help}"
    );
    assert!(
        root_help.contains(
            "openyak --permission-mode danger-full-access --allowedTools BrowserObserve prompt"
        ),
        "{root_help}"
    );
    assert!(
        root_help.contains(
            "openyak --permission-mode danger-full-access --allowedTools BrowserInteract prompt"
        ),
        "{root_help}"
    );

    let prompt_help = sandbox.run_success(&["prompt", "--help"]);
    assert!(
        prompt_help.contains("Use --tool-profile to apply a named local tool-profile ceiling"),
        "{prompt_help}"
    );
    assert!(
        prompt_help.contains(
            "Hidden optional browser tools such as BrowserObserve and BrowserInteract still require browserControl.enabled=true plus explicit --allowedTools BrowserObserve or --allowedTools BrowserInteract"
        ),
        "{prompt_help}"
    );

    let doctor_help = sandbox.run_success(&["doctor", "--help"]);
    for marker in [
        "current-workspace local daemon/thread-server discovery readiness",
        "openyak --model MODEL doctor",
    ] {
        assert!(
            doctor_help.contains(marker),
            "doctor help should mention {marker}: {doctor_help}"
        );
    }

    let server_help = sandbox.run_success(&["server", "--help"]);
    for marker in [
        "local `/v1/threads` protocol plus legacy `/sessions` compatibility routes",
        "openyak server start --detach",
        "openyak server stop",
        "workspace `.openyak/state.sqlite3` SQLite store",
        "only supports loopback binds",
    ] {
        assert!(
            server_help.contains(marker),
            "server help should mention {marker}: {server_help}"
        );
    }
}

#[test]
fn openyak_direct_commands_match_verified_smoke_paths() {
    let _guard = env_lock();
    let sandbox = CliSandbox::new("openyak-command-surface");

    let dump_manifests = sandbox.run_success(&["dump-manifests"]);
    assert!(dump_manifests.contains("commands:"), "{dump_manifests}");
    assert!(dump_manifests.contains("tools:"), "{dump_manifests}");
    assert!(
        dump_manifests.contains("bootstrap phases:"),
        "{dump_manifests}"
    );

    let bootstrap_plan = sandbox.run_success(&["bootstrap-plan"]);
    assert!(bootstrap_plan.contains("- CliEntry"), "{bootstrap_plan}");
    assert!(bootstrap_plan.contains("- MainRuntime"), "{bootstrap_plan}");

    let agents = sandbox.run_success(&["agents"]);
    assert!(agents.contains("No agents found."), "{agents}");

    let skills = sandbox.run_success(&["skills"]);
    assert!(skills.contains("No skills found."), "{skills}");

    let system_prompt = sandbox.run_success_owned(&[
        "system-prompt".to_string(),
        "--cwd".to_string(),
        sandbox.workspace.display().to_string(),
        "--date".to_string(),
        "2030-02-03".to_string(),
    ]);
    assert!(
        system_prompt.contains("Date: 2030-02-03"),
        "{system_prompt}"
    );
    assert!(
        system_prompt.contains(&sandbox.workspace.display().to_string()),
        "{system_prompt}"
    );

    let logout = sandbox.run_success(&["logout"]);
    assert!(
        logout.contains("openyak OAuth credentials cleared."),
        "{logout}"
    );

    let init = sandbox.run_success(&["init"]);
    assert!(init.contains("Init"), "{init}");
    assert!(sandbox.workspace.join("OPENYAK.md").is_file());
    assert!(sandbox.workspace.join(".openyak.json").is_file());
    assert!(sandbox.workspace.join(".openyak").is_dir());

    let foundations = sandbox.run_success(&["foundations"]);
    assert!(foundations.contains("Foundations"), "{foundations}");
    assert!(foundations.contains("TaskCreate"), "{foundations}");
    assert!(foundations.contains("process_local_v1"), "{foundations}");
    assert!(
        foundations.contains("it does not create a new control plane"),
        "{foundations}"
    );

    let foundations_task = sandbox.run_success(&["foundations", "task"]);
    assert!(
        foundations_task.contains("Family           task"),
        "{foundations_task}"
    );
    assert!(foundations_task.contains("TaskWait"), "{foundations_task}");
    assert!(
        foundations_task.contains("process_local_v1 current-runtime registry"),
        "{foundations_task}"
    );
}

#[test]
fn daemon_truth_docs_keep_threads_and_foundations_split() {
    let repo_root = repo_root();
    let rust_root = repo_root.join("rust");
    let readme = fs::read_to_string(rust_root.join("README.md")).expect("README should exist");
    let contributing =
        fs::read_to_string(rust_root.join("CONTRIBUTING.md")).expect("CONTRIBUTING should exist");
    let parity_doc = fs::read_to_string(rust_root.join("docs/parity-foundation-registries.md"))
        .expect("parity foundation doc should exist");

    for marker in [
        "`truth_layer = daemon_local_v1`",
        "`operator_plane = local_loopback_operator_v1`",
        "`recovery.failure_kind` / `recovery.recovery_kind` / `recovery.recommended_actions`",
        "`/v1/threads`",
        "恢复 guidance",
        "未有：daemon-backed worker/task/team truth layer",
        "failure taxonomy / recovery recipes",
        "Task / Team / Cron registry 保持 `process_local_v1` 语义",
    ] {
        assert!(readme.contains(marker), "README missing {marker}: {readme}");
    }

    for marker in [
        "Task / Team / Cron 的 V1 contract 当前固定为 `process_local_v1`",
        "不要偷偷引入持久化、恢复、租约、共享服务或 crash recovery 语义",
    ] {
        assert!(
            contributing.contains(marker),
            "CONTRIBUTING missing {marker}: {contributing}"
        );
    }

    for marker in [
        "`origin = \"process_local_v1\"`",
        "`operator_plane = \"local_loopback_operator_v1\"`",
        "`failure_kind` / `recovery_kind` / `recommended_actions`",
        "不提供持久化、恢复、租约语义",
    ] {
        assert!(
            parity_doc.contains(marker),
            "parity foundation doc missing {marker}: {parity_doc}"
        );
    }
}

#[test]
fn attach_first_sdk_docs_stay_narrow_about_daemon_operator_plane() {
    let repo_root = repo_root();

    for relative_path in ["sdk/python/README.md", "sdk/typescript/README.md"] {
        let readme =
            fs::read_to_string(repo_root.join(relative_path)).expect("SDK README should exist");
        for marker in [
            "legacy `/sessions` compatibility routes",
            "public contract remains `/v1/threads` only",
            "attach-first",
            "`failure_kind`, `recovery_kind`, `recommended_actions`",
            "not yet a client for daemon start/stop/status/recover operator APIs",
        ] {
            assert!(
                readme.contains(marker),
                "{relative_path} missing {marker}: {readme}"
            );
        }
    }
}

#[test]
fn openyak_skills_lifecycle_uses_packaged_registry_with_temp_config_home() {
    let _guard = env_lock();
    let sandbox = CliSandbox::new("openyak-skills-lifecycle");
    let repo_root = repo_root();

    let available = sandbox.run_success_in(&repo_root, &["skills", "available"]);
    assert!(available.contains("Skills catalog"), "{available}");
    assert!(available.contains("release-checklist"), "{available}");

    let info_before_install =
        sandbox.run_success_in(&repo_root, &["skills", "info", "release-checklist"]);
    assert!(
        info_before_install.contains("Installed        no managed install"),
        "{info_before_install}"
    );

    let install = sandbox.run_success_owned_in(
        &repo_root,
        &[
            "skills".to_string(),
            "install".to_string(),
            "release-checklist".to_string(),
            "--version".to_string(),
            "1.0.0".to_string(),
        ],
    );
    assert!(install.contains("Result           installed"), "{install}");
    assert!(install.contains("Pinned version   1.0.0"), "{install}");
    assert!(
        sandbox
            .config_home
            .join("skills")
            .join(".managed")
            .join("release-checklist")
            .join("SKILL.md")
            .is_file(),
        "installed managed skill should exist"
    );

    let pinned_update =
        sandbox.run_success_in(&repo_root, &["skills", "update", "release-checklist"]);
    assert!(
        pinned_update.contains("Result           pinned"),
        "{pinned_update}"
    );

    let explicit_update = sandbox.run_success_owned_in(
        &repo_root,
        &[
            "skills".to_string(),
            "update".to_string(),
            "release-checklist".to_string(),
            "--version".to_string(),
            "2.0.0".to_string(),
        ],
    );
    assert!(
        explicit_update.contains("Result           updated"),
        "{explicit_update}"
    );
    assert!(
        explicit_update.contains("Old version      1.0.0"),
        "{explicit_update}"
    );
    assert!(
        explicit_update.contains("New version      2.0.0"),
        "{explicit_update}"
    );

    let info_after_update =
        sandbox.run_success_in(&repo_root, &["skills", "info", "release-checklist"]);
    assert!(
        info_after_update.contains("Installed        v2.0.0"),
        "{info_after_update}"
    );

    let uninstall =
        sandbox.run_success_in(&repo_root, &["skills", "uninstall", "release-checklist"]);
    assert!(
        uninstall.contains("Result           uninstalled"),
        "{uninstall}"
    );
    assert!(
        !sandbox
            .config_home
            .join("skills")
            .join(".managed")
            .join("release-checklist")
            .exists(),
        "managed install should be removed after uninstall"
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn openyak_direct_slash_entry_and_resume_safe_commands_work_from_persisted_session() {
    let _guard = env_lock();
    let sandbox = CliSandbox::new("openyak-resume-flow");

    let slash_agents = sandbox.run_success(&["/agents"]);
    assert!(slash_agents.contains("No agents found."), "{slash_agents}");

    let slash_skills = sandbox.run_success(&["/skills"]);
    assert!(slash_skills.contains("No skills found."), "{slash_skills}");

    let slash_foundations = sandbox.run_success(&["/foundations"]);
    assert!(
        slash_foundations.contains("Foundations"),
        "{slash_foundations}"
    );
    assert!(
        slash_foundations.contains("TaskCreate"),
        "{slash_foundations}"
    );

    let slash_foundations_task = sandbox.run_success(&["/foundations", "task"]);
    assert!(
        slash_foundations_task.contains("Family           task"),
        "{slash_foundations_task}"
    );

    let slash_skills_help = sandbox.run_success(&["/skills", "help"]);
    assert!(
        slash_skills_help.contains("Usage            /skills"),
        "{slash_skills_help}"
    );
    assert!(
        slash_skills_help.contains("managed installs land under <openyak-home>/skills/.managed"),
        "{slash_skills_help}"
    );

    let init = sandbox.run_success(&["init"]);
    assert!(init.contains("Init"), "{init}");

    let prompt_failure = sandbox.run_failure(&["prompt", "hello"]);
    assert!(
        prompt_failure.contains("missing openyak credentials"),
        "{prompt_failure}"
    );

    let session_path = sandbox
        .managed_session_paths()
        .into_iter()
        .next()
        .expect("prompt failure should persist a managed session");
    assert!(session_path.is_file(), "managed session file should exist");

    let resume_output = sandbox.run_success_owned(&[
        "--resume".to_string(),
        session_path.display().to_string(),
        "/status".to_string(),
        "/config".to_string(),
        "env".to_string(),
        "/memory".to_string(),
        "/foundations".to_string(),
        "mcp".to_string(),
        "/version".to_string(),
        "/agents".to_string(),
        "/skills".to_string(),
        "/init".to_string(),
        "/diff".to_string(),
        "/export".to_string(),
        "notes.txt".to_string(),
        "/cost".to_string(),
        "/compact".to_string(),
        "/clear".to_string(),
        "--confirm".to_string(),
    ]);

    assert!(resume_output.contains("Session"), "{resume_output}");
    assert!(resume_output.contains("Session file"), "{resume_output}");
    assert!(
        resume_output.contains("Merged section: env"),
        "{resume_output}"
    );
    assert!(resume_output.contains("Memory"), "{resume_output}");
    assert!(
        resume_output.contains("Family           mcp"),
        "{resume_output}"
    );
    assert!(resume_output.contains("ListMcpServers"), "{resume_output}");
    assert!(resume_output.contains("openyak"), "{resume_output}");
    assert!(
        resume_output.contains("No agents found."),
        "{resume_output}"
    );
    assert!(
        resume_output.contains("No skills found."),
        "{resume_output}"
    );
    assert!(resume_output.contains("Init"), "{resume_output}");
    assert!(
        resume_output.contains("Diff\n  Result           unavailable"),
        "{resume_output}"
    );
    assert!(
        resume_output.contains("Export\n  Result           wrote transcript"),
        "{resume_output}"
    );
    assert!(resume_output.contains("Cost"), "{resume_output}");
    assert!(
        resume_output.contains("Compact\n  Result           skipped"),
        "{resume_output}"
    );
    assert!(
        resume_output.contains("Cleared resumed session file"),
        "{resume_output}"
    );

    let notes =
        fs::read_to_string(sandbox.workspace.join("notes.txt")).expect("notes should export");
    assert!(notes.contains("# Conversation Export"), "{notes}");
    assert!(notes.contains("## 1. user"), "{notes}");
    assert!(notes.contains("hello"), "{notes}");

    let cleared: Value = serde_json::from_str(
        &fs::read_to_string(&session_path).expect("cleared session file should load"),
    )
    .expect("cleared session json should parse");
    assert_eq!(
        cleared["messages"]
            .as_array()
            .map(std::vec::Vec::len)
            .unwrap_or_default(),
        0,
        "cleared resumed session should contain no messages"
    );
}

#[test]
fn openyak_login_fails_cleanly_when_oauth_is_not_configured() {
    let _guard = env_lock();
    let sandbox = CliSandbox::new("openyak-login-missing-oauth");

    let failure = sandbox.run_failure(&["login"]);
    assert!(
        failure.contains("requires settings.oauth.clientId, authorizeUrl, and tokenUrl"),
        "{failure}"
    );
}

struct CliSandbox {
    root: PathBuf,
    workspace: PathBuf,
    config_home: PathBuf,
    codex_home: PathBuf,
    home_dir: PathBuf,
}

impl CliSandbox {
    fn new(prefix: &str) -> Self {
        let root = unique_temp_dir(prefix);
        let workspace = root.join("workspace");
        let config_home = root.join("openyak-home");
        let codex_home = root.join("codex-home");
        let home_dir = root.join("home");
        fs::create_dir_all(&workspace).expect("workspace should exist");
        fs::create_dir_all(&config_home).expect("config home should exist");
        fs::create_dir_all(&codex_home).expect("codex home should exist");
        fs::create_dir_all(&home_dir).expect("home dir should exist");
        Self {
            root,
            workspace,
            config_home,
            codex_home,
            home_dir,
        }
    }

    fn managed_session_paths(&self) -> Vec<PathBuf> {
        fs::read_dir(self.workspace.join(".openyak").join("sessions"))
            .expect("managed sessions dir should exist")
            .map(|entry| entry.expect("session entry should load").path())
            .collect()
    }

    fn run_success(&self, args: &[&str]) -> String {
        self.run_success_in(&self.workspace, args)
    }

    fn run_success_in(&self, cwd: &Path, args: &[&str]) -> String {
        let owned_args = args
            .iter()
            .map(|item| (*item).to_string())
            .collect::<Vec<_>>();
        self.run_success_owned_in(cwd, &owned_args)
    }

    fn run_success_owned(&self, args: &[String]) -> String {
        self.run_success_owned_in(&self.workspace, args)
    }

    fn run_success_owned_in(&self, cwd: &Path, args: &[String]) -> String {
        let output = self.run_output_owned(cwd, args);
        let rendered = output_text(&output);
        assert!(
            output.status.success(),
            "expected success for `{}`\n{}",
            args.join(" "),
            rendered
        );
        rendered
    }

    fn run_failure(&self, args: &[&str]) -> String {
        let owned_args = args
            .iter()
            .map(|item| (*item).to_string())
            .collect::<Vec<_>>();
        let output = self.run_output_owned(&self.workspace, &owned_args);
        let rendered = output_text(&output);
        assert!(
            !output.status.success(),
            "expected failure for `{}`\n{}",
            args.join(" "),
            rendered
        );
        rendered
    }

    fn run_output_owned(&self, cwd: &Path, args: &[String]) -> Output {
        let executable = common::openyak_binary();
        self.command_owned(cwd, args, &executable)
            .output()
            .unwrap_or_else(|error| {
                panic!(
                    "command should run: exe={} cwd={} args=`{}`: {error}",
                    executable.display(),
                    cwd.display(),
                    args.join(" ")
                )
            })
    }

    fn command_owned(&self, cwd: &Path, args: &[String], executable: &Path) -> Command {
        let mut command = Command::new(executable);
        command
            .args(args)
            .current_dir(cwd)
            .env("OPENYAK_CONFIG_HOME", &self.config_home)
            .env("CODEX_HOME", &self.codex_home)
            .env("HOME", &self.home_dir)
            .env("USERPROFILE", &self.home_dir)
            .env_remove("ANTHROPIC_API_KEY")
            .env_remove("ANTHROPIC_AUTH_TOKEN")
            .env_remove("OPENAI_API_KEY");
        command
    }
}

impl Drop for CliSandbox {
    fn drop(&mut self) {
        cleanup_temp_dir(&self.root);
    }
}

fn repo_root() -> PathBuf {
    common::repo_root()
}

fn output_text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn cleanup_temp_dir(path: impl AsRef<Path>) {
    let path = path.as_ref();
    for attempt in 0..10 {
        match fs::remove_dir_all(path) {
            Ok(()) => return,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return,
            Err(error) if cfg!(windows) && attempt < 9 => {
                let _ = error;
                thread::sleep(Duration::from_millis(50));
            }
            Err(error) => panic!("cleanup temp dir {}: {error}", path.display()),
        }
    }
    panic!("cleanup temp dir exhausted retries for {}", path.display());
}

fn unique_temp_dir(prefix: &str) -> PathBuf {
    let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time should be after epoch")
        .as_nanos();
    std::env::temp_dir().join(format!("{prefix}-{nanos}-{counter}"))
}

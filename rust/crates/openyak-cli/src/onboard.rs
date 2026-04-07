use std::env;
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};

use api::{detect_provider_kind, resolve_model_alias, ProviderKind};
use runtime::{load_oauth_credentials, ConfigEntry, ConfigLoader, OAuthTokenSet, RuntimeConfig};
use serde_json::{Map, Value};

use crate::init::{assess_repo_init, initialize_repo, InitReport, RepoInitAssessment};

const OAUTH_LOOPBACK_TEMPLATE: &str = "rust/docs/oauth.settings.loopback.template.json";
const OAUTH_MANUAL_TEMPLATE: &str = "rust/docs/oauth.settings.manual-redirect.template.json";

pub(crate) fn run_onboard(
    requested_model: Option<&str>,
    output_format: crate::CliOutputFormat,
) -> Result<(), Box<dyn std::error::Error>> {
    if !matches!(output_format, crate::CliOutputFormat::Text) {
        return Err(io::Error::other(
            "openyak onboard only supports text output because the phase-1 wizard is interactive",
        )
        .into());
    }

    let cwd = env::current_dir()?;
    let assessment = collect_assessment(&cwd, requested_model);
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        let mut stdout = io::stdout();
        render_noninteractive_guidance(&assessment, &mut stdout)?;
        return Err(io::Error::other("openyak onboard requires an interactive terminal").into());
    }

    let mut stdout = io::stdout();
    let mut prompter = TerminalPrompter;
    let mut actions = LiveOnboardingActions;
    let initial_model_override = requested_model.map(resolve_model_alias);
    let outcome = run_onboarding_session(
        &cwd,
        initial_model_override,
        &mut prompter,
        &mut actions,
        &mut stdout,
    )?;
    if outcome.doctor_has_errors {
        return Err(io::Error::other("openyak onboard found blocking issues").into());
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct OnboardingAssessment {
    workspace: PathBuf,
    config_home: PathBuf,
    discovered_entries: Vec<ConfigEntry>,
    loaded_entries: Vec<ConfigEntry>,
    config_error: Option<String>,
    repo_init: RepoInitAssessment,
    effective_model: String,
    user_default_model: Option<String>,
    user_default_model_error: Option<String>,
    auth: AuthAssessment,
    doctor_report: crate::DoctorReport,
}

impl OnboardingAssessment {
    fn doctor_status_label(&self) -> &'static str {
        let (_ok, warnings, errors) = self.doctor_report.counts();
        if errors > 0 {
            "blocked"
        } else if warnings > 0 {
            "warnings expected"
        } else {
            "likely clean"
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AuthAssessment {
    provider: ProviderKind,
    provider_label: &'static str,
    summary: String,
    guidance: Vec<String>,
    login_available: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PromptChoice {
    Run,
    Skip,
    Cancel,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ModelChoice {
    Skip,
    Persist(String),
    Cancel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OnboardingStatus {
    Completed,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct OnboardingOutcome {
    status: OnboardingStatus,
    doctor_has_errors: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModelPersistStatus {
    Created,
    Updated,
    Unchanged,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ModelPersistResult {
    path: PathBuf,
    status: ModelPersistStatus,
}

trait OnboardingPrompter {
    fn prompt_repo_init(
        &mut self,
        out: &mut dyn Write,
        assessment: &OnboardingAssessment,
    ) -> io::Result<PromptChoice>;
    fn prompt_model(
        &mut self,
        out: &mut dyn Write,
        assessment: &OnboardingAssessment,
    ) -> io::Result<ModelChoice>;
    fn prompt_login(
        &mut self,
        out: &mut dyn Write,
        assessment: &OnboardingAssessment,
    ) -> io::Result<PromptChoice>;
    fn prompt_doctor(&mut self, out: &mut dyn Write) -> io::Result<PromptChoice>;
}

trait OnboardingActions {
    fn initialize_repo(&mut self, cwd: &Path) -> Result<InitReport, Box<dyn std::error::Error>>;
    fn persist_default_model(
        &mut self,
        config_home: &Path,
        model: &str,
    ) -> Result<ModelPersistResult, Box<dyn std::error::Error>>;
    fn run_login(&mut self) -> Result<(), Box<dyn std::error::Error>>;
    fn collect_doctor_report(&mut self, cwd: &Path) -> crate::DoctorReport;
}

struct LiveOnboardingActions;

impl OnboardingActions for LiveOnboardingActions {
    fn initialize_repo(&mut self, cwd: &Path) -> Result<InitReport, Box<dyn std::error::Error>> {
        initialize_repo(cwd)
    }

    fn persist_default_model(
        &mut self,
        config_home: &Path,
        model: &str,
    ) -> Result<ModelPersistResult, Box<dyn std::error::Error>> {
        persist_default_model(config_home, model)
    }

    fn run_login(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        crate::run_login()
    }

    fn collect_doctor_report(&mut self, cwd: &Path) -> crate::DoctorReport {
        crate::collect_doctor_report(cwd)
    }
}

struct TerminalPrompter;

impl TerminalPrompter {
    fn read_line(out: &mut dyn Write, prompt: &str) -> io::Result<String> {
        write!(out, "{prompt}")?;
        out.flush()?;
        let mut input = String::new();
        let read = io::stdin().read_line(&mut input)?;
        if read == 0 {
            return Ok("q".to_string());
        }
        Ok(input.trim().to_string())
    }
}

impl OnboardingPrompter for TerminalPrompter {
    fn prompt_repo_init(
        &mut self,
        out: &mut dyn Write,
        _assessment: &OnboardingAssessment,
    ) -> io::Result<PromptChoice> {
        writeln!(out, "Repo init handoff")?;
        writeln!(out, "  y) run openyak init now")?;
        writeln!(out, "  Enter / n) skip for now")?;
        writeln!(out, "  q) cancel onboarding")?;
        match Self::read_line(out, "Selection: ")?.as_str() {
            "y" | "Y" => Ok(PromptChoice::Run),
            "q" | "Q" | "cancel" => Ok(PromptChoice::Cancel),
            _ => Ok(PromptChoice::Skip),
        }
    }

    fn prompt_model(
        &mut self,
        out: &mut dyn Write,
        assessment: &OnboardingAssessment,
    ) -> io::Result<ModelChoice> {
        writeln!(out, "Default model setup")?;
        writeln!(out, "  Enter) leave user settings unchanged")?;
        writeln!(
            out,
            "  1) persist current effective model ({})",
            assessment.effective_model
        )?;
        writeln!(out, "  2) claude-opus-4-6")?;
        writeln!(out, "  3) claude-sonnet-4-6")?;
        writeln!(out, "  4) claude-haiku-4-5-20251213")?;
        writeln!(out, "  5) enter a custom model string")?;
        writeln!(out, "  q) cancel onboarding")?;
        match Self::read_line(out, "Selection: ")?.as_str() {
            "1" => Ok(ModelChoice::Persist(assessment.effective_model.clone())),
            "2" => Ok(ModelChoice::Persist("claude-opus-4-6".to_string())),
            "3" => Ok(ModelChoice::Persist("claude-sonnet-4-6".to_string())),
            "4" => Ok(ModelChoice::Persist(
                "claude-haiku-4-5-20251213".to_string(),
            )),
            "5" => {
                let value = Self::read_line(out, "Custom model: ")?;
                if value.eq_ignore_ascii_case("q") || value.eq_ignore_ascii_case("cancel") {
                    Ok(ModelChoice::Cancel)
                } else if value.is_empty() {
                    Ok(ModelChoice::Skip)
                } else {
                    Ok(ModelChoice::Persist(resolve_model_alias(&value)))
                }
            }
            "q" | "Q" | "cancel" => Ok(ModelChoice::Cancel),
            _ => Ok(ModelChoice::Skip),
        }
    }

    fn prompt_login(
        &mut self,
        out: &mut dyn Write,
        _assessment: &OnboardingAssessment,
    ) -> io::Result<PromptChoice> {
        writeln!(out, "OAuth handoff")?;
        writeln!(out, "  y) run openyak login now")?;
        writeln!(out, "  Enter / n) skip for now")?;
        writeln!(out, "  q) cancel onboarding")?;
        match Self::read_line(out, "Selection: ")?.as_str() {
            "y" | "Y" => Ok(PromptChoice::Run),
            "q" | "Q" | "cancel" => Ok(PromptChoice::Cancel),
            _ => Ok(PromptChoice::Skip),
        }
    }

    fn prompt_doctor(&mut self, out: &mut dyn Write) -> io::Result<PromptChoice> {
        writeln!(out, "Doctor handoff")?;
        writeln!(out, "  Enter) run openyak doctor now")?;
        writeln!(out, "  q) stop here")?;
        match Self::read_line(out, "Selection: ")?.as_str() {
            "q" | "Q" | "cancel" => Ok(PromptChoice::Cancel),
            _ => Ok(PromptChoice::Run),
        }
    }
}

fn run_onboarding_session<P, A, W>(
    cwd: &Path,
    initial_model_override: Option<String>,
    prompter: &mut P,
    actions: &mut A,
    out: &mut W,
) -> Result<OnboardingOutcome, Box<dyn std::error::Error>>
where
    P: OnboardingPrompter,
    A: OnboardingActions,
    W: Write,
{
    let mut model_override = initial_model_override;
    let mut assessment = collect_assessment(cwd, model_override.as_deref());
    render_assessment(&assessment, out)?;

    if !assessment.repo_init.is_ready() {
        match prompter.prompt_repo_init(out, &assessment)? {
            PromptChoice::Run => {
                let report = actions.initialize_repo(cwd)?;
                writeln!(out)?;
                writeln!(out, "{}", report.render())?;
                assessment = collect_assessment(cwd, model_override.as_deref());
            }
            PromptChoice::Skip => {}
            PromptChoice::Cancel => {
                writeln!(out)?;
                writeln!(out, "Onboarding cancelled before repo init.")?;
                return Ok(OnboardingOutcome {
                    status: OnboardingStatus::Cancelled,
                    doctor_has_errors: false,
                });
            }
        }
    }

    match prompter.prompt_model(out, &assessment)? {
        ModelChoice::Skip => {}
        ModelChoice::Persist(model) => {
            let result = actions.persist_default_model(&assessment.config_home, &model)?;
            writeln!(out)?;
            writeln!(out, "Persisted default model at {}.", result.path.display())?;
            match result.status {
                ModelPersistStatus::Created => writeln!(out, "  Status            created")?,
                ModelPersistStatus::Updated => writeln!(out, "  Status            updated")?,
                ModelPersistStatus::Unchanged => writeln!(out, "  Status            unchanged")?,
            }
            writeln!(out, "  Model             {model}")?;
            writeln!(
                out,
                "  Note              session-only /model stays unchanged"
            )?;
            model_override = Some(model);
            assessment = collect_assessment(cwd, model_override.as_deref());
        }
        ModelChoice::Cancel => {
            writeln!(out)?;
            writeln!(out, "Onboarding cancelled before auth guidance.")?;
            return Ok(OnboardingOutcome {
                status: OnboardingStatus::Cancelled,
                doctor_has_errors: false,
            });
        }
    }

    writeln!(out)?;
    render_auth_guidance(&assessment, out)?;
    if matches!(assessment.auth.provider, ProviderKind::OpenyakApi)
        && assessment.auth.login_available
    {
        match prompter.prompt_login(out, &assessment)? {
            PromptChoice::Run => {
                actions.run_login()?;
                writeln!(out, "Completed openyak login handoff.")?;
                assessment = collect_assessment(cwd, model_override.as_deref());
            }
            PromptChoice::Skip => {}
            PromptChoice::Cancel => {
                writeln!(out)?;
                writeln!(out, "Onboarding cancelled before doctor.")?;
                return Ok(OnboardingOutcome {
                    status: OnboardingStatus::Cancelled,
                    doctor_has_errors: false,
                });
            }
        }
    }

    match prompter.prompt_doctor(out)? {
        PromptChoice::Run => {
            let report = actions.collect_doctor_report(cwd);
            writeln!(out)?;
            write!(out, "{}", crate::render_doctor_report(&report))?;
            writeln!(out)?;
            render_completion_summary(&assessment, &report, out)?;
            Ok(OnboardingOutcome {
                status: OnboardingStatus::Completed,
                doctor_has_errors: report.has_errors(),
            })
        }
        PromptChoice::Skip | PromptChoice::Cancel => {
            writeln!(out)?;
            writeln!(out, "Onboarding stopped before the final doctor handoff.")?;
            Ok(OnboardingOutcome {
                status: OnboardingStatus::Cancelled,
                doctor_has_errors: false,
            })
        }
    }
}

fn collect_assessment(cwd: &Path, requested_model: Option<&str>) -> OnboardingAssessment {
    let loader = ConfigLoader::default_for(cwd);
    let discovered_entries = loader.discover();
    let config_result = loader.load();
    let doctor_report = crate::collect_doctor_report(cwd);
    let config_home = loader.config_home().to_path_buf();
    let repo_init = assess_repo_init(cwd);
    let user_default_model_result = read_user_default_model(&config_home);

    let (loaded_entries, config_error, effective_model, auth) = match config_result {
        Ok(config) => {
            let effective_model = requested_model.map_or_else(
                || {
                    config
                        .model()
                        .map_or(crate::DEFAULT_MODEL.to_string(), resolve_model_alias)
                },
                resolve_model_alias,
            );
            let auth = assess_auth(&effective_model, Some(&config));
            (
                config.loaded_entries().to_vec(),
                None,
                effective_model,
                auth,
            )
        }
        Err(error) => {
            let effective_model = requested_model
                .map_or_else(|| crate::DEFAULT_MODEL.to_string(), resolve_model_alias);
            let auth = assess_auth(&effective_model, None);
            (Vec::new(), Some(error.to_string()), effective_model, auth)
        }
    };

    let (user_default_model, user_default_model_error) = match user_default_model_result {
        Ok(model) => (model, None),
        Err(error) => (None, Some(error)),
    };

    OnboardingAssessment {
        workspace: cwd.to_path_buf(),
        config_home,
        discovered_entries,
        loaded_entries,
        config_error,
        repo_init,
        effective_model,
        user_default_model,
        user_default_model_error,
        auth,
        doctor_report,
    }
}

fn render_assessment(assessment: &OnboardingAssessment, out: &mut impl Write) -> io::Result<()> {
    writeln!(out, "openyak onboarding")?;
    writeln!(
        out,
        "  Workspace         {}",
        assessment.workspace.display()
    )?;
    writeln!(
        out,
        "  Config home       {}",
        assessment.config_home.display()
    )?;
    writeln!(
        out,
        "  Config files      {} discovered / {} loaded",
        assessment.discovered_entries.len(),
        assessment.loaded_entries.len()
    )?;
    writeln!(
        out,
        "  Repo init         {}",
        if assessment.repo_init.is_ready() {
            "ready".to_string()
        } else {
            format!(
                "needs attention ({})",
                assessment.repo_init.missing_items().join(", ")
            )
        }
    )?;
    writeln!(out, "  Effective model   {}", assessment.effective_model)?;
    writeln!(
        out,
        "  User default      {}",
        assessment
            .user_default_model
            .as_deref()
            .unwrap_or("<unset>")
    )?;
    writeln!(
        out,
        "  Provider          {}",
        assessment.auth.provider_label
    )?;
    writeln!(out, "  Auth readiness    {}", assessment.auth.summary)?;
    writeln!(
        out,
        "  Doctor            {}",
        assessment.doctor_status_label()
    )?;
    if let Some(error) = &assessment.config_error {
        writeln!(out, "  Config issue      {error}")?;
    }
    if let Some(error) = &assessment.user_default_model_error {
        writeln!(out, "  User settings     {error}")?;
    }
    if !assessment.loaded_entries.is_empty() {
        writeln!(out)?;
        writeln!(out, "Loaded config files")?;
        for entry in &assessment.loaded_entries {
            writeln!(out, "  {}", entry.path.display())?;
        }
    }
    writeln!(out)?;
    writeln!(out, "Notes")?;
    writeln!(
        out,
        "  - openyak onboard is explicit, local-only, interactive-only, and rerunnable."
    )?;
    writeln!(
        out,
        "  - onboarding only writes a user default model if you approve it."
    )?;
    Ok(())
}

fn render_auth_guidance(assessment: &OnboardingAssessment, out: &mut impl Write) -> io::Result<()> {
    writeln!(out, "Auth guidance")?;
    writeln!(
        out,
        "  Provider          {}",
        assessment.auth.provider_label
    )?;
    writeln!(out, "  Summary           {}", assessment.auth.summary)?;
    for line in &assessment.auth.guidance {
        writeln!(out, "  Next step         {line}")?;
    }
    Ok(())
}

fn render_noninteractive_guidance(
    assessment: &OnboardingAssessment,
    out: &mut impl Write,
) -> io::Result<()> {
    writeln!(out, "openyak onboarding")?;
    writeln!(out, "  Result            interactive terminal required")?;
    writeln!(out, "  Manual repo init  openyak init")?;
    writeln!(out, "  Manual health     openyak doctor")?;
    writeln!(out, "  Effective model   {}", assessment.effective_model)?;
    for line in assessment.auth.guidance.iter().take(2) {
        writeln!(out, "  Manual auth       {line}")?;
    }
    writeln!(
        out,
        "  Docs              README.md, rust/README.md, {OAUTH_LOOPBACK_TEMPLATE}, {OAUTH_MANUAL_TEMPLATE}"
    )?;
    Ok(())
}

fn render_completion_summary(
    assessment: &OnboardingAssessment,
    report: &crate::DoctorReport,
    out: &mut impl Write,
) -> io::Result<()> {
    let (_ok, warnings, errors) = report.counts();
    writeln!(out, "Onboarding summary")?;
    writeln!(
        out,
        "  Status            {}",
        if errors > 0 {
            "needs follow-up"
        } else if warnings > 0 {
            "ready with warnings"
        } else {
            "ready"
        }
    )?;
    writeln!(out, "  Next commands     openyak")?;
    writeln!(
        out,
        "  One-shot prompt   openyak prompt \"summarize this repo\""
    )?;
    if !assessment.repo_init.is_ready() {
        writeln!(out, "  Repo init         openyak init")?;
    }
    if matches!(assessment.auth.provider, ProviderKind::OpenyakApi)
        && assessment.auth.login_available
    {
        writeln!(out, "  OAuth handoff     openyak login")?;
    }
    if errors > 0 || warnings > 0 {
        writeln!(out, "  Re-check          openyak doctor")?;
    }
    Ok(())
}

fn assess_auth(model: &str, config: Option<&RuntimeConfig>) -> AuthAssessment {
    let provider = detect_provider_kind(model);
    match provider {
        ProviderKind::OpenyakApi => assess_openyak_auth(config),
        ProviderKind::Xai => assess_env_auth(
            provider,
            "xai",
            "XAI_API_KEY",
            Some("Optional advanced override: XAI_BASE_URL."),
        ),
        ProviderKind::OpenAi => assess_env_auth(
            provider,
            "openai-compatible",
            "OPENAI_API_KEY",
            Some("Optional advanced override: OPENAI_BASE_URL."),
        ),
    }
}

fn assess_env_auth(
    provider: ProviderKind,
    provider_label: &'static str,
    env_var: &'static str,
    extra: Option<&'static str>,
) -> AuthAssessment {
    let env_ready = env_var_present(env_var);
    let mut guidance = vec![format!(
        "Set {env_var} in your shell before starting openyak."
    )];
    if let Some(extra) = extra {
        guidance.push(extra.to_string());
    }
    AuthAssessment {
        provider,
        provider_label,
        summary: if env_ready {
            format!("Environment-backed auth is ready via {env_var}.")
        } else {
            format!("Environment-backed auth is not ready; {env_var} is missing.")
        },
        guidance,
        login_available: false,
    }
}

fn assess_openyak_auth(config: Option<&RuntimeConfig>) -> AuthAssessment {
    let env_ready = env_var_present("ANTHROPIC_API_KEY") || env_var_present("ANTHROPIC_AUTH_TOKEN");
    let saved_oauth_state = read_saved_oauth_state();
    let oauth_config = config.map(crate::configured_oauth_config);
    let (summary, guidance, login_available) = if env_ready {
        (
            "Environment-backed auth is ready via ANTHROPIC_API_KEY or ANTHROPIC_AUTH_TOKEN."
                .to_string(),
            vec![
                "Keep API keys in environment variables; onboarding never writes provider secrets to config."
                    .to_string(),
                "If you prefer OAuth instead, complete settings.oauth first and then rerun openyak login."
                    .to_string(),
            ],
            matches!(oauth_config, Some(Ok(Some(_)))),
        )
    } else {
        match (oauth_config, saved_oauth_state) {
            (Some(Ok(Some(_))), SavedOAuthState::Ready) => (
                "OAuth-backed auth is ready from saved credentials.".to_string(),
                vec!["Run openyak login again only if you need to refresh or replace the saved OAuth token.".to_string()],
                true,
            ),
            (Some(Ok(Some(_))), SavedOAuthState::ExpiredRefreshable) => (
                "Saved OAuth credentials are expired but refreshable.".to_string(),
                vec!["Run openyak login if the next provider refresh attempt fails.".to_string()],
                true,
            ),
            (Some(Ok(Some(_))), SavedOAuthState::ExpiredUnrefreshable) => (
                "OAuth is configured, but the saved credentials are expired.".to_string(),
                vec!["Run openyak login to replace the expired OAuth token.".to_string()],
                true,
            ),
            (Some(Ok(Some(_))), SavedOAuthState::Missing) => (
                "OAuth is configured, but no saved OAuth credentials were found yet.".to_string(),
                vec!["Run openyak login to complete browser-based auth.".to_string()],
                true,
            ),
            (Some(Ok(Some(_))), SavedOAuthState::Error) => (
                "OAuth is configured, but saved credentials could not be read.".to_string(),
                vec![
                    "Repair the credentials store or rerun openyak login after confirming settings.oauth is complete."
                        .to_string(),
                ],
                true,
            ),
            (Some(Err(error)), _) => (
                format!("OAuth config is incomplete: {error}"),
                vec![
                    format!(
                        "Complete settings.oauth manually or start from the templates at {OAUTH_LOOPBACK_TEMPLATE} or {OAUTH_MANUAL_TEMPLATE}."
                    ),
                    "Onboarding will not author OAuth endpoints or store secrets for you.".to_string(),
                ],
                false,
            ),
            _ => (
                "No openyak/Anthropic auth is configured yet.".to_string(),
                vec![
                    "Set ANTHROPIC_API_KEY or ANTHROPIC_AUTH_TOKEN in your shell for env-backed auth.".to_string(),
                    format!(
                        "For OAuth, fill in settings.oauth.clientId, authorizeUrl, and tokenUrl manually or start from {OAUTH_LOOPBACK_TEMPLATE} or {OAUTH_MANUAL_TEMPLATE}."
                    ),
                ],
                false,
            ),
        }
    };

    AuthAssessment {
        provider: ProviderKind::OpenyakApi,
        provider_label: "openyak",
        summary,
        guidance,
        login_available,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SavedOAuthState {
    Ready,
    ExpiredRefreshable,
    ExpiredUnrefreshable,
    Missing,
    Error,
}

fn read_saved_oauth_state() -> SavedOAuthState {
    match load_oauth_credentials() {
        Ok(Some(token_set))
            if oauth_token_expired(&token_set) && token_set.refresh_token.is_some() =>
        {
            SavedOAuthState::ExpiredRefreshable
        }
        Ok(Some(token_set)) if oauth_token_expired(&token_set) => {
            SavedOAuthState::ExpiredUnrefreshable
        }
        Ok(Some(_)) => SavedOAuthState::Ready,
        Ok(None) => SavedOAuthState::Missing,
        Err(_) => SavedOAuthState::Error,
    }
}

fn oauth_token_expired(token_set: &OAuthTokenSet) -> bool {
    crate::doctor_token_is_expired(token_set.expires_at)
}

fn env_var_present(name: &str) -> bool {
    std::env::var(name)
        .ok()
        .is_some_and(|value| !value.trim().is_empty())
}

fn user_settings_path(config_home: &Path) -> PathBuf {
    config_home.join("settings.json")
}

fn read_user_default_model(config_home: &Path) -> Result<Option<String>, String> {
    let settings = read_user_settings(config_home)?;
    Ok(settings
        .get("model")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned))
}

fn read_user_settings(config_home: &Path) -> Result<Map<String, Value>, String> {
    let path = user_settings_path(config_home);
    let contents = match fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Map::new()),
        Err(error) => return Err(format!("{}: {error}", path.display())),
    };
    if contents.trim().is_empty() {
        return Ok(Map::new());
    }
    let value: Value =
        serde_json::from_str(&contents).map_err(|error| format!("{}: {error}", path.display()))?;
    let object = value.as_object().ok_or_else(|| {
        format!(
            "{}: top-level settings value must be a JSON object",
            path.display()
        )
    })?;
    Ok(object.clone())
}

fn persist_default_model(
    config_home: &Path,
    model: &str,
) -> Result<ModelPersistResult, Box<dyn std::error::Error>> {
    let settings = read_user_settings(config_home).map_err(io::Error::other)?;
    let previous = settings
        .get("model")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let status = match previous.as_deref() {
        Some(existing) if existing == model => ModelPersistStatus::Unchanged,
        Some(_) => ModelPersistStatus::Updated,
        None => ModelPersistStatus::Created,
    };
    let loader = ConfigLoader::new(PathBuf::from("."), config_home.to_path_buf());
    let path = loader.write_user_model(model)?;
    Ok(ModelPersistResult { path, status })
}

#[cfg(test)]
mod tests {
    use super::{
        collect_assessment, persist_default_model, render_noninteractive_guidance,
        run_onboarding_session, ModelChoice, ModelPersistStatus, OnboardingActions,
        OnboardingPrompter, OnboardingStatus, PromptChoice,
    };
    use crate::init::{initialize_repo, InitReport};
    use std::ffi::OsString;
    use std::fs;
    use std::io;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    struct ScriptedPrompter {
        repo_init: PromptChoice,
        model: ModelChoice,
        login: PromptChoice,
        doctor: PromptChoice,
    }

    impl OnboardingPrompter for ScriptedPrompter {
        fn prompt_repo_init(
            &mut self,
            _out: &mut dyn io::Write,
            _assessment: &super::OnboardingAssessment,
        ) -> io::Result<PromptChoice> {
            Ok(self.repo_init)
        }

        fn prompt_model(
            &mut self,
            _out: &mut dyn io::Write,
            _assessment: &super::OnboardingAssessment,
        ) -> io::Result<ModelChoice> {
            Ok(self.model.clone())
        }

        fn prompt_login(
            &mut self,
            _out: &mut dyn io::Write,
            _assessment: &super::OnboardingAssessment,
        ) -> io::Result<PromptChoice> {
            Ok(self.login)
        }

        fn prompt_doctor(&mut self, _out: &mut dyn io::Write) -> io::Result<PromptChoice> {
            Ok(self.doctor)
        }
    }

    #[derive(Default)]
    struct RecordingActions {
        init_called: bool,
        login_called: bool,
        doctor_called: bool,
    }

    impl OnboardingActions for RecordingActions {
        fn initialize_repo(
            &mut self,
            cwd: &Path,
        ) -> Result<InitReport, Box<dyn std::error::Error>> {
            self.init_called = true;
            initialize_repo(cwd)
        }

        fn persist_default_model(
            &mut self,
            config_home: &Path,
            model: &str,
        ) -> Result<super::ModelPersistResult, Box<dyn std::error::Error>> {
            persist_default_model(config_home, model)
        }

        fn run_login(&mut self) -> Result<(), Box<dyn std::error::Error>> {
            self.login_called = true;
            Ok(())
        }

        fn collect_doctor_report(&mut self, cwd: &Path) -> crate::DoctorReport {
            self.doctor_called = true;
            crate::collect_doctor_report(cwd)
        }
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

    fn temp_dir(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time should be after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("{prefix}-{nanos}"))
    }

    fn write_fake_command(dir: &Path, name: &str) {
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
    }

    #[test]
    fn assessment_reports_first_run_state() {
        let _lock = env_lock();
        let root = temp_dir("openyak-onboard-first-run");
        let workspace = root.join("workspace");
        let config_home = root.join("openyak-home");
        fs::create_dir_all(&workspace).expect("workspace should exist");
        fs::create_dir_all(&config_home).expect("config home should exist");
        let _openyak_home =
            EnvVarGuard::set("OPENYAK_CONFIG_HOME", Some(&config_home.to_string_lossy()));
        let _api_key = EnvVarGuard::set("ANTHROPIC_API_KEY", None);
        let _auth_token = EnvVarGuard::set("ANTHROPIC_AUTH_TOKEN", None);

        let assessment = collect_assessment(&workspace, None);

        assert!(!assessment.repo_init.is_ready());
        assert_eq!(assessment.effective_model, crate::DEFAULT_MODEL);
        assert!(assessment
            .auth
            .summary
            .contains("No openyak/Anthropic auth"));
        assert_eq!(assessment.doctor_status_label(), "warnings expected");

        fs::remove_dir_all(root).expect("cleanup temp dir");
    }

    #[test]
    fn persist_default_model_merges_existing_settings() {
        let root = temp_dir("openyak-onboard-model-write");
        let config_home = root.join("openyak-home");
        fs::create_dir_all(&config_home).expect("config home should exist");
        fs::write(
            config_home.join("settings.json"),
            "{\n  \"oauth\": {\n    \"clientId\": \"demo\"\n  }\n}\n",
        )
        .expect("settings should write");

        let result = persist_default_model(&config_home, "claude-sonnet-4-6")
            .expect("model write should succeed");
        assert_eq!(result.status, ModelPersistStatus::Created);
        let written =
            fs::read_to_string(config_home.join("settings.json")).expect("settings should read");
        assert!(written.contains("\"model\": \"claude-sonnet-4-6\""));
        assert!(written.contains("\"clientId\": \"demo\""));

        fs::remove_dir_all(root).expect("cleanup temp dir");
    }

    #[test]
    fn skip_flow_leaves_repo_uninitialized_but_runs_doctor() {
        let _lock = env_lock();
        let root = temp_dir("openyak-onboard-skip");
        let workspace = root.join("workspace");
        let config_home = root.join("openyak-home");
        let bin_dir = root.join("bin");
        fs::create_dir_all(&workspace).expect("workspace should exist");
        fs::create_dir_all(&config_home).expect("config home should exist");
        fs::create_dir_all(&bin_dir).expect("bin dir should exist");
        write_fake_command(&bin_dir, "gh");
        let _openyak_home =
            EnvVarGuard::set("OPENYAK_CONFIG_HOME", Some(&config_home.to_string_lossy()));
        let _path = EnvVarGuard::set(
            "PATH",
            Some(
                &std::env::join_paths([bin_dir.as_path()])
                    .expect("path should join")
                    .to_string_lossy(),
            ),
        );
        let _api_key = EnvVarGuard::set("ANTHROPIC_API_KEY", Some("test-key"));
        let _auth_token = EnvVarGuard::set("ANTHROPIC_AUTH_TOKEN", None);

        let mut output = Vec::new();
        let mut prompter = ScriptedPrompter {
            repo_init: PromptChoice::Skip,
            model: ModelChoice::Skip,
            login: PromptChoice::Skip,
            doctor: PromptChoice::Run,
        };
        let mut actions = RecordingActions::default();

        let outcome =
            run_onboarding_session(&workspace, None, &mut prompter, &mut actions, &mut output)
                .expect("onboarding should succeed");

        assert_eq!(outcome.status, OnboardingStatus::Completed);
        assert!(actions.doctor_called);
        assert!(!actions.init_called);
        assert!(!workspace.join(".openyak.json").exists());

        fs::remove_dir_all(root).expect("cleanup temp dir");
    }

    #[test]
    fn onboarding_can_init_and_persist_model_before_doctor() {
        let _lock = env_lock();
        let root = temp_dir("openyak-onboard-init-model");
        let workspace = root.join("workspace");
        let config_home = root.join("openyak-home");
        let bin_dir = root.join("bin");
        fs::create_dir_all(&workspace).expect("workspace should exist");
        fs::create_dir_all(&config_home).expect("config home should exist");
        fs::create_dir_all(&bin_dir).expect("bin dir should exist");
        write_fake_command(&bin_dir, "gh");
        let _openyak_home =
            EnvVarGuard::set("OPENYAK_CONFIG_HOME", Some(&config_home.to_string_lossy()));
        let _path = EnvVarGuard::set(
            "PATH",
            Some(
                &std::env::join_paths([bin_dir.as_path()])
                    .expect("path should join")
                    .to_string_lossy(),
            ),
        );
        let _api_key = EnvVarGuard::set("ANTHROPIC_API_KEY", Some("test-key"));

        let mut output = Vec::new();
        let mut prompter = ScriptedPrompter {
            repo_init: PromptChoice::Run,
            model: ModelChoice::Persist("claude-sonnet-4-6".to_string()),
            login: PromptChoice::Skip,
            doctor: PromptChoice::Run,
        };
        let mut actions = RecordingActions::default();

        let outcome =
            run_onboarding_session(&workspace, None, &mut prompter, &mut actions, &mut output)
                .expect("onboarding should succeed");

        assert_eq!(outcome.status, OnboardingStatus::Completed);
        assert!(workspace.join(".openyak.json").is_file());
        assert!(workspace.join("OPENYAK.md").is_file());
        let settings =
            fs::read_to_string(config_home.join("settings.json")).expect("settings should read");
        assert!(settings.contains("\"model\": \"claude-sonnet-4-6\""));
        let output = String::from_utf8(output).expect("stdout should be utf8");
        assert!(output.contains("openyak Doctor"));

        fs::remove_dir_all(root).expect("cleanup temp dir");
    }

    #[test]
    fn onboarding_can_stop_before_doctor_after_earlier_writes() {
        let _lock = env_lock();
        let root = temp_dir("openyak-onboard-cancel");
        let workspace = root.join("workspace");
        let config_home = root.join("openyak-home");
        fs::create_dir_all(&workspace).expect("workspace should exist");
        fs::create_dir_all(&config_home).expect("config home should exist");
        let _openyak_home =
            EnvVarGuard::set("OPENYAK_CONFIG_HOME", Some(&config_home.to_string_lossy()));
        let _api_key = EnvVarGuard::set("ANTHROPIC_API_KEY", Some("test-key"));

        let mut output = Vec::new();
        let mut prompter = ScriptedPrompter {
            repo_init: PromptChoice::Skip,
            model: ModelChoice::Persist("gpt-5.3".to_string()),
            login: PromptChoice::Skip,
            doctor: PromptChoice::Cancel,
        };
        let mut actions = RecordingActions::default();

        let outcome =
            run_onboarding_session(&workspace, None, &mut prompter, &mut actions, &mut output)
                .expect("onboarding should succeed");
        assert_eq!(outcome.status, OnboardingStatus::Cancelled);
        assert!(!actions.doctor_called);
        let settings =
            fs::read_to_string(config_home.join("settings.json")).expect("settings should read");
        assert!(settings.contains("\"model\": \"gpt-5.3\""));

        fs::remove_dir_all(root).expect("cleanup temp dir");
    }

    #[test]
    fn rerun_assessment_detects_existing_state_and_provider_guidance() {
        let _lock = env_lock();
        let root = temp_dir("openyak-onboard-rerun");
        let workspace = root.join("workspace");
        let config_home = root.join("openyak-home");
        fs::create_dir_all(&workspace).expect("workspace should exist");
        fs::create_dir_all(&config_home).expect("config home should exist");
        let _openyak_home =
            EnvVarGuard::set("OPENYAK_CONFIG_HOME", Some(&config_home.to_string_lossy()));
        initialize_repo(&workspace).expect("repo init should succeed");
        persist_default_model(&config_home, "gpt-5.3").expect("model write should succeed");
        let _openai_key = EnvVarGuard::set("OPENAI_API_KEY", Some("openai-key"));
        let _anthropic_key = EnvVarGuard::set("ANTHROPIC_API_KEY", None);

        let assessment = collect_assessment(&workspace, None);

        assert!(assessment.repo_init.is_ready());
        assert_eq!(assessment.user_default_model.as_deref(), Some("gpt-5.3"));
        assert!(assessment
            .auth
            .summary
            .contains("Environment-backed auth is ready via OPENAI_API_KEY"));

        fs::remove_dir_all(root).expect("cleanup temp dir");
    }

    #[test]
    fn oauth_guidance_only_enables_login_when_oauth_config_is_complete() {
        let _lock = env_lock();
        let root = temp_dir("openyak-onboard-oauth-guidance");
        let workspace = root.join("workspace");
        let config_home = root.join("openyak-home");
        fs::create_dir_all(&workspace).expect("workspace should exist");
        fs::create_dir_all(&config_home).expect("config home should exist");
        let _openyak_home =
            EnvVarGuard::set("OPENYAK_CONFIG_HOME", Some(&config_home.to_string_lossy()));
        let _api_key = EnvVarGuard::set("ANTHROPIC_API_KEY", None);
        let _auth_token = EnvVarGuard::set("ANTHROPIC_AUTH_TOKEN", None);

        fs::write(
            config_home.join("settings.json"),
            "{\n  \"oauth\": {\n    \"callbackPort\": 4557\n  }\n}\n",
        )
        .expect("partial oauth settings should write");
        let incomplete = collect_assessment(&workspace, None);
        assert!(!incomplete.auth.login_available);
        assert!(incomplete
            .auth
            .summary
            .contains("OAuth config is incomplete"));

        fs::write(
            config_home.join("settings.json"),
            "{\n  \"oauth\": {\n    \"clientId\": \"runtime-client\",\n    \"authorizeUrl\": \"https://oauth.example.test/authorize\",\n    \"tokenUrl\": \"https://oauth.example.test/token\"\n  }\n}\n",
        )
        .expect("complete oauth settings should write");
        let complete = collect_assessment(&workspace, None);
        assert!(complete.auth.login_available);
        assert!(complete.auth.summary.contains("OAuth is configured"));

        fs::remove_dir_all(root).expect("cleanup temp dir");
    }

    #[test]
    fn doctor_handoff_surfaces_blocking_oauth_config_errors() {
        let _lock = env_lock();
        let root = temp_dir("openyak-onboard-doctor-error");
        let workspace = root.join("workspace");
        let config_home = root.join("openyak-home");
        let bin_dir = root.join("bin");
        fs::create_dir_all(&workspace).expect("workspace should exist");
        fs::create_dir_all(&config_home).expect("config home should exist");
        fs::create_dir_all(&bin_dir).expect("bin dir should exist");
        write_fake_command(&bin_dir, "gh");
        fs::write(
            config_home.join("settings.json"),
            "{\n  \"oauth\": {\n    \"callbackPort\": 4557\n  }\n}\n",
        )
        .expect("partial oauth settings should write");
        let _openyak_home =
            EnvVarGuard::set("OPENYAK_CONFIG_HOME", Some(&config_home.to_string_lossy()));
        let _path = EnvVarGuard::set(
            "PATH",
            Some(
                &std::env::join_paths([bin_dir.as_path()])
                    .expect("path should join")
                    .to_string_lossy(),
            ),
        );

        let mut output = Vec::new();
        let mut prompter = ScriptedPrompter {
            repo_init: PromptChoice::Skip,
            model: ModelChoice::Skip,
            login: PromptChoice::Skip,
            doctor: PromptChoice::Run,
        };
        let mut actions = RecordingActions::default();

        let outcome =
            run_onboarding_session(&workspace, None, &mut prompter, &mut actions, &mut output)
                .expect("onboarding should complete the doctor handoff");

        assert_eq!(outcome.status, OnboardingStatus::Completed);
        assert!(outcome.doctor_has_errors);
        let output = String::from_utf8(output).expect("stdout should be utf8");
        assert!(output.contains("settings.oauth is incomplete"));
        assert!(output.contains("needs follow-up"));

        fs::remove_dir_all(root).expect("cleanup temp dir");
    }

    #[test]
    fn noninteractive_guidance_mentions_manual_fallbacks() {
        let _lock = env_lock();
        let root = temp_dir("openyak-onboard-noninteractive");
        let workspace = root.join("workspace");
        let config_home = root.join("openyak-home");
        fs::create_dir_all(&workspace).expect("workspace should exist");
        fs::create_dir_all(&config_home).expect("config home should exist");
        let _openyak_home =
            EnvVarGuard::set("OPENYAK_CONFIG_HOME", Some(&config_home.to_string_lossy()));
        let assessment = collect_assessment(&workspace, None);
        let mut output = Vec::new();

        render_noninteractive_guidance(&assessment, &mut output).expect("guidance should render");
        let output = String::from_utf8(output).expect("guidance should be utf8");
        assert!(output.contains("interactive terminal required"));
        assert!(output.contains("openyak init"));
        assert!(output.contains("openyak doctor"));

        fs::remove_dir_all(root).expect("cleanup temp dir");
    }
}

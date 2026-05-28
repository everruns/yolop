// CLI-owned capabilities for yolop.
//
// These are host/example behavior rather than runtime primitives: `bash` runs
// against the local workspace, and `/model` mutates this process's provider
// selection.

use crate::approval::ApprovalGate;
use crate::runtime::{ProviderChoice, SUPPORTED_PROVIDERS};
use crate::settings::SettingsStore;
use crate::tools::{BashTool, Workspace};
use async_trait::async_trait;
use chrono::Local;
use everruns_core::capabilities::{Capability, CapabilityStatus, SystemPromptContext};
use everruns_core::command::{
    CommandArg, CommandDescriptor, CommandExecutionContext, CommandResult, CommandSource,
    ExecuteCommandRequest,
};
use everruns_core::tools::Tool;
use everruns_runtime::RuntimeProviderStore;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, RwLock};

// ---------- environment context ----------

pub(crate) const ENVIRONMENT_CONTEXT_CAPABILITY_ID: &str = "code_environment_context";

pub(crate) struct CodingCliEnvironmentCapability {
    base: EnvironmentContextBase,
}

impl CodingCliEnvironmentCapability {
    pub(crate) fn new(workspace_root: PathBuf) -> Self {
        Self {
            base: EnvironmentContextBase::collect(workspace_root),
        }
    }
}

#[async_trait]
impl Capability for CodingCliEnvironmentCapability {
    fn id(&self) -> &str {
        ENVIRONMENT_CONTEXT_CAPABILITY_ID
    }
    fn name(&self) -> &str {
        "Coding CLI Environment Context"
    }
    fn description(&self) -> &str {
        "Adds current workspace, shell, date, timezone, and Git context to the prompt."
    }
    fn status(&self) -> CapabilityStatus {
        CapabilityStatus::Available
    }
    fn category(&self) -> Option<&str> {
        Some("Examples")
    }
    async fn system_prompt_contribution(&self, _ctx: &SystemPromptContext) -> Option<String> {
        Some(render_environment_context(&EnvironmentContext::from_base(
            &self.base,
        )))
    }
    fn system_prompt_preview(&self) -> Option<String> {
        Some(
            "\
<environment_context>
  <cwd>/path/to/workspace</cwd>
  <shell>zsh</shell>
  <current_date>YYYY-MM-DD</current_date>
  <timezone>Region/City</timezone>
  <git_repo>git remote or workspace root</git_repo>
  <git_user>Git user name</git_user>
  <git_email>Git user email</git_email>
  <git_current_branch>branch or short commit</git_current_branch>
</environment_context>"
                .to_string(),
        )
    }
}

#[derive(Debug)]
struct EnvironmentContextBase {
    cwd: String,
    shell: String,
    timezone: String,
    git_repo: Option<String>,
    git_user: Option<String>,
    git_email: Option<String>,
    workspace_root: PathBuf,
}

impl EnvironmentContextBase {
    fn collect(workspace_root: PathBuf) -> Self {
        Self {
            cwd: workspace_root.display().to_string(),
            shell: shell_name(),
            timezone: local_timezone(),
            git_repo: git_output(&workspace_root, &["config", "--get", "remote.origin.url"])
                .map(|remote| redact_git_remote_secret(&remote))
                .or_else(|| git_output(&workspace_root, &["rev-parse", "--show-toplevel"])),
            git_user: git_output(&workspace_root, &["config", "--get", "user.name"]),
            git_email: git_output(&workspace_root, &["config", "--get", "user.email"]),
            workspace_root,
        }
    }
}

#[derive(Debug)]
struct EnvironmentContext {
    cwd: String,
    shell: String,
    current_date: String,
    timezone: String,
    git_repo: Option<String>,
    git_user: Option<String>,
    git_email: Option<String>,
    git_current_branch: Option<String>,
}

impl EnvironmentContext {
    fn from_base(base: &EnvironmentContextBase) -> Self {
        Self {
            cwd: base.cwd.clone(),
            shell: base.shell.clone(),
            current_date: Local::now().format("%Y-%m-%d").to_string(),
            timezone: base.timezone.clone(),
            git_repo: base.git_repo.clone(),
            git_user: base.git_user.clone(),
            git_email: base.git_email.clone(),
            git_current_branch: git_current_branch(&base.workspace_root),
        }
    }
}

fn render_environment_context(context: &EnvironmentContext) -> String {
    let mut out = String::new();
    out.push_str("<environment_context>\n");
    push_xml_field(&mut out, "cwd", &context.cwd);
    push_xml_field(&mut out, "shell", &context.shell);
    push_xml_field(&mut out, "current_date", &context.current_date);
    push_xml_field(&mut out, "timezone", &context.timezone);
    if let Some(value) = &context.git_repo {
        push_xml_field(&mut out, "git_repo", value);
    }
    if let Some(value) = &context.git_user {
        push_xml_field(&mut out, "git_user", value);
    }
    if let Some(value) = &context.git_email {
        push_xml_field(&mut out, "git_email", value);
    }
    if let Some(value) = &context.git_current_branch {
        push_xml_field(&mut out, "git_current_branch", value);
    }
    out.push_str("</environment_context>");
    out
}

fn push_xml_field(out: &mut String, name: &str, value: &str) {
    out.push_str("  <");
    out.push_str(name);
    out.push('>');
    out.push_str(&xml_escape(value));
    out.push_str("</");
    out.push_str(name);
    out.push_str(">\n");
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn redact_git_remote_secret(remote: &str) -> String {
    // Only strip userinfo from http(s) remotes — scp-style (`git@host:path`)
    // and `ssh://user@host/path` rely on the user component for routing, not
    // credentialing. PATs and basic-auth passwords typically appear in
    // `https://token@host/...` or `https://user:pass@host/...`.
    let scheme_end = remote.find("://");
    let Some(scheme_end) = scheme_end else {
        return remote.to_string();
    };
    let scheme = &remote[..scheme_end];
    if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
        return remote.to_string();
    }
    let authority_start = scheme_end + 3;
    let authority = &remote[authority_start..];
    let authority_end_offset = authority.find(['/', '?', '#']).unwrap_or(authority.len());
    let authority_end = authority_start + authority_end_offset;
    let userinfo = &remote[authority_start..authority_end];
    if let Some(at_offset) = userinfo.find('@') {
        let host_port = &userinfo[at_offset + 1..];
        return format!("{}{}", &remote[..authority_start], host_port) + &remote[authority_end..];
    }
    remote.to_string()
}

fn shell_name() -> String {
    std::env::var("SHELL")
        .ok()
        .and_then(|shell| {
            Path::new(&shell)
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
        })
        .filter(|shell| !shell.is_empty())
        .unwrap_or_else(|| "sh".to_string())
}

fn local_timezone() -> String {
    if let Ok(tz) = std::env::var("TZ")
        && !tz.trim().is_empty()
    {
        return tz.trim().to_string();
    }
    if let Ok(target) = std::fs::read_link("/etc/localtime") {
        let target = target.to_string_lossy();
        if let Some((_, timezone)) = target.split_once("/zoneinfo/") {
            return timezone.to_string();
        }
    }
    "local".to_string()
}

fn git_current_branch(workspace_root: &Path) -> Option<String> {
    git_output(workspace_root, &["branch", "--show-current"])
        .filter(|branch| !branch.is_empty())
        .or_else(|| git_output(workspace_root, &["rev-parse", "--short", "HEAD"]))
}

fn git_output(workspace_root: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(workspace_root)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!text.is_empty()).then_some(text)
}

// ---------- bash ----------

pub(crate) struct CodingBashCapability {
    pub(crate) workspace: Workspace,
    pub(crate) gate: Arc<ApprovalGate>,
}

impl Capability for CodingBashCapability {
    fn id(&self) -> &str {
        "yolop_bash"
    }
    fn name(&self) -> &str {
        "Coding CLI Bash"
    }
    fn description(&self) -> &str {
        "Shell command execution rooted at the host workspace. Requires user approval."
    }
    fn status(&self) -> CapabilityStatus {
        CapabilityStatus::Available
    }
    fn category(&self) -> Option<&str> {
        Some("Examples")
    }
    fn system_prompt_addition(&self) -> Option<&str> {
        // Harness prompt already documents the `bash` tool. Returning None
        // keeps the capability's contribution out of the system prompt so we
        // don't repeat ourselves.
        None
    }
    fn tools(&self) -> Vec<Box<dyn Tool>> {
        vec![Box::new(BashTool::new(
            self.workspace.clone(),
            self.gate.clone(),
        ))]
    }
}

// ---------- /model ----------
//
// Demonstrates the `Capability::execute_command` hook: `/model` lives entirely
// outside the TUI's `handle_command` branches. The capability owns the runtime
// provider store and shares an Arc<RwLock<ProviderChoice>> with the
// UI-facing `ModelState` so the banner label stays in sync after a switch.

pub(crate) const MODEL_SWITCHER_CAPABILITY_ID: &str = "yolop_model_switcher";

pub(crate) struct ModelSwitcherCapability {
    pub(crate) provider: Arc<RwLock<ProviderChoice>>,
    pub(crate) provider_store: Arc<dyn RuntimeProviderStore>,
    pub(crate) settings: Arc<SettingsStore>,
}

#[async_trait]
impl Capability for ModelSwitcherCapability {
    fn id(&self) -> &str {
        MODEL_SWITCHER_CAPABILITY_ID
    }
    fn name(&self) -> &str {
        "Coding CLI Model Switcher"
    }
    fn description(&self) -> &str {
        "Show or change the active provider/model via /model."
    }
    fn status(&self) -> CapabilityStatus {
        CapabilityStatus::Available
    }
    fn category(&self) -> Option<&str> {
        Some("Examples")
    }
    fn system_prompt_addition(&self) -> Option<&str> {
        None
    }
    fn commands(&self) -> Vec<CommandDescriptor> {
        vec![CommandDescriptor {
            name: "model".to_string(),
            description: "Show or change the active provider/model.".to_string(),
            source: CommandSource::System,
            args: vec![CommandArg {
                name: "spec".to_string(),
                description: "<provider>/<id> — omit to print the current model.".to_string(),
                required: false,
                // Declarative completions so renderers can populate the
                // autocomplete dropdown directly from the descriptor — no
                // per-keystroke callback into the capability.
                suggestions: ProviderChoice::model_suggestions()
                    .iter()
                    .map(|s| (*s).to_string())
                    .collect(),
            }],
        }]
    }

    async fn execute_command(
        &self,
        request: &ExecuteCommandRequest,
        _ctx: &CommandExecutionContext,
    ) -> everruns_core::Result<CommandResult> {
        if request.name != "model" {
            return Err(everruns_core::AgentLoopError::config(format!(
                "{} cannot execute /{}",
                self.id(),
                request.name
            )));
        }
        let raw = request.arguments.as_deref().unwrap_or("").trim();
        if raw.is_empty() {
            let label = self
                .provider
                .read()
                .expect("provider lock poisoned")
                .label();
            return Ok(CommandResult {
                success: true,
                message: format!(
                    "model: {label}; suggestions: {}",
                    ProviderChoice::model_suggestions().join(", ")
                ),
                error_code: None,
                error_fields: None,
            });
        }

        let current = self
            .provider
            .read()
            .expect("provider lock poisoned")
            .clone();
        let next = match current.resolve_model_spec(raw) {
            Ok(n) => n,
            Err(err) => {
                return Ok(failed_result(format!("model change failed: {err}")));
            }
        };
        let mw = match next.model_with_provider(&self.settings.snapshot()) {
            Ok(m) => m,
            Err(err) => {
                return Ok(failed_result(format!("model change failed: {err}")));
            }
        };
        if let Err(err) = self.provider_store.set_default_model(mw).await {
            return Ok(failed_result(format!("model change failed: {err}")));
        }
        let label = next.label();
        *self.provider.write().expect("provider lock poisoned") = next;
        Ok(CommandResult {
            success: true,
            message: format!("model changed: {label}"),
            error_code: None,
            error_fields: None,
        })
    }
}

fn failed_result(message: String) -> CommandResult {
    CommandResult {
        success: false,
        message,
        error_code: None,
        error_fields: None,
    }
}

// ---------- /provider ----------
//
// Companion to `/model`: picks the *provider* (OpenAI, Anthropic, Google,
// OpenRouter, Ollama, llmsim) using that provider's default model, and
// persists the choice to `<config_dir>/yolop/settings.toml` so the next
// run starts on the same provider. The model is still mutated via `/model`
// afterward — both commands share the same provider Arc.

pub(crate) const PROVIDER_SWITCHER_CAPABILITY_ID: &str = "yolop_provider_switcher";

pub(crate) struct ProviderSwitcherCapability {
    pub(crate) provider: Arc<RwLock<ProviderChoice>>,
    pub(crate) provider_store: Arc<dyn RuntimeProviderStore>,
    pub(crate) settings: Arc<SettingsStore>,
}

#[async_trait]
impl Capability for ProviderSwitcherCapability {
    fn id(&self) -> &str {
        PROVIDER_SWITCHER_CAPABILITY_ID
    }
    fn name(&self) -> &str {
        "Coding CLI Provider Switcher"
    }
    fn description(&self) -> &str {
        "Show or change the active LLM provider, persisted across runs."
    }
    fn status(&self) -> CapabilityStatus {
        CapabilityStatus::Available
    }
    fn category(&self) -> Option<&str> {
        Some("Examples")
    }
    fn system_prompt_addition(&self) -> Option<&str> {
        None
    }
    fn commands(&self) -> Vec<CommandDescriptor> {
        vec![CommandDescriptor {
            name: "provider".to_string(),
            description: "Show or change the active provider (saved to yolop settings)."
                .to_string(),
            source: CommandSource::System,
            args: vec![CommandArg {
                name: "name".to_string(),
                description: format!(
                    "{} — omit to print the current provider.",
                    SUPPORTED_PROVIDERS.join(" | ")
                ),
                required: false,
                suggestions: SUPPORTED_PROVIDERS
                    .iter()
                    .map(|s| (*s).to_string())
                    .collect(),
            }],
        }]
    }

    async fn execute_command(
        &self,
        request: &ExecuteCommandRequest,
        _ctx: &CommandExecutionContext,
    ) -> everruns_core::Result<CommandResult> {
        if request.name != "provider" {
            return Err(everruns_core::AgentLoopError::config(format!(
                "{} cannot execute /{}",
                self.id(),
                request.name
            )));
        }
        let raw = request.arguments.as_deref().unwrap_or("").trim();
        if raw.is_empty() {
            let current = self
                .provider
                .read()
                .expect("provider lock poisoned")
                .clone();
            let saved = self
                .settings
                .snapshot()
                .provider
                .unwrap_or_else(|| "<unset>".to_string());
            return Ok(CommandResult {
                success: true,
                message: format!(
                    "provider: {} (model: {}); saved: {saved}; options: {}",
                    current.provider_name(),
                    current.label(),
                    SUPPORTED_PROVIDERS.join(", "),
                ),
                error_code: None,
                error_fields: None,
            });
        }

        if raw.split_whitespace().count() > 1 {
            return Ok(failed_result(
                "usage: /provider <name> — pick a single provider, then use /model to tune the model"
                    .to_string(),
            ));
        }

        let next = match ProviderChoice::default_for_provider_name(raw) {
            Ok(n) => n,
            Err(err) => return Ok(failed_result(format!("provider change failed: {err}"))),
        };
        let mw = match next.model_with_provider(&self.settings.snapshot()) {
            Ok(m) => m,
            Err(err) => return Ok(failed_result(format!("provider change failed: {err}"))),
        };
        if let Err(err) = self.provider_store.set_default_model(mw).await {
            return Ok(failed_result(format!("provider change failed: {err}")));
        }
        let provider_name = next.provider_name().to_string();
        let label = next.label();
        *self.provider.write().expect("provider lock poisoned") = next;
        let persist_note = match self.settings.set_provider(Some(provider_name.clone())) {
            Ok(()) => format!("saved to {}", self.settings.path().display()),
            Err(err) => format!("warning: settings not saved: {err}"),
        };
        Ok(CommandResult {
            success: true,
            message: format!("provider changed: {label} ({persist_note})"),
            error_code: None,
            error_fields: None,
        })
    }
}

// ---------- /token ----------
//
// Stores per-provider API tokens in the same settings file as `/provider`.
// Env vars still win at runtime (see `runtime::resolve_token`) so a
// per-run override is always possible. Slash commands don't echo their
// arguments to the transcript or session log, so `/token openai sk-...`
// is the safest entry point we have — but the resulting settings file
// sits in plain text on disk (0o600 on Unix).

pub(crate) const TOKEN_CAPABILITY_ID: &str = "yolop_token_manager";

/// Providers that meaningfully consume an API token. `llmsim` is excluded
/// (no key needed) and `ollama` is included for completeness even though
/// most local setups don't authenticate.
const TOKEN_PROVIDERS: &[&str] = &["openai", "anthropic", "google", "openrouter", "ollama"];

pub(crate) struct TokenCapability {
    pub(crate) settings: Arc<SettingsStore>,
}

#[async_trait]
impl Capability for TokenCapability {
    fn id(&self) -> &str {
        TOKEN_CAPABILITY_ID
    }
    fn name(&self) -> &str {
        "Coding CLI Token Manager"
    }
    fn description(&self) -> &str {
        "Store provider API tokens in yolop settings so env vars are not required."
    }
    fn status(&self) -> CapabilityStatus {
        CapabilityStatus::Available
    }
    fn category(&self) -> Option<&str> {
        Some("Examples")
    }
    fn system_prompt_addition(&self) -> Option<&str> {
        None
    }
    fn commands(&self) -> Vec<CommandDescriptor> {
        vec![CommandDescriptor {
            name: "token".to_string(),
            description: "Show or store API tokens. Env vars still override saved tokens."
                .to_string(),
            source: CommandSource::System,
            args: vec![CommandArg {
                name: "provider [value | clear]".to_string(),
                description:
                    "<provider> <value> to store; <provider> clear to remove; omit to list status."
                        .to_string(),
                required: false,
                suggestions: TOKEN_PROVIDERS.iter().map(|s| (*s).to_string()).collect(),
            }],
        }]
    }

    async fn execute_command(
        &self,
        request: &ExecuteCommandRequest,
        _ctx: &CommandExecutionContext,
    ) -> everruns_core::Result<CommandResult> {
        if request.name != "token" {
            return Err(everruns_core::AgentLoopError::config(format!(
                "{} cannot execute /{}",
                self.id(),
                request.name
            )));
        }
        let raw = request.arguments.as_deref().unwrap_or("").trim();
        if raw.is_empty() {
            let snapshot = self.settings.snapshot();
            let status: Vec<String> = TOKEN_PROVIDERS
                .iter()
                .map(|p| {
                    let marker = if snapshot.has_token(p) { "stored" } else { "-" };
                    format!("{p}: {marker}")
                })
                .collect();
            return Ok(CommandResult {
                success: true,
                message: format!(
                    "tokens ({}): {}",
                    self.settings.path().display(),
                    status.join(", ")
                ),
                error_code: None,
                error_fields: None,
            });
        }

        let mut parts = raw.splitn(2, char::is_whitespace);
        let provider = parts.next().unwrap_or_default().to_ascii_lowercase();
        let rest = parts.next().unwrap_or_default().trim();

        if !TOKEN_PROVIDERS.contains(&provider.as_str()) {
            return Ok(failed_result(format!(
                "unknown provider `{provider}`; expected one of {}",
                TOKEN_PROVIDERS.join(", ")
            )));
        }
        if rest.is_empty() {
            return Ok(failed_result(
                "usage: /token <provider> <value|clear>".to_string(),
            ));
        }

        if rest.eq_ignore_ascii_case("clear") {
            return Ok(match self.settings.clear_token(&provider) {
                Ok(true) => CommandResult {
                    success: true,
                    message: format!("token cleared for {provider}"),
                    error_code: None,
                    error_fields: None,
                },
                Ok(false) => CommandResult {
                    success: true,
                    message: format!("no token was stored for {provider}"),
                    error_code: None,
                    error_fields: None,
                },
                Err(err) => failed_result(format!("token clear failed: {err}")),
            });
        }

        match self.settings.set_token(provider.clone(), rest.to_string()) {
            Ok(()) => Ok(CommandResult {
                success: true,
                message: format!(
                    "token stored for {provider} (in {})",
                    self.settings.path().display()
                ),
                error_code: None,
                error_fields: None,
            }),
            Err(err) => Ok(failed_result(format!("token save failed: {err}"))),
        }
    }
}

// ---------- /onboard ----------
//
// First-run setup helper. The capability itself only registers the
// `/onboard` slash command and reports the current setup state — the
// actual interactive wizard lives in the TUI because capabilities are
// stateless command handlers and can't pause for user input.
//
// The TUI matches `descriptor.name == "onboard"` after dispatching the
// command and enters wizard mode, which then drives `/provider`,
// `/token`, and `/model` against the same capabilities.

pub(crate) const ONBOARDING_CAPABILITY_ID: &str = "yolop_onboarding";

pub(crate) struct OnboardingCapability {
    pub(crate) settings: Arc<SettingsStore>,
}

impl OnboardingCapability {
    /// True when no provider preference is saved and no API token is set —
    /// either via env var or in the settings file. Used by the TUI at
    /// startup to auto-open the wizard on a fresh install.
    pub(crate) fn needs_onboarding(settings: &crate::settings::Settings) -> bool {
        if settings.provider.is_some() {
            return false;
        }
        if env_credential_present() {
            return false;
        }
        settings.tokens.is_empty()
    }
}

fn env_credential_present() -> bool {
    const VARS: &[&str] = &[
        "OPENAI_API_KEY",
        "ANTHROPIC_API_KEY",
        "OPENROUTER_API_KEY",
        "GEMINI_API_KEY",
        "GOOGLE_API_KEY",
        "OLLAMA_BASE_URL",
        "OLLAMA_API_KEY",
    ];
    VARS.iter()
        .any(|var| std::env::var(var).map(|v| !v.is_empty()).unwrap_or(false))
}

#[async_trait]
impl Capability for OnboardingCapability {
    fn id(&self) -> &str {
        ONBOARDING_CAPABILITY_ID
    }
    fn name(&self) -> &str {
        "Coding CLI Onboarding"
    }
    fn description(&self) -> &str {
        "Guided first-run setup for provider, token, and default model."
    }
    fn status(&self) -> CapabilityStatus {
        CapabilityStatus::Available
    }
    fn category(&self) -> Option<&str> {
        Some("Examples")
    }
    fn system_prompt_addition(&self) -> Option<&str> {
        None
    }
    fn commands(&self) -> Vec<CommandDescriptor> {
        vec![CommandDescriptor {
            name: "onboard".to_string(),
            description: "Walk through provider/token/model setup.".to_string(),
            source: CommandSource::System,
            args: vec![],
        }]
    }

    async fn execute_command(
        &self,
        request: &ExecuteCommandRequest,
        _ctx: &CommandExecutionContext,
    ) -> everruns_core::Result<CommandResult> {
        if request.name != "onboard" {
            return Err(everruns_core::AgentLoopError::config(format!(
                "{} cannot execute /{}",
                self.id(),
                request.name
            )));
        }
        // The TUI starts the actual wizard after dispatch (see
        // `App::invoke_capability_command`). Reporting current state here
        // makes the command useful in `--print` mode too, where the wizard
        // can't run.
        let snapshot = self.settings.snapshot();
        let provider = snapshot
            .provider
            .clone()
            .unwrap_or_else(|| "<unset>".to_string());
        let stored: Vec<&str> = snapshot.tokens.keys().map(String::as_str).collect();
        let stored_label = if stored.is_empty() {
            "none".to_string()
        } else {
            stored.join(", ")
        };
        Ok(CommandResult {
            success: true,
            message: format!(
                "onboarding: provider={provider}, stored tokens={stored_label}, env keys present={}",
                env_credential_present()
            ),
            error_code: None,
            error_fields: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn needs_onboarding_true_for_empty_settings() {
        // Serialize against every other env-mutating test in this
        // binary; cf. `crate::test_env`.
        let _guard = crate::test_env::lock();
        unsafe {
            std::env::remove_var("OPENAI_API_KEY");
            std::env::remove_var("ANTHROPIC_API_KEY");
            std::env::remove_var("OPENROUTER_API_KEY");
            std::env::remove_var("GEMINI_API_KEY");
            std::env::remove_var("GOOGLE_API_KEY");
            std::env::remove_var("OLLAMA_BASE_URL");
            std::env::remove_var("OLLAMA_API_KEY");
        }
        let settings = crate::settings::Settings::default();
        assert!(OnboardingCapability::needs_onboarding(&settings));
    }

    #[test]
    fn needs_onboarding_false_when_provider_is_saved() {
        let settings = crate::settings::Settings {
            provider: Some("anthropic".to_string()),
            ..Default::default()
        };
        assert!(!OnboardingCapability::needs_onboarding(&settings));
    }

    #[test]
    fn needs_onboarding_false_when_token_is_saved() {
        let mut tokens = std::collections::BTreeMap::new();
        tokens.insert("openai".to_string(), "sk-test".to_string());
        let settings = crate::settings::Settings {
            provider: None,
            tokens,
        };
        assert!(!OnboardingCapability::needs_onboarding(&settings));
    }

    #[test]
    fn environment_context_renders_requested_fields() {
        let rendered = render_environment_context(&EnvironmentContext {
            cwd: "/repo".to_string(),
            shell: "zsh".to_string(),
            current_date: "2026-05-20".to_string(),
            timezone: "America/Chicago".to_string(),
            git_repo: Some("https://github.com/everruns/everruns.git".to_string()),
            git_user: Some("Chal & Yi".to_string()),
            git_email: Some("chalyi@example.com".to_string()),
            git_current_branch: Some("feature<context>".to_string()),
        });

        assert!(rendered.starts_with("<environment_context>\n"));
        assert!(rendered.contains("  <cwd>/repo</cwd>\n"));
        assert!(rendered.contains("  <shell>zsh</shell>\n"));
        assert!(rendered.contains("  <current_date>2026-05-20</current_date>\n"));
        assert!(rendered.contains("  <timezone>America/Chicago</timezone>\n"));
        assert!(
            rendered.contains("  <git_repo>https://github.com/everruns/everruns.git</git_repo>\n")
        );
        assert!(rendered.contains("  <git_user>Chal &amp; Yi</git_user>\n"));
        assert!(rendered.contains("  <git_email>chalyi@example.com</git_email>\n"));
        assert!(
            rendered
                .contains("  <git_current_branch>feature&lt;context&gt;</git_current_branch>\n")
        );
        assert!(rendered.ends_with("</environment_context>"));
    }

    #[test]
    fn redact_git_remote_secret_removes_http_userinfo() {
        let remote = "https://user:ghp_SUPERSECRET@github.com/org/private.git";
        assert_eq!(
            redact_git_remote_secret(remote),
            "https://github.com/org/private.git"
        );
    }

    #[test]
    fn redact_git_remote_secret_leaves_non_url_remote_unchanged() {
        let remote = "git@github.com:everruns/everruns.git";
        assert_eq!(redact_git_remote_secret(remote), remote);
    }

    #[test]
    fn redact_git_remote_secret_preserves_ssh_url_username() {
        let remote = "ssh://git@github.com/everruns/everruns.git";
        assert_eq!(redact_git_remote_secret(remote), remote);
    }

    #[test]
    fn redact_git_remote_secret_removes_http_token_only_userinfo() {
        let remote = "https://ghp_TOKEN@github.com/org/private.git";
        assert_eq!(
            redact_git_remote_secret(remote),
            "https://github.com/org/private.git"
        );
    }

    #[test]
    fn redact_git_remote_secret_leaves_https_without_userinfo_unchanged() {
        let remote = "https://github.com/everruns/everruns.git";
        assert_eq!(redact_git_remote_secret(remote), remote);
    }
}

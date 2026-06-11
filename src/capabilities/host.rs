// Host/example capabilities for yolop: local environment context, bash, and
// TUI-facing slash commands that mutate this process's provider selection.

use crate::config_service::ConfigService;
use crate::runtime::{ProviderChoice, SUPPORTED_PROVIDERS};
use crate::settings::{ApprovalMode, SettingsStore};
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
pub(crate) const ATTRIBUTION_CAPABILITY_ID: &str = "yolop_attribution";
pub(crate) const YOLOP_ATTRIBUTION_TRAILER: &str = "Co-Authored-By: yolop <yolop@everruns.com>";
pub(crate) const YOLOP_PR_ATTRIBUTION: &str = "Generated with yolop";

fn yolop_attribution_prompt() -> String {
    format!(
        "\
## Attribution

When you create or amend commits for changes you made, keep the user's
git author/committer identity intact and append this git trailer once:
{YOLOP_ATTRIBUTION_TRAILER}

When creating or editing pull request descriptions with `gh`, add this
footer once:
{YOLOP_PR_ATTRIBUTION}

Do not add duplicate attribution or session links."
    )
}

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

pub(crate) struct AttributionCapability {
    /// Reads through the shared config service rather than the concrete store —
    /// it only needs to know whether attribution is enabled.
    pub(crate) config: Arc<dyn ConfigService>,
}

#[async_trait]
impl Capability for AttributionCapability {
    fn id(&self) -> &str {
        ATTRIBUTION_CAPABILITY_ID
    }
    fn name(&self) -> &str {
        "Yolop Attribution"
    }
    fn description(&self) -> &str {
        "Adds optional commit and pull request attribution guidance."
    }
    fn status(&self) -> CapabilityStatus {
        CapabilityStatus::Available
    }
    fn category(&self) -> Option<&str> {
        Some("Examples")
    }
    async fn system_prompt_contribution(&self, _ctx: &SystemPromptContext) -> Option<String> {
        self.config
            .attribution_enabled()
            .then(yolop_attribution_prompt)
    }
    fn system_prompt_preview(&self) -> Option<String> {
        Some(yolop_attribution_prompt())
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
}

impl Capability for CodingBashCapability {
    fn id(&self) -> &str {
        "yolop_bash"
    }
    fn name(&self) -> &str {
        "Coding CLI Bash"
    }
    fn description(&self) -> &str {
        "Shell command execution rooted at the host workspace."
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
        vec![Box::new(BashTool::new(self.workspace.clone()))]
    }
}

// ---------- /setup ----------
//
// One user-facing command owns provider, token, and model setup. The TUI starts
// an interactive wizard for `/setup`; the internal setup subcommands below let
// that wizard mutate runtime state without exposing `/provider`, `/token`, and
// `/model` as separate commands.

pub(crate) const SETUP_CAPABILITY_ID: &str = "yolop_setup";

/// Providers that meaningfully consume an API token. `llmsim` is excluded
/// (no key needed); `ollama` and `custom` are included for completeness even
/// though most local setups don't authenticate.
const TOKEN_PROVIDERS: &[&str] = &[
    "openai",
    "anthropic",
    "google",
    "openrouter",
    "ollama",
    "custom",
];

/// Providers whose endpoint base URL is user configuration stored in
/// settings (vs. a compiled-in default with env override).
const BASE_URL_PROVIDERS: &[&str] = &["custom"];

pub(crate) struct SetupCapability {
    pub(crate) provider: Arc<RwLock<ProviderChoice>>,
    pub(crate) provider_store: Arc<dyn RuntimeProviderStore>,
    pub(crate) settings: Arc<SettingsStore>,
}

#[async_trait]
impl Capability for SetupCapability {
    fn id(&self) -> &str {
        SETUP_CAPABILITY_ID
    }
    fn name(&self) -> &str {
        "Coding CLI Setup"
    }
    fn description(&self) -> &str {
        "Configure provider, API key, and model."
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
            name: "setup".to_string(),
            description: "Configure provider, API key, and model.".to_string(),
            source: CommandSource::System,
            args: vec![setup_command_arg()],
        }]
    }

    async fn execute_command(
        &self,
        request: &ExecuteCommandRequest,
        _ctx: &CommandExecutionContext,
    ) -> everruns_core::Result<CommandResult> {
        if request.name != "setup" {
            return Err(everruns_core::AgentLoopError::config(format!(
                "{} cannot execute /{}",
                self.id(),
                request.name
            )));
        }
        let raw = request.arguments.as_deref().unwrap_or("").trim();
        if raw.is_empty() || raw == "status" {
            return Ok(self.status_result());
        }

        let mut parts = raw.splitn(2, char::is_whitespace);
        let action = parts.next().unwrap_or_default();
        let rest = parts.next().unwrap_or_default().trim();
        match action {
            "provider" => self.change_provider(rest).await,
            "model" => self.change_model(rest).await,
            "effort" => self.change_effort(rest).await,
            "token" => self.change_token(rest),
            "url" => self.change_base_url(rest),
            "attribution" => self.change_attribution(rest),
            "approval" => self.change_approval(rest),
            _ => Ok(failed_result(
                "usage: /setup — run guided setup; internal forms: status, provider <name> [model], token <provider> <value|clear>, url <provider> <base-url|clear>, model <id> [reasoning-effort], effort <reasoning-effort>, attribution <on|off>, approval <protective|normal|off>".to_string(),
            )),
        }
    }
}

fn setup_command_arg() -> CommandArg {
    let mut suggestions = vec![
        "status".to_string(),
        "model".to_string(),
        "attribution on".to_string(),
        "attribution off".to_string(),
        "approval protective".to_string(),
        "approval normal".to_string(),
        "approval off".to_string(),
    ];
    suggestions.extend(TOKEN_PROVIDERS.iter().flat_map(|provider| {
        [
            format!("token {provider} "),
            format!("token {provider} clear"),
        ]
    }));
    suggestions.extend(
        BASE_URL_PROVIDERS
            .iter()
            .flat_map(|provider| [format!("url {provider} "), format!("url {provider} clear")]),
    );
    suggestions.extend(
        SUPPORTED_PROVIDERS
            .iter()
            .map(|provider| format!("provider {provider}")),
    );
    suggestions.extend(
        SUPPORTED_PROVIDERS
            .iter()
            .flat_map(|provider| ProviderChoice::model_suggestions_for_provider(provider))
            .copied()
            .map(|model| format!("model {model}")),
    );
    suggestions.extend(
        ProviderChoice::reasoning_effort_suggestions()
            .iter()
            .copied()
            .map(|effort| format!("effort {effort}")),
    );

    CommandArg {
        name: "action".to_string(),
        description: "status | provider <name> [model] | token <provider> <value|clear> | url <provider> <base-url|clear> | model <id> | effort <level> | attribution <on|off> | approval <protective|normal|off>".to_string(),
        required: false,
        suggestions,
    }
}

impl SetupCapability {
    fn status_result(&self) -> CommandResult {
        let current = self
            .provider
            .read()
            .expect("provider lock poisoned")
            .clone();
        let snapshot = self.settings.snapshot();
        let saved = snapshot
            .provider
            .clone()
            .unwrap_or_else(|| "<unset>".to_string());
        let stored: Vec<&str> = snapshot.tokens.keys().map(String::as_str).collect();
        let stored_label = if stored.is_empty() {
            "none".to_string()
        } else {
            stored.join(", ")
        };
        CommandResult {
            success: true,
            message: format!(
                "setup: provider={} model={} saved={saved} attribution={} approval={} stored tokens={stored_label} env keys present={}",
                current.provider_name(),
                current.label(),
                on_off(snapshot.attribution_enabled()),
                snapshot.approval_mode(),
                env_credential_present()
            ),
            error_code: None,
            error_fields: None,
        }
    }

    /// `provider <name> [model [effort]]`. The optional model spec switches
    /// provider and model atomically — the wizard needs this for the custom
    /// provider, whose `/setup model` form would otherwise have no provider
    /// context to resolve against on first-time setup.
    async fn change_provider(&self, raw: &str) -> everruns_core::Result<CommandResult> {
        let mut parts = raw.splitn(2, char::is_whitespace);
        let name = parts.next().unwrap_or_default();
        let model_spec = parts.next().unwrap_or_default().trim();
        if name.is_empty() {
            return Ok(failed_result(format!(
                "setup provider failed: choose one of {}",
                SUPPORTED_PROVIDERS.join(", ")
            )));
        }

        let next = match ProviderChoice::default_for_provider_name(name) {
            Ok(n) => n.with_saved_model(&self.settings.snapshot()),
            Err(err) => return Ok(failed_result(format!("setup provider failed: {err}"))),
        };
        let next = if model_spec.is_empty() {
            next
        } else {
            match next.resolve_model_spec(model_spec) {
                Ok(n) => n,
                Err(err) => return Ok(failed_result(format!("setup provider failed: {err}"))),
            }
        };
        if next.model_id().trim().is_empty() {
            return Ok(failed_result(format!(
                "setup provider failed: no model configured for {name}; pick one with /setup"
            )));
        }
        let mw = match next.model_with_provider(&self.settings.snapshot()) {
            Ok(m) => m,
            Err(err) => return Ok(failed_result(format!("setup provider failed: {err}"))),
        };
        if let Err(err) = self.provider_store.set_default_model(mw).await {
            return Ok(failed_result(format!("setup provider failed: {err}")));
        }
        let provider_name = next.provider_name().to_string();
        let label = next.label();
        // Persist the model only when explicitly given: a plain provider
        // switch must not clobber the saved model with the default.
        let model_persist = if model_spec.is_empty() {
            Ok(())
        } else {
            self.settings
                .set_model(provider_name.clone(), next.model_spec())
        };
        *self.provider.write().expect("provider lock poisoned") = next;
        let persist_note = match model_persist
            .and_then(|()| self.settings.set_provider(Some(provider_name.clone())))
        {
            Ok(()) => format!("saved to {}", self.settings.path().display()),
            Err(err) => format!("warning: settings not saved: {err}"),
        };
        Ok(CommandResult {
            success: true,
            message: format!("setup provider changed: {label} ({persist_note})"),
            error_code: None,
            error_fields: None,
        })
    }

    async fn change_model(&self, raw: &str) -> everruns_core::Result<CommandResult> {
        if raw.is_empty() {
            let current = self
                .provider
                .read()
                .expect("provider lock poisoned")
                .clone();
            let label = current.label();
            return Ok(CommandResult {
                success: true,
                message: format!(
                    "setup model: {label}; {}",
                    self.model_suggestions_message(&current).await
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
                return Ok(failed_result(format!("setup model failed: {err}")));
            }
        };
        let mw = match next.model_with_provider(&self.settings.snapshot()) {
            Ok(m) => m,
            Err(err) => {
                return Ok(failed_result(format!("setup model failed: {err}")));
            }
        };
        if let Err(err) = self.provider_store.set_default_model(mw).await {
            return Ok(failed_result(format!("setup model failed: {err}")));
        }
        let label = next.label();
        let persist_note = self.persist_model_choice(&next);
        *self.provider.write().expect("provider lock poisoned") = next;
        Ok(CommandResult {
            success: true,
            message: format!("setup model changed: {label}{persist_note}"),
            error_code: None,
            error_fields: None,
        })
    }

    /// Persist a model switch: the provider preference and the
    /// provider-relative `model [effort]` spec, so both survive a restart
    /// (the model picker promises "future sessions"). Best-effort — a failed
    /// save is reported in the message but never blocks the in-session
    /// switch.
    fn persist_model_choice(&self, next: &ProviderChoice) -> String {
        let provider_name = next.provider_name().to_string();
        let result = self
            .settings
            .set_provider(Some(provider_name.clone()))
            .and_then(|()| self.settings.set_model(provider_name, next.model_spec()));
        match result {
            Ok(()) => String::new(),
            Err(err) => format!(" (warning: settings not saved: {err})"),
        }
    }

    /// Live model suggestions for the current provider, queried from its
    /// models API; falls back to the curated static list when the provider
    /// does not support listing (or the query fails/times out).
    async fn model_suggestions_message(&self, current: &ProviderChoice) -> String {
        const MODEL_SUGGESTION_LIMIT: usize = 20;
        const DISCOVERY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

        let discovered = tokio::time::timeout(
            DISCOVERY_TIMEOUT,
            super::model_discovery::discover_provider_models(current, &self.settings.snapshot()),
        )
        .await;
        if let Ok(Ok(Some(models))) = discovered
            && !models.is_empty()
        {
            let provider = current.provider_name();
            let shown: Vec<&str> = models
                .iter()
                .take(MODEL_SUGGESTION_LIMIT)
                .map(|model| model.model_id.as_str())
                .collect();
            let suffix = if models.len() > MODEL_SUGGESTION_LIMIT {
                format!(" … and {} more", models.len() - MODEL_SUGGESTION_LIMIT)
            } else {
                String::new()
            };
            return format!("models from {provider} API: {}{suffix}", shown.join(", "));
        }
        format!(
            "suggestions: {}",
            ProviderChoice::model_suggestions_for_provider(current.provider_name()).join(", ")
        )
    }

    async fn change_effort(&self, raw: &str) -> everruns_core::Result<CommandResult> {
        let current = self
            .provider
            .read()
            .expect("provider lock poisoned")
            .clone();
        if raw.is_empty() {
            return Ok(CommandResult {
                success: true,
                message: format!(
                    "setup effort: {}; suggestions: {}",
                    current.label(),
                    ProviderChoice::reasoning_effort_suggestions().join(", ")
                ),
                error_code: None,
                error_fields: None,
            });
        }

        let next = match current.resolve_reasoning_effort(raw) {
            Ok(next) => next,
            Err(err) => return Ok(failed_result(format!("setup effort failed: {err}"))),
        };
        let label = next.label();
        let persist_note = self.persist_model_choice(&next);
        *self.provider.write().expect("provider lock poisoned") = next;
        Ok(CommandResult {
            success: true,
            message: format!("setup effort changed: {label}{persist_note}"),
            error_code: None,
            error_fields: None,
        })
    }

    fn change_token(&self, raw: &str) -> everruns_core::Result<CommandResult> {
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
                    "setup tokens ({}): {}",
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
                "setup token failed: unknown provider `{provider}`; expected one of {}",
                TOKEN_PROVIDERS.join(", ")
            )));
        }
        if rest.is_empty() {
            return Ok(failed_result(
                "setup token failed: expected <provider> <value|clear>".to_string(),
            ));
        }

        if rest.eq_ignore_ascii_case("clear") {
            return Ok(match self.settings.clear_token(&provider) {
                Ok(true) => CommandResult {
                    success: true,
                    message: format!("setup token cleared for {provider}"),
                    error_code: None,
                    error_fields: None,
                },
                Ok(false) => CommandResult {
                    success: true,
                    message: format!("setup token: no token was stored for {provider}"),
                    error_code: None,
                    error_fields: None,
                },
                Err(err) => failed_result(format!("setup token clear failed: {err}")),
            });
        }

        match self.settings.set_token(provider.clone(), rest.to_string()) {
            Ok(()) => Ok(CommandResult {
                success: true,
                message: format!(
                    "setup token stored for {provider} (in {})",
                    self.settings.path().display()
                ),
                error_code: None,
                error_fields: None,
            }),
            Err(err) => Ok(failed_result(format!("setup token save failed: {err}"))),
        }
    }

    fn change_base_url(&self, raw: &str) -> everruns_core::Result<CommandResult> {
        let mut parts = raw.splitn(2, char::is_whitespace);
        let provider = parts.next().unwrap_or_default().to_ascii_lowercase();
        let rest = parts.next().unwrap_or_default().trim();
        if !BASE_URL_PROVIDERS.contains(&provider.as_str()) {
            return Ok(failed_result(format!(
                "setup url failed: unknown provider `{provider}`; expected one of {}",
                BASE_URL_PROVIDERS.join(", ")
            )));
        }
        if rest.is_empty() {
            let snapshot = self.settings.snapshot();
            let current = snapshot
                .base_url_for(&provider)
                .unwrap_or("<unset>")
                .to_string();
            return Ok(CommandResult {
                success: true,
                message: format!("setup url for {provider}: {current}"),
                error_code: None,
                error_fields: None,
            });
        }
        if rest.eq_ignore_ascii_case("clear") {
            return Ok(match self.settings.clear_base_url(&provider) {
                Ok(true) => CommandResult {
                    success: true,
                    message: format!("setup url cleared for {provider}"),
                    error_code: None,
                    error_fields: None,
                },
                Ok(false) => CommandResult {
                    success: true,
                    message: format!("setup url: no base URL was stored for {provider}"),
                    error_code: None,
                    error_fields: None,
                },
                Err(err) => failed_result(format!("setup url clear failed: {err}")),
            });
        }
        if !rest.starts_with("http://") && !rest.starts_with("https://") {
            return Ok(failed_result(
                "setup url failed: base URL must start with http:// or https://".to_string(),
            ));
        }
        match self
            .settings
            .set_base_url(provider.clone(), rest.to_string())
        {
            Ok(()) => Ok(CommandResult {
                success: true,
                message: format!(
                    "setup url stored for {provider} (in {})",
                    self.settings.path().display()
                ),
                error_code: None,
                error_fields: None,
            }),
            Err(err) => Ok(failed_result(format!("setup url save failed: {err}"))),
        }
    }

    fn change_attribution(&self, raw: &str) -> everruns_core::Result<CommandResult> {
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("status") {
            let enabled = self.settings.snapshot().attribution_enabled();
            return Ok(CommandResult {
                success: true,
                message: format!(
                    "setup attribution: {} ({})",
                    on_off(enabled),
                    self.settings.path().display()
                ),
                error_code: None,
                error_fields: None,
            });
        }

        let enabled = match parse_on_off(trimmed) {
            Some(enabled) => enabled,
            None => {
                return Ok(failed_result(
                    "setup attribution failed: expected on/off".to_string(),
                ));
            }
        };
        match self.settings.set_attribution(enabled) {
            Ok(()) => Ok(CommandResult {
                success: true,
                message: format!("setup attribution: {}", on_off(enabled)),
                error_code: None,
                error_fields: None,
            }),
            Err(err) => Ok(failed_result(format!(
                "setup attribution save failed: {err}"
            ))),
        }
    }

    fn change_approval(&self, raw: &str) -> everruns_core::Result<CommandResult> {
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("status") {
            return Ok(CommandResult {
                success: true,
                message: format!(
                    "setup approval: {} (protective | normal | off) ({})",
                    self.settings.snapshot().approval_mode(),
                    self.settings.path().display()
                ),
                error_code: None,
                error_fields: None,
            });
        }

        let mode = match ApprovalMode::parse(trimmed) {
            Some(mode) => mode,
            None => {
                return Ok(failed_result(
                    "setup approval failed: expected protective, normal, or off".to_string(),
                ));
            }
        };
        match self.settings.set_approval_mode(mode) {
            Ok(()) => Ok(CommandResult {
                success: true,
                message: format!("setup approval: {mode}"),
                error_code: None,
                error_fields: None,
            }),
            Err(err) => Ok(failed_result(format!("setup approval save failed: {err}"))),
        }
    }
}

fn parse_on_off(raw: &str) -> Option<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "on" | "true" | "yes" | "1" => Some(true),
        "off" | "false" | "no" | "0" => Some(false),
        _ => None,
    }
}

fn on_off(enabled: bool) -> &'static str {
    if enabled { "on" } else { "off" }
}

fn failed_result(message: String) -> CommandResult {
    CommandResult {
        success: false,
        message,
        error_code: None,
        error_fields: None,
    }
}

impl SetupCapability {
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
        // CUSTOM_API_KEY is deliberately absent: the custom endpoint is
        // unusable without a base URL, so a stray key alone must not
        // suppress first-run onboarding.
        "CUSTOM_BASE_URL",
    ];
    VARS.iter()
        .any(|var| std::env::var(var).map(|v| !v.is_empty()).unwrap_or(false))
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
            std::env::remove_var("CUSTOM_BASE_URL");
            std::env::remove_var("CUSTOM_API_KEY");
        }
        let settings = crate::settings::Settings::default();
        assert!(SetupCapability::needs_onboarding(&settings));
    }

    #[test]
    fn needs_onboarding_ignores_custom_api_key_without_base_url() {
        // A stray CUSTOM_API_KEY is not a usable credential: without a base
        // URL the custom provider cannot run, so onboarding must still open.
        let _guard = crate::test_env::lock();
        unsafe {
            std::env::remove_var("OPENAI_API_KEY");
            std::env::remove_var("ANTHROPIC_API_KEY");
            std::env::remove_var("OPENROUTER_API_KEY");
            std::env::remove_var("GEMINI_API_KEY");
            std::env::remove_var("GOOGLE_API_KEY");
            std::env::remove_var("OLLAMA_BASE_URL");
            std::env::remove_var("OLLAMA_API_KEY");
            std::env::remove_var("CUSTOM_BASE_URL");
            std::env::set_var("CUSTOM_API_KEY", "sk-orphan");
        }
        let settings = crate::settings::Settings::default();
        assert!(SetupCapability::needs_onboarding(&settings));

        unsafe {
            std::env::set_var("CUSTOM_BASE_URL", "http://localhost:8000/v1");
        }
        assert!(!SetupCapability::needs_onboarding(&settings));
        unsafe {
            std::env::remove_var("CUSTOM_BASE_URL");
            std::env::remove_var("CUSTOM_API_KEY");
        }
    }

    #[test]
    fn needs_onboarding_false_when_provider_is_saved() {
        let settings = crate::settings::Settings {
            provider: Some("anthropic".to_string()),
            ..Default::default()
        };
        assert!(!SetupCapability::needs_onboarding(&settings));
    }

    #[test]
    fn needs_onboarding_false_when_token_is_saved() {
        let mut tokens = std::collections::BTreeMap::new();
        tokens.insert("openai".to_string(), "sk-test".to_string());
        let settings = crate::settings::Settings {
            provider: None,
            tokens,
            ..Default::default()
        };
        assert!(!SetupCapability::needs_onboarding(&settings));
    }

    #[tokio::test]
    async fn attribution_prompt_follows_settings() {
        let tmp = tempfile::tempdir().expect("tmp");
        let settings = Arc::new(SettingsStore::open(tmp.path().join("settings.toml")));
        let capability = AttributionCapability {
            config: settings.clone(),
        };
        let ctx =
            SystemPromptContext::without_file_store(everruns_core::typed_id::SessionId::new());

        let enabled = capability
            .system_prompt_contribution(&ctx)
            .await
            .expect("enabled attribution prompt");
        assert!(enabled.contains(YOLOP_ATTRIBUTION_TRAILER));
        assert!(enabled.contains(YOLOP_PR_ATTRIBUTION));

        settings
            .set_attribution(false)
            .expect("disable attribution");
        assert!(capability.system_prompt_contribution(&ctx).await.is_none());
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

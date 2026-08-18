// Host/example capabilities for yolop: local environment context, bash, and
// TUI-facing slash commands that mutate this process's provider selection.

use crate::capabilities::model_discovery::search_configured_models;
use crate::capabilities::narration::stable_labeled;
use crate::config::service::ConfigService;
use crate::config::{ApprovalMode, SettingsStore};
use crate::exec::tools::{BashTool, Workspace};
use crate::runtime::{ProviderChoice, SUPPORTED_PROVIDERS, resolve_for_settings};
use async_trait::async_trait;
use chrono::Local;
use everruns_core::command::{
    CommandArg, CommandDescriptor, CommandExecutionContext, CommandResult, CommandSource,
    ExecuteCommandRequest,
};
use everruns_core::tool_narration::{ToolNarrationPhase, arg_str, truncate};
use everruns_core::{Capability, CapabilityStatus, SystemPromptContext};
use everruns_core::{Tool, ToolExecutionResult};
use everruns_host::RuntimeProviderStore;
use everruns_provider::ToolCall;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, RwLock};

// ---------- environment context ----------

pub(crate) const ENVIRONMENT_CONTEXT_CAPABILITY_ID: &str = "code_environment_context";

/// Mutable per-turn context rendered by the final prompt capability.
///
/// Contributors update named entries before this capability runs. Keeping
/// dynamic data in one trailing prompt segment avoids invalidating the stable
/// capability prefix in provider prompt caches.
#[derive(Clone, Default)]
pub(crate) struct EnvironmentContextRegistry {
    entries: Arc<RwLock<BTreeMap<String, String>>>,
}

impl EnvironmentContextRegistry {
    pub(crate) fn set(&self, key: impl Into<String>, value: impl Into<String>) {
        self.entries
            .write()
            .expect("environment context registry poisoned")
            .insert(key.into(), value.into());
    }

    pub(crate) fn remove(&self, key: &str) {
        self.entries
            .write()
            .expect("environment context registry poisoned")
            .remove(key);
    }

    pub(crate) fn snapshot(&self) -> BTreeMap<String, String> {
        self.entries
            .read()
            .expect("environment context registry poisoned")
            .clone()
    }
}

pub(crate) struct CodingCliEnvironmentCapability {
    repo_root: PathBuf,
    active_root: Arc<RwLock<PathBuf>>,
    client_ui: ClientUiContext,
    registry: EnvironmentContextRegistry,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ClientUiContext {
    Acp,
    Print,
    Tui,
    None,
}

impl ClientUiContext {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Acp => "ACP",
            Self::Print => "print",
            Self::Tui => "TUI",
            Self::None => "none",
        }
    }

    /// What the host is known to render, so the model can decide how much
    /// formatting an answer is worth.
    ///
    /// A listed capability is a claim yolop can stand behind; an absent one
    /// means "not known to render", which is why the list is additive rather
    /// than a per-capability true/false. That distinction is load-bearing for
    /// ACP: the editor renders agent messages as markdown, but nothing in the
    /// protocol says whether it draws Mermaid, so `supports_markdown` is listed
    /// and `supports_markdown_mermaid` is not.
    ///
    /// The TUI renders the transcript itself and claims both — full-screen and
    /// the inline mirror share one markdown path, so they cannot diverge.
    /// `--print` writes raw text to stdout and headless runs have no viewer at
    /// all, so neither claims anything.
    fn ui_capabilities(&self) -> &'static [&'static str] {
        match self {
            Self::Tui => &["supports_markdown", "supports_markdown_mermaid"],
            Self::Acp => &["supports_markdown"],
            Self::Print | Self::None => &[],
        }
    }
}

impl CodingCliEnvironmentCapability {
    pub(crate) fn new(
        repo_root: PathBuf,
        active_root: Arc<RwLock<PathBuf>>,
        client_ui: ClientUiContext,
        registry: EnvironmentContextRegistry,
    ) -> Self {
        Self {
            repo_root,
            active_root,
            client_ui,
            registry,
        }
    }

    fn collect(&self) -> EnvironmentContext {
        let active_path = self
            .active_root
            .read()
            .map(|guard| guard.clone())
            .unwrap_or_else(|_| self.repo_root.clone());
        EnvironmentContext {
            cwd: active_path.display().to_string(),
            client_ui: self.client_ui.clone(),
            shell: shell_name(),
            current_date: Local::now().format("%Y-%m-%d").to_string(),
            timezone: local_timezone(),
            git_repo: git_output(&self.repo_root, &["config", "--get", "remote.origin.url"])
                .map(|remote| redact_git_remote_secret(&remote))
                .or_else(|| {
                    git_output(&self.repo_root, &["rev-parse", "--show-toplevel"])
                        .map(|_| self.repo_root.display().to_string())
                }),
            git_user: git_output(&self.repo_root, &["config", "--get", "user.name"]),
            git_email: git_output(&self.repo_root, &["config", "--get", "user.email"]),
            repo_root: self.repo_root.display().to_string(),
            git_current_branch: git_current_branch(&active_path),
            worktree_path: if active_path != self.repo_root {
                Some(active_path.display().to_string())
            } else {
                None
            },
            contributions: self.registry.snapshot(),
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
        "Adds current workspace, shell, date, timezone, Git, and UI rendering context to the prompt."
    }
    fn status(&self) -> CapabilityStatus {
        CapabilityStatus::Available
    }
    fn category(&self) -> Option<&str> {
        Some("Examples")
    }
    async fn system_prompt_contribution(&self, _ctx: &SystemPromptContext) -> Option<String> {
        Some(render_environment_context(&self.collect()))
    }
    fn system_prompt_preview(&self) -> Option<String> {
        Some(
            "\
<environment_context>
  <cwd>/path/to/workspace</cwd>
  <client_ui>TUI|print|ACP|none</client_ui>
  <ui_capabilities>supports_markdown, supports_markdown_mermaid</ui_capabilities>
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
struct EnvironmentContext {
    cwd: String,
    client_ui: ClientUiContext,
    shell: String,
    current_date: String,
    timezone: String,
    git_repo: Option<String>,
    git_user: Option<String>,
    git_email: Option<String>,
    repo_root: String,
    git_current_branch: Option<String>,
    worktree_path: Option<String>,
    contributions: BTreeMap<String, String>,
}

fn render_environment_context(context: &EnvironmentContext) -> String {
    let mut out = String::new();
    out.push_str("<environment_context>\n");
    push_xml_field(&mut out, "cwd", &context.cwd);
    push_xml_field(&mut out, "client_ui", context.client_ui.as_str());
    // Empty stays "none" rather than an empty element: a host that renders
    // nothing is a fact worth stating, and an absent field would read as one
    // yolop failed to compute.
    let ui_capabilities = context.client_ui.ui_capabilities();
    push_xml_field(
        &mut out,
        "ui_capabilities",
        &if ui_capabilities.is_empty() {
            "none".to_string()
        } else {
            ui_capabilities.join(", ")
        },
    );
    push_xml_field(&mut out, "repo_root", &context.repo_root);
    if let Some(path) = &context.worktree_path {
        out.push_str("  <git_worktree>\n");
        push_xml_field(&mut out, "path", path);
        if let Some(branch) = &context.git_current_branch {
            push_xml_field(&mut out, "branch", branch);
        }
        out.push_str("  </git_worktree>\n");
    }
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
    if context.worktree_path.is_none()
        && let Some(value) = &context.git_current_branch
    {
        push_xml_field(&mut out, "git_current_branch", value);
    }
    for (key, value) in &context.contributions {
        out.push_str("  <contribution name=\"");
        out.push_str(&xml_escape(key));
        out.push_str("\">");
        out.push_str(&xml_escape(value));
        out.push_str("</contribution>\n");
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

pub(crate) const CODING_BASH_CAPABILITY_ID: &str = "yolop_bash";

pub(crate) struct CodingBashCapability {
    pub(crate) workspace: Workspace,
    pub(crate) sandbox: Arc<dyn crate::exec::sandbox::SandboxProvider>,
    pub(crate) expose_command: bool,
    pub(crate) approval_policy: crate::config::ApprovalPolicy,
    pub(crate) approval_gate: Arc<crate::sandbox_approval::ApprovalGate>,
    pub(crate) control: Option<Arc<dyn crate::control::ControlService>>,
}

#[async_trait]
impl Capability for CodingBashCapability {
    fn id(&self) -> &str {
        CODING_BASH_CAPABILITY_ID
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
        vec![Box::new(
            BashTool::with_policy(
                self.workspace.clone(),
                self.sandbox.clone(),
                self.approval_policy,
                self.approval_gate.clone(),
            )
            .with_control(self.control.clone()),
        )]
    }
    fn commands(&self) -> Vec<CommandDescriptor> {
        if !self.expose_command {
            return Vec::new();
        }
        vec![CommandDescriptor {
            name: "shell".to_string(),
            description: "run a shell command".to_string(),
            source: CommandSource::System,
            args: vec![CommandArg {
                name: "command".to_string(),
                description: "shell command".to_string(),
                required: true,
                suggestions: Vec::new(),
            }],
        }]
    }
    async fn execute_command(
        &self,
        request: &ExecuteCommandRequest,
        _ctx: &CommandExecutionContext,
    ) -> everruns_provider::error::Result<CommandResult> {
        if request.name != "shell" {
            return Err(everruns_provider::error::AgentLoopError::config(format!(
                "{} cannot execute /{}",
                self.id(),
                request.name
            )));
        }
        let command = request
            .arguments
            .as_deref()
            .map(str::trim)
            .filter(|command| !command.is_empty())
            .ok_or_else(|| {
                everruns_provider::error::AgentLoopError::config("/shell requires: command")
            })?;
        let result = BashTool::with_policy(
            self.workspace.clone(),
            self.sandbox.clone(),
            self.approval_policy,
            self.approval_gate.clone(),
        )
        .with_control(self.control.clone())
        .execute(json!({ "command": command, "output": "normal" }))
        .await;
        Ok(shell_command_result(result))
    }
}

fn shell_command_result(result: ToolExecutionResult) -> CommandResult {
    match result {
        ToolExecutionResult::Success(value)
        | ToolExecutionResult::SuccessWithImages { result: value, .. } => {
            let success = value["success"].as_bool().unwrap_or(true);
            CommandResult {
                success,
                message: format_shell_output(&value),
                error_code: None,
                error_fields: None,
            }
        }
        ToolExecutionResult::ToolError(message) => CommandResult {
            success: false,
            message,
            error_code: None,
            error_fields: None,
        },
        ToolExecutionResult::InternalError(_) => CommandResult {
            success: false,
            message: "shell command failed internally".to_string(),
            error_code: None,
            error_fields: None,
        },
        ToolExecutionResult::ConnectionRequired { provider } => CommandResult {
            success: false,
            message: format!("shell command requires connection: {provider}"),
            error_code: None,
            error_fields: None,
        },
    }
}

fn format_shell_output(value: &serde_json::Value) -> String {
    let mut parts = Vec::new();
    if let Some(stdout) = value["stdout"]
        .as_str()
        .map(str::trim_end)
        .filter(|text| !text.is_empty())
    {
        parts.push(stdout.to_string());
    }
    if let Some(stderr) = value["stderr"]
        .as_str()
        .map(str::trim_end)
        .filter(|text| !text.is_empty())
    {
        parts.push(stderr.to_string());
    }
    if parts.is_empty() {
        let exit_code = value["exit_code"].as_i64().unwrap_or(0);
        format!("exit {exit_code}")
    } else {
        parts.join("\n")
    }
}

// ---------- /setup ----------
//
// One user-facing command owns provider, token, and model setup. The TUI starts
// an interactive wizard for `/setup`; the internal setup subcommands below let
// that wizard mutate runtime state without exposing `/provider`, `/token`, and
// `/model` as separate commands.

pub(crate) const MODELS_CAPABILITY_ID: &str = "models";

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

pub(crate) struct ModelsCapability {
    pub(crate) provider: Arc<RwLock<ProviderChoice>>,
    pub(crate) provider_store: Arc<dyn RuntimeProviderStore>,
    /// Reads current configuration through the shared config service…
    pub(crate) config: Arc<dyn ConfigService>,
    /// …and writes provider/token/model choices through the concrete store.
    pub(crate) settings: Arc<SettingsStore>,
    /// Model choice awaiting proof that the next model turn succeeds.
    pub(crate) pending_model_choice: Arc<RwLock<Option<ProviderChoice>>>,
}

impl ModelsCapability {
    /// The shared, cloneable handle the `/setup` command and the model-facing
    /// `set_*` tools both drive. Everything that mutates the live provider/model
    /// lives on [`SetupController`] so the slash command and the agent tools
    /// route through one implementation — there is no second config path.
    fn controller(&self) -> SetupController {
        SetupController {
            provider: self.provider.clone(),
            provider_store: self.provider_store.clone(),
            config: self.config.clone(),
            settings: self.settings.clone(),
            pending_model_choice: self.pending_model_choice.clone(),
        }
    }
}

/// Live provider/model/effort controller. Holds the same handles as
/// [`ModelsCapability`] and owns every mutation (`change_provider`,
/// `change_model`, `change_effort`, tokens, urls, attribution, approval). The
/// slash command (`execute_command`) and the agent-facing `set_*` tools both
/// call these methods, so a natural-language request and a typed `/setup`
/// apply identically and take effect on the live session.
#[derive(Clone)]
pub(crate) struct SetupController {
    provider: Arc<RwLock<ProviderChoice>>,
    provider_store: Arc<dyn RuntimeProviderStore>,
    config: Arc<dyn ConfigService>,
    settings: Arc<SettingsStore>,
    pending_model_choice: Arc<RwLock<Option<ProviderChoice>>>,
}

#[async_trait]
impl Capability for ModelsCapability {
    fn id(&self) -> &str {
        MODELS_CAPABILITY_ID
    }
    fn name(&self) -> &str {
        "Models"
    }
    fn description(&self) -> &str {
        "Discover and control models, providers, reasoning, and provider credentials."
    }
    fn status(&self) -> CapabilityStatus {
        CapabilityStatus::Available
    }
    fn category(&self) -> Option<&str> {
        Some("Examples")
    }
    fn system_prompt_addition(&self) -> Option<&str> {
        Some(MODELS_PROMPT)
    }
    fn commands(&self) -> Vec<CommandDescriptor> {
        vec![CommandDescriptor {
            name: "setup".to_string(),
            description: "Configure provider, API key, and model.".to_string(),
            source: CommandSource::System,
            args: vec![setup_command_arg()],
        }]
    }

    fn tools(&self) -> Vec<Box<dyn Tool>> {
        // The model-facing surface for live session control. Each tool routes
        // through the same `SetupController` the `/setup` command uses, so a
        // natural-language request ("switch to high effort", "use gpt-5.4")
        // applies to the running session exactly like the slash command — no
        // overlay, no next-run-only deferral. See knowledge/specs/conversational-control.md.
        vec![
            Box::new(SetReasoningEffortTool {
                controller: self.controller(),
            }),
            Box::new(SearchModelsTool {
                settings: self.settings.clone(),
            }),
            Box::new(SetModelTool {
                controller: self.controller(),
            }),
            Box::new(SetProviderTool {
                controller: self.controller(),
            }),
        ]
    }

    async fn execute_command(
        &self,
        request: &ExecuteCommandRequest,
        _ctx: &CommandExecutionContext,
    ) -> everruns_provider::error::Result<CommandResult> {
        if request.name != "setup" {
            return Err(everruns_provider::error::AgentLoopError::config(format!(
                "{} cannot execute /{}",
                self.id(),
                request.name
            )));
        }
        let controller = self.controller();
        let raw = request.arguments.as_deref().unwrap_or("").trim();
        if raw.is_empty() || raw == "status" {
            return Ok(controller.status_result());
        }

        let mut parts = raw.splitn(2, char::is_whitespace);
        let action = parts.next().unwrap_or_default();
        let rest = parts.next().unwrap_or_default().trim();
        match action {
            "provider" => controller.change_provider(rest).await,
            "model" => controller.change_model(rest).await,
            "effort" => controller.change_effort(rest).await,
            "token" => controller.change_token(rest),
            "url" => controller.change_base_url(rest),
            "attribution" => controller.change_attribution(rest),
            "approval" => controller.change_approval(rest),
            _ => Ok(failed_result(
                "usage: /setup — run guided setup; internal forms: status, provider <name> [model], token <provider> <value|clear>, url <provider> <base-url|clear>, model <id> [reasoning-effort], effort <reasoning-effort>, attribution <on|off>, approval <protective|normal|off>".to_string(),
            )),
        }
    }
}

/// Always-on guidance so the agent knows it can reconfigure the live session
/// itself, in prose, without the user typing a slash command or touching an
/// overlay. Mirrors the conversational-control contract (knowledge/specs/conversational-control.md).
// Discovery, not how-to: without this the model does not know it may retune the
// live session at all. When to escalate effort, and not thrashing the model
// mid-task, are judgement calls left to the model.
pub(crate) const MODELS_PROMPT: &str = "<capability id=\"models\">\n\
    `set_reasoning_effort`, `set_model`, and `set_provider` apply next turn. For partial \
    model names, call `search_models`, show ambiguous matches, and never guess an ID. \
    Unknown effort levels return accepted values.\n\
    </capability>";

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

    CommandArg {
        name: "action".to_string(),
        description: "status | provider <name> [model] | token <provider> <value|clear> | url <provider> <base-url|clear> | model <id> | effort <level> | attribution <on|off> | approval <protective|normal|off>".to_string(),
        required: false,
        suggestions,
    }
}

impl SetupController {
    fn status_result(&self) -> CommandResult {
        let current = self
            .provider
            .read()
            .expect("provider lock poisoned")
            .clone();
        let snapshot = self.config.snapshot();
        let saved = snapshot
            .default_provider
            .clone()
            .unwrap_or_else(|| "<unset>".to_string());
        let stored: Vec<&str> = snapshot.tokens.keys().map(String::as_str).collect();
        let stored_label = if stored.is_empty() {
            if snapshot.has_codex_auth() {
                "codex_auth".to_string()
            } else {
                "none".to_string()
            }
        } else {
            let mut stored = stored;
            if snapshot.has_codex_auth() {
                stored.push("codex_auth");
            }
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
    async fn change_provider(&self, raw: &str) -> everruns_provider::error::Result<CommandResult> {
        let mut parts = raw.splitn(2, char::is_whitespace);
        let name = parts.next().unwrap_or_default();
        let model_spec = parts.next().unwrap_or_default().trim();
        if name.is_empty() {
            return Ok(failed_result(format!(
                "setup provider failed: choose one of {}",
                SUPPORTED_PROVIDERS.join(", ")
            )));
        }

        // One snapshot reused across both reads in this command: consistent and
        // avoids cloning the full Settings (token strings included) twice.
        let snapshot = self.config.snapshot();
        let resolved = match resolve_for_settings(name, &snapshot) {
            Ok(resolved) => resolved,
            Err(err) => return Ok(failed_result(format!("setup provider failed: {err}"))),
        };
        let mut notes = resolved.notes;
        let next = if model_spec.is_empty() {
            resolved.choice
        } else {
            match resolved.choice.resolve_model_spec(model_spec) {
                Ok(n) => n,
                Err(err) => return Ok(failed_result(format!("setup provider failed: {err}"))),
            }
        };
        let (next, reconcile_notes) =
            super::model_discovery::reconcile_provider_with_catalog(next, &snapshot).await;
        notes.extend(reconcile_notes);
        if next.model_id().trim().is_empty() {
            return Ok(failed_result(format!(
                "setup provider failed: no model configured for {name}; pick one with /setup"
            )));
        }
        let mw = match next.model_with_provider(&snapshot) {
            Ok(m) => m,
            Err(err) => return Ok(failed_result(format!("setup provider failed: {err}"))),
        };
        if let Err(err) = self
            .provider_store
            .set_default_model_spec(mw.model_spec())
            .await
        {
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
        let persist_note = match model_persist.and_then(|()| {
            self.settings
                .set_default_provider(Some(provider_name.clone()))
        }) {
            Ok(()) => format!("saved to {}", self.settings.path().display()),
            Err(err) => format!("warning: settings not saved: {err}"),
        };
        Ok(CommandResult {
            success: true,
            message: format!(
                "setup provider changed: {label} ({persist_note}){}",
                if notes.is_empty() {
                    String::new()
                } else {
                    format!("; {}", notes.join("; "))
                }
            ),
            error_code: None,
            error_fields: None,
        })
    }

    async fn change_model(&self, raw: &str) -> everruns_provider::error::Result<CommandResult> {
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
        let mw = match next.model_with_provider(&self.config.snapshot()) {
            Ok(m) => m,
            Err(err) => {
                return Ok(failed_result(format!("setup model failed: {err}")));
            }
        };
        if let Err(err) = self
            .provider_store
            .set_default_model_spec(mw.model_spec())
            .await
        {
            return Ok(failed_result(format!("setup model failed: {err}")));
        }
        let label = next.label();
        *self.provider.write().expect("provider lock poisoned") = next.clone();
        *self
            .pending_model_choice
            .write()
            .expect("pending model lock poisoned") = Some(next);
        Ok(CommandResult {
            success: true,
            message: format!(
                "setup model changed: {label}; applies to this session until a turn succeeds"
            ),
            error_code: None,
            error_fields: None,
        })
    }

    /// Persist a model switch: the provider preference and the
    /// provider-relative `model [effort]` spec, so both survive a restart
    /// (the model picker promises "future sessions"). A failed save is kept
    /// pending so a later successful turn can retry it.
    pub(crate) fn persist_pending_model_choice(
        settings: &SettingsStore,
        pending_model_choice: &RwLock<Option<ProviderChoice>>,
    ) -> String {
        let mut pending = pending_model_choice
            .write()
            .expect("pending model lock poisoned");
        let Some(next) = pending.as_ref() else {
            return String::new();
        };
        let warning = Self::persist_model_choice(settings, next);
        if warning.is_empty() {
            pending.take();
        }
        warning
    }

    fn persist_model_choice(settings: &SettingsStore, next: &ProviderChoice) -> String {
        let provider_name = next.provider_name().to_string();
        let result = settings
            .set_default_provider(Some(provider_name.clone()))
            .and_then(|()| settings.set_model(provider_name, next.model_spec()));
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
            super::model_discovery::discover_provider_models(current, &self.config.snapshot()),
        )
        .await;
        if let Ok(Ok(Some(models))) = discovered
            && !models.is_empty()
        {
            let provider = current.provider_name();
            let ranked = super::model_ranking::rank_discovered_models(
                provider,
                models,
                Some(current.model_id()),
            );
            let shown: Vec<&str> = ranked
                .models
                .iter()
                .take(MODEL_SUGGESTION_LIMIT)
                .map(|model| model.model_id.as_str())
                .collect();
            let suffix = if ranked.models.len() > MODEL_SUGGESTION_LIMIT {
                format!(
                    " … and {} more",
                    ranked.models.len() - MODEL_SUGGESTION_LIMIT
                )
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

    async fn change_effort(&self, raw: &str) -> everruns_provider::error::Result<CommandResult> {
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
                    current
                        .reasoning_effort_options()
                        .iter()
                        .map(|effort| effort.value.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
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
        let persist_note = Self::persist_model_choice(&self.settings, &next);
        *self.provider.write().expect("provider lock poisoned") = next;
        Ok(CommandResult {
            success: true,
            message: format!("setup effort changed: {label}{persist_note}"),
            error_code: None,
            error_fields: None,
        })
    }

    fn change_token(&self, raw: &str) -> everruns_provider::error::Result<CommandResult> {
        if raw.is_empty() {
            let snapshot = self.config.snapshot();
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

    fn change_base_url(&self, raw: &str) -> everruns_provider::error::Result<CommandResult> {
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
            let snapshot = self.config.snapshot();
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

    fn change_attribution(&self, raw: &str) -> everruns_provider::error::Result<CommandResult> {
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("status") {
            let enabled = self.config.attribution_enabled();
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

    fn change_approval(&self, raw: &str) -> everruns_provider::error::Result<CommandResult> {
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("status") {
            return Ok(CommandResult {
                success: true,
                message: format!(
                    "setup approval: {} (protective | normal | off) ({})",
                    self.config.approval_mode(),
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

// ---------- model-facing live-config tools ----------
//
// These expose the `SetupController` mutations as agent tools so the model can
// reconfigure the running session from a natural-language request (or on its
// own), instead of asking the user to type `/setup` or confirm an overlay. They
// reuse the exact `change_*` logic the slash command uses, so behavior — live
// application, validation, persistence — is identical across both entry points.

/// Map a `change_*` outcome onto a tool result: a failed (but non-erroring)
/// `CommandResult` becomes a recoverable tool error carrying the same message
/// (e.g. an unknown effort plus the valid set), so the model can correct itself.
fn into_tool_result(
    outcome: everruns_provider::error::Result<CommandResult>,
) -> ToolExecutionResult {
    match outcome {
        Ok(result) if result.success => ToolExecutionResult::success(json!({
            "success": true,
            "message": result.message,
        })),
        Ok(result) => ToolExecutionResult::tool_error(result.message),
        Err(err) => ToolExecutionResult::tool_error(err.to_string()),
    }
}

fn required_str_arg<'a>(arguments: &'a Value, key: &str) -> Result<&'a str, ToolExecutionResult> {
    match arguments.get(key).and_then(Value::as_str) {
        Some(value) if !value.trim().is_empty() => Ok(value.trim()),
        _ => Err(ToolExecutionResult::tool_error(format!(
            "'{key}' is required"
        ))),
    }
}

struct SetReasoningEffortTool {
    controller: SetupController,
}

#[async_trait]
impl Tool for SetReasoningEffortTool {
    fn narrate(
        &self,
        tool_call: &ToolCall,
        phase: ToolNarrationPhase,
        locale: Option<&str>,
        _ctx: everruns_core::tool_narration::ToolNarrationContext<'_>,
    ) -> Option<String> {
        let _ = locale;
        let effort = arg_str(&tool_call.arguments, &["effort"]).map(|value| truncate(value, 24));
        Some(stable_labeled("Set reasoning effort", effort, phase))
    }

    fn name(&self) -> &str {
        "set_reasoning_effort"
    }
    fn display_name(&self) -> Option<&str> {
        Some("Set reasoning effort")
    }
    fn description(&self) -> &str {
        "Change the current model's reasoning effort for this session (e.g. escalate to think \
         harder before a difficult step, or deescalate for cheap follow-ups). Applies on the next \
         turn — no restart. The valid set is model-specific; if the level is unknown the tool \
         returns the accepted values so you can retry."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "effort": {
                    "type": "string",
                    "description": "Reasoning-effort level for the active model, e.g. low / medium / high (model-specific)."
                }
            },
            "required": ["effort"],
            "additionalProperties": false
        })
    }
    async fn execute(&self, arguments: Value) -> ToolExecutionResult {
        let effort = match required_str_arg(&arguments, "effort") {
            Ok(value) => value,
            Err(err) => return err,
        };
        into_tool_result(self.controller.change_effort(effort).await)
    }
}

struct SearchModelsTool {
    settings: Arc<SettingsStore>,
}

#[async_trait]
impl Tool for SearchModelsTool {
    fn name(&self) -> &str {
        "search_models"
    }
    fn display_name(&self) -> Option<&str> {
        Some("Search models")
    }
    fn description(&self) -> &str {
        "Search model IDs and display names across all currently usable providers. Use this before set_model for a partial or unqualified model name."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type":"object","properties":{"query":{"type":"string"}},"required":["query"],"additionalProperties":false})
    }
    async fn execute(&self, arguments: Value) -> ToolExecutionResult {
        let Some(query) = arguments.get("query").and_then(Value::as_str) else {
            return ToolExecutionResult::tool_error("missing required string 'query'");
        };
        if query.trim().is_empty() {
            return ToolExecutionResult::tool_error("'query' must not be empty");
        }
        let result = search_configured_models(&self.settings.snapshot(), query).await;
        ToolExecutionResult::success(
            json!({"query":query,"matches":result.matches.into_iter().map(|item| json!({"provider":item.provider,"model":item.model_id,"display_name":item.display_name})).collect::<Vec<_>>(),"providers_searched":result.providers_searched,"provider_errors":result.provider_errors}),
        )
    }
}

struct SetModelTool {
    controller: SetupController,
}

#[async_trait]
impl Tool for SetModelTool {
    fn narrate(
        &self,
        tool_call: &ToolCall,
        phase: ToolNarrationPhase,
        locale: Option<&str>,
        _ctx: everruns_core::tool_narration::ToolNarrationContext<'_>,
    ) -> Option<String> {
        let _ = locale;
        let model = arg_str(&tool_call.arguments, &["model"]).map(|value| truncate(value, 48));
        Some(stable_labeled("Set model", model, phase))
    }

    fn name(&self) -> &str {
        "set_model"
    }
    fn display_name(&self) -> Option<&str> {
        Some("Set model")
    }
    fn description(&self) -> &str {
        "Switch to an exact model ID for the current provider. For a partial name, call search_models first; never pass an unresolved fragment."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "model": {
                    "type": "string",
                    "description": "Model id for the active provider, e.g. `gpt-5.4` or `claude-sonnet-4-5`."
                },
                "reasoning_effort": {
                    "type": "string",
                    "description": "Optional reasoning-effort level to apply with the model (model-specific)."
                }
            },
            "required": ["model"],
            "additionalProperties": false
        })
    }
    async fn execute(&self, arguments: Value) -> ToolExecutionResult {
        let query = match required_str_arg(&arguments, "model") {
            Ok(value) => value,
            Err(err) => return err,
        };
        let search = search_configured_models(&self.controller.settings.snapshot(), query).await;
        let current_provider = self
            .controller
            .provider
            .read()
            .expect("provider lock poisoned")
            .provider_name()
            .to_string();
        let resolved = match resolve_model_match(query, &search.matches) {
            Ok(model) => Some(model),
            Err(_) if search.matches.is_empty() => None,
            Err(message) => return ToolExecutionResult::tool_error(message),
        };
        let model_id = resolved.map_or(query, |model| model.model_id.as_str());
        let spec = match arguments.get("reasoning_effort").and_then(Value::as_str) {
            Some(effort) if !effort.trim().is_empty() => {
                format!("{model_id} {}", effort.trim())
            }
            _ => model_id.to_string(),
        };
        let result = match resolved {
            Some(model) if model.provider != current_provider => {
                self.controller
                    .change_provider(&format!("{} {spec}", model.provider))
                    .await
            }
            _ => self.controller.change_model(&spec).await,
        };
        into_tool_result(result)
    }
}

fn resolve_model_match<'a>(
    query: &str,
    matches: &'a [crate::capabilities::model_discovery::ModelSearchMatch],
) -> Result<&'a crate::capabilities::model_discovery::ModelSearchMatch, String> {
    if let Some(exact) = matches.iter().find(|candidate| candidate.model_id == query) {
        return Ok(exact);
    }
    match matches {
        [model] => Ok(model),
        [] => Err(format!(
            "No configured model matches `{query}`. Call search_models to see available models."
        )),
        _ => {
            let choices = matches
                .iter()
                .take(5)
                .map(|model| format!("{}: {}", model.provider, model.model_id))
                .collect::<Vec<_>>()
                .join(", ");
            Err(format!(
                "Multiple configured models match `{query}`. Top matches: {choices}. Pass one exact model ID to set_model."
            ))
        }
    }
}

struct SetProviderTool {
    controller: SetupController,
}

#[async_trait]
impl Tool for SetProviderTool {
    fn narrate(
        &self,
        tool_call: &ToolCall,
        phase: ToolNarrationPhase,
        locale: Option<&str>,
        _ctx: everruns_core::tool_narration::ToolNarrationContext<'_>,
    ) -> Option<String> {
        let _ = locale;
        let provider =
            arg_str(&tool_call.arguments, &["provider"]).map(|value| truncate(value, 24));
        Some(stable_labeled("Set provider", provider, phase))
    }

    fn name(&self) -> &str {
        "set_provider"
    }
    fn display_name(&self) -> Option<&str> {
        Some("Set provider")
    }
    fn description(&self) -> &str {
        "Switch the LLM provider for this session. Applies on the next turn — no restart. \
         Optionally pin a model for the new provider at the same time. Requires that provider's \
         credentials to already be configured."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "provider": {
                    "type": "string",
                    "description": "Provider name.",
                    "enum": SUPPORTED_PROVIDERS
                },
                "model": {
                    "type": "string",
                    "description": "Optional model id to select for the new provider."
                }
            },
            "required": ["provider"],
            "additionalProperties": false
        })
    }
    async fn execute(&self, arguments: Value) -> ToolExecutionResult {
        let provider = match required_str_arg(&arguments, "provider") {
            Ok(value) => value,
            Err(err) => return err,
        };
        // `change_provider` accepts `provider [model]`, switching both at once.
        let spec = match arguments.get("model").and_then(Value::as_str) {
            Some(model) if !model.trim().is_empty() => format!("{provider} {}", model.trim()),
            _ => provider.to_string(),
        };
        into_tool_result(self.controller.change_provider(&spec).await)
    }
}

impl ModelsCapability {
    /// True when no provider preference is saved and no API token is set —
    /// either via env var or in the settings file. Used by the TUI at
    /// startup to auto-open the wizard on a fresh install.
    pub(crate) fn needs_onboarding(settings: &crate::config::Settings) -> bool {
        if settings.default_provider.is_some() {
            return false;
        }
        if env_credential_present() {
            return false;
        }
        settings.tokens.is_empty() && !settings.has_codex_auth()
    }
}

fn env_credential_present() -> bool {
    const VARS: &[&str] = &[
        "OPENAI_API_KEY",
        "CODEX_ACCESS_TOKEN",
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

    // ---------- live-config tools (set_model / set_provider / set_reasoning_effort) ----------

    /// Minimal provider store for controller tests: `change_*` only ever calls
    /// `set_default_model`, which we accept; the reads are never exercised here.
    struct StubProviderStore;

    #[async_trait::async_trait]
    impl everruns_core::ProviderStore for StubProviderStore {
        async fn get_model_spec(
            &self,
            _model_id: everruns_provider::typed_id::ModelId,
        ) -> everruns_provider::error::Result<Option<everruns_provider::model_spec::ModelSpec>>
        {
            Ok(None)
        }
        async fn get_default_model_spec(
            &self,
        ) -> everruns_provider::error::Result<Option<everruns_provider::model_spec::ModelSpec>>
        {
            Ok(None)
        }
        async fn get_provider_config(
            &self,
            _provider: &everruns_provider::runtime_provider::ProviderKey,
        ) -> everruns_provider::error::Result<
            Option<everruns_provider::driver_registry::ProviderConfig>,
        > {
            // This stub owns no credentials.
            Ok(None)
        }
    }

    #[async_trait::async_trait]
    impl RuntimeProviderStore for StubProviderStore {
        async fn set_default_model_spec(
            &self,
            _model: everruns_provider::model_spec::ModelSpec,
        ) -> everruns_provider::error::Result<()> {
            Ok(())
        }
    }

    /// A controller wired to a temp settings file and the stub store, plus the
    /// shared provider handle so the test can observe live changes. The returned
    /// `TempDir` must be kept alive for the settings path to stay valid.
    fn test_controller(
        provider: ProviderChoice,
    ) -> (
        SetupController,
        Arc<RwLock<ProviderChoice>>,
        tempfile::TempDir,
    ) {
        let dir = tempfile::tempdir().expect("tempdir");
        let settings = Arc::new(SettingsStore::open(dir.path().join("settings.toml")));
        // Seed credentials in the settings file so model resolution never depends
        // on ambient env vars (other tests in this binary clear API keys). This
        // keeps the controller tests deterministic without holding the env lock.
        settings
            .set_token("openai".to_string(), "sk-test".to_string())
            .expect("seed openai token");
        settings
            .set_token("anthropic".to_string(), "sk-test".to_string())
            .expect("seed anthropic token");
        let provider = Arc::new(RwLock::new(provider));
        let controller = SetupController {
            provider: provider.clone(),
            provider_store: Arc::new(StubProviderStore),
            config: settings.clone(),
            settings,
            pending_model_choice: Arc::new(RwLock::new(None)),
        };
        (controller, provider, dir)
    }

    #[tokio::test]
    async fn set_reasoning_effort_tool_applies_live_and_validates() {
        let (controller, provider, _dir) = test_controller(ProviderChoice::OpenAi {
            model: "gpt-5.5".to_string(),
            reasoning_effort: Some("medium".to_string()),
        });
        let tool = SetReasoningEffortTool {
            controller: controller.clone(),
        };

        // Escalate: the shared handle reflects the new effort immediately.
        let result = tool.execute(json!({ "effort": "high" })).await;
        assert!(result.is_success(), "escalate: {result:?}");
        assert_eq!(provider.read().unwrap().reasoning_effort(), Some("high"));

        // A missing argument is a tool error before anything is mutated.
        let result = tool.execute(json!({})).await;
        assert!(result.is_error(), "missing effort should error");

        // An unknown level errors and leaves the prior selection intact.
        let result = tool.execute(json!({ "effort": "ludicrous" })).await;
        assert!(result.is_error(), "unknown effort should error");
        assert_eq!(provider.read().unwrap().reasoning_effort(), Some("high"));
    }

    #[tokio::test]
    async fn model_choice_is_persisted_only_after_a_successful_turn() {
        let (controller, _provider, _dir) = test_controller(ProviderChoice::OpenAi {
            model: "gpt-5.5".to_string(),
            reasoning_effort: None,
        });

        controller
            .change_model("gpt-5.4")
            .await
            .expect("change model");
        let before = controller.settings.snapshot();
        assert_eq!(before.models.get("openai"), None);
        assert_eq!(before.default_provider, None);

        let warning = SetupController::persist_pending_model_choice(
            &controller.settings,
            &controller.pending_model_choice,
        );
        assert!(warning.is_empty(), "unexpected warning: {warning}");
        let after = controller.settings.snapshot();
        assert_eq!(
            after.models.get("openai").map(String::as_str),
            Some("gpt-5.4 none")
        );
        assert_eq!(after.default_provider.as_deref(), Some("openai"));
    }

    #[test]
    fn model_match_resolution_returns_five_actionable_choices() {
        use crate::capabilities::model_discovery::ModelSearchMatch;

        let mut matches = vec![ModelSearchMatch {
            provider: "openrouter".to_string(),
            model_id: "openrouter/terra".to_string(),
            display_name: Some("Terra".to_string()),
        }];
        assert_eq!(
            resolve_model_match("terra", &matches).unwrap().model_id,
            "openrouter/terra"
        );

        for index in 2..=6 {
            matches.push(ModelSearchMatch {
                provider: format!("provider-{index}"),
                model_id: format!("terra-v{index}"),
                display_name: Some(format!("Terra v{index}")),
            });
        }
        let error = resolve_model_match("terra", &matches).unwrap_err();
        assert!(error.contains("openrouter: openrouter/terra"));
        assert!(error.contains("provider-5: terra-v5"));
        assert!(!error.contains("provider-6: terra-v6"));
        assert!(error.contains("Pass one exact model ID to set_model"));
        assert_eq!(
            resolve_model_match("terra-v2", &matches).unwrap().provider,
            "provider-2"
        );
    }

    #[tokio::test]
    async fn set_model_tool_switches_model_and_optional_effort() {
        let (controller, provider, _dir) = test_controller(ProviderChoice::OpenAi {
            model: "gpt-5.5".to_string(),
            reasoning_effort: Some("medium".to_string()),
        });
        let tool = SetModelTool { controller };

        let result = tool
            .execute(json!({ "model": "gpt-5.4", "reasoning_effort": "high" }))
            .await;
        assert!(result.is_success(), "set_model: {result:?}");
        assert_eq!(provider.read().unwrap().model_id(), "gpt-5.4");
        assert_eq!(provider.read().unwrap().reasoning_effort(), Some("high"));

        let result = tool.execute(json!({})).await;
        assert!(result.is_error(), "missing model should error");
    }

    #[test]
    fn models_capability_exposes_live_config_tools() {
        let (controller, _provider, _dir) = test_controller(ProviderChoice::Sim);
        let capability = ModelsCapability {
            provider: controller.provider.clone(),
            provider_store: controller.provider_store.clone(),
            config: controller.config.clone(),
            settings: controller.settings.clone(),
            pending_model_choice: controller.pending_model_choice.clone(),
        };
        let names: Vec<String> = capability
            .tools()
            .iter()
            .map(|t| t.name().to_string())
            .collect();
        for expected in [
            "set_reasoning_effort",
            "search_models",
            "set_model",
            "set_provider",
        ] {
            assert!(
                names.iter().any(|n| n == expected),
                "{expected} should be exposed by ModelsCapability: {names:?}"
            );
        }
        // The agent is told these tools exist so it uses them instead of asking
        // the user to type a slash command.
        let prompt = capability.system_prompt_addition().expect("setup prompt");
        assert!(prompt.contains("set_reasoning_effort"));
        assert!(prompt.contains("set_model"));
        assert!(prompt.contains("search_models"));
        assert!(prompt.contains("set_provider"));
    }

    #[tokio::test]
    async fn set_provider_tool_switches_provider_live() {
        let (controller, provider, _dir) = test_controller(ProviderChoice::Sim);
        let tool = SetProviderTool { controller };

        let result = tool.execute(json!({ "provider": "openai" })).await;
        assert!(result.is_success(), "set_provider: {result:?}");
        assert_eq!(provider.read().unwrap().provider_name(), "openai");

        let result = tool.execute(json!({ "provider": "nope" })).await;
        assert!(result.is_error(), "unknown provider should error");
    }

    #[test]
    fn needs_onboarding_true_for_empty_settings() {
        // Serialize against every other env-mutating test in this
        // binary; cf. `crate::testing::test_env`.
        let _guard = crate::testing::test_env::lock();
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
        let settings = crate::config::Settings::default();
        assert!(ModelsCapability::needs_onboarding(&settings));
    }

    #[test]
    fn needs_onboarding_ignores_custom_api_key_without_base_url() {
        // A stray CUSTOM_API_KEY is not a usable credential: without a base
        // URL the custom provider cannot run, so onboarding must still open.
        let _guard = crate::testing::test_env::lock();
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
        let settings = crate::config::Settings::default();
        assert!(ModelsCapability::needs_onboarding(&settings));

        unsafe {
            std::env::set_var("CUSTOM_BASE_URL", "http://localhost:8000/v1");
        }
        assert!(!ModelsCapability::needs_onboarding(&settings));
        unsafe {
            std::env::remove_var("CUSTOM_BASE_URL");
            std::env::remove_var("CUSTOM_API_KEY");
        }
    }

    #[test]
    fn needs_onboarding_false_when_provider_is_saved() {
        let settings = crate::config::Settings {
            default_provider: Some("anthropic".to_string()),
            ..Default::default()
        };
        assert!(!ModelsCapability::needs_onboarding(&settings));
    }

    #[test]
    fn needs_onboarding_false_when_token_is_saved() {
        let mut tokens = std::collections::BTreeMap::new();
        tokens.insert("openai".to_string(), "sk-test".to_string());
        let settings = crate::config::Settings {
            default_provider: None,
            tokens,
            ..Default::default()
        };
        assert!(!ModelsCapability::needs_onboarding(&settings));
    }

    #[test]
    fn shell_output_format_ignores_whitespace_only_streams() {
        let value = serde_json::json!({
            "stdout": "\n",
            "stderr": "   \n",
            "exit_code": 0,
        });

        assert_eq!(format_shell_output(&value), "exit 0");
    }

    #[test]
    fn environment_context_renders_requested_fields() {
        let rendered = render_environment_context(&EnvironmentContext {
            cwd: "/repo".to_string(),
            client_ui: ClientUiContext::Tui,
            shell: "zsh".to_string(),
            current_date: "2026-05-20".to_string(),
            timezone: "America/Chicago".to_string(),
            git_repo: Some("https://github.com/everruns/everruns.git".to_string()),
            git_user: Some("Chal & Yi".to_string()),
            git_email: Some("chalyi@example.com".to_string()),
            repo_root: "/repo".to_string(),
            git_current_branch: Some("feature<context>".to_string()),
            worktree_path: None,
            contributions: BTreeMap::from([
                ("sandbox_mode".to_string(), "workspace-write".to_string()),
                ("unsafe<name>".to_string(), "value & more".to_string()),
            ]),
        });

        assert!(rendered.starts_with("<environment_context>\n"));
        assert!(rendered.contains("  <cwd>/repo</cwd>\n"));
        assert!(rendered.contains("  <client_ui>TUI</client_ui>\n"));
        assert!(rendered.contains(
            "  <ui_capabilities>supports_markdown, supports_markdown_mermaid</ui_capabilities>\n"
        ));
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
        assert!(
            rendered
                .contains("  <contribution name=\"sandbox_mode\">workspace-write</contribution>\n")
        );
        assert!(rendered.contains(
            "  <contribution name=\"unsafe&lt;name&gt;\">value &amp; more</contribution>\n"
        ));
        assert!(rendered.ends_with("</environment_context>"));
    }

    /// The transcript renderer is what decides what gets drawn, so the list
    /// follows the host: yolop paints the TUI itself, an ACP editor renders the
    /// markdown its own way, and `--print` emits raw text.
    ///
    /// Mermaid over ACP is the case the additive list exists for — unlisted,
    /// because the protocol never says whether the editor draws it.
    #[test]
    fn ui_capabilities_follow_the_client_ui() {
        assert_eq!(
            ClientUiContext::Tui.ui_capabilities(),
            ["supports_markdown", "supports_markdown_mermaid"]
        );
        assert_eq!(
            ClientUiContext::Acp.ui_capabilities(),
            ["supports_markdown"]
        );
        assert!(ClientUiContext::Print.ui_capabilities().is_empty());
        assert!(ClientUiContext::None.ui_capabilities().is_empty());
    }

    /// End to end through the capability the runtime registers, so the field
    /// is proven to reach the assembled prompt rather than only the renderer.
    #[tokio::test]
    async fn environment_capability_contributes_ui_capabilities() {
        let dir = std::env::temp_dir();
        let capability = CodingCliEnvironmentCapability::new(
            dir.clone(),
            Arc::new(RwLock::new(dir)),
            ClientUiContext::Tui,
            EnvironmentContextRegistry::default(),
        );
        let ctx = everruns_core::capabilities::SystemPromptContext::without_file_store(
            everruns_provider::typed_id::SessionId::new(),
        );

        let contribution = capability
            .system_prompt_contribution(&ctx)
            .await
            .expect("environment context always contributes");

        assert!(
            contribution.contains(
                "<ui_capabilities>supports_markdown, supports_markdown_mermaid</ui_capabilities>"
            ),
            "{contribution}"
        );
    }

    /// A host that renders nothing says so, rather than dropping the field and
    /// leaving the model to guess whether it was simply not computed.
    #[test]
    fn environment_context_reports_no_ui_capabilities_for_print() {
        let rendered = render_environment_context(&EnvironmentContext {
            cwd: "/repo".to_string(),
            client_ui: ClientUiContext::Print,
            shell: "zsh".to_string(),
            current_date: "2026-05-20".to_string(),
            timezone: "America/Chicago".to_string(),
            git_repo: None,
            git_user: None,
            git_email: None,
            repo_root: "/repo".to_string(),
            git_current_branch: None,
            worktree_path: None,
            contributions: BTreeMap::new(),
        });

        assert!(rendered.contains("  <client_ui>print</client_ui>\n"));
        assert!(rendered.contains("  <ui_capabilities>none</ui_capabilities>\n"));
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

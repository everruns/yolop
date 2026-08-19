//! Extension administration shared by the standalone CLI and the running host.
//!
//! Administration deliberately is not exposed as model tools. A direct
//! foreground `yolop extensions ...` invocation can cross the attached control
//! boundary, while ordinary CLI invocations mutate global state.

use super::client::AskSink;
use super::manager::LiveProcessRegistry;
use super::package::{discover_extensions, extension_capability_id};
use super::protocol::UiAskParams;
use super::scaffold::{self, HookSpec, Language, ScaffoldRequest, ToolSpec};
use super::secrets::{ExtensionSecrets, Secret};
use super::store::{self, CrateFetcher, GitRunner, Source, SystemCrateFetcher, SystemGit};
use crate::capabilities::narration::stable_labeled;
use crate::config::SettingsStore;
use crate::control::{
    CliCapability, ControlCapability, ControlRequest, ControlResponse, ControlRoute,
};
use crate::tui::host_ui::{UiCommand, UiRequest};
use async_trait::async_trait;
use clap::{Args, CommandFactory, FromArgMatches, Parser, Subcommand};
use everruns_core::Capability;
use everruns_core::command::{
    CommandArg, CommandDescriptor, CommandExecutionContext, CommandResult, CommandSource,
    ExecuteCommandRequest,
};
use everruns_core::tool_narration::{ToolNarrationPhase, arg_str, truncate};
use everruns_core::{Tool, ToolExecutionResult};
use everruns_provider::ToolCall;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc::UnboundedSender;

pub const EXTENSIONS_CAPABILITY_ID: &str = "extensions";
const EXTENSIONS_COMMAND_NAME: &str = "extensions";

/// A constant so the shared control-plane prompt block can be measured against
/// the always-on prompt budget without constructing a session.
pub(crate) const EXTENSIONS_CONTROL_ROUTE: ControlRoute = ControlRoute {
    resource: EXTENSIONS_CAPABILITY_ID,
    cli_subcommand: EXTENSIONS_CAPABILITY_ID,
    read_only_operations: &["list"],
    summary: "install, enable, configure, reload, and scaffold extension packages",
};

#[derive(Subcommand, Debug)]
enum ExtensionCommand {
    /// List installed extensions.
    List {
        /// Emit the full machine-readable result.
        #[arg(long)]
        json: bool,
    },
    /// Install an extension without enabling it.
    Install { source: String },
    /// Remove an installed extension and its persisted enablement/secrets.
    Remove { name: String },
    /// Persist enablement and apply it to the attached session when present.
    Enable { name: String },
    /// Persist disablement and apply it to the attached session when present.
    Disable { name: String },
    /// Restart an extension server in the attached session.
    Reload { name: String },
    /// Probe an installed extension's YEP conformance.
    Doctor { name: String },
    /// Prompt for and store an extension secret.
    Secret(ExtensionSecretArgs),
    /// Create a new extension package skeleton.
    Scaffold {
        name: String,
        #[arg(long, default_value = "")]
        description: String,
        #[arg(long, default_value = "python")]
        language: String,
        #[arg(long = "tool")]
        tools: Vec<String>,
        #[arg(long = "command")]
        commands: Vec<String>,
        #[arg(long)]
        prompt: Option<String>,
        #[arg(long)]
        status: bool,
        #[arg(long)]
        skills: bool,
        #[arg(long)]
        dir: Option<PathBuf>,
    },
}

#[derive(Args, Debug)]
pub struct ExtensionSecretArgs {
    #[command(subcommand)]
    command: ExtensionSecretCommand,
}

#[derive(Subcommand, Debug)]
enum ExtensionSecretCommand {
    /// Prompt for a secret field; the value is never accepted as an argument.
    Set { name: String, field: String },
}

#[derive(Parser)]
#[command(
    name = "extensions",
    about = "Manage Yolop extensions. A direct invocation from Yolop's foreground Bash updates the current session as well as persisted state",
    disable_help_subcommand = true
)]
struct ExtensionCommandLine {
    /// Optional so a bare `yolop extensions` lists, the way `/extensions` does.
    /// Clap would otherwise treat it as a usage error: help text on stderr with
    /// exit 2, which reads as a failure for what is really a request to look.
    #[command(subcommand)]
    command: Option<ExtensionCommand>,
}

/// Version-independent extension operations carried by the internal control
/// plane. The wire envelope owns protocol versioning; this enum owns the
/// extension domain vocabulary.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum ExtensionAction {
    List {
        json: bool,
    },
    Install {
        source: String,
    },
    Remove {
        name: String,
    },
    Enable {
        name: String,
    },
    Disable {
        name: String,
    },
    Reload {
        name: String,
    },
    Doctor {
        name: String,
    },
    SetSecret {
        name: String,
        field: String,
    },
    Scaffold {
        name: String,
        description: String,
        language: String,
        tools: Vec<String>,
        commands: Vec<String>,
        prompt: Option<String>,
        status: bool,
        skills: bool,
        dir: Option<PathBuf>,
    },
}

impl ExtensionAction {
    fn from_command(command: ExtensionCommand) -> Self {
        match command {
            ExtensionCommand::List { json } => Self::List { json },
            ExtensionCommand::Install { source } => Self::Install { source },
            ExtensionCommand::Remove { name } => Self::Remove { name },
            ExtensionCommand::Enable { name } => Self::Enable { name },
            ExtensionCommand::Disable { name } => Self::Disable { name },
            ExtensionCommand::Reload { name } => Self::Reload { name },
            ExtensionCommand::Doctor { name } => Self::Doctor { name },
            ExtensionCommand::Secret(args) => match args.command {
                ExtensionSecretCommand::Set { name, field } => Self::SetSecret { name, field },
            },
            ExtensionCommand::Scaffold {
                name,
                description,
                language,
                tools,
                commands,
                prompt,
                status,
                skills,
                dir,
            } => Self::Scaffold {
                name,
                description,
                language,
                tools,
                commands,
                prompt,
                status,
                skills,
                dir,
            },
        }
    }

    fn parse_command(arguments: Option<&str>) -> Result<Self, String> {
        let arguments = arguments.unwrap_or("").trim();
        if arguments.is_empty() {
            return Ok(Self::List { json: false });
        }
        let argv = std::iter::once("extensions").chain(arguments.split_ascii_whitespace());
        ExtensionCommandLine::try_parse_from(argv)
            .map(|parsed| {
                parsed
                    .command
                    .map_or(Self::List { json: false }, Self::from_command)
            })
            .map_err(|error| error.to_string())
    }

    fn control_request(self) -> ControlRequest {
        ControlRequest::new(EXTENSIONS_CAPABILITY_ID, self)
            .expect("extension control actions are serializable")
    }

    fn verb_and_arguments(&self) -> (Verb, Value) {
        match self {
            Self::List { .. } => (Verb::List, json!({})),
            Self::Install { source } => (Verb::Install, json!({ "source": source })),
            Self::Remove { name } => (Verb::Remove, json!({ "name": name })),
            Self::Enable { name } => (Verb::Enable, json!({ "name": name })),
            Self::Disable { name } => (Verb::Disable, json!({ "name": name })),
            Self::Reload { name } => (Verb::Reload, json!({ "name": name })),
            Self::Doctor { name } => (Verb::Doctor, json!({ "name": name })),
            Self::SetSecret { name, field } => {
                (Verb::SetSecret, json!({ "name": name, "field": field }))
            }
            Self::Scaffold {
                name,
                description,
                language,
                tools,
                commands,
                prompt,
                status,
                skills,
                dir,
            } => (
                Verb::Scaffold,
                json!({
                    "name": name,
                    "description": description,
                    "language": language,
                    "tools": tools.iter().map(|name| json!({ "name": name })).collect::<Vec<_>>(),
                    "commands": commands,
                    "prompt": prompt,
                    "status": status,
                    "skills": skills,
                    "dir": dir,
                }),
            ),
        }
    }
}

pub struct ExtensionsCapability {
    extensions_dir: PathBuf,
    workspace_root: PathBuf,
    settings: Arc<SettingsStore>,
    git: Arc<dyn GitRunner>,
    crates: Arc<dyn CrateFetcher>,
    /// Live server processes, so `yolop extensions reload` can restart one in place.
    live_processes: LiveProcessRegistry,
    /// UI-command sink (the TUI's `ui_rx`) used to activate/deactivate an
    /// extension on the live session after enable/disable; `None` in
    /// `--print`/ACP, where enable/disable only persists for the next session.
    ui_tx: Option<UnboundedSender<UiRequest>>,
    /// Per-extension secret store (for `yolop extensions secret set` and the
    /// redacted config status in `yolop extensions list`). `None` in tests
    /// without secrets.
    secrets: Option<ExtensionSecrets>,
    /// Prompt surface for interactive secret entry; `None` refuses it (headless).
    ask_sink: Option<AskSink>,
}

impl ExtensionsCapability {
    pub fn new(
        extensions_dir: PathBuf,
        workspace_root: PathBuf,
        settings: Arc<SettingsStore>,
        live_processes: LiveProcessRegistry,
        ui_tx: Option<UnboundedSender<UiRequest>>,
    ) -> Self {
        Self {
            extensions_dir,
            workspace_root,
            settings,
            git: Arc::new(SystemGit),
            crates: Arc::new(SystemCrateFetcher::default()),
            live_processes,
            ui_tx,
            secrets: None,
            ask_sink: None,
        }
    }

    /// Wire the secret store so the CLI can persist credentials and report
    /// their redacted set/unset status.
    pub fn with_secrets(mut self, secrets: ExtensionSecrets) -> Self {
        self.secrets = Some(secrets);
        self
    }

    /// Wire the interactive prompt surface for secret entry (TUI only).
    pub fn with_ask_sink(mut self, ask_sink: Option<AskSink>) -> Self {
        self.ask_sink = ask_sink;
        self
    }

    fn context(&self) -> Arc<ManageCtx> {
        Arc::new(ManageCtx {
            extensions_dir: self.extensions_dir.clone(),
            workspace_root: self.workspace_root.clone(),
            settings: self.settings.clone(),
            git: self.git.clone(),
            crates: self.crates.clone(),
            live_processes: self.live_processes.clone(),
            ui_tx: self.ui_tx.clone(),
            secrets: self.secrets.clone(),
            ask_sink: self.ask_sink.clone(),
        })
    }

    pub async fn execute_action(&self, action: &ExtensionAction) -> ToolExecutionResult {
        let (verb, arguments) = action.verb_and_arguments();
        ManageTool::new(self.context(), verb)
            .execute(arguments)
            .await
    }

    #[cfg(test)]
    fn management_tools(&self) -> Vec<Box<dyn Tool>> {
        let ctx = self.context();
        [
            Verb::Scaffold,
            Verb::List,
            Verb::Install,
            Verb::Remove,
            Verb::Enable,
            Verb::Disable,
            Verb::Reload,
            Verb::SetSecret,
            Verb::Doctor,
        ]
        .into_iter()
        .map(|verb| Box::new(ManageTool::new(ctx.clone(), verb)) as Box<dyn Tool>)
        .collect()
    }
}

#[async_trait]
impl Capability for ExtensionsCapability {
    fn id(&self) -> &str {
        EXTENSIONS_CAPABILITY_ID
    }

    fn name(&self) -> &str {
        "Extensions"
    }

    fn description(&self) -> &str {
        "Extension packages served over the extension protocol. Administration is available \
         through `yolop extensions ...`."
    }

    fn category(&self) -> Option<&str> {
        Some("Extensions")
    }

    fn system_prompt_addition(&self) -> Option<&str> {
        // Every fact this block used to state — the install sources, the
        // scaffold→install→doctor→enable loop, next-session timing, and the
        // "runs third-party code, confirm the source" warning — is in the
        // `ManageTool` descriptions below. Restating it per turn bought nothing.
        None
    }

    fn tools(&self) -> Vec<Box<dyn Tool>> {
        Vec::new()
    }

    fn commands(&self) -> Vec<CommandDescriptor> {
        vec![CommandDescriptor {
            name: EXTENSIONS_COMMAND_NAME.to_string(),
            description: "Manage extension packages using the `yolop extensions` grammar."
                .to_string(),
            source: CommandSource::System,
            args: vec![CommandArg {
                name: "operation".to_string(),
                description: "CLI-style operation and arguments; omit to list extensions."
                    .to_string(),
                required: false,
                suggestions: vec![
                    "list".to_string(),
                    "enable".to_string(),
                    "disable".to_string(),
                    "reload".to_string(),
                    "doctor".to_string(),
                ],
            }],
        }]
    }

    async fn execute_command(
        &self,
        request: &ExecuteCommandRequest,
        _ctx: &CommandExecutionContext,
    ) -> everruns_provider::error::Result<CommandResult> {
        if request.name != EXTENSIONS_COMMAND_NAME {
            return Err(everruns_provider::error::AgentLoopError::config(format!(
                "{} cannot execute /{}",
                self.id(),
                request.name
            )));
        }
        let action = ExtensionAction::parse_command(request.arguments.as_deref())
            .map_err(everruns_provider::error::AgentLoopError::config)?;
        let response = ControlResponse::from_tool_result(self.execute_action(&action).await);
        Ok(CommandResult {
            success: response.ok,
            message: render_extension_response(&action, &response),
            error_code: None,
            error_fields: None,
        })
    }
}

#[async_trait]
impl ControlCapability for ExtensionsCapability {
    fn control_route(&self) -> ControlRoute {
        EXTENSIONS_CONTROL_ROUTE
    }

    async fn execute_control(&self, action: &Value) -> ToolExecutionResult {
        let action = match serde_json::from_value::<ExtensionAction>(action.clone()) {
            Ok(action) => action,
            Err(error) => {
                return ToolExecutionResult::ToolError(format!(
                    "invalid extension control action: {error}"
                ));
            }
        };
        self.execute_action(&action).await
    }

    fn render_control(&self, action: &Value, response: &ControlResponse) -> String {
        serde_json::from_value::<ExtensionAction>(action.clone())
            .map(|action| render_extension_response(&action, response))
            .unwrap_or_else(|_| response.render_default())
    }
}

#[async_trait]
impl CliCapability for ExtensionsCapability {
    fn cli_command(&self) -> clap::Command {
        ExtensionCommandLine::command()
    }

    fn control_request_from_cli(
        &self,
        matches: &clap::ArgMatches,
    ) -> anyhow::Result<ControlRequest> {
        let parsed = ExtensionCommandLine::from_arg_matches(matches)?;
        Ok(parsed
            .command
            .map_or(
                ExtensionAction::List { json: false },
                ExtensionAction::from_command,
            )
            .control_request())
    }

    async fn execute_cli(&self, request: &ControlRequest) -> anyhow::Result<()> {
        let action = serde_json::from_value::<ExtensionAction>(request.action.clone())?;
        if matches!(action, ExtensionAction::Reload { .. }) {
            anyhow::bail!(
                "extension reload requires a running Yolop session; invoke it directly through that session's Bash"
            );
        }
        let response = ControlResponse::from_tool_result(self.execute_action(&action).await);
        let rendered = self.render_control(&request.action, &response);
        if response.ok {
            println!("{rendered}");
            Ok(())
        } else {
            anyhow::bail!(rendered)
        }
    }
}

fn render_extension_response(action: &ExtensionAction, response: &ControlResponse) -> String {
    if !response.ok {
        return response.render_default();
    }
    let value = response.value.as_ref().unwrap_or(&Value::Null);
    if !matches!(action, ExtensionAction::List { json: false }) {
        return response.render_default();
    }
    let Some(items) = value.get("extensions").and_then(Value::as_array) else {
        return response.render_default();
    };
    if items.is_empty() {
        return "No extensions installed.".to_string();
    }
    let mut lines = Vec::with_capacity(items.len() + 1);
    lines.push(format!("{:<24} {:<10} {}", "NAME", "STATE", "VERSION"));
    for item in items {
        let name = item.get("name").and_then(Value::as_str).unwrap_or("?");
        let state = if item
            .get("enabled")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            "enabled"
        } else {
            "disabled"
        };
        let version = item.get("version").and_then(Value::as_str).unwrap_or("-");
        lines.push(format!("{name:<24} {state:<10} {version}"));
    }
    lines.join("\n")
}

struct ManageCtx {
    extensions_dir: PathBuf,
    workspace_root: PathBuf,
    settings: Arc<SettingsStore>,
    git: Arc<dyn GitRunner>,
    crates: Arc<dyn CrateFetcher>,
    live_processes: LiveProcessRegistry,
    ui_tx: Option<UnboundedSender<UiRequest>>,
    secrets: Option<ExtensionSecrets>,
    ask_sink: Option<AskSink>,
}

#[derive(Clone, Copy)]
enum Verb {
    Scaffold,
    List,
    Install,
    Remove,
    Enable,
    Disable,
    Reload,
    SetSecret,
    Doctor,
}

struct ManageTool {
    ctx: Arc<ManageCtx>,
    verb: Verb,
}

impl ManageTool {
    fn new(ctx: Arc<ManageCtx>, verb: Verb) -> Self {
        Self { ctx, verb }
    }

    fn scaffold(&self, args: &Value) -> ToolExecutionResult {
        let Some(name) = args.get("name").and_then(Value::as_str) else {
            return ToolExecutionResult::ToolError("`name` is required".into());
        };
        let language = match Language::parse(
            args.get("language")
                .and_then(Value::as_str)
                .unwrap_or("python"),
        ) {
            Ok(lang) => lang,
            Err(err) => return ToolExecutionResult::ToolError(err),
        };
        let tools = args
            .get("tools")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|t| {
                        let name = t.get("name").and_then(Value::as_str)?.to_string();
                        let description = t
                            .get("description")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        Some(ToolSpec { name, description })
                    })
                    .collect()
            })
            .unwrap_or_default();
        let hooks = args
            .get("hooks")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|h| {
                        let event = h.get("event").and_then(Value::as_str)?.to_string();
                        let tool_name_glob = h
                            .get("tool_name_glob")
                            .and_then(Value::as_str)
                            .unwrap_or("*")
                            .to_string();
                        Some(HookSpec {
                            event,
                            tool_name_glob,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        let prompt = args
            .get("prompt")
            .and_then(Value::as_str)
            .map(str::to_string);
        let status = args.get("status").and_then(Value::as_bool).unwrap_or(false);
        let skills = args.get("skills").and_then(Value::as_bool).unwrap_or(false);
        let commands = args
            .get("commands")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|c| c.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        // Default target: a sibling `scaffold/<name>` of the extensions dir, so
        // authored packages sit next to where they install without cluttering
        // the workspace. An explicit `dir` (a parent) overrides.
        let dir = match args.get("dir").and_then(Value::as_str) {
            Some(parent) => PathBuf::from(parent).join(name),
            None => self
                .ctx
                .extensions_dir
                .parent()
                .unwrap_or(&self.ctx.extensions_dir)
                .join("scaffold")
                .join(name),
        };
        let req = ScaffoldRequest {
            name: name.to_string(),
            description: args
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            language,
            tools,
            hooks,
            commands,
            prompt,
            status,
            skills,
            dir,
        };
        match scaffold::scaffold(&req) {
            Ok(out) => {
                // Rust needs a build step before the binary exists; the
                // zero-build templates go straight to install.
                let build_step = match &out.build {
                    Some(cmd) => format!("build it (`{cmd}`), then "),
                    None => String::new(),
                };
                ToolExecutionResult::Success(json!({
                    "scaffolded": name,
                    "dir": out.dir.display().to_string(),
                    "edit": out.edit.display().to_string(),
                    "files": out.files,
                    "build": out.build,
                    "next": format!(
                        "Edit the handler bodies in {}, then {build_step}`yolop extensions install \
                         {}` → `yolop extensions doctor {name}` → `yolop extensions enable {name}` \
                         (next session).",
                        out.edit.display(),
                        out.dir.display(),
                    ),
                }))
            }
            Err(err) => ToolExecutionResult::ToolError(format!("scaffold failed: {err}")),
        }
    }

    fn list(&self) -> ToolExecutionResult {
        let installed = discover_extensions(&self.ctx.extensions_dir);
        let settings = self.ctx.settings.snapshot();
        let items: Vec<Value> = installed
            .iter()
            .map(|pkg| {
                let cap_id = extension_capability_id(&pkg.manifest.name);
                let overrides = settings.capability_overrides_for(&cap_id);
                let enabled = overrides.iter().any(|(_, entry)| !entry.is_remove());
                let ext_config = overrides
                    .iter()
                    .rev()
                    .find(|(_, entry)| !entry.is_remove())
                    .map(|(_, entry)| entry.config.clone())
                    .unwrap_or(Value::Null);
                json!({
                    "name": pkg.manifest.name,
                    "description": pkg.manifest.description,
                    "version": pkg.manifest.version,
                    "enabled": enabled,
                    "capability_ref": cap_id,
                    "tools": pkg.manifest.tools.iter().map(|t| &t.name).collect::<Vec<_>>(),
                    // Setup fields with only their set/unset status — secret
                    // VALUES are never included (the agent-leak guard).
                    "config": self.config_status(&pkg.manifest, &ext_config),
                })
            })
            .collect();
        ToolExecutionResult::Success(json!({ "extensions": items }))
    }

    /// Per-field setup status the agent may see: name, whether it's a secret,
    /// whether it's required, and whether a value is set — never the value.
    fn config_status(
        &self,
        manifest: &super::package::ExtensionManifest,
        ext_config: &Value,
    ) -> Vec<Value> {
        manifest
            .config_fields()
            .into_iter()
            .map(|field| {
                let set = if field.secret {
                    self.ctx
                        .secrets
                        .as_ref()
                        .map(|s| s.is_set(&manifest.name, &field.name))
                        .unwrap_or(false)
                } else {
                    ext_config
                        .get(&field.name)
                        .map(|v| !v.is_null())
                        .unwrap_or(false)
                };
                json!({
                    "name": field.name,
                    "secret": field.secret,
                    "required": field.required,
                    "set": set,
                })
            })
            .collect()
    }

    async fn install(&self, args: &Value) -> ToolExecutionResult {
        let Some(spec) = args.get("source").and_then(Value::as_str) else {
            return ToolExecutionResult::ToolError("`source` is required".into());
        };
        let source = match Source::parse(spec) {
            Ok(source) => source,
            Err(err) => return ToolExecutionResult::ToolError(err.to_string()),
        };
        match store::install(
            &self.ctx.extensions_dir,
            &source,
            self.ctx.git.as_ref(),
            self.ctx.crates.as_ref(),
        )
        .await
        {
            Ok(installed) => {
                let m = &installed.manifest;
                ToolExecutionResult::Success(json!({
                    "installed": m.name,
                    "version": m.version,
                    // The contribution summary the user consents to (D4).
                    "contributes": {
                        "server_command": m.capability_server.command,
                        "tools": m.tools.iter().map(|t| &t.name).collect::<Vec<_>>(),
                        "prompt": m.prompt,
                    },
                    "content_hash": installed.content_hash,
                    "grant_changed": installed.previous_hash.is_some(),
                    "note": format!(
                        "Installed but not enabled. Run `yolop extensions enable {}` to enable it.",
                        m.name
                    ),
                }))
            }
            Err(err) => ToolExecutionResult::ToolError(format!("install failed: {err}")),
        }
    }

    fn remove(&self, args: &Value) -> ToolExecutionResult {
        let Some(name) = args.get("name").and_then(Value::as_str) else {
            return ToolExecutionResult::ToolError("`name` is required".into());
        };
        // Also drop the enabling override so a reinstall starts clean.
        let _ = self
            .ctx
            .settings
            .set_capability_enabled(&extension_capability_id(name), false);
        // And drop any stored secrets so a reinstall doesn't inherit stale ones.
        if let Some(secrets) = &self.ctx.secrets {
            let _ = secrets.clear(name);
        }
        match store::remove(&self.ctx.extensions_dir, name) {
            Ok(true) => ToolExecutionResult::Success(json!({ "removed": name })),
            Ok(false) => ToolExecutionResult::ToolError(format!("no extension named `{name}`")),
            Err(err) => ToolExecutionResult::ToolError(format!("remove failed: {err}")),
        }
    }

    async fn toggle(&self, args: &Value, enable: bool) -> ToolExecutionResult {
        let Some(name) = args.get("name").and_then(Value::as_str) else {
            return ToolExecutionResult::ToolError("`name` is required".into());
        };
        if enable
            && !discover_extensions(&self.ctx.extensions_dir)
                .iter()
                .any(|pkg| pkg.manifest.name == name)
        {
            return ToolExecutionResult::ToolError(format!(
                "no extension named `{name}` is installed; run `yolop extensions install` first"
            ));
        }
        let cap_id = extension_capability_id(name);
        match self.ctx.settings.set_capability_enabled(&cap_id, enable) {
            Ok(changed) => {
                // Persisted for next session; also apply live if the host has a
                // session to mutate (the TUI). The App answers on `ui_rx` by
                // calling activate/deactivate_capability, so the extension's
                // tools/prompt/hooks land on the next turn — no restart.
                let mut needs_restart = false;
                let live = if let Some(tx) = &self.ctx.ui_tx {
                    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                    if tx
                        .send(UiRequest {
                            command: UiCommand::SetExtensionActive {
                                capability_id: cap_id.clone(),
                                name: name.to_string(),
                                activate: enable,
                            },
                            reply: Some(reply_tx),
                        })
                        .is_err()
                    {
                        false
                    } else {
                        // The TUI event loop replies only after the runtime
                        // overlay mutation completes. This prevents a queued
                        // request from being reported as live when activation
                        // actually failed (for example, a package installed
                        // after this session registered its manifests).
                        let messages =
                            tokio::time::timeout(std::time::Duration::from_secs(10), reply_rx)
                                .await
                                .ok()
                                .and_then(Result::ok);
                        needs_restart = messages.as_ref().is_some_and(|messages| {
                            messages.iter().any(|message| {
                                message
                                    .contains(crate::tui::host_ui::EXTENSION_INSTALLED_MID_SESSION)
                            })
                        });
                        messages.is_some_and(|messages| {
                            messages.is_empty()
                                || messages.iter().any(|message| {
                                    message.contains("for this session")
                                        && message.contains("effective on the next turn")
                                })
                        })
                    }
                } else {
                    false
                };
                let note = if live {
                    "Applying to the running session now; effective on the next turn (and \
                     persisted for future sessions)."
                } else if needs_restart {
                    // The package reached disk after this session composed its
                    // capability registry, so there is nothing to activate here.
                    // Do not tell the agent to keep trying in this session.
                    "Enabled and persisted, but this extension was installed after the session \
                     started, so the running session has no server for it. It works after a \
                     yolop restart; enabling again in this session will not change that."
                } else {
                    "Takes effect on the next session."
                };
                ToolExecutionResult::Success(json!({
                    "name": name,
                    "enabled": enable,
                    "changed": changed,
                    "live": live,
                    "note": note,
                }))
            }
            Err(err) => ToolExecutionResult::ToolError(format!("config write failed: {err}")),
        }
    }

    /// Prompt the user for one setup field, honoring its kind: a `secret` is
    /// masked, an `enum` becomes a selector, otherwise free text. The value
    /// comes from the user through the ask surface — never from the agent, so it
    /// never enters the model's context. Returns the entered value (`None` on
    /// cancel/empty).
    async fn prompt_field(
        &self,
        manifest: &super::package::ExtensionManifest,
        field: &super::package::ConfigField,
    ) -> Result<Option<String>, String> {
        let Some(ask) = &self.ctx.ask_sink else {
            let hint = field
                .env
                .as_ref()
                .map(|e| format!(" Set the `{e}` environment variable instead."))
                .unwrap_or_default();
            return Err(format!(
                "interactive setup is not available in this session (headless).{hint}"
            ));
        };
        let prompt = if field.description.is_empty() {
            format!("Set `{}` for extension `{}`", field.name, manifest.name)
        } else {
            format!(
                "{} — `{}` for extension `{}`",
                field.description, field.name, manifest.name
            )
        };
        let answer = ask(UiAskParams {
            prompt,
            placeholder: field
                .secret
                .then(|| "stored securely, not shown to the agent".to_string()),
            secret: field.secret,
            options: field.options.clone(),
        })
        .await;
        if answer.cancelled || answer.answer.trim().is_empty() {
            Ok(None)
        } else {
            Ok(Some(answer.answer))
        }
    }

    /// Prompt for a `secret` field and store it in the credential store.
    async fn prompt_and_store_secret(
        &self,
        manifest: &super::package::ExtensionManifest,
        field: &super::package::ConfigField,
    ) -> Result<bool, String> {
        // Prompt first, so a headless session reports the env-var guidance
        // (from `prompt_field`) rather than a storage-availability error.
        match self.prompt_field(manifest, field).await? {
            Some(value) => {
                let Some(secrets) = self.ctx.secrets.clone() else {
                    return Err("secret storage is not available in this session".into());
                };
                secrets
                    .set(&manifest.name, &field.name, Secret::new(value))
                    .map_err(|e| format!("could not store secret: {e}"))?;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// Prompt for a non-secret field and persist it to the extension's config.
    async fn prompt_and_store_config(
        &self,
        manifest: &super::package::ExtensionManifest,
        field: &super::package::ConfigField,
    ) -> Result<bool, String> {
        match self.prompt_field(manifest, field).await? {
            Some(value) => {
                self.ctx
                    .settings
                    .set_capability_config(
                        &extension_capability_id(&manifest.name),
                        &field.name,
                        Value::String(value),
                    )
                    .map_err(|e| format!("config write failed: {e}"))?;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    async fn set_secret(&self, args: &Value) -> ToolExecutionResult {
        let Some(name) = args.get("name").and_then(Value::as_str) else {
            return ToolExecutionResult::ToolError("`name` is required".into());
        };
        let Some(field_name) = args.get("field").and_then(Value::as_str) else {
            return ToolExecutionResult::ToolError("`field` is required".into());
        };
        // A value must never be passed by the agent — that would put the secret
        // in the transcript. Refuse it explicitly.
        if args.get("value").is_some() {
            return ToolExecutionResult::ToolError(
                "do not pass `value`: secrets are entered by the user, never through the agent. \
                 Call with just `name` and `field`."
                    .into(),
            );
        }
        let Some(pkg) = discover_extensions(&self.ctx.extensions_dir)
            .into_iter()
            .find(|pkg| pkg.manifest.name == name)
        else {
            return ToolExecutionResult::ToolError(format!(
                "no extension named `{name}` is installed"
            ));
        };
        let Some(field) = pkg
            .manifest
            .secret_fields()
            .into_iter()
            .find(|f| f.name == field_name)
        else {
            return ToolExecutionResult::ToolError(format!(
                "extension `{name}` has no secret config field `{field_name}`"
            ));
        };
        match self.prompt_and_store_secret(&pkg.manifest, &field).await {
            Ok(true) => ToolExecutionResult::Success(json!({
                "name": name,
                "field": field_name,
                "set": true,
                "note": "Stored securely. Reload or restart the extension to apply it.",
            })),
            Ok(false) => ToolExecutionResult::Success(json!({
                "name": name, "field": field_name, "set": false,
                "note": "Cancelled; nothing stored.",
            })),
            Err(err) => ToolExecutionResult::ToolError(err),
        }
    }

    /// Enable, then run setup: prompt for any required field (secret, select, or
    /// text) that isn't set yet. Best-effort — with no prompt surface the
    /// extension still enables and stays inert until configured.
    async fn enable(&self, args: &Value) -> ToolExecutionResult {
        let mut result = self.toggle(args, true).await;
        if !matches!(result, ToolExecutionResult::Success(_)) {
            return result;
        }
        // Required fields the user did not supply. Enable still applies, but an
        // extension missing a required token is inert, and reporting a bare
        // success for that reads as "it works now" when it does not.
        let mut unconfigured: Vec<String> = Vec::new();
        if let Some(name) = args.get("name").and_then(Value::as_str)
            && self.ctx.ask_sink.is_some()
            && let Some(pkg) = discover_extensions(&self.ctx.extensions_dir)
                .into_iter()
                .find(|pkg| pkg.manifest.name == name)
        {
            let settings = self.ctx.settings.snapshot();
            let ext_config = settings
                .capability_overrides_for(&extension_capability_id(name))
                .iter()
                .rev()
                .find(|(_, entry)| !entry.is_remove())
                .map(|(_, entry)| entry.config.clone())
                .unwrap_or(Value::Null);
            for field in pkg.manifest.config_fields() {
                if !field.required {
                    continue;
                }
                let set = if field.secret {
                    self.ctx
                        .secrets
                        .as_ref()
                        .map(|s| s.is_set(name, &field.name))
                        .unwrap_or(false)
                } else {
                    ext_config
                        .get(&field.name)
                        .map(|v| !v.is_null())
                        .unwrap_or(false)
                };
                if set {
                    continue;
                }
                // Dispatch by field kind (secret → store; text/select → config).
                // A cancelled or failed prompt leaves enable applied, so record
                // the field rather than dropping the outcome.
                let stored = match field.kind() {
                    super::package::ConfigFieldKind::Secret => {
                        self.prompt_and_store_secret(&pkg.manifest, &field).await
                    }
                    super::package::ConfigFieldKind::Text
                    | super::package::ConfigFieldKind::Select => {
                        self.prompt_and_store_config(&pkg.manifest, &field).await
                    }
                };
                // `Ok(false)` is a cancelled prompt and `Err` a failed one;
                // only a stored value counts as configured.
                if !matches!(stored, Ok(true)) {
                    unconfigured.push(field.name.clone());
                }
            }
        }
        if !unconfigured.is_empty()
            && let ToolExecutionResult::Success(value) = &mut result
            && let Some(object) = value.as_object_mut()
        {
            let name = args.get("name").and_then(Value::as_str).unwrap_or("<name>");
            object.insert("setup_incomplete".to_string(), json!(unconfigured.clone()));
            object.insert(
                "note".to_string(),
                json!(format!(
                    "Enabled, but required setup is missing ({}), so the extension stays inert \
                     until it is provided. Ask the user to run `yolop extensions secret set \
                     {name} <field>` for a secret, or set the field with `yolop extensions \
                     enable {name}` again.",
                    unconfigured.join(", ")
                )),
            );
        }
        result
    }

    async fn reload(&self, args: &Value) -> ToolExecutionResult {
        let Some(name) = args.get("name").and_then(Value::as_str) else {
            return ToolExecutionResult::ToolError("`name` is required".into());
        };
        if !discover_extensions(&self.ctx.extensions_dir)
            .iter()
            .any(|pkg| pkg.manifest.name == name)
        {
            return ToolExecutionResult::ToolError(format!(
                "no extension named `{name}` is installed"
            ));
        }
        // Reload restarts the *running* server so implementation edits take
        // effect. The approved surface (manifest tools/prompt) is fixed for the
        // session, so a manifest change (a new tool) still needs a restart —
        // and reload can never widen the grant.
        match self.ctx.live_processes.reload(name).await {
            Some(true) => ToolExecutionResult::Success(json!({
                "name": name,
                "reloaded": true,
                "note": "Server restarted; the next call runs the current on-disk code. \
                         Manifest changes (new tools) still need a session restart.",
            })),
            Some(false) => ToolExecutionResult::Success(json!({
                "name": name,
                "reloaded": false,
                "note": "No server was running yet; the next call spawns the current code.",
            })),
            None => ToolExecutionResult::ToolError(format!(
                "extension `{name}` is not enabled this session, so it has no running server to \
                 reload; run `yolop extensions enable {name}` and restart yolop to load it"
            )),
        }
    }

    async fn doctor(&self, args: &Value) -> ToolExecutionResult {
        let Some(name) = args.get("name").and_then(Value::as_str) else {
            return ToolExecutionResult::ToolError("`name` is required".into());
        };
        let Some(pkg) = discover_extensions(&self.ctx.extensions_dir)
            .into_iter()
            .find(|pkg| pkg.manifest.name == name)
        else {
            return ToolExecutionResult::ToolError(format!(
                "no extension named `{name}` is installed"
            ));
        };
        // Bounded probe: spawn the server, handshake, check, shut down.
        let report = super::doctor::doctor(
            &pkg.manifest,
            &pkg.dir,
            &self.ctx.workspace_root,
            std::time::Duration::from_secs(20),
        )
        .await;
        ToolExecutionResult::Success(
            json!({ "name": name, "ok": report.ok, "checks": report.checks }),
        )
    }
}

#[async_trait]
impl Tool for ManageTool {
    fn narrate(
        &self,
        tool_call: &ToolCall,
        phase: ToolNarrationPhase,
        locale: Option<&str>,
        _ctx: everruns_core::tool_narration::ToolNarrationContext<'_>,
    ) -> Option<String> {
        let _ = locale;
        let label = match self.verb {
            Verb::Scaffold => "Scaffold extension",
            Verb::List => "List extensions",
            Verb::Install => "Install extension",
            Verb::Remove => "Remove extension",
            Verb::Enable => "Enable extension",
            Verb::Disable => "Disable extension",
            Verb::Reload => "Reload extension",
            Verb::SetSecret => "Set extension secret",
            Verb::Doctor => "Check extension",
        };
        let key = if matches!(self.verb, Verb::Install) {
            "source"
        } else {
            "name"
        };
        let detail = arg_str(&tool_call.arguments, &[key]).map(|value| truncate(value, 48));
        Some(stable_labeled(label, detail, phase))
    }

    fn name(&self) -> &str {
        match self.verb {
            Verb::Scaffold => "scaffold_extension",
            Verb::List => "list_extensions",
            Verb::Install => "install_extension",
            Verb::Remove => "remove_extension",
            Verb::Enable => "enable_extension",
            Verb::Disable => "disable_extension",
            Verb::Reload => "reload_extension",
            Verb::SetSecret => "set_extension_secret",
            Verb::Doctor => "doctor_extension",
        }
    }

    fn description(&self) -> &str {
        match self.verb {
            Verb::Scaffold => {
                "Scaffold a new, ready-to-edit extension package (manifest + a self-contained \
                 capability server) so you can author an extension end-to-end. Generates a \
                 correct-by-construction skeleton that installs and passes doctor out of the \
                 box; you then fill in the `handle_*` bodies. Declare what it contributes via \
                 `tools`, `hooks`, and/or `prompt`. After editing, install it with \
                 `install_extension source=<dir>`, verify with `doctor_extension`, and turn it \
                 on with `enable_extension` (effective next session)."
            }
            Verb::List => "List installed extensions and whether each is enabled.",
            Verb::Install => {
                "Install an extension from crates.io (`crates.io:yolop-extension-<name>[@ver]`, \
                 or the bare `<name>` shorthand), a git URL (`https://…[@rev]`), or a local \
                 path. crates.io installs are toolchain-free (no cargo/rustc). Runs third-party \
                 code; confirm the source with the user. Does not enable it."
            }
            Verb::Remove => "Uninstall an extension by name and drop its enable override.",
            Verb::Enable => {
                "Enable an installed extension (adds `ext:<name>` to the harness). In the TUI it \
                 is also applied to the running session immediately — its tools/prompt/hooks are \
                 live on the next turn — and persisted for future sessions. An extension \
                 installed during this session is the exception: the session's capability set is \
                 composed at startup, so that one needs a yolop restart, and the result says so."
            }
            Verb::Disable => {
                "Disable an extension without uninstalling it. Applied to the running session \
                 immediately in the TUI (if it was enabled this session) and persisted for \
                 future sessions."
            }
            Verb::Reload => {
                "Reload an enabled extension's server in place so edits to its implementation \
                 take effect this session (no yolop restart). Use after editing a running \
                 extension's server code — e.g. while iterating on one you authored. Manifest \
                 changes (adding a tool, changing a schema) still need a restart; reload never \
                 changes the approved surface."
            }
            Verb::SetSecret => {
                "Set a secret config field (e.g. an API token) for an installed extension. \
                 The user is prompted to enter the value directly — you never provide it and \
                 never see it; it is stored in the credential store, not in settings, and \
                 injected into the extension's server as env. Call with `name` and `field` \
                 only (never `value`). Use for `secret` fields shown by `list_extensions`."
            }
            Verb::Doctor => {
                "Conformance-check an installed extension: spawn its server, run the \
                 protocol handshake, and verify its tools/prompt against the manifest. \
                 Returns per-check pass/warn/fail. Use to diagnose why an extension \
                 isn't working."
            }
        }
    }

    fn parameters_schema(&self) -> Value {
        match self.verb {
            Verb::Scaffold => json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string",
                        "description": "Extension name (ascii letters, digits, `-`, `_`)." },
                    "description": { "type": "string",
                        "description": "One-line summary of what the extension does." },
                    "language": { "type": "string", "enum": ["python", "typescript", "rust"],
                        "default": "python",
                        "description": "Server language template. `python` and `typescript` \
                            (a dependency-free Node.js server) are single-file and need no build \
                            step; `rust` emits a serde_json-only crate whose binary you build \
                            into bin/ before install (the result includes the build command)." },
                    "tools": { "type": "array", "description": "Tool contributions.",
                        "items": { "type": "object", "properties": {
                            "name": { "type": "string" },
                            "description": { "type": "string" }
                        }, "required": ["name"] } },
                    "hooks": { "type": "array",
                        "description": "Lifecycle hook subscriptions. A pre_tool_use hook can \
                            block a tool call (e.g. deny git).",
                        "items": { "type": "object", "properties": {
                            "event": { "type": "string",
                                "enum": ["pre_tool_use", "post_tool_use"] },
                            "tool_name_glob": { "type": "string", "default": "*",
                                "description": "Glob over tool names to fire for." }
                        }, "required": ["event"] } },
                    "commands": { "type": "array",
                        "description": "Slash-command names the extension contributes; each is \
                            registered as /<name>:<command> and dispatched to the server.",
                        "items": { "type": "string" } },
                    "prompt": { "type": "string",
                        "description": "Static system-prompt contribution text, if any." },
                    "status": { "type": "boolean",
                        "description": "Contribute a status-bar field the server updates by \
                            pushing status/changed (e.g. a live counter)." },
                    "skills": { "type": "boolean",
                        "description": "Contribute a skills/ directory (a starter SKILL.md is \
                            generated); loaded read-only for the enabled extension." },
                    "dir": { "type": "string",
                        "description": "Parent directory to create `<name>/` under. Defaults to \
                            a `scaffold/` dir beside the extensions store." }
                },
                "required": ["name"]
            }),
            Verb::List => json!({ "type": "object", "properties": {} }),
            Verb::Install => json!({
                "type": "object",
                "properties": {
                    "source": { "type": "string",
                        "description": "`crates.io:<crate>[@ver]` (or bare `<name>` → \
                            `yolop-extension-<name>`), a git URL (optionally `@rev`), or a \
                            local directory path." }
                },
                "required": ["source"]
            }),
            Verb::SetSecret => json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Extension name." },
                    "field": { "type": "string",
                        "description": "The secret config field to set (from list_extensions)." }
                },
                "required": ["name", "field"]
            }),
            _ => json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Extension name." }
                },
                "required": ["name"]
            }),
        }
    }

    async fn execute(&self, arguments: Value) -> ToolExecutionResult {
        match self.verb {
            Verb::Scaffold => self.scaffold(&arguments),
            Verb::List => self.list(),
            Verb::Install => self.install(&arguments).await,
            Verb::Remove => self.remove(&arguments),
            Verb::Enable => self.enable(&arguments).await,
            Verb::Disable => self.toggle(&arguments, false).await,
            Verb::Reload => self.reload(&arguments).await,
            Verb::SetSecret => self.set_secret(&arguments).await,
            Verb::Doctor => self.doctor(&arguments).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extensions::package::MANIFEST_FILE;
    use std::path::Path;

    fn seed_package(dir: &Path, name: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join(MANIFEST_FILE),
            json!({
                "name": name, "description": "T.",
                "yolop": { "protocol_version": "1.0",
                    "capabilityServer": { "command": "x" }, "tools": [{"name": "t"}] }
            })
            .to_string(),
        )
        .unwrap();
    }

    fn capability(tmp: &Path) -> (ExtensionsCapability, Arc<SettingsStore>, PathBuf) {
        let ext_dir = tmp.join("extensions");
        let settings = Arc::new(SettingsStore::open(tmp.join("settings.toml")));
        let cap = ExtensionsCapability {
            extensions_dir: ext_dir.clone(),
            workspace_root: tmp.to_path_buf(),
            settings: settings.clone(),
            git: Arc::new(crate::extensions::store::SystemGit),
            crates: Arc::new(crate::extensions::store::SystemCrateFetcher::default()),
            live_processes: LiveProcessRegistry::default(),
            ui_tx: None,
            secrets: None,
            ask_sink: None,
        };
        (cap, settings, ext_dir)
    }

    #[test]
    fn management_capability_contributes_command_but_no_model_tools() {
        let tmp = tempfile::tempdir().unwrap();
        let (capability, _, _) = capability(tmp.path());
        assert!(Capability::tools(&capability).is_empty());
        let commands = Capability::commands(&capability);
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].name, "extensions");
    }

    #[test]
    fn extension_command_uses_the_cli_action_grammar() {
        assert_eq!(
            ExtensionAction::parse_command(Some("enable demo")).unwrap(),
            ExtensionAction::Enable {
                name: "demo".to_string()
            }
        );
        assert_eq!(
            ExtensionAction::parse_command(None).unwrap(),
            ExtensionAction::List { json: false }
        );
        assert!(ExtensionAction::parse_command(Some("unknown demo")).is_err());

        let tmp = tempfile::tempdir().unwrap();
        let (capability, _, _) = capability(tmp.path());
        let matches = capability
            .cli_command()
            .try_get_matches_from(["extensions", "enable", "demo"])
            .unwrap();
        let request = capability.control_request_from_cli(&matches).unwrap();
        assert_eq!(request.resource, "extensions");
        assert_eq!(request.action["operation"], "enable");
        assert_eq!(request.action["name"], "demo");
    }

    #[test]
    fn manage_tool_narration_names_action_and_extension() {
        let tmp = tempfile::tempdir().unwrap();
        let (cap, _, _) = capability(tmp.path());
        let mut tools = cap.management_tools();
        let tool = tools.remove(4);
        let call = ToolCall {
            id: "call-1".into(),
            name: "enable_extension".into(),
            arguments: json!({ "name": "linear" }),
        };

        assert_eq!(
            tool.narrate(
                &call,
                ToolNarrationPhase::Started,
                None,
                everruns_core::tool_narration::ToolNarrationContext::default(),
            ),
            Some("Enable extension: linear".into())
        );
    }

    #[tokio::test]
    async fn scaffold_then_install_flow() {
        let tmp = tempfile::tempdir().unwrap();
        let (cap, _settings, _ext_dir) = capability(tmp.path());
        let tools = cap.management_tools();
        let get = |name: &str| tools.iter().find(|t| t.name() == name).unwrap();

        // Scaffold a hook extension into an explicit parent dir.
        let parent = tmp.path().join("authored");
        let dir = match get("scaffold_extension")
            .execute(json!({
                "name": "git-guard",
                "description": "Blocks git.",
                "hooks": [{ "event": "pre_tool_use" }],
                "dir": parent.to_str().unwrap(),
            }))
            .await
        {
            ToolExecutionResult::Success(v) => {
                assert_eq!(v["scaffolded"], "git-guard");
                assert!(parent.join("git-guard").join("plugin.json").exists());
                v["dir"].as_str().unwrap().to_string()
            }
            other => panic!("{other:?}"),
        };

        // The scaffolded package installs (manifest parses, tree copied).
        match get("install_extension")
            .execute(json!({ "source": dir }))
            .await
        {
            ToolExecutionResult::Success(v) => assert_eq!(v["installed"], "git-guard"),
            other => panic!("{other:?}"),
        }
        match get("list_extensions").execute(json!({})).await {
            ToolExecutionResult::Success(v) => assert_eq!(v["extensions"][0]["name"], "git-guard"),
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn scaffold_rejects_no_contributions() {
        let tmp = tempfile::tempdir().unwrap();
        let (cap, _s, _e) = capability(tmp.path());
        let tools = cap.management_tools();
        let scaffold = tools
            .iter()
            .find(|t| t.name() == "scaffold_extension")
            .unwrap();
        match scaffold.execute(json!({ "name": "empty" })).await {
            ToolExecutionResult::ToolError(m) => assert!(m.contains("contribute"), "{m}"),
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn install_list_enable_disable_remove_flow() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        seed_package(&src, "echo");
        let (cap, settings, _ext_dir) = capability(tmp.path());
        let tools = cap.management_tools();
        let get = |name: &str| tools.iter().find(|t| t.name() == name).unwrap();

        // install
        let r = get("install_extension")
            .execute(json!({ "source": src.to_str().unwrap() }))
            .await;
        assert!(matches!(r, ToolExecutionResult::Success(_)));

        // list shows it, disabled
        match get("list_extensions").execute(json!({})).await {
            ToolExecutionResult::Success(v) => {
                assert_eq!(v["extensions"][0]["name"], "echo");
                assert_eq!(v["extensions"][0]["enabled"], false);
            }
            other => panic!("{other:?}"),
        }

        // enable writes the override
        get("enable_extension")
            .execute(json!({"name": "echo"}))
            .await;
        assert!(
            settings
                .snapshot()
                .capability_overrides_for("ext:echo")
                .iter()
                .any(|(_, e)| !e.is_remove())
        );
        // list now enabled
        match get("list_extensions").execute(json!({})).await {
            ToolExecutionResult::Success(v) => assert_eq!(v["extensions"][0]["enabled"], true),
            other => panic!("{other:?}"),
        }

        // disable drops it
        get("disable_extension")
            .execute(json!({"name": "echo"}))
            .await;
        assert!(
            settings
                .snapshot()
                .capability_overrides_for("ext:echo")
                .is_empty()
        );

        // enable on a missing extension errors
        match get("enable_extension")
            .execute(json!({"name": "ghost"}))
            .await
        {
            ToolExecutionResult::ToolError(m) => assert!(m.contains("no extension")),
            other => panic!("{other:?}"),
        }

        // remove
        match get("remove_extension")
            .execute(json!({"name": "echo"}))
            .await
        {
            ToolExecutionResult::Success(v) => assert_eq!(v["removed"], "echo"),
            other => panic!("{other:?}"),
        }
    }

    fn seed_secret_package(dir: &Path, name: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join(MANIFEST_FILE),
            json!({
                "name": name, "description": "T.",
                "yolop": { "protocol_version": "1.0",
                    "capabilityServer": { "command": "x" }, "tools": [{"name": "t"}],
                    "config_schema": { "type": "object", "required": ["token"], "properties": {
                        "token": { "type": "string", "secret": true, "env": "TOK" } } } }
            })
            .to_string(),
        )
        .unwrap();
    }

    #[tokio::test]
    async fn set_extension_secret_refuses_agent_value_and_needs_a_prompt() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        seed_secret_package(&src, "logfire");
        let (cap, _settings, _ext_dir) = capability(tmp.path());
        let tools = cap.management_tools();
        let get = |name: &str| tools.iter().find(|t| t.name() == name).unwrap();
        get("install_extension")
            .execute(json!({ "source": src.to_str().unwrap() }))
            .await;

        // A value from the agent is refused outright — secrets never enter the
        // transcript.
        match get("set_extension_secret")
            .execute(json!({ "name": "logfire", "field": "token", "value": "pylf_leak" }))
            .await
        {
            ToolExecutionResult::ToolError(m) => assert!(m.contains("do not pass `value`"), "{m}"),
            other => panic!("{other:?}"),
        }

        // Without a prompt surface (no ask_sink in this unit test), interactive
        // entry is refused with guidance — never silently no-ops.
        match get("set_extension_secret")
            .execute(json!({ "name": "logfire", "field": "token" }))
            .await
        {
            ToolExecutionResult::ToolError(m) => {
                assert!(m.contains("not available") && m.contains("TOK"), "{m}")
            }
            other => panic!("{other:?}"),
        }

        // A non-secret / unknown field is rejected.
        match get("set_extension_secret")
            .execute(json!({ "name": "logfire", "field": "nope" }))
            .await
        {
            ToolExecutionResult::ToolError(m) => {
                assert!(m.contains("no secret config field"), "{m}")
            }
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn list_reports_secret_status_never_values() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        seed_secret_package(&src, "logfire");
        let ext_dir = tmp.path().join("extensions");
        let settings = Arc::new(SettingsStore::open(tmp.path().join("settings.toml")));
        let secrets = crate::extensions::secrets::ExtensionSecrets::open_at(
            tmp.path().join("connections.toml"),
        );
        let cap = ExtensionsCapability {
            extensions_dir: ext_dir,
            workspace_root: tmp.path().to_path_buf(),
            settings,
            git: Arc::new(crate::extensions::store::SystemGit),
            crates: Arc::new(crate::extensions::store::SystemCrateFetcher::default()),
            live_processes: LiveProcessRegistry::default(),
            ui_tx: None,
            secrets: Some(secrets.clone()),
            ask_sink: None,
        };
        let tools = cap.management_tools();
        let get = |name: &str| tools.iter().find(|t| t.name() == name).unwrap();
        get("install_extension")
            .execute(json!({ "source": src.to_str().unwrap() }))
            .await;

        // Unset secret: status shows `set:false`, and no value anywhere.
        let before = match get("list_extensions").execute(json!({})).await {
            ToolExecutionResult::Success(v) => v,
            other => panic!("{other:?}"),
        };
        let token = &before["extensions"][0]["config"][0];
        assert_eq!(token["name"], "token");
        assert_eq!(token["secret"], true);
        assert_eq!(token["set"], false);

        // Store a secret, then the status flips to set — still no value shown.
        secrets
            .set(
                "logfire",
                "token",
                crate::extensions::secrets::Secret::new("pylf_v1_topsecret"),
            )
            .unwrap();
        let after = match get("list_extensions").execute(json!({})).await {
            ToolExecutionResult::Success(v) => v,
            other => panic!("{other:?}"),
        };
        assert_eq!(after["extensions"][0]["config"][0]["set"], true);
        assert!(
            !after.to_string().contains("topsecret"),
            "secret value must never appear in list output"
        );
    }

    #[tokio::test]
    async fn setup_on_enable_prompts_by_field_kind() {
        use crate::extensions::protocol::{UiAskParams, UiAskResult};
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(
            src.join(MANIFEST_FILE),
            json!({
                "name": "obs", "description": "T.",
                "yolop": { "protocol_version": "1.0",
                    "capabilityServer": { "command": "x" }, "tools": [{"name": "t"}],
                    "config_schema": { "type": "object",
                        "required": ["token", "mode", "label"],
                        "properties": {
                            "token": { "type": "string", "secret": true, "env": "TOK" },
                            "mode": { "type": "string", "enum": ["fast", "slow"] },
                            "label": { "type": "string" }
                        } } }
            })
            .to_string(),
        )
        .unwrap();

        // A scripted prompt surface: picks option[1] for selects, a fixed
        // secret for secret fields, and fixed text otherwise.
        let ask: crate::extensions::AskSink = Arc::new(|p: UiAskParams| {
            let answer = if !p.options.is_empty() {
                p.options.get(1).cloned().unwrap_or_default()
            } else if p.secret {
                "s3cr3t".to_string()
            } else {
                "typed-text".to_string()
            };
            Box::pin(async move {
                UiAskResult {
                    answer,
                    cancelled: false,
                }
            })
                as std::pin::Pin<Box<dyn std::future::Future<Output = UiAskResult> + Send>>
        });
        let settings = Arc::new(SettingsStore::open(tmp.path().join("settings.toml")));
        let secrets = crate::extensions::secrets::ExtensionSecrets::open_at(
            tmp.path().join("connections.toml"),
        );
        let cap = ExtensionsCapability {
            extensions_dir: tmp.path().join("extensions"),
            workspace_root: tmp.path().to_path_buf(),
            settings: settings.clone(),
            git: Arc::new(crate::extensions::store::SystemGit),
            crates: Arc::new(crate::extensions::store::SystemCrateFetcher::default()),
            live_processes: LiveProcessRegistry::default(),
            ui_tx: None,
            secrets: Some(secrets.clone()),
            ask_sink: Some(ask),
        };
        let tools = cap.management_tools();
        let get = |name: &str| tools.iter().find(|t| t.name() == name).unwrap();
        get("install_extension")
            .execute(json!({ "source": src.to_str().unwrap() }))
            .await;

        get("enable_extension")
            .execute(json!({ "name": "obs" }))
            .await;

        // Secret went to the credential store (redacted); text/select to config.
        assert!(secrets.is_set("obs", "token"));
        let snap = settings.snapshot();
        let config = snap
            .capability_overrides_for("ext:obs")
            .iter()
            .rev()
            .find(|(_, e)| !e.is_remove())
            .map(|(_, e)| e.config.clone())
            .unwrap();
        assert_eq!(config["mode"], "slow"); // selector picked option[1]
        assert_eq!(config["label"], "typed-text");
        // The secret is never written to settings config.
        assert!(config.get("token").is_none());
    }

    #[tokio::test]
    async fn reload_reports_when_nothing_is_live() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        seed_package(&src, "echo");
        let (cap, _settings, _ext_dir) = capability(tmp.path());
        let tools = cap.management_tools();
        let get = |name: &str| tools.iter().find(|t| t.name() == name).unwrap();
        get("install_extension")
            .execute(json!({ "source": src.to_str().unwrap() }))
            .await;

        // Unknown extension is rejected outright.
        match get("reload_extension")
            .execute(json!({"name": "ghost"}))
            .await
        {
            ToolExecutionResult::ToolError(m) => assert!(m.contains("no extension"), "{m}"),
            other => panic!("{other:?}"),
        }

        // Installed but with no running server this session (the management
        // capability's registry is empty in this unit test): reload explains
        // it isn't live rather than silently succeeding.
        match get("reload_extension")
            .execute(json!({"name": "echo"}))
            .await
        {
            ToolExecutionResult::ToolError(m) => assert!(m.contains("not enabled"), "{m}"),
            other => panic!("{other:?}"),
        }
    }

    /// An extension installed mid-session cannot activate: the runtime composes
    /// its capability registry at startup, so `activate_capability` reports an
    /// unknown id. The result must name that, not "takes effect on the next
    /// session", which invites the agent to keep enabling in this one.
    #[tokio::test]
    async fn enable_reports_a_restart_when_the_package_arrived_mid_session() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        seed_package(&src, "echo");
        let ext_dir = tmp.path().join("extensions");
        let settings = Arc::new(SettingsStore::open(tmp.path().join("settings.toml")));
        let (ui_tx, mut ui_rx) = tokio::sync::mpsc::unbounded_channel::<UiRequest>();
        let cap = ExtensionsCapability {
            extensions_dir: ext_dir,
            workspace_root: tmp.path().to_path_buf(),
            settings,
            git: Arc::new(crate::extensions::store::SystemGit),
            crates: Arc::new(crate::extensions::store::SystemCrateFetcher::default()),
            live_processes: LiveProcessRegistry::default(),
            ui_tx: Some(ui_tx),
            secrets: None,
            ask_sink: None,
        };
        let tools = cap.management_tools();
        let get = |name: &str| tools.iter().find(|t| t.name() == name).unwrap();
        get("install_extension")
            .execute(json!({ "source": src.to_str().unwrap() }))
            .await;

        // Stand in for the App answering an activation it cannot perform.
        let ui = tokio::spawn(async move {
            let request = ui_rx.recv().await.unwrap();
            request
                .reply
                .unwrap()
                .send(vec![format!(
                    "extension `echo` is enabled for future sessions, but it was {}, so this \
                     session has no server for it. Restart yolop to use it now.",
                    crate::tui::host_ui::EXTENSION_INSTALLED_MID_SESSION
                )])
                .unwrap();
        });

        let result = get("enable_extension")
            .execute(json!({ "name": "echo" }))
            .await;
        ui.await.unwrap();

        let ToolExecutionResult::Success(value) = result else {
            panic!("enable should still persist");
        };
        assert_eq!(value["live"], json!(false));
        let note = value["note"].as_str().expect("note");
        assert!(
            note.contains("installed after the session started"),
            "{note}"
        );
        assert!(note.contains("restart"), "{note}");
        assert!(
            !note.contains("Takes effect on the next session"),
            "the next-session wording hides that a restart is required: {note}"
        );
    }

    #[tokio::test]
    async fn enable_disable_emit_live_activation_when_wired() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        seed_package(&src, "echo");
        let ext_dir = tmp.path().join("extensions");
        let settings = Arc::new(SettingsStore::open(tmp.path().join("settings.toml")));
        let (ui_tx, mut ui_rx) = tokio::sync::mpsc::unbounded_channel::<UiRequest>();
        let cap = ExtensionsCapability {
            extensions_dir: ext_dir,
            workspace_root: tmp.path().to_path_buf(),
            settings,
            git: Arc::new(crate::extensions::store::SystemGit),
            crates: Arc::new(crate::extensions::store::SystemCrateFetcher::default()),
            live_processes: LiveProcessRegistry::default(),
            ui_tx: Some(ui_tx),
            secrets: None,
            ask_sink: None,
        };
        let tools = cap.management_tools();
        let get = |name: &str| tools.iter().find(|t| t.name() == name).unwrap();
        get("install_extension")
            .execute(json!({ "source": src.to_str().unwrap() }))
            .await;

        let ui = tokio::spawn(async move {
            let mut commands = Vec::new();
            for _ in 0..2 {
                let request = ui_rx.recv().await.unwrap();
                commands.push(request.command);
                request
                    .reply
                    .unwrap()
                    .send(vec![
                        "extension applied for this session; effective on the next turn.".into(),
                    ])
                    .unwrap();
            }
            commands
        });

        // enable persists AND waits for confirmed live activation from the UI.
        match get("enable_extension")
            .execute(json!({"name": "echo"}))
            .await
        {
            ToolExecutionResult::Success(v) => assert_eq!(v["live"], true),
            other => panic!("{other:?}"),
        }

        // disable signals deactivation.
        match get("disable_extension")
            .execute(json!({"name": "echo"}))
            .await
        {
            ToolExecutionResult::Success(v) => assert_eq!(v["live"], true),
            other => panic!("{other:?}"),
        }
        let commands = ui.await.unwrap();
        assert_eq!(
            commands,
            vec![
                UiCommand::SetExtensionActive {
                    capability_id: "ext:echo".into(),
                    name: "echo".into(),
                    activate: true,
                },
                UiCommand::SetExtensionActive {
                    capability_id: "ext:echo".into(),
                    name: "echo".into(),
                    activate: false,
                },
            ]
        );
    }
}

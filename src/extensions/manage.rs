//! The `extensions` capability: the management surface for installing,
//! listing, enabling, and removing extension packages. Distinct from the
//! per-package `ext:<name>` capabilities — this one is always on the default
//! harness (like `connectors`) and owns the verbs. Tools so both the user
//! and the model can drive setup; a thin `/extensions` command mirrors them.

use super::client::AskSink;
use super::manager::LiveProcessRegistry;
use super::package::{discover_extensions, extension_capability_id};
use super::protocol::UiAskParams;
use super::scaffold::{self, HookSpec, Language, ScaffoldRequest, ToolSpec};
use super::secrets::{ExtensionSecrets, Secret};
use super::store::{self, CrateFetcher, GitRunner, Source, SystemCrateFetcher, SystemGit};
use crate::config::SettingsStore;
use crate::tui::host_ui::UiCommand;
use async_trait::async_trait;
use everruns_core::capabilities::Capability;
use everruns_core::tools::{Tool, ToolExecutionResult};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc::UnboundedSender;

pub const EXTENSIONS_CAPABILITY_ID: &str = "extensions";

pub struct ExtensionsCapability {
    extensions_dir: PathBuf,
    workspace_root: PathBuf,
    settings: Arc<SettingsStore>,
    git: Arc<dyn GitRunner>,
    crates: Arc<dyn CrateFetcher>,
    /// Live server processes, so `reload_extension` can restart one in place.
    live_processes: LiveProcessRegistry,
    /// UI-command sink (the TUI's `ui_rx`) used to activate/deactivate an
    /// extension on the live session after enable/disable; `None` in
    /// `--print`/ACP, where enable/disable only persists for the next session.
    ui_tx: Option<UnboundedSender<UiCommand>>,
    /// Per-extension secret store (for `set_extension_secret` and the redacted
    /// config status in `list_extensions`). `None` in tests without secrets.
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
        ui_tx: Option<UnboundedSender<UiCommand>>,
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

    /// Wire the secret store so `set_extension_secret` can persist credentials
    /// and `list_extensions` can report their (redacted) set/unset status.
    pub fn with_secrets(mut self, secrets: ExtensionSecrets) -> Self {
        self.secrets = Some(secrets);
        self
    }

    /// Wire the interactive prompt surface for secret entry (TUI only).
    pub fn with_ask_sink(mut self, ask_sink: Option<AskSink>) -> Self {
        self.ask_sink = ask_sink;
        self
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
        "Install, list, enable, and remove yolop extensions — capability-level \
         packages served over the extension protocol."
    }

    fn category(&self) -> Option<&str> {
        Some("Extensions")
    }

    fn system_prompt_addition(&self) -> Option<&str> {
        Some(
            "Extensions are installable capability packages. Use `list_extensions` to see what is \
             installed and enabled, `install_extension` for a crates.io crate \
             (`crates.io:yolop-extension-<name>`), git URL, or local path, \
             `enable_extension`/`disable_extension` to toggle one (applied to the running \
             session immediately in the TUI — effective on the next turn — and persisted for \
             future sessions), `reload_extension` to restart an enabled extension's server in \
             place after editing its code, and `remove_extension` to uninstall. To build a NEW \
             yourself, `scaffold_extension` generates a ready-to-edit package (manifest + \
             capability server) — declare what it contributes via `tools`, `hooks`, and/or \
             `prompt` — then edit the generated `handle_*` bodies, `install_extension \
             source=<dir>`, `doctor_extension` to verify, and `enable_extension`. Installing \
             runs third-party code on the user's machine — confirm the source with the user \
             first.",
        )
    }

    fn tools(&self) -> Vec<Box<dyn Tool>> {
        let ctx = Arc::new(ManageCtx {
            extensions_dir: self.extensions_dir.clone(),
            workspace_root: self.workspace_root.clone(),
            settings: self.settings.clone(),
            git: self.git.clone(),
            crates: self.crates.clone(),
            live_processes: self.live_processes.clone(),
            ui_tx: self.ui_tx.clone(),
            secrets: self.secrets.clone(),
            ask_sink: self.ask_sink.clone(),
        });
        vec![
            Box::new(ManageTool::new(ctx.clone(), Verb::Scaffold)),
            Box::new(ManageTool::new(ctx.clone(), Verb::List)),
            Box::new(ManageTool::new(ctx.clone(), Verb::Install)),
            Box::new(ManageTool::new(ctx.clone(), Verb::Remove)),
            Box::new(ManageTool::new(ctx.clone(), Verb::Enable)),
            Box::new(ManageTool::new(ctx.clone(), Verb::Disable)),
            Box::new(ManageTool::new(ctx.clone(), Verb::Reload)),
            Box::new(ManageTool::new(ctx.clone(), Verb::SetSecret)),
            Box::new(ManageTool::new(ctx, Verb::Doctor)),
        ]
    }
}

struct ManageCtx {
    extensions_dir: PathBuf,
    workspace_root: PathBuf,
    settings: Arc<SettingsStore>,
    git: Arc<dyn GitRunner>,
    crates: Arc<dyn CrateFetcher>,
    live_processes: LiveProcessRegistry,
    ui_tx: Option<UnboundedSender<UiCommand>>,
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
                        "Edit the handler bodies in {}, then {build_step}install_extension \
                         source={} → doctor_extension name={name} → enable_extension name={name} \
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
                        "Installed but not enabled. Run enable_extension name={} to add it to the harness.",
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

    fn toggle(&self, args: &Value, enable: bool) -> ToolExecutionResult {
        let Some(name) = args.get("name").and_then(Value::as_str) else {
            return ToolExecutionResult::ToolError("`name` is required".into());
        };
        if enable
            && !discover_extensions(&self.ctx.extensions_dir)
                .iter()
                .any(|pkg| pkg.manifest.name == name)
        {
            return ToolExecutionResult::ToolError(format!(
                "no extension named `{name}` is installed; install_extension first"
            ));
        }
        let cap_id = extension_capability_id(name);
        match self.ctx.settings.set_capability_enabled(&cap_id, enable) {
            Ok(changed) => {
                // Persisted for next session; also apply live if the host has a
                // session to mutate (the TUI). The App answers on `ui_rx` by
                // calling activate/deactivate_capability, so the extension's
                // tools/prompt/hooks land on the next turn — no restart.
                let live = self
                    .ctx
                    .ui_tx
                    .as_ref()
                    .map(|tx| {
                        tx.send(UiCommand::SetExtensionActive {
                            capability_id: cap_id.clone(),
                            name: name.to_string(),
                            activate: enable,
                        })
                        .is_ok()
                    })
                    .unwrap_or(false);
                let note = if live {
                    "Applying to the running session now; effective on the next turn (and \
                     persisted for future sessions)."
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
        let result = self.toggle(args, true);
        if !matches!(result, ToolExecutionResult::Success(_)) {
            return result;
        }
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
                // Ignore prompt failures/cancellations: enable already applied.
                let _ = match field.kind() {
                    super::package::ConfigFieldKind::Secret => {
                        self.prompt_and_store_secret(&pkg.manifest, &field).await
                    }
                    super::package::ConfigFieldKind::Text
                    | super::package::ConfigFieldKind::Select => {
                        self.prompt_and_store_config(&pkg.manifest, &field).await
                    }
                };
            }
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
                 reload; enable_extension name={name} and restart yolop to load it"
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
                 live on the next turn — and persisted for future sessions."
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
            Verb::Disable => self.toggle(&arguments, false),
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

    #[tokio::test]
    async fn scaffold_then_install_flow() {
        let tmp = tempfile::tempdir().unwrap();
        let (cap, _settings, _ext_dir) = capability(tmp.path());
        let tools = cap.tools();
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
        let tools = cap.tools();
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
        let tools = cap.tools();
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
        let tools = cap.tools();
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
        let tools = cap.tools();
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
        let tools = cap.tools();
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
        let tools = cap.tools();
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

    #[tokio::test]
    async fn enable_disable_emit_live_activation_when_wired() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        seed_package(&src, "echo");
        let ext_dir = tmp.path().join("extensions");
        let settings = Arc::new(SettingsStore::open(tmp.path().join("settings.toml")));
        let (ui_tx, mut ui_rx) = tokio::sync::mpsc::unbounded_channel::<UiCommand>();
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
        let tools = cap.tools();
        let get = |name: &str| tools.iter().find(|t| t.name() == name).unwrap();
        get("install_extension")
            .execute(json!({ "source": src.to_str().unwrap() }))
            .await;

        // enable persists AND signals live activation on the UI channel.
        match get("enable_extension")
            .execute(json!({"name": "echo"}))
            .await
        {
            ToolExecutionResult::Success(v) => assert_eq!(v["live"], true),
            other => panic!("{other:?}"),
        }
        assert_eq!(
            ui_rx.try_recv().unwrap(),
            UiCommand::SetExtensionActive {
                capability_id: "ext:echo".into(),
                name: "echo".into(),
                activate: true,
            }
        );

        // disable signals deactivation.
        get("disable_extension")
            .execute(json!({"name": "echo"}))
            .await;
        assert_eq!(
            ui_rx.try_recv().unwrap(),
            UiCommand::SetExtensionActive {
                capability_id: "ext:echo".into(),
                name: "echo".into(),
                activate: false,
            }
        );
    }
}

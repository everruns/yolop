// The `config` capability owns the attached model-selection command.
//
// It delegates model-list actions to `ModelListCapability`, which keeps the
// shared control route and persistence behavior without registering a separate
// top-level CLI command. The legacy configuration tool implementations remain
// internal for focused tests, but this capability advertises no agent tools.

use crate::capabilities::model_list::{ModelListAction, ModelListCapability, ModelsCliCommand};
use crate::capabilities::narration::{narrate_get_config, narrate_set_config};
use crate::config::capability_settings::{
    CapabilityCatalog, apply_capability_settings, build_capability_override,
    capability_catalog_json, capability_catalog_list, effective_harness_json, overrides_to_json,
    parse_override_from_json, stored_override_json,
};
use crate::config::schema::{KeyTarget, ValueKind, known_keys, parse_key, schema};
use crate::config::service::{ConfigService, current_value, scoped_current};
use crate::config::{ApprovalMode, Settings, SettingsStore};
use crate::control::{
    CliCapability, ControlCapability, ControlRequest, ControlResponse, ControlRoute,
};
use crate::runtime::{SUPPORTED_PROVIDERS, coding_harness_defaults, resolve_for_settings};
use async_trait::async_trait;
use clap::{Args, Command, FromArgMatches, Subcommand};
use everruns_core::tool_narration::ToolNarrationPhase;
use everruns_core::{Capability, CapabilityStatus, SystemPromptContext};
use everruns_core::{Tool, ToolExecutionResult};
use everruns_provider::ToolCall;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;

#[derive(Debug, Args)]
pub(crate) struct ConfigCommandLine {
    #[command(subcommand)]
    command: ConfigCommand,
}

#[derive(Debug, Subcommand)]
enum ConfigCommand {
    Get {
        key: Option<String>,
    },
    Set {
        key: String,
        value: Option<String>,
        #[arg(long, value_name = "OBJECT", conflicts_with = "value")]
        json: Option<String>,
    },
    Clear {
        key: String,
    },
    Model {
        #[command(subcommand)]
        command: ConfigModelCommand,
    },
    Models {
        #[command(subcommand)]
        command: Option<ModelsCliCommand>,
    },
}

#[derive(Debug, Subcommand)]
enum ConfigModelCommand {
    Show,
    Set { model: String },
    Clear,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum ConfigAction {
    Get {
        key: Option<String>,
    },
    Set {
        key: String,
        value: Option<String>,
        json: Option<Value>,
    },
    Clear {
        key: String,
    },
    ModelShow,
    ModelSet {
        model: String,
    },
    ModelClear,
}

impl ConfigCommandLine {
    fn request(matches: &clap::ArgMatches) -> anyhow::Result<ControlRequest> {
        let command = Self::from_arg_matches(matches)?.command;
        let action = match command {
            ConfigCommand::Get { key } => serde_json::to_value(ConfigAction::Get { key })?,
            ConfigCommand::Set { key, value, json } => {
                if value.is_none() && json.is_none() {
                    anyhow::bail!("config set requires VALUE or --json OBJECT");
                }
                let json = json
                    .map(|raw| {
                        serde_json::from_str::<Value>(&raw)
                            .map_err(|error| anyhow::anyhow!("invalid --json value: {error}"))
                    })
                    .transpose()?;
                serde_json::to_value(ConfigAction::Set { key, value, json })?
            }
            ConfigCommand::Clear { key } => serde_json::to_value(ConfigAction::Clear { key })?,
            ConfigCommand::Model { command } => match command {
                ConfigModelCommand::Show => serde_json::to_value(ConfigAction::ModelShow)?,
                ConfigModelCommand::Set { model } => {
                    serde_json::to_value(ConfigAction::ModelSet { model })?
                }
                ConfigModelCommand::Clear => serde_json::to_value(ConfigAction::ModelClear)?,
            },
            ConfigCommand::Models { command } => serde_json::to_value(match command {
                Some(command) => ModelListAction::from(command),
                None => ModelListAction::List {
                    json: false,
                    connected: false,
                },
            })?,
        };
        Ok(ControlRequest::new("models", action)?)
    }
}

pub(crate) const CONFIG_CAPABILITY_ID: &str = "yolop_config";

pub(crate) struct ConfigCapability {
    pub(crate) settings: Arc<SettingsStore>,
    pub(crate) catalog: Arc<CapabilityCatalog>,
    pub(crate) model_list: Arc<ModelListCapability>,
}

#[async_trait]
impl ControlCapability for ConfigCapability {
    fn control_route(&self) -> ControlRoute {
        self.model_list.control_route()
    }

    async fn execute_control(&self, action: &Value) -> ToolExecutionResult {
        match serde_json::from_value::<ConfigAction>(action.clone()) {
            Ok(action) => self.execute_config_action(action).await,
            Err(_) => self.model_list.execute_control(action).await,
        }
    }

    fn render_control(&self, action: &Value, response: &ControlResponse) -> String {
        if serde_json::from_value::<ConfigAction>(action.clone()).is_ok() {
            response.render_default()
        } else {
            self.model_list.render_control(action, response)
        }
    }
}

#[async_trait]
impl CliCapability for ConfigCapability {
    fn cli_command(&self) -> clap::Command {
        ConfigCommandLine::augment_args(Command::new("config"))
    }

    fn control_request_from_cli(
        &self,
        matches: &clap::ArgMatches,
    ) -> anyhow::Result<ControlRequest> {
        ConfigCommandLine::request(matches)
    }

    async fn execute_cli(&self, request: &ControlRequest) -> anyhow::Result<()> {
        if serde_json::from_value::<ConfigAction>(request.action.clone()).is_err() {
            return self.model_list.execute_cli_request(request).await;
        }
        let response =
            ControlResponse::from_tool_result(self.execute_control(&request.action).await);
        let rendered = self.render_control(&request.action, &response);
        if response.ok {
            println!("{rendered}");
            Ok(())
        } else {
            anyhow::bail!(rendered)
        }
    }
}

#[async_trait]
impl Capability for ConfigCapability {
    fn id(&self) -> &str {
        CONFIG_CAPABILITY_ID
    }
    fn name(&self) -> &str {
        "Configuration"
    }
    fn description(&self) -> &str {
        "Schema-described, human-friendly editing of yolop's settings file."
    }
    fn status(&self) -> CapabilityStatus {
        CapabilityStatus::Available
    }
    fn category(&self) -> Option<&str> {
        Some("Personalization")
    }

    async fn system_prompt_contribution(&self, _ctx: &SystemPromptContext) -> Option<String> {
        None
    }

    fn system_prompt_preview(&self) -> Option<String> {
        None
    }

    fn tools(&self) -> Vec<Box<dyn Tool>> {
        Vec::new()
    }
}

impl ConfigCapability {
    #[allow(dead_code)]
    fn legacy_tools(&self) -> Vec<Box<dyn Tool>> {
        vec![
            Box::new(GetConfigTool {
                settings: self.settings.clone(),
                catalog: self.catalog.clone(),
            }),
            Box::new(SetConfigTool {
                settings: self.settings.clone(),
                catalog: self.catalog.clone(),
            }),
        ]
    }

    async fn execute_config_action(&self, action: ConfigAction) -> ToolExecutionResult {
        match action {
            ConfigAction::Get { key } => {
                GetConfigTool {
                    settings: self.settings.clone(),
                    catalog: self.catalog.clone(),
                }
                .execute(json!({ "key": key }))
                .await
            }
            ConfigAction::Set { key, value, json } => {
                let mut arguments = json!({ "key": key });
                if let Some(value) = value {
                    arguments["value"] = parse_cli_value(&value);
                }
                if let Some(json) = json {
                    arguments["json"] = json;
                }
                SetConfigTool {
                    settings: self.settings.clone(),
                    catalog: self.catalog.clone(),
                }
                .execute(arguments)
                .await
            }
            ConfigAction::Clear { key } => {
                SetConfigTool {
                    settings: self.settings.clone(),
                    catalog: self.catalog.clone(),
                }
                .execute(json!({ "key": key, "value": "clear" }))
                .await
            }
            ConfigAction::ModelShow => {
                let settings = self.settings.snapshot();
                let model = settings.default_provider.as_ref().and_then(|provider| {
                    settings
                        .default_models
                        .get(provider)
                        .map(|model| format!("{provider}/{model}"))
                });
                ToolExecutionResult::success(json!({ "model": model }))
            }
            ConfigAction::ModelSet { model } => {
                let models = self.settings.snapshot().model_list();
                let index = match ModelListCapability::find(&models, &model) {
                    Ok(index) => index,
                    Err(error) => return ToolExecutionResult::tool_error(error),
                };
                let entry = &models[index];
                if let Err(error) = self
                    .settings
                    .set_configured_model(entry.provider.clone(), entry.model.clone())
                {
                    return ToolExecutionResult::tool_error(error.to_string());
                }
                ToolExecutionResult::success(
                    json!({ "model": format!("{}/{}", entry.provider, entry.model) }),
                )
            }
            ConfigAction::ModelClear => {
                if let Err(error) = self.settings.clear_configured_model() {
                    return ToolExecutionResult::tool_error(error.to_string());
                }
                ToolExecutionResult::success(json!({ "model": Value::Null }))
            }
        }
    }
}

fn parse_cli_value(value: &str) -> Value {
    serde_json::from_str(value).unwrap_or_else(|_| Value::String(value.to_string()))
}

// ---------- field rendering ----------
//
// The per-target read helpers (`current_value`, `scoped_current`) live in
// `crate::config::service` so any capability can reuse them through the
// `ConfigService`; here we only assemble the schema-described field view.

/// JSON description of a schema field, optionally with its current value(s).
fn field_json(settings: &Settings, field: &crate::config::schema::ConfigField) -> Value {
    let current = if field.key == "capabilities" {
        overrides_to_json(&settings.capabilities)
    } else if field.provider_scoped {
        scoped_current(settings, field.key)
    } else {
        // Scalar fields map 1:1 to a target keyed by `field.key`.
        let target = parse_key(field.key).expect("schema key parses");
        current_value(settings, &target)
    };
    json!({
        "key": field.key,
        "aliases": field.aliases,
        "title": field.title,
        "description": field.description,
        "type": field.kind.as_str(),
        "secret": field.kind == ValueKind::Secret,
        "provider_scoped": field.provider_scoped,
        "default": field.default,
        "examples": field.examples,
        "current": current,
    })
}

// ---------- get_config ----------

struct GetConfigTool {
    settings: Arc<SettingsStore>,
    catalog: Arc<CapabilityCatalog>,
}

#[async_trait]
impl Tool for GetConfigTool {
    fn narrate(
        &self,
        tool_call: &ToolCall,
        phase: ToolNarrationPhase,
        locale: Option<&str>,
        _ctx: everruns_core::tool_narration::ToolNarrationContext<'_>,
    ) -> Option<String> {
        let _ = locale;
        Some(narrate_get_config(tool_call, phase))
    }

    fn name(&self) -> &str {
        "get_config"
    }
    fn display_name(&self) -> Option<&str> {
        Some("Get config")
    }
    fn description(&self) -> &str {
        "Inspect yolop configuration. With no `key`, returns every configuration key with its \
         title, description, type, default, examples, and current value (secrets redacted). \
         With a `key`, returns just that entry. Use `key=capabilities` for the full \
         registered catalog plus stored overrides, or `key=capabilities.<ref>` for one \
         capability's schema metadata."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "key": {
                    "type": "string",
                    "description": "Optional single key to describe, e.g. `default_provider` or `models.anthropic`."
                }
            },
            "additionalProperties": false
        })
    }
    async fn execute(&self, arguments: Value) -> ToolExecutionResult {
        let settings = self.settings.snapshot();
        let path = self.settings.path().display().to_string();
        let active_profile = self.settings.active_profile_name();
        let profile_path = self
            .settings
            .active_profile_path()
            .map(|path| path.display().to_string());

        if let Some(key) = arguments.get("key").and_then(Value::as_str) {
            let key = key.trim();
            if !key.is_empty() {
                let target = match parse_key(key) {
                    Ok(t) => t,
                    Err(err) => return ToolExecutionResult::tool_error(err),
                };
                return match &target {
                    KeyTarget::Capabilities => {
                        let defaults = coding_harness_defaults(false);
                        let effective = apply_capability_settings(defaults, &settings.capabilities);
                        let field = target.field();
                        ToolExecutionResult::success(json!({
                            "settings_path": path,
                            "active_profile": active_profile,
                            "profile_path": profile_path,
                            "field": field_json(&settings, field),
                            "catalog": capability_catalog_list(&self.catalog),
                            "stored_overrides": overrides_to_json(&settings.capabilities),
                            "effective_harness": effective_harness_json(&effective),
                            "note": "Use `catalog` for registered refs and schema metadata; \
                                     `capabilities.<ref>` narrows to one entry. Append with \
                                     `set_config key=capabilities json=…`; `value=clear` drops all overrides.",
                        }))
                    }
                    KeyTarget::CapabilityRef(cap_ref) => {
                        let defaults = coding_harness_defaults(false);
                        let effective = apply_capability_settings(defaults, &settings.capabilities);
                        let catalog = match capability_catalog_json(&self.catalog, cap_ref) {
                            Ok(entry) => entry,
                            Err(err) => return ToolExecutionResult::tool_error(err),
                        };
                        let stored: Vec<Value> = settings
                            .capability_overrides_for(cap_ref)
                            .into_iter()
                            .map(|(index, entry)| stored_override_json(index, entry))
                            .collect();
                        let effective_for_id: Vec<Value> = effective
                            .iter()
                            .enumerate()
                            .filter(|(_, cap)| cap.capability_id() == cap_ref)
                            .map(|(index, cap)| {
                                json!({
                                    "index": index,
                                    "ref": cap.capability_id(),
                                    "config": cap.config_value(),
                                })
                            })
                            .collect();
                        let field = target.field();
                        ToolExecutionResult::success(json!({
                            "settings_path": path,
                            "active_profile": active_profile,
                            "profile_path": profile_path,
                            "field": field_json(&settings, field),
                            "capability": catalog,
                            "stored_overrides": stored,
                            "effective_instances": effective_for_id,
                        }))
                    }
                    _ => {
                        let field = target.field();
                        let mut entry = field_json(&settings, field);
                        let value = self.settings.current(key).unwrap_or(Value::Null);
                        if field.provider_scoped
                            && field.key != "capabilities"
                            && let Value::Object(map) = &mut entry
                        {
                            let table = map.get("current").cloned().unwrap_or(Value::Null);
                            map.insert("table".to_string(), table);
                            map.insert("key".to_string(), Value::String(key.to_string()));
                        }
                        entry["current"] = value;
                        ToolExecutionResult::success(json!({
                            "settings_path": path,
                            "active_profile": active_profile,
                            "profile_path": profile_path,
                            "source": self.settings.source_for(&target),
                            "field": entry,
                        }))
                    }
                };
            }
        }

        let fields: Vec<Value> = schema().iter().map(|f| field_json(&settings, f)).collect();
        ToolExecutionResult::success(json!({
            "settings_path": path,
            "active_profile": active_profile,
            "profile_path": profile_path,
            "fields": fields,
            "note": "Set any key with `set_config`. Harness overrides: `set_config key=capabilities json=…`. \
                     Provider/model edits apply on the next run; use /setup to switch the live model now.",
        }))
    }
}

// ---------- set_config ----------

struct SetConfigTool {
    settings: Arc<SettingsStore>,
    catalog: Arc<CapabilityCatalog>,
}

fn is_profileable_target(target: &KeyTarget) -> bool {
    matches!(
        target,
        KeyTarget::DefaultProvider
            | KeyTarget::ApprovalMode
            | KeyTarget::ApprovalPolicy
            | KeyTarget::Worktrees
            | KeyTarget::Sandbox
            | KeyTarget::Model(_)
            | KeyTarget::BaseUrl(_)
    )
}

#[async_trait]
impl Tool for SetConfigTool {
    fn narrate(
        &self,
        tool_call: &ToolCall,
        phase: ToolNarrationPhase,
        locale: Option<&str>,
        _ctx: everruns_core::tool_narration::ToolNarrationContext<'_>,
    ) -> Option<String> {
        let _ = locale;
        Some(narrate_set_config(tool_call, phase))
    }

    fn name(&self) -> &str {
        "set_config"
    }
    fn display_name(&self) -> Option<&str> {
        Some("Set config")
    }
    fn description(&self) -> &str {
        "Set or clear a yolop configuration value, validated against the schema and persisted to \
         the settings file. Scalar keys use `value` (pass `clear` to unset). Harness capability \
         overrides use `key=capabilities` with a `json` override object (or `value=clear` to drop \
         all overrides). Run `get_config` first to see valid keys."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "key": {
                    "type": "string",
                    "description": "Schema key, e.g. `default_provider`, `tokens.openai`, or `capabilities`."
                },
                "value": {
                    "type": "string",
                    "description": "New scalar value, or `clear` to unset."
                },
                "json": {
                    "type": "object",
                    "description": "For `key=capabilities`: append one `[[capabilities]]` entry with `ref`, optional `enabled`, `append`, and config fields."
                }
            },
            "required": ["key"],
            "additionalProperties": false
        })
    }
    async fn execute(&self, arguments: Value) -> ToolExecutionResult {
        let key = match arguments.get("key").and_then(Value::as_str) {
            Some(k) if !k.trim().is_empty() => k.trim(),
            _ => {
                return ToolExecutionResult::tool_error(format!(
                    "'key' is required; known keys: {}",
                    known_keys()
                ));
            }
        };
        let target = match parse_key(key) {
            Ok(t) => t,
            Err(err) => return ToolExecutionResult::tool_error(err),
        };
        let json = arguments.get("json");
        let value = arguments
            .get("value")
            .and_then(Value::as_str)
            .map(str::trim);

        if matches!(target, KeyTarget::CapabilityRef(_)) {
            return ToolExecutionResult::tool_error(
                "capabilities.<ref> is read-only; append overrides with `set_config key=capabilities json=…`"
                    .to_string(),
            );
        }

        if matches!(target, KeyTarget::Capabilities) {
            if let Some(json) = json {
                let parsed = match parse_override_from_json(json) {
                    Ok(entry) => entry,
                    Err(err) => return ToolExecutionResult::tool_error(err),
                };
                let entry = match build_capability_override(
                    &self.catalog,
                    &parsed.capability_ref,
                    parsed.enabled,
                    parsed.append,
                    Some(&parsed.config),
                ) {
                    Ok(entry) => entry,
                    Err(err) => return ToolExecutionResult::tool_error(err),
                };
                let index = match self.settings.append_capability_override(entry.clone()) {
                    Ok(index) => index,
                    Err(err) => {
                        return ToolExecutionResult::tool_error(format!(
                            "could not save settings: {err}"
                        ));
                    }
                };
                return ToolExecutionResult::success(json!({
                    "ok": true,
                    "key": key,
                    "index": index,
                    "message": format!("appended capabilities override at index {index}"),
                    "settings_path": self.settings.path().display().to_string(),
                    "stored": entry,
                    "note": "Restart yolop for harness changes to take effect.",
                }));
            }
            let clearing = value.is_some_and(|v| v.eq_ignore_ascii_case("clear"));
            if clearing {
                if let Err(err) = self.settings.clear_capability_overrides() {
                    return ToolExecutionResult::tool_error(format!(
                        "could not save settings: {err}"
                    ));
                }
                return ToolExecutionResult::success(json!({
                    "ok": true,
                    "key": key,
                    "message": "cleared all stored capability overrides",
                    "settings_path": self.settings.path().display().to_string(),
                }));
            }
            return ToolExecutionResult::tool_error(
                "capabilities expects `json` (append one override) or `value=clear`".to_string(),
            );
        }

        let value = match value {
            Some(v) => v,
            None => {
                return ToolExecutionResult::tool_error(
                    "'value' is required for scalar keys (use `clear` to unset)",
                );
            }
        };
        let clearing = value.eq_ignore_ascii_case("clear");
        if value.is_empty() {
            return ToolExecutionResult::tool_error(
                "empty value; provide a value or `clear` to unset".to_string(),
            );
        }

        let result = self.apply(&target, value, clearing);
        match result {
            Ok(message) => ToolExecutionResult::success(json!({
                "ok": true,
                "key": key,
                "message": message,
                "settings_path": if is_profileable_target(&target) {
                    self.settings.active_config_path()
                } else {
                    self.settings.path().to_path_buf()
                }.display().to_string(),
            })),
            Err(err) => ToolExecutionResult::tool_error(err),
        }
    }
}

impl SetConfigTool {
    fn apply(&self, target: &KeyTarget, value: &str, clearing: bool) -> Result<String, String> {
        let path = if is_profileable_target(target) {
            self.settings.active_config_path()
        } else {
            self.settings.path().to_path_buf()
        };
        let path = path.display().to_string();
        let saved = |what: String| format!("{what} (saved to {path})");
        let map_err = |e: anyhow::Error| format!("could not save settings: {e}");

        match target {
            KeyTarget::DefaultProvider => {
                if clearing {
                    self.settings.set_default_provider(None).map_err(map_err)?;
                    return Ok(saved(
                        "cleared default_provider; it will be auto-detected from credentials"
                            .to_string(),
                    ));
                }
                let provider = value.to_ascii_lowercase();
                if !SUPPORTED_PROVIDERS.contains(&provider.as_str()) {
                    return Err(format!(
                        "unknown provider `{provider}`; expected one of {}",
                        SUPPORTED_PROVIDERS.join(", ")
                    ));
                }
                self.settings
                    .set_default_provider(Some(provider.clone()))
                    .map_err(map_err)?;
                let preview = resolve_for_settings(&provider, &self.settings.snapshot())
                    .map(|resolved| resolved.next_run_preview())
                    .unwrap_or_else(|err| format!("→ next run: could not resolve model: {err}"));
                Ok(saved(format!(
                    "default_provider = {provider}; applies on the next run (use /setup to switch now)\n{preview}"
                )))
            }
            KeyTarget::Attribution => {
                let enabled = parse_on_off(value)
                    .ok_or_else(|| "attribution expects on/off (true/false, yes/no)".to_string())?;
                self.settings.set_attribution(enabled).map_err(map_err)?;
                Ok(saved(format!("attribution = {}", on_off(enabled))))
            }
            KeyTarget::ProactiveWake => {
                // `clear` reverts to the default (on), keeping settings.toml sparse.
                if clearing {
                    self.settings.set_proactive_wake(true).map_err(map_err)?;
                    return Ok(saved("cleared proactive_wake (default on)".to_string()));
                }
                let enabled = parse_on_off(value).ok_or_else(|| {
                    "proactive_wake expects on/off (true/false, yes/no)".to_string()
                })?;
                self.settings.set_proactive_wake(enabled).map_err(map_err)?;
                Ok(saved(format!("proactive_wake = {}", on_off(enabled))))
            }
            KeyTarget::AcpSetupPage => {
                // `clear` reverts to the default (off), keeping settings.toml sparse.
                if clearing {
                    self.settings.set_acp_setup_page(false).map_err(map_err)?;
                    return Ok(saved("cleared acp_setup_page (default off)".to_string()));
                }
                let enabled = parse_on_off(value).ok_or_else(|| {
                    "acp_setup_page expects on/off (true/false, yes/no)".to_string()
                })?;
                self.settings.set_acp_setup_page(enabled).map_err(map_err)?;
                Ok(saved(format!("acp_setup_page = {}", on_off(enabled))))
            }
            KeyTarget::ApprovalMode => {
                if clearing {
                    self.settings.clear_approval_mode().map_err(map_err)?;
                    return Ok(saved(
                        "cleared approval_mode; inherited/default value is active".to_string(),
                    ));
                }
                let mode = ApprovalMode::parse(value).ok_or_else(|| {
                    "approval_mode expects protective, normal, or off".to_string()
                })?;
                self.settings.set_approval_mode(mode).map_err(map_err)?;
                Ok(saved(format!(
                    "approval_mode = {}; applies next turn",
                    mode.as_str()
                )))
            }
            KeyTarget::ApprovalPolicy => {
                if clearing {
                    self.settings.clear_approval_policy().map_err(map_err)?;
                    return Ok(saved(
                        "cleared approval_policy; inherited/default value is active".to_string(),
                    ));
                }
                let policy = crate::config::ApprovalPolicy::parse(value).ok_or_else(|| {
                    "approval_policy expects untrusted, on-failure, on-request, or never"
                        .to_string()
                })?;
                self.settings.set_approval_policy(policy).map_err(map_err)?;
                Ok(saved(format!(
                    "approval_policy = {}; applies next run",
                    policy.as_str()
                )))
            }
            KeyTarget::Worktrees => {
                if clearing {
                    self.settings.clear_worktrees_mode().map_err(map_err)?;
                    return Ok(saved(
                        "cleared worktrees; inherited/default value is active".to_string(),
                    ));
                }
                let mode = crate::config::WorktreesMode::parse(value)
                    .ok_or_else(|| "worktrees expects auto, always, or off".to_string())?;
                self.settings.set_worktrees_mode(mode).map_err(map_err)?;
                Ok(saved(format!(
                    "worktrees = {}; applies to new sessions and future turns",
                    mode.as_str()
                )))
            }
            KeyTarget::Sandbox => {
                if clearing {
                    self.settings.clear_sandbox_mode().map_err(map_err)?;
                    return Ok(saved(
                        "cleared sandbox_mode; inherited/default value applies next run"
                            .to_string(),
                    ));
                }
                let mode = crate::config::SandboxMode::parse(value).ok_or_else(|| {
                    "sandbox_mode expects read-only, workspace-write, or danger-full-access"
                        .to_string()
                })?;
                self.settings.set_sandbox_mode(mode).map_err(map_err)?;
                if mode == crate::config::SandboxMode::DangerFullAccess {
                    Ok(saved("sandbox_mode = danger-full-access; DANGER: next run uses UNSAFE HOST execution with unrestricted file, process, and network access".to_string()))
                } else {
                    Ok(saved(format!(
                        "sandbox_mode = {}; applies next run",
                        mode.as_str()
                    )))
                }
            }
            KeyTarget::Theme => {
                if clearing {
                    self.settings.set_theme(None).map_err(map_err)?;
                    return Ok(saved(
                        "cleared theme (default: yolop's own palette)".to_string(),
                    ));
                }
                // Validate against the same names `--theme` accepts (yolop + tuika presets).
                if crate::tui::fullscreen::resolve_theme(value).is_none() {
                    return Err(format!(
                        "unknown theme `{value}`; expected one of: {}",
                        crate::tui::fullscreen::theme_names().join(", ")
                    ));
                }
                self.settings
                    .set_theme(Some(value.to_string()))
                    .map_err(map_err)?;
                Ok(saved(format!(
                    "theme = {value}; applies to new interactive sessions"
                )))
            }
            // The list has one editor: `yolop config models`, which validates
            // provider/model pairs and owns ordering. A key/value setter here
            // would be a second, weaker write path for the same file.
            KeyTarget::Models => Err(
                "the model list is edited with `yolop config models add|rm|move` or configured with `yolop config model set MODEL`, not `set_config`; \
                 run `yolop config models list` to see it"
                    .to_string(),
            ),
            KeyTarget::Model(provider) => {
                if clearing {
                    let existed = self.settings.clear_model(provider).map_err(map_err)?;
                    return Ok(saved(if existed {
                        format!("cleared default_models.{provider}")
                    } else {
                        format!("default_models.{provider} was already unset")
                    }));
                }
                self.settings
                    .set_model(provider.clone(), value.to_string())
                    .map_err(map_err)?;
                Ok(saved(format!(
                    "default_models.{provider} = {value}; applies on the next run for that provider"
                )))
            }
            KeyTarget::Token(provider) => {
                if clearing {
                    let existed = self.settings.clear_token(provider).map_err(map_err)?;
                    return Ok(saved(if existed {
                        format!("cleared tokens.{provider}")
                    } else {
                        format!("tokens.{provider} was already unset")
                    }));
                }
                self.settings
                    .set_token(provider.clone(), value.to_string())
                    .map_err(map_err)?;
                // Never echo the secret back.
                Ok(saved(format!("stored API token for {provider}")))
            }
            KeyTarget::BaseUrl(provider) => {
                if clearing {
                    let existed = self.settings.clear_base_url(provider).map_err(map_err)?;
                    return Ok(saved(if existed {
                        format!("cleared base_urls.{provider}")
                    } else {
                        format!("base_urls.{provider} was already unset")
                    }));
                }
                if !value.starts_with("http://") && !value.starts_with("https://") {
                    return Err("base URL must start with http:// or https://".to_string());
                }
                self.settings
                    .set_base_url(provider.clone(), value.to_string())
                    .map_err(map_err)?;
                Ok(saved(format!("base_urls.{provider} = {value}")))
            }
            KeyTarget::Capabilities | KeyTarget::CapabilityRef(_) => {
                Err("capabilities are configured via set_config with key=capabilities".to_string())
            }
            KeyTarget::Mcp => Err(
                "mcp servers are configured in settings.toml under [mcp.servers.<name>]"
                    .to_string(),
            ),
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

#[cfg(test)]
mod tests {
    use super::*;
    use everruns_builtins::{MESSAGE_METADATA_CAPABILITY_ID, MessageMetadataCapability};
    use everruns_core::tool_narration::ToolNarrationPhase;
    use everruns_provider::ToolCall;

    fn cli_request(args: &[&str]) -> ControlRequest {
        let command = ConfigCommandLine::augment_args(Command::new("config"));
        let matches = command.try_get_matches_from(args).expect("parse config");
        ConfigCommandLine::request(&matches).expect("config request")
    }

    #[test]
    fn config_cli_parses_persistent_grammar() {
        for args in [
            &["config", "get"][..],
            &["config", "get", "theme"][..],
            &["config", "set", "theme", "blue"][..],
            &["config", "clear", "theme"][..],
            &["config", "model", "show"][..],
            &["config", "model", "set", "openai/gpt-5.6"][..],
            &["config", "model", "clear"][..],
            &["config", "models"][..],
        ] {
            cli_request(args);
        }
    }

    #[test]
    fn config_commands_dispatch_to_config_actions() {
        let cases = [
            (["config", "get", "theme"].as_slice(), "get"),
            (["config", "set", "theme", "blue"].as_slice(), "set"),
            (["config", "clear", "theme"].as_slice(), "clear"),
            (["config", "model", "show"].as_slice(), "model_show"),
            (["config", "model", "clear"].as_slice(), "model_clear"),
        ];
        for (args, expected) in cases {
            let action = cli_request(args).action;
            assert_eq!(action.get("op").and_then(Value::as_str), Some(expected));
        }
    }

    #[test]
    fn set_json_dispatches_object_separately_from_scalar_value() {
        let request = cli_request(&[
            "config",
            "set",
            "capabilities",
            "--json",
            r#"{"ref":"web_fetch","enabled":false}"#,
        ]);
        assert!(matches!(
            serde_json::from_value::<ConfigAction>(request.action).expect("config action"),
            ConfigAction::Set { key, value: None, json: Some(Value::Object(_)) }
                if key == "capabilities"
        ));
    }

    #[test]
    fn set_requires_exactly_one_value_form() {
        let missing = ConfigCommandLine::augment_args(Command::new("config"))
            .try_get_matches_from(["config", "set", "theme"])
            .expect("clap accepts the optional positional before request validation");
        assert!(
            ConfigCommandLine::request(&missing)
                .expect_err("missing value must fail")
                .to_string()
                .contains("requires VALUE or --json OBJECT")
        );
        assert!(
            ConfigCommandLine::augment_args(Command::new("config"))
                .try_get_matches_from(["config", "set", "theme", "blue", "--json", "{}",])
                .is_err(),
            "scalar and JSON forms must be mutually exclusive"
        );
    }

    #[test]
    fn singular_model_dispatches_to_persistent_action() {
        let request = cli_request(&["config", "model", "set", "openai/gpt-5.6"]);
        assert!(matches!(
            serde_json::from_value::<ConfigAction>(request.action).expect("config action"),
            ConfigAction::ModelSet { model } if model == "openai/gpt-5.6"
        ));
    }

    #[test]
    fn models_dispatches_to_existing_catalog_action() {
        for args in [&["config", "models"][..], &["config", "models", "list"][..]] {
            let request = cli_request(args);
            assert!(matches!(
                serde_json::from_value::<ModelListAction>(request.action)
                    .expect("model-list action"),
                ModelListAction::List { .. }
            ));
        }
    }

    fn store() -> (tempfile::TempDir, Arc<SettingsStore>) {
        let tmp = tempfile::tempdir().expect("tmp");
        let store = Arc::new(SettingsStore::open(tmp.path().join("settings.toml")));
        (tmp, store)
    }

    fn catalog() -> Arc<CapabilityCatalog> {
        let mut catalog = CapabilityCatalog::new();
        catalog.register_arc(Arc::new(MessageMetadataCapability));
        Arc::new(catalog)
    }

    fn get_config_tool(settings: Arc<SettingsStore>) -> GetConfigTool {
        GetConfigTool {
            settings,
            catalog: catalog(),
        }
    }

    fn set_config_tool(settings: Arc<SettingsStore>) -> SetConfigTool {
        SetConfigTool {
            settings,
            catalog: catalog(),
        }
    }

    #[test]
    fn config_capability_advertises_no_agent_tools() {
        let (_tmp, settings) = store();
        let capability = ConfigCapability {
            model_list: Arc::new(ModelListCapability::new(settings.clone(), None)),
            settings,
            catalog: catalog(),
        };

        assert!(capability.tools().is_empty());
    }

    #[test]
    fn set_config_narration_shows_key_and_bool_value() {
        let (_tmp, settings) = store();
        let tool = set_config_tool(settings);
        let call = ToolCall {
            id: "call-1".to_owned(),
            name: "set_config".to_owned(),
            arguments: json!({ "key": "attribution", "value": "on" }),
        };
        let narration = tool.narrate(
            &call,
            ToolNarrationPhase::Completed,
            None,
            everruns_core::tool_narration::ToolNarrationContext::default(),
        );
        assert_eq!(narration.as_deref(), Some("Set config: attribution=true"));
    }

    #[test]
    fn get_config_narration_uses_bare_verb_without_key() {
        let (_tmp, settings) = store();
        let tool = get_config_tool(settings);
        let call = ToolCall {
            id: "call-1".to_owned(),
            name: "get_config".to_owned(),
            arguments: json!({}),
        };
        let narration = tool.narrate(
            &call,
            ToolNarrationPhase::Completed,
            None,
            everruns_core::tool_narration::ToolNarrationContext::default(),
        );
        assert_eq!(narration.as_deref(), Some("Get config"));
    }

    #[tokio::test]
    async fn set_config_persists_default_provider_and_per_provider_model() {
        let (_tmp, settings) = store();
        let tool = set_config_tool(settings.clone());

        let r = tool
            .execute(json!({ "key": "default_provider", "value": "anthropic" }))
            .await;
        match &r {
            ToolExecutionResult::Success(msg) => {
                let text = msg.to_string();
                assert!(text.contains("→ next run:"), "{text}");
                assert!(text.contains("anthropic/"), "{text}");
            }
            other => panic!("expected success, got {other:?}"),
        }
        assert_eq!(
            settings.snapshot().default_provider.as_deref(),
            Some("anthropic")
        );

        tool.execute(json!({ "key": "models.anthropic", "value": "claude-opus-4-5" }))
            .await;
        assert_eq!(
            settings.snapshot().model_for("anthropic"),
            Some("claude-opus-4-5")
        );
    }

    #[tokio::test]
    async fn set_config_rejects_unknown_provider_and_key() {
        let (_tmp, settings) = store();
        let tool = set_config_tool(settings);

        let bad_provider = tool
            .execute(json!({ "key": "default_provider", "value": "nope" }))
            .await;
        assert!(matches!(bad_provider, ToolExecutionResult::ToolError(_)));

        let bad_key = tool
            .execute(json!({ "key": "frobnicate", "value": "x" }))
            .await;
        assert!(matches!(bad_key, ToolExecutionResult::ToolError(_)));
    }

    #[tokio::test]
    async fn set_config_routes_approval_mode() {
        let (_tmp, settings) = store();
        let tool = set_config_tool(settings.clone());
        let ok = tool
            .execute(json!({ "key": "approval_mode", "value": "protective" }))
            .await;
        assert!(matches!(ok, ToolExecutionResult::Success(_)));
        assert_eq!(
            settings.snapshot().approval_mode(),
            crate::config::ApprovalMode::Protective
        );

        // Alias and lenient synonyms route through the same path.
        tool.execute(json!({ "key": "approval", "value": "yolo" }))
            .await;
        assert_eq!(
            settings.snapshot().approval_mode(),
            crate::config::ApprovalMode::Off
        );

        let bad = tool
            .execute(json!({ "key": "approval_mode", "value": "whenever" }))
            .await;
        assert!(matches!(bad, ToolExecutionResult::ToolError(_)));
    }

    #[tokio::test]
    async fn set_config_routes_theme() {
        let (_tmp, settings) = store();
        let tool = set_config_tool(settings.clone());

        // A bundled preset persists.
        let ok = tool
            .execute(json!({ "key": "theme", "value": "gruvbox-dark" }))
            .await;
        assert!(matches!(ok, ToolExecutionResult::Success(_)));
        assert_eq!(settings.snapshot().theme(), Some("gruvbox-dark"));

        // `yolop` means the default and is not persisted.
        tool.execute(json!({ "key": "theme", "value": "yolop" }))
            .await;
        assert_eq!(settings.snapshot().theme(), None);

        // An unknown theme is rejected at the entry point.
        let bad = tool
            .execute(json!({ "key": "theme", "value": "no-such-theme" }))
            .await;
        assert!(matches!(bad, ToolExecutionResult::ToolError(_)));
    }

    #[tokio::test]
    async fn set_config_routes_hard_approval_policy() {
        let (_tmp, settings) = store();
        let tool = set_config_tool(settings.clone());
        let result = tool
            .execute(json!({ "key": "approval_policy", "value": "on-failure" }))
            .await;
        assert!(matches!(result, ToolExecutionResult::Success(_)));
        assert_eq!(
            settings.snapshot().approval_policy(),
            crate::config::ApprovalPolicy::OnFailure
        );

        let bad = tool
            .execute(json!({ "key": "approval_policy", "value": "sometimes" }))
            .await;
        assert!(matches!(bad, ToolExecutionResult::ToolError(_)));
    }

    #[tokio::test]
    async fn set_config_routes_proactive_wake_with_alias_and_clear() {
        let (_tmp, settings) = store();
        let tool = set_config_tool(settings.clone());

        tool.execute(json!({ "key": "proactive_wake", "value": "off" }))
            .await;
        assert!(!settings.snapshot().proactive_wake_enabled());

        // Alias routes through the same target.
        tool.execute(json!({ "key": "wake", "value": "on" })).await;
        assert!(settings.snapshot().proactive_wake_enabled());

        // `clear` reverts to the default (on).
        tool.execute(json!({ "key": "proactive_wake", "value": "off" }))
            .await;
        let cleared = tool
            .execute(json!({ "key": "proactive_wake", "value": "clear" }))
            .await;
        assert!(matches!(cleared, ToolExecutionResult::Success(_)));
        assert!(settings.snapshot().proactive_wake_enabled());

        let bad = tool
            .execute(json!({ "key": "proactive_wake", "value": "maybe" }))
            .await;
        assert!(matches!(bad, ToolExecutionResult::ToolError(_)));
    }

    #[tokio::test]
    async fn set_config_requires_explicit_unsafe_sandbox_opt_out_and_warns() {
        let (_tmp, settings) = store();
        let tool = set_config_tool(settings.clone());
        let result = tool
            .execute(json!({ "key": "sandbox", "value": "off" }))
            .await;
        let ToolExecutionResult::Success(message) = result else {
            panic!("expected success");
        };
        assert!(message.to_string().contains("DANGER"), "{message}");
        assert!(message.to_string().contains("UNSAFE HOST"), "{message}");
        assert_eq!(
            settings.snapshot().sandbox_mode(),
            crate::config::SandboxMode::DangerFullAccess
        );

        tool.execute(json!({ "key": "containment", "value": "clear" }))
            .await;
        assert_eq!(
            settings.snapshot().sandbox_mode(),
            crate::config::SandboxMode::DangerFullAccess
        );
    }

    #[tokio::test]
    async fn set_config_validates_base_url_scheme() {
        let (_tmp, settings) = store();
        let tool = set_config_tool(settings);
        let r = tool
            .execute(json!({ "key": "base_urls.custom", "value": "localhost:8000" }))
            .await;
        assert!(matches!(r, ToolExecutionResult::ToolError(_)));
    }

    #[tokio::test]
    async fn set_and_clear_token_roundtrip() {
        let (_tmp, settings) = store();
        let tool = set_config_tool(settings.clone());
        tool.execute(json!({ "key": "tokens.openai", "value": "sk-secret" }))
            .await;
        assert!(settings.snapshot().has_token("openai"));

        tool.execute(json!({ "key": "tokens.openai", "value": "clear" }))
            .await;
        assert!(!settings.snapshot().has_token("openai"));
    }

    #[tokio::test]
    async fn get_config_redacts_tokens() {
        let (_tmp, settings) = store();
        settings
            .set_token("openai".to_string(), "sk-secret".to_string())
            .unwrap();
        let tool = get_config_tool(settings.clone());
        let r = tool.execute(json!({ "key": "tokens.openai" })).await;
        let ToolExecutionResult::Success(value) = r else {
            panic!("expected success");
        };
        let text = value.to_string();
        assert!(
            !text.contains("sk-secret"),
            "token value must be redacted: {text}"
        );
        assert!(text.contains("stored"));
    }

    #[tokio::test]
    async fn get_config_lists_all_fields() {
        let (_tmp, settings) = store();
        let tool = get_config_tool(settings);
        let ToolExecutionResult::Success(value) = tool.execute(json!({})).await else {
            panic!("expected success");
        };
        let fields = value["fields"].as_array().expect("fields array");
        assert_eq!(fields.len(), schema().len());
    }

    #[tokio::test]
    async fn get_config_renders_attribution_as_bool() {
        let (_tmp, settings) = store();
        let tool = get_config_tool(settings);
        let ToolExecutionResult::Success(value) =
            tool.execute(json!({ "key": "attribution" })).await
        else {
            panic!("expected success");
        };
        // type=bool, so `current` must be a real JSON boolean, not "on"/"off".
        assert_eq!(value["field"]["type"], "bool");
        assert_eq!(value["field"]["current"], Value::Bool(true));
    }

    #[tokio::test]
    async fn get_config_scoped_key_keeps_table_and_narrows_current() {
        let (_tmp, settings) = store();
        settings
            .set_model("openai".to_string(), "gpt-5.5 high".to_string())
            .unwrap();
        settings
            .set_model("anthropic".to_string(), "claude-opus-4-5".to_string())
            .unwrap();
        let tool = get_config_tool(settings);
        let ToolExecutionResult::Success(value) =
            tool.execute(json!({ "key": "models.openai" })).await
        else {
            panic!("expected success");
        };
        // `current` is narrowed to the requested provider...
        assert_eq!(value["field"]["current"], "gpt-5.5 high");
        // ...while the whole-table view is preserved under `table`.
        assert_eq!(value["field"]["table"]["openai"], "gpt-5.5 high");
        assert_eq!(value["field"]["table"]["anthropic"], "claude-opus-4-5");
    }

    #[tokio::test]
    async fn get_config_table_omits_unsupported_providers() {
        let (_tmp, settings) = store();
        // Tolerant loading can leave entries for providers set_config cannot
        // address; get_config must not list them. Exercised via the full
        // listing, whose `models` field renders the whole table.
        settings
            .set_model("openai".to_string(), "gpt-5.5".to_string())
            .unwrap();
        settings
            .set_model("frobnicate".to_string(), "whatever".to_string())
            .unwrap();
        let tool = get_config_tool(settings);
        let ToolExecutionResult::Success(value) = tool.execute(json!({})).await else {
            panic!("expected success");
        };
        let models = value["fields"]
            .as_array()
            .expect("fields array")
            .iter()
            .find(|f| f["key"] == "default_models")
            .expect("default_models field present");
        assert_eq!(models["current"]["openai"], "gpt-5.5");
        assert!(
            models["current"].get("frobnicate").is_none(),
            "unsupported provider must be omitted: {}",
            models["current"]
        );
    }

    #[tokio::test]
    async fn set_config_appends_capabilities_override() {
        let (_tmp, settings) = store();
        let tool = set_config_tool(settings.clone());
        let result = tool
            .execute(json!({
                "key": "capabilities",
                "json": {
                    "ref": MESSAGE_METADATA_CAPABILITY_ID,
                    "enabled": true,
                    "fields": ["timestamp"]
                }
            }))
            .await;
        assert!(matches!(result, ToolExecutionResult::Success(_)));
        let snapshot = settings.snapshot();
        assert_eq!(snapshot.capabilities.len(), 1);
        assert_eq!(
            snapshot.capabilities[0].capability_ref,
            MESSAGE_METADATA_CAPABILITY_ID
        );
        assert_eq!(
            snapshot.capabilities[0].config["fields"],
            json!(["timestamp"])
        );
    }

    #[tokio::test]
    async fn set_config_capabilities_rejects_invalid_config() {
        let (_tmp, settings) = store();
        let tool = set_config_tool(settings.clone());
        let result = tool
            .execute(json!({
                "key": "capabilities",
                "json": {
                    "ref": MESSAGE_METADATA_CAPABILITY_ID,
                    "fields": ["llm_model"]
                }
            }))
            .await;
        assert!(matches!(result, ToolExecutionResult::ToolError(_)));
    }

    #[tokio::test]
    async fn get_config_capabilities_includes_catalog() {
        let (_tmp, settings) = store();
        let tool = get_config_tool(settings);
        let ToolExecutionResult::Success(value) =
            tool.execute(json!({ "key": "capabilities" })).await
        else {
            panic!("expected success");
        };
        let catalog = value["catalog"].as_array().expect("catalog array");
        assert!(
            catalog
                .iter()
                .any(|entry| entry["id"] == MESSAGE_METADATA_CAPABILITY_ID),
            "catalog must list registered capabilities: {catalog:?}"
        );
        let meta = catalog
            .iter()
            .find(|entry| entry["id"] == MESSAGE_METADATA_CAPABILITY_ID)
            .expect("message_metadata entry");
        assert!(meta["config_schema"].is_object());
        assert!(value["stored_overrides"].as_array().unwrap().is_empty());
        assert!(!value["effective_harness"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn get_config_capabilities_ref_exposes_schema() {
        let (_tmp, settings) = store();
        let tool = get_config_tool(settings);
        let ToolExecutionResult::Success(value) = tool
            .execute(json!({ "key": format!("capabilities.{MESSAGE_METADATA_CAPABILITY_ID}") }))
            .await
        else {
            panic!("expected success");
        };
        assert!(value["capability"]["config_schema"].is_object());
        assert!(value["stored_overrides"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn set_config_capabilities_disable_appends_remove_entry() {
        let (_tmp, settings) = store();
        let tool = set_config_tool(settings.clone());
        tool.execute(json!({
            "key": "capabilities",
            "json": { "ref": MESSAGE_METADATA_CAPABILITY_ID, "enabled": true }
        }))
        .await;
        tool.execute(json!({
            "key": "capabilities",
            "json": { "ref": MESSAGE_METADATA_CAPABILITY_ID, "enabled": false }
        }))
        .await;
        let snapshot = settings.snapshot();
        assert_eq!(snapshot.capabilities.len(), 2);
        assert!(snapshot.capabilities[1].is_remove());
    }
}

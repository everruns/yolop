// The `config` capability — schema-described, human-friendly editing of the
// yolop settings file.
//
// `settings.toml` is loaded tolerantly (unknown keys are ignored, never
// fatal). This capability layers *semantics* on top of that file via the
// informational schema in `crate::config_schema`: it exposes `get_config` (read
// the schema + current values) and `set_config` (validate + persist any known
// key) so the agent can configure yolop the way a user describes it, and it
// drops a short always-on pointer into the system prompt so the agent knows the
// configuration surface exists.
//
// Provider/model edits are persisted here and take effect on the next run; use
// the interactive `/setup` command to switch the *live* model mid-session.

use crate::capability_settings::{
    CapabilityCatalog, apply_capability_settings, build_capability_override,
    capability_catalog_json, effective_harness_json, stored_override_json,
};
use crate::config_schema::{KeyTarget, ValueKind, known_keys, parse_key, schema};
use crate::config_service::{ConfigService, current_value, scoped_current};
use crate::runtime::{SUPPORTED_PROVIDERS, coding_harness_defaults};
use crate::settings::{ApprovalMode, Settings, SettingsStore};
use async_trait::async_trait;
use everruns_core::capabilities::{Capability, CapabilityStatus, SystemPromptContext};
use everruns_core::tools::{Tool, ToolExecutionResult};
use serde_json::{Value, json};
use std::sync::Arc;

pub(crate) const CONFIG_CAPABILITY_ID: &str = "yolop_config";

pub(crate) struct ConfigCapability {
    pub(crate) settings: Arc<SettingsStore>,
    pub(crate) catalog: Arc<CapabilityCatalog>,
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
        Some(format!(
            "<capability id=\"{}\">\nyolop's settings live at {} and are schema-described \
             (default provider/model, per-provider API tokens and models, endpoint base URLs, \
             attribution, harness capabilities). To inspect or change settings keys, call \
             `get_config` and `set_config`. To add, remove, or reconfigure harness capabilities, \
             call `get_capabilities` and `set_capability` (schemas come from each capability's \
             `config_schema` / `validate_config`). Or activate the `yolop-config` skill. Unknown \
             keys in the file are ignored, never fatal. Provider/model and capability edits apply \
             on the next run; use `/setup` to switch the live model now.\n</capability>",
            self.id(),
            self.settings.path().display()
        ))
    }

    fn system_prompt_preview(&self) -> Option<String> {
        Some(
            "<capability id=\"yolop_config\">\nyolop's settings and harness capabilities are \
             schema-described; use `get_config` / `set_config` and `get_capabilities` / \
             `set_capability`, or the `yolop-config` skill.\n\
             </capability>"
                .to_string(),
        )
    }

    fn tools(&self) -> Vec<Box<dyn Tool>> {
        vec![
            Box::new(GetConfigTool {
                settings: self.settings.clone(),
            }),
            Box::new(SetConfigTool {
                settings: self.settings.clone(),
            }),
            Box::new(GetCapabilitiesTool {
                settings: self.settings.clone(),
                catalog: self.catalog.clone(),
            }),
            Box::new(SetCapabilityTool {
                settings: self.settings.clone(),
                catalog: self.catalog.clone(),
            }),
        ]
    }
}

// ---------- field rendering ----------
//
// The per-target read helpers (`current_value`, `scoped_current`) live in
// `crate::config_service` so any capability can reuse them through the
// `ConfigService`; here we only assemble the schema-described field view.

/// JSON description of a schema field, optionally with its current value(s).
fn field_json(settings: &Settings, field: &crate::config_schema::ConfigField) -> Value {
    let current = if field.provider_scoped {
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
}

#[async_trait]
impl Tool for GetConfigTool {
    fn name(&self) -> &str {
        "get_config"
    }
    fn display_name(&self) -> Option<&str> {
        Some("Get config")
    }
    fn description(&self) -> &str {
        "Inspect yolop configuration. With no `key`, returns every configuration key with its \
         title, description, type, default, examples, and current value (secrets redacted). \
         With a `key` (e.g. `default_provider`, `tokens.openai`), returns just that entry."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "key": {
                    "type": "string",
                    "description": "Optional single key to describe, e.g. `default_model` or `models.anthropic`."
                }
            },
            "additionalProperties": false
        })
    }
    async fn execute(&self, arguments: Value) -> ToolExecutionResult {
        let settings = self.settings.snapshot();
        let path = self.settings.path().display().to_string();

        if let Some(key) = arguments.get("key").and_then(Value::as_str) {
            let key = key.trim();
            if !key.is_empty() {
                let target = match parse_key(key) {
                    Ok(t) => t,
                    Err(err) => return ToolExecutionResult::tool_error(err),
                };
                let field = target.field();
                let mut entry = field_json(&settings, field);
                // Read the single value through the config service (the same
                // path any capability uses), which narrows a provider-scoped
                // key to the requested provider. For those keys, preserve the
                // whole-table view that field_json seeded into `current`.
                let value = self.settings.current(key).unwrap_or(Value::Null);
                if field.provider_scoped
                    && let Value::Object(map) = &mut entry
                {
                    let table = map.get("current").cloned().unwrap_or(Value::Null);
                    map.insert("table".to_string(), table);
                    map.insert("key".to_string(), Value::String(key.to_string()));
                }
                entry["current"] = value;
                return ToolExecutionResult::success(json!({
                    "settings_path": path,
                    "field": entry,
                }));
            }
        }

        let fields: Vec<Value> = schema().iter().map(|f| field_json(&settings, f)).collect();
        ToolExecutionResult::success(json!({
            "settings_path": path,
            "fields": fields,
            "note": "Set any key with `set_config`. Provider/model edits apply on the next run; \
                     use /setup to switch the live model now. Unknown keys in the file are ignored.",
        }))
    }
}

// ---------- set_config ----------

struct SetConfigTool {
    settings: Arc<SettingsStore>,
}

#[async_trait]
impl Tool for SetConfigTool {
    fn name(&self) -> &str {
        "set_config"
    }
    fn display_name(&self) -> Option<&str> {
        Some("Set config")
    }
    fn description(&self) -> &str {
        "Set or clear a yolop configuration value, validated against the schema and persisted to \
         the settings file. `key` is a schema key (e.g. `default_provider`, `default_model`, \
         `models.openai`, `tokens.anthropic`, `base_urls.custom`, `attribution`). Pass `clear` as \
         the value to remove an optional/secret key. Run `get_config` first to see valid keys."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "key": {
                    "type": "string",
                    "description": "Schema key, e.g. `default_provider` or `tokens.openai`."
                },
                "value": {
                    "type": "string",
                    "description": "New value, or `clear` to unset an optional/secret key."
                }
            },
            "required": ["key", "value"],
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
        let value = match arguments.get("value").and_then(Value::as_str) {
            Some(v) => v.trim(),
            None => {
                return ToolExecutionResult::tool_error(
                    "'value' is required (use `clear` to unset)",
                );
            }
        };
        let target = match parse_key(key) {
            Ok(t) => t,
            Err(err) => return ToolExecutionResult::tool_error(err),
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
                "settings_path": self.settings.path().display().to_string(),
            })),
            Err(err) => ToolExecutionResult::tool_error(err),
        }
    }
}

impl SetConfigTool {
    fn apply(&self, target: &KeyTarget, value: &str, clearing: bool) -> Result<String, String> {
        let path = self.settings.path().display().to_string();
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
                Ok(saved(format!(
                    "default_provider = {provider}; applies on the next run (use /setup to switch now)"
                )))
            }
            KeyTarget::DefaultModel => {
                if clearing {
                    self.settings.set_default_model(None).map_err(map_err)?;
                    return Ok(saved("cleared default_model".to_string()));
                }
                self.settings
                    .set_default_model(Some(value.to_string()))
                    .map_err(map_err)?;
                Ok(saved(format!(
                    "default_model = {value}; applies on the next run (use /setup to switch now)"
                )))
            }
            KeyTarget::Attribution => {
                let enabled = parse_on_off(value)
                    .ok_or_else(|| "attribution expects on/off (true/false, yes/no)".to_string())?;
                self.settings.set_attribution(enabled).map_err(map_err)?;
                Ok(saved(format!("attribution = {}", on_off(enabled))))
            }
            KeyTarget::ApprovalMode => {
                let mode = ApprovalMode::parse(value).ok_or_else(|| {
                    "approval_mode expects protective, normal, or off".to_string()
                })?;
                self.settings.set_approval_mode(mode).map_err(map_err)?;
                Ok(saved(format!(
                    "approval_mode = {}; applies next turn",
                    mode.as_str()
                )))
            }
            KeyTarget::Model(provider) => {
                if clearing {
                    let existed = self.settings.clear_model(provider).map_err(map_err)?;
                    return Ok(saved(if existed {
                        format!("cleared models.{provider}")
                    } else {
                        format!("models.{provider} was already unset")
                    }));
                }
                self.settings
                    .set_model(provider.clone(), value.to_string())
                    .map_err(map_err)?;
                Ok(saved(format!(
                    "models.{provider} = {value}; applies on the next run for that provider"
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

// ---------- get_capabilities ----------

struct GetCapabilitiesTool {
    settings: Arc<SettingsStore>,
    catalog: Arc<CapabilityCatalog>,
}

#[async_trait]
impl Tool for GetCapabilitiesTool {
    fn name(&self) -> &str {
        "get_capabilities"
    }
    fn display_name(&self) -> Option<&str> {
        Some("Get capabilities")
    }
    fn description(&self) -> &str {
        "Inspect yolop harness capabilities. With no `id`, returns the registered capability \
         catalog, stored `[[capabilities]]` overrides, and the effective harness list. With an \
         `id`, returns catalog metadata plus stored overrides and effective instances for that \
         capability ref."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "string",
                    "description": "Optional capability ref, e.g. `message_metadata` or `web_fetch`."
                }
            },
            "additionalProperties": false
        })
    }
    async fn execute(&self, arguments: Value) -> ToolExecutionResult {
        let settings = self.settings.snapshot();
        let path = self.settings.path().display().to_string();
        let defaults = coding_harness_defaults(false);
        let effective = apply_capability_settings(defaults, &settings.capabilities);

        if let Some(id) = arguments.get("id").and_then(Value::as_str) {
            let id = id.trim();
            if !id.is_empty() {
                let catalog = match capability_catalog_json(&self.catalog, id) {
                    Ok(entry) => entry,
                    Err(err) => return ToolExecutionResult::tool_error(err),
                };
                let stored: Vec<Value> = settings
                    .capability_overrides_for(id)
                    .into_iter()
                    .map(|(index, entry)| stored_override_json(index, entry))
                    .collect();
                let effective_for_id: Vec<Value> = effective
                    .iter()
                    .enumerate()
                    .filter(|(_, cap)| cap.capability_id() == id)
                    .map(|(index, cap)| {
                        json!({
                            "index": index,
                            "ref": cap.capability_id(),
                            "config": cap.config,
                        })
                    })
                    .collect();
                return ToolExecutionResult::success(json!({
                    "settings_path": path,
                    "capability": catalog,
                    "stored_overrides": stored,
                    "effective_instances": effective_for_id,
                }));
            }
        }

        let mut catalog_entries = Vec::new();
        for cap in self.catalog.list() {
            match capability_catalog_json(&self.catalog, cap.id()) {
                Ok(entry) => catalog_entries.push(entry),
                Err(err) => return ToolExecutionResult::tool_error(err),
            }
        }
        catalog_entries.sort_by(|a, b| {
            a["id"]
                .as_str()
                .unwrap_or_default()
                .cmp(b["id"].as_str().unwrap_or_default())
        });
        let stored: Vec<Value> = settings
            .capability_overrides()
            .iter()
            .enumerate()
            .map(|(index, entry)| stored_override_json(index, entry))
            .collect();
        ToolExecutionResult::success(json!({
            "settings_path": path,
            "catalog": catalog_entries,
            "stored_overrides": stored,
            "effective_harness": effective_harness_json(&effective),
            "note": "Overrides are an ordered [[capabilities]] list. Use `set_capability` to append an entry; changes apply on the next run.",
        }))
    }
}

// ---------- set_capability ----------

struct SetCapabilityTool {
    settings: Arc<SettingsStore>,
    catalog: Arc<CapabilityCatalog>,
}

#[async_trait]
impl Tool for SetCapabilityTool {
    fn name(&self) -> &str {
        "set_capability"
    }
    fn display_name(&self) -> Option<&str> {
        Some("Set capability")
    }
    fn description(&self) -> &str {
        "Append a harness capability override to settings.toml. Pass `id` and `enabled=false` to \
         remove every harness instance with that ref, `append=true` to add a duplicate instance, \
         and optional `config` (JSON object) for per-instance settings. Config is validated \
         against the capability's schema via `validate_config`. Run `get_capabilities` first."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "string",
                    "description": "Capability ref, e.g. `message_metadata`."
                },
                "enabled": {
                    "type": "boolean",
                    "description": "When false, appends a remove entry for this ref."
                },
                "append": {
                    "type": "boolean",
                    "description": "When true, always append a new harness instance instead of merging into the first matching ref."
                },
                "remove_index": {
                    "type": "integer",
                    "description": "Optional index of a stored [[capabilities]] entry to delete before appending the new override."
                },
                "config": {
                    "type": "object",
                    "description": "Per-capability JSON config for this override entry."
                }
            },
            "required": ["id"],
            "additionalProperties": false
        })
    }
    async fn execute(&self, arguments: Value) -> ToolExecutionResult {
        let id = match arguments.get("id").and_then(Value::as_str) {
            Some(id) if !id.trim().is_empty() => id.trim(),
            _ => return ToolExecutionResult::tool_error("'id' is required".to_string()),
        };
        let enabled = arguments.get("enabled").and_then(Value::as_bool);
        let append = arguments
            .get("append")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let config = arguments.get("config");
        let remove_index = arguments
            .get("remove_index")
            .and_then(Value::as_u64)
            .map(|index| index as usize);
        if enabled.is_none() && config.is_none() && !append && remove_index.is_none() {
            return ToolExecutionResult::tool_error(
                "provide `enabled`, `config`, `remove_index`, and/or `append=true`; call `get_capabilities` to inspect"
                    .to_string(),
            );
        }

        if let Some(index) = remove_index {
            if let Err(err) = self.settings.remove_capability_override(index) {
                return ToolExecutionResult::tool_error(format!("could not save settings: {err}"));
            }
            if enabled.is_none() && config.is_none() && !append {
                return ToolExecutionResult::success(json!({
                    "ok": true,
                    "removed_index": index,
                    "message": format!("removed stored override at index {index}"),
                    "settings_path": self.settings.path().display().to_string(),
                    "note": "Restart yolop for harness changes to take effect.",
                }));
            }
        }

        let entry = match build_capability_override(&self.catalog, id, enabled, append, config) {
            Ok(entry) => entry,
            Err(err) => return ToolExecutionResult::tool_error(err),
        };

        let index = match self.settings.append_capability_override(entry.clone()) {
            Ok(index) => index,
            Err(err) => {
                return ToolExecutionResult::tool_error(format!("could not save settings: {err}"));
            }
        };

        let message = if entry.is_remove() {
            format!("appended remove entry for `{id}` at index {index}")
        } else if append {
            format!("appended new `{id}` harness instance at index {index}")
        } else if enabled == Some(true) && config.is_none() {
            format!("appended enable entry for `{id}` at index {index}")
        } else {
            format!("appended config override for `{id}` at index {index}")
        };

        ToolExecutionResult::success(json!({
            "ok": true,
            "id": id,
            "index": index,
            "message": message,
            "settings_path": self.settings.path().display().to_string(),
            "stored": entry,
            "note": "Restart yolop for harness changes to take effect.",
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use everruns_core::capabilities::{MESSAGE_METADATA_CAPABILITY_ID, MessageMetadataCapability};

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

    #[tokio::test]
    async fn set_config_persists_default_provider_and_model() {
        let (_tmp, settings) = store();
        let tool = SetConfigTool {
            settings: settings.clone(),
        };

        let r = tool
            .execute(json!({ "key": "default_provider", "value": "anthropic" }))
            .await;
        assert!(matches!(r, ToolExecutionResult::Success(_)));
        assert_eq!(
            settings.snapshot().default_provider.as_deref(),
            Some("anthropic")
        );

        // Alias `model` routes to default_model.
        tool.execute(json!({ "key": "model", "value": "claude-opus-4-5" }))
            .await;
        assert_eq!(settings.snapshot().default_model(), Some("claude-opus-4-5"));
    }

    #[tokio::test]
    async fn set_config_rejects_unknown_provider_and_key() {
        let (_tmp, settings) = store();
        let tool = SetConfigTool { settings };

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
        let tool = SetConfigTool {
            settings: settings.clone(),
        };
        let ok = tool
            .execute(json!({ "key": "approval_mode", "value": "protective" }))
            .await;
        assert!(matches!(ok, ToolExecutionResult::Success(_)));
        assert_eq!(
            settings.snapshot().approval_mode(),
            crate::settings::ApprovalMode::Protective
        );

        // Alias and lenient synonyms route through the same path.
        tool.execute(json!({ "key": "approval", "value": "yolo" }))
            .await;
        assert_eq!(
            settings.snapshot().approval_mode(),
            crate::settings::ApprovalMode::Off
        );

        let bad = tool
            .execute(json!({ "key": "approval_mode", "value": "whenever" }))
            .await;
        assert!(matches!(bad, ToolExecutionResult::ToolError(_)));
    }

    #[tokio::test]
    async fn set_config_validates_base_url_scheme() {
        let (_tmp, settings) = store();
        let tool = SetConfigTool { settings };
        let r = tool
            .execute(json!({ "key": "base_urls.custom", "value": "localhost:8000" }))
            .await;
        assert!(matches!(r, ToolExecutionResult::ToolError(_)));
    }

    #[tokio::test]
    async fn set_and_clear_token_roundtrip() {
        let (_tmp, settings) = store();
        let tool = SetConfigTool {
            settings: settings.clone(),
        };
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
        let tool = GetConfigTool {
            settings: settings.clone(),
        };
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
        let tool = GetConfigTool { settings };
        let ToolExecutionResult::Success(value) = tool.execute(json!({})).await else {
            panic!("expected success");
        };
        let fields = value["fields"].as_array().expect("fields array");
        assert_eq!(fields.len(), schema().len());
    }

    #[tokio::test]
    async fn get_config_renders_attribution_as_bool() {
        let (_tmp, settings) = store();
        let tool = GetConfigTool { settings };
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
        let tool = GetConfigTool { settings };
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
        let tool = GetConfigTool { settings };
        let ToolExecutionResult::Success(value) = tool.execute(json!({})).await else {
            panic!("expected success");
        };
        let models = value["fields"]
            .as_array()
            .expect("fields array")
            .iter()
            .find(|f| f["key"] == "models")
            .expect("models field present");
        assert_eq!(models["current"]["openai"], "gpt-5.5");
        assert!(
            models["current"].get("frobnicate").is_none(),
            "unsupported provider must be omitted: {}",
            models["current"]
        );
    }

    #[tokio::test]
    async fn set_capability_appends_message_metadata_override() {
        let (_tmp, settings) = store();
        let tool = SetCapabilityTool {
            settings: settings.clone(),
            catalog: catalog(),
        };
        let result = tool
            .execute(json!({
                "id": MESSAGE_METADATA_CAPABILITY_ID,
                "enabled": true,
                "config": { "fields": ["timestamp"] }
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
    async fn set_capability_rejects_invalid_config() {
        let (_tmp, settings) = store();
        let tool = SetCapabilityTool {
            settings: settings.clone(),
            catalog: catalog(),
        };
        let result = tool
            .execute(json!({
                "id": MESSAGE_METADATA_CAPABILITY_ID,
                "enabled": true,
                "config": { "fields": ["llm_model"] }
            }))
            .await;
        assert!(matches!(result, ToolExecutionResult::ToolError(_)));
    }

    #[tokio::test]
    async fn get_capabilities_exposes_schema_for_message_metadata() {
        let (_tmp, settings) = store();
        let tool = GetCapabilitiesTool {
            settings,
            catalog: catalog(),
        };
        let ToolExecutionResult::Success(value) = tool
            .execute(json!({ "id": MESSAGE_METADATA_CAPABILITY_ID }))
            .await
        else {
            panic!("expected success");
        };
        assert!(value["capability"]["config_schema"].is_object());
        assert!(value["stored_overrides"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn set_capability_disable_appends_remove_entry() {
        let (_tmp, settings) = store();
        let tool = SetCapabilityTool {
            settings: settings.clone(),
            catalog: catalog(),
        };
        tool.execute(json!({
            "id": MESSAGE_METADATA_CAPABILITY_ID,
            "enabled": true
        }))
        .await;
        tool.execute(json!({
            "id": MESSAGE_METADATA_CAPABILITY_ID,
            "enabled": false
        }))
        .await;
        let snapshot = settings.snapshot();
        assert_eq!(snapshot.capabilities.len(), 2);
        assert!(snapshot.capabilities[1].is_remove());
    }
}

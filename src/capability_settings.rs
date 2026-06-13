//! Persisted harness capability overrides in `settings.toml`.
//!
//! The default coding harness is fixed at compile time; `[capabilities]` entries
//! let users add optional capabilities, disable defaults, or override per-capability
//! JSON config. Overrides are validated through each capability's
//! `validate_config` before persisting.

use everruns_core::capabilities::Capability;
use everruns_core::{AgentCapabilityConfig, CapabilityInfo};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use toml::Table;
use toml::Value as TomlValue;

/// Read-only view of registered capabilities for schema lookup and validation.
pub struct CapabilityCatalog {
    capabilities: HashMap<String, Arc<dyn Capability>>,
}

impl CapabilityCatalog {
    pub fn new() -> Self {
        Self {
            capabilities: HashMap::new(),
        }
    }

    pub fn register_arc(&mut self, capability: Arc<dyn Capability>) {
        self.capabilities
            .insert(capability.id().to_string(), capability);
    }

    pub fn get(&self, id: &str) -> Option<&Arc<dyn Capability>> {
        self.capabilities.get(id)
    }

    pub fn list(&self) -> impl Iterator<Item = &Arc<dyn Capability>> {
        self.capabilities.values()
    }

    pub fn has(&self, id: &str) -> bool {
        self.capabilities.contains_key(id)
    }

    pub fn validate(&self, id: &str, config: &Value) -> Result<(), String> {
        let cap = self
            .get(id)
            .ok_or_else(|| format!("unknown capability `{id}`; not registered in yolop"))?;
        cap.validate_config(config)
    }
}

/// One persisted capability override under `[capabilities.<id>]` in settings.toml.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct CapabilitySetting {
    /// `Some(false)` removes the capability from the harness. `Some(true)` forces
    /// it on even when absent from defaults. `None` inherits default presence unless
    /// `config` is non-empty (which implies an explicit override).
    pub enabled: Option<bool>,
    /// Per-capability JSON config merged over the harness default.
    pub config: Value,
}

impl CapabilitySetting {
    pub fn disabled() -> Self {
        Self {
            enabled: Some(false),
            config: Value::Null,
        }
    }

    pub fn is_explicitly_disabled(&self) -> bool {
        self.enabled == Some(false)
    }
}

pub fn parse_capabilities_table(table: &Table) -> BTreeMap<String, CapabilitySetting> {
    let mut capabilities = BTreeMap::new();
    let Some(caps) = table.get("capabilities").and_then(TomlValue::as_table) else {
        return capabilities;
    };
    for (id, entry) in caps {
        let Some(entry_table) = entry.as_table() else {
            continue;
        };
        let enabled = entry_table.get("enabled").and_then(TomlValue::as_bool);
        let mut config_table = entry_table.clone();
        config_table.remove("enabled");
        let config = if config_table.is_empty() {
            Value::Null
        } else {
            toml_value_to_json(&TomlValue::Table(config_table))
        };
        capabilities.insert(id.clone(), CapabilitySetting { enabled, config });
    }
    capabilities
}

pub fn capabilities_to_table(capabilities: &BTreeMap<String, CapabilitySetting>) -> Table {
    let mut caps = Table::new();
    for (id, setting) in capabilities {
        if setting.enabled.is_none() && setting.config.is_null() {
            continue;
        }
        let mut entry = Table::new();
        if let Some(enabled) = setting.enabled {
            entry.insert("enabled".to_string(), TomlValue::Boolean(enabled));
        }
        if let TomlValue::Table(config) = json_to_toml(&setting.config) {
            for (k, v) in config {
                entry.insert(k, v);
            }
        }
        if !entry.is_empty() {
            caps.insert(id.clone(), TomlValue::Table(entry));
        }
    }
    caps
}

fn toml_value_to_json(value: &TomlValue) -> Value {
    match value {
        TomlValue::String(s) => Value::String(s.clone()),
        TomlValue::Integer(i) => json!(*i),
        TomlValue::Float(f) => json!(*f),
        TomlValue::Boolean(b) => Value::Bool(*b),
        TomlValue::Datetime(dt) => Value::String(dt.to_string()),
        TomlValue::Array(items) => Value::Array(items.iter().map(toml_value_to_json).collect()),
        TomlValue::Table(table) => {
            let mut map = serde_json::Map::new();
            for (k, v) in table {
                map.insert(k.clone(), toml_value_to_json(v));
            }
            Value::Object(map)
        }
    }
}

fn json_to_toml(value: &Value) -> TomlValue {
    match value {
        Value::Null => TomlValue::Table(Table::new()),
        Value::Bool(b) => TomlValue::Boolean(*b),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                TomlValue::Integer(i)
            } else if let Some(f) = n.as_f64() {
                TomlValue::Float(f)
            } else {
                TomlValue::String(n.to_string())
            }
        }
        Value::String(s) => TomlValue::String(s.clone()),
        Value::Array(items) => TomlValue::Array(items.iter().map(json_to_toml).collect()),
        Value::Object(map) => {
            let mut table = Table::new();
            for (k, v) in map {
                if v.is_null() {
                    continue;
                }
                table.insert(k.clone(), json_to_toml(v));
            }
            TomlValue::Table(table)
        }
    }
}

/// Merge user overrides into the compile-time default harness list.
pub fn apply_capability_settings(
    defaults: Vec<AgentCapabilityConfig>,
    settings: &BTreeMap<String, CapabilitySetting>,
) -> Vec<AgentCapabilityConfig> {
    let mut by_id: BTreeMap<String, AgentCapabilityConfig> = defaults
        .into_iter()
        .map(|cap| (cap.capability_id().to_string(), cap))
        .collect();

    for (id, setting) in settings {
        if setting.is_explicitly_disabled() {
            by_id.remove(id);
            continue;
        }
        match by_id.get_mut(id) {
            Some(existing) => {
                existing.config = merge_config(&existing.config, &setting.config);
            }
            None => {
                by_id.insert(
                    id.clone(),
                    AgentCapabilityConfig::with_config(id.clone(), setting.config.clone()),
                );
            }
        }
    }

    by_id.into_values().collect()
}

fn merge_config(default: &Value, override_config: &Value) -> Value {
    if override_config.is_null() {
        return default.clone();
    }
    match (default, override_config) {
        (Value::Object(base), Value::Object(over)) => {
            let mut merged = base.clone();
            for (k, v) in over {
                merged.insert(k.clone(), v.clone());
            }
            Value::Object(merged)
        }
        (_, over) => over.clone(),
    }
}

pub fn effective_config(default: &Value, stored: &CapabilitySetting) -> Value {
    if stored.is_explicitly_disabled() {
        return Value::Null;
    }
    merge_config(default, &stored.config)
}

pub fn capability_entry_json(
    catalog: &CapabilityCatalog,
    id: &str,
    default: Option<&AgentCapabilityConfig>,
    stored: Option<&CapabilitySetting>,
) -> Result<Value, String> {
    let cap = catalog
        .get(id)
        .ok_or_else(|| format!("unknown capability `{id}`"))?;
    let info = CapabilityInfo::from_core(cap.as_ref());
    let default_config = default.map(|d| d.config.clone()).unwrap_or(json!({}));
    let stored_setting = stored.cloned().unwrap_or_default();
    let enabled = if stored_setting.is_explicitly_disabled() {
        false
    } else {
        default.is_some()
            || stored_setting.enabled == Some(true)
            || !stored_setting.config.is_null()
    };
    Ok(json!({
        "id": id,
        "name": info.name,
        "description": info.description,
        "category": info.category,
        "default_in_harness": default.is_some(),
        "enabled": enabled,
        "config_schema": info.config_schema,
        "config_ui_schema": info.config_ui_schema,
        "config_description": cap.describe_schema(None),
        "default_config": default_config,
        "stored": stored_setting,
        "current_config": effective_config(&default_config, &stored_setting),
        "note": "Changes apply on the next run. Use `set_capability` to add, remove, or reconfigure.",
    }))
}

pub fn validate_capability_mutation(
    catalog: &CapabilityCatalog,
    id: &str,
    enabled: Option<bool>,
    config: Option<&Value>,
    defaults: &[AgentCapabilityConfig],
    stored: &BTreeMap<String, CapabilitySetting>,
) -> Result<CapabilitySetting, String> {
    if !catalog.has(id) {
        return Err(format!(
            "unknown capability `{id}`; call `get_capabilities` for registered ids"
        ));
    }
    if enabled == Some(false) {
        return Ok(CapabilitySetting::disabled());
    }
    let default = defaults.iter().find(|c| c.capability_id() == id);
    let prior = stored.get(id);
    let merged_config = match (config, prior) {
        (Some(new), Some(old)) if !new.is_null() => merge_config(&old.config, new),
        (Some(new), None) if !new.is_null() => {
            let base = default.map(|d| d.config.clone()).unwrap_or(json!({}));
            merge_config(&base, new)
        }
        (Some(new), _) if !new.is_null() => new.clone(),
        (_, Some(old)) => old.config.clone(),
        _ => default.map(|d| d.config.clone()).unwrap_or(json!({})),
    };
    catalog.validate(id, &merged_config)?;
    let stored_enabled = match enabled {
        Some(true) => Some(true),
        Some(false) => Some(false),
        None if prior.is_some_and(|p| p.enabled == Some(true)) => Some(true),
        None if config.is_some() => None,
        None => Some(true),
    };
    Ok(CapabilitySetting {
        enabled: stored_enabled,
        config: merged_config,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use everruns_core::capabilities::{MESSAGE_METADATA_CAPABILITY_ID, MessageMetadataCapability};

    fn defaults() -> Vec<AgentCapabilityConfig> {
        vec![AgentCapabilityConfig::with_config(
            "web_fetch",
            json!({ "enable_file_download": true }),
        )]
    }

    #[test]
    fn apply_adds_optional_capability() {
        let mut settings = BTreeMap::new();
        settings.insert(
            MESSAGE_METADATA_CAPABILITY_ID.to_string(),
            CapabilitySetting {
                enabled: Some(true),
                config: json!({ "fields": ["timestamp"] }),
            },
        );
        let resolved = apply_capability_settings(defaults(), &settings);
        assert!(
            resolved
                .iter()
                .any(|c| c.capability_id() == MESSAGE_METADATA_CAPABILITY_ID)
        );
    }

    #[test]
    fn apply_removes_disabled_default() {
        let mut settings = BTreeMap::new();
        settings.insert("web_fetch".to_string(), CapabilitySetting::disabled());
        let resolved = apply_capability_settings(defaults(), &settings);
        assert!(!resolved.iter().any(|c| c.capability_id() == "web_fetch"));
    }

    #[test]
    fn apply_merges_config_over_default() {
        let mut settings = BTreeMap::new();
        settings.insert(
            "web_fetch".to_string(),
            CapabilitySetting {
                enabled: None,
                config: json!({ "enable_file_download": false }),
            },
        );
        let resolved = apply_capability_settings(defaults(), &settings);
        let cap = resolved
            .iter()
            .find(|c| c.capability_id() == "web_fetch")
            .expect("web_fetch still enabled");
        assert_eq!(cap.config["enable_file_download"], false);
    }

    #[test]
    fn capabilities_table_roundtrip() {
        let mut capabilities = BTreeMap::new();
        capabilities.insert(
            MESSAGE_METADATA_CAPABILITY_ID.to_string(),
            CapabilitySetting {
                enabled: Some(true),
                config: json!({ "fields": ["timestamp"] }),
            },
        );
        capabilities.insert("duckduckgo".to_string(), CapabilitySetting::disabled());

        let mut table = Table::new();
        let caps = capabilities_to_table(&capabilities);
        table.insert("capabilities".to_string(), TomlValue::Table(caps));

        let parsed = parse_capabilities_table(&table);
        assert_eq!(parsed, capabilities);
    }

    #[test]
    fn validate_rejects_bad_message_metadata_config() {
        let mut catalog = CapabilityCatalog::new();
        catalog.register_arc(Arc::new(MessageMetadataCapability));
        let err = catalog
            .validate(
                MESSAGE_METADATA_CAPABILITY_ID,
                &json!({ "fields": ["llm_model"] }),
            )
            .unwrap_err();
        assert!(err.contains("invalid message_metadata config"), "{err}");
    }

    #[test]
    fn capability_entry_json_includes_schema() {
        let mut catalog = CapabilityCatalog::new();
        catalog.register_arc(Arc::new(MessageMetadataCapability));
        let entry = capability_entry_json(&catalog, MESSAGE_METADATA_CAPABILITY_ID, None, None)
            .expect("entry");
        assert!(entry["config_schema"].is_object());
        assert_eq!(entry["enabled"], false);
    }
}

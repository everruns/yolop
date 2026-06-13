//! Persisted harness capability overrides in `settings.toml`.
//!
//! Overrides are stored as an ordered `[[capabilities]]` list — matching the
//! runtime's `Vec<AgentCapabilityConfig>` — so the same capability can appear
//! more than once with different configs. Each entry is applied in order:
//! - `enabled = false` removes every harness instance with that `ref`
//! - default (`append = false`) merges config into the first matching `ref`, or
//!   appends when absent
//! - `append = true` always appends a new harness instance (duplicates allowed)

use everruns_core::capabilities::Capability;
use everruns_core::{AgentCapabilityConfig, CapabilityInfo};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
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

/// One ordered harness override under `[[capabilities]]` in settings.toml.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityOverride {
    /// Capability id (`ref` on disk, matching `AgentCapabilityConfig`).
    #[serde(rename = "ref")]
    pub capability_ref: String,
    /// When `false`, removes every harness instance with this `ref`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// When `true`, always append a new harness instance instead of merging into
    /// the first matching `ref`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub append: bool,
    /// Per-capability JSON config (inline keys in TOML, or omitted when empty).
    #[serde(default)]
    pub config: Value,
}

impl CapabilityOverride {
    pub fn remove(capability_ref: impl Into<String>) -> Self {
        Self {
            capability_ref: capability_ref.into(),
            enabled: Some(false),
            append: false,
            config: Value::Null,
        }
    }

    pub fn is_remove(&self) -> bool {
        self.enabled == Some(false)
    }
}

pub fn parse_capabilities_table(table: &Table) -> Vec<CapabilityOverride> {
    let Some(raw) = table.get("capabilities") else {
        return Vec::new();
    };

    match raw {
        TomlValue::Array(items) => items.iter().filter_map(parse_capability_entry).collect(),
        // Legacy map form `[capabilities.<id>]` — converted to an ordered list.
        TomlValue::Table(map) => map
            .iter()
            .filter_map(|(id, value)| parse_legacy_capability_entry(id, value))
            .collect(),
        _ => Vec::new(),
    }
}

fn parse_capability_entry(value: &TomlValue) -> Option<CapabilityOverride> {
    let entry = value.as_table()?;
    let capability_ref = entry
        .get("ref")
        .or_else(|| entry.get("id"))
        .and_then(TomlValue::as_str)?
        .to_string();
    let enabled = entry.get("enabled").and_then(TomlValue::as_bool);
    let append = entry
        .get("append")
        .and_then(TomlValue::as_bool)
        .unwrap_or(false);
    let mut config_table = entry.clone();
    for key in ["ref", "id", "enabled", "append"] {
        config_table.remove(key);
    }
    let config = if config_table.is_empty() {
        Value::Null
    } else {
        toml_value_to_json(&TomlValue::Table(config_table))
    };
    Some(CapabilityOverride {
        capability_ref,
        enabled,
        append,
        config,
    })
}

fn parse_legacy_capability_entry(id: &str, value: &TomlValue) -> Option<CapabilityOverride> {
    let entry = value.as_table()?;
    let enabled = entry.get("enabled").and_then(TomlValue::as_bool);
    let mut config_table = entry.clone();
    config_table.remove("enabled");
    let config = if config_table.is_empty() {
        Value::Null
    } else {
        toml_value_to_json(&TomlValue::Table(config_table))
    };
    Some(CapabilityOverride {
        capability_ref: id.to_string(),
        enabled,
        append: false,
        config,
    })
}

pub fn capabilities_to_toml(overrides: &[CapabilityOverride]) -> TomlValue {
    let items: Vec<TomlValue> = overrides.iter().map(override_to_toml).collect();
    TomlValue::Array(items)
}

fn override_to_toml(entry: &CapabilityOverride) -> TomlValue {
    let mut table = Table::new();
    table.insert(
        "ref".to_string(),
        TomlValue::String(entry.capability_ref.clone()),
    );
    if let Some(enabled) = entry.enabled {
        table.insert("enabled".to_string(), TomlValue::Boolean(enabled));
    }
    if entry.append {
        table.insert("append".to_string(), TomlValue::Boolean(true));
    }
    if let TomlValue::Table(config) = json_to_toml(&entry.config) {
        for (k, v) in config {
            table.insert(k, v);
        }
    }
    TomlValue::Table(table)
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

/// Apply ordered user overrides to the compile-time default harness list.
pub fn apply_capability_settings(
    defaults: Vec<AgentCapabilityConfig>,
    overrides: &[CapabilityOverride],
) -> Vec<AgentCapabilityConfig> {
    let mut caps = defaults;
    for entry in overrides {
        if entry.is_remove() {
            caps.retain(|cap| cap.capability_id() != entry.capability_ref);
            continue;
        }
        if entry.append {
            caps.push(AgentCapabilityConfig::with_config(
                entry.capability_ref.clone(),
                entry.config.clone(),
            ));
            continue;
        }
        if let Some(existing) = caps
            .iter_mut()
            .find(|cap| cap.capability_id() == entry.capability_ref)
        {
            existing.config = merge_config(&existing.config, &entry.config);
        } else {
            caps.push(AgentCapabilityConfig::with_config(
                entry.capability_ref.clone(),
                entry.config.clone(),
            ));
        }
    }
    caps
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

pub fn capability_catalog_json(catalog: &CapabilityCatalog, id: &str) -> Result<Value, String> {
    let cap = catalog
        .get(id)
        .ok_or_else(|| format!("unknown capability `{id}`"))?;
    let info = CapabilityInfo::from_core(cap.as_ref());
    Ok(json!({
        "id": id,
        "name": info.name,
        "description": info.description,
        "category": info.category,
        "config_schema": info.config_schema,
        "config_ui_schema": info.config_ui_schema,
        "config_description": cap.describe_schema(None),
    }))
}

pub fn stored_override_json(index: usize, entry: &CapabilityOverride) -> Value {
    json!({
        "index": index,
        "ref": entry.capability_ref,
        "enabled": entry.enabled,
        "append": entry.append,
        "config": entry.config,
    })
}

pub fn effective_harness_json(caps: &[AgentCapabilityConfig]) -> Vec<Value> {
    caps.iter()
        .enumerate()
        .map(|(index, cap)| {
            json!({
                "index": index,
                "ref": cap.capability_id(),
                "config": cap.config,
            })
        })
        .collect()
}

pub fn build_capability_override(
    catalog: &CapabilityCatalog,
    capability_ref: &str,
    enabled: Option<bool>,
    append: bool,
    config: Option<&Value>,
) -> Result<CapabilityOverride, String> {
    if !catalog.has(capability_ref) {
        return Err(format!(
            "unknown capability `{capability_ref}`; call `get_capabilities` for registered ids"
        ));
    }
    if enabled == Some(false) {
        return Ok(CapabilityOverride::remove(capability_ref));
    }
    let config = config.cloned().unwrap_or(Value::Null);
    if !config.is_null() {
        catalog.validate(capability_ref, &config)?;
    }
    Ok(CapabilityOverride {
        capability_ref: capability_ref.to_string(),
        enabled,
        append,
        config,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use everruns_core::capabilities::{MESSAGE_METADATA_CAPABILITY_ID, MessageMetadataCapability};

    fn defaults() -> Vec<AgentCapabilityConfig> {
        vec![
            AgentCapabilityConfig::new("duckduckgo"),
            AgentCapabilityConfig::with_config(
                "web_fetch",
                json!({ "enable_file_download": true }),
            ),
        ]
    }

    #[test]
    fn apply_adds_optional_capability() {
        let overrides = vec![CapabilityOverride {
            capability_ref: MESSAGE_METADATA_CAPABILITY_ID.to_string(),
            enabled: Some(true),
            append: false,
            config: json!({ "fields": ["timestamp"] }),
        }];
        let resolved = apply_capability_settings(defaults(), &overrides);
        assert!(
            resolved
                .iter()
                .any(|c| c.capability_id() == MESSAGE_METADATA_CAPABILITY_ID)
        );
    }

    #[test]
    fn apply_removes_all_instances_with_ref() {
        let mut base = defaults();
        base.push(AgentCapabilityConfig::new("duckduckgo"));
        let overrides = vec![CapabilityOverride::remove("duckduckgo")];
        let resolved = apply_capability_settings(base, &overrides);
        assert!(!resolved.iter().any(|c| c.capability_id() == "duckduckgo"));
    }

    #[test]
    fn apply_merges_config_into_first_match() {
        let overrides = vec![CapabilityOverride {
            capability_ref: "web_fetch".to_string(),
            enabled: None,
            append: false,
            config: json!({ "enable_file_download": false }),
        }];
        let resolved = apply_capability_settings(defaults(), &overrides);
        assert_eq!(
            resolved
                .iter()
                .filter(|c| c.capability_id() == "web_fetch")
                .count(),
            1
        );
        let cap = resolved
            .iter()
            .find(|c| c.capability_id() == "web_fetch")
            .expect("web_fetch still enabled");
        assert_eq!(cap.config["enable_file_download"], false);
    }

    #[test]
    fn apply_append_allows_duplicate_refs() {
        let overrides = vec![
            CapabilityOverride {
                capability_ref: "duckduckgo".to_string(),
                enabled: None,
                append: true,
                config: json!({}),
            },
            CapabilityOverride {
                capability_ref: "duckduckgo".to_string(),
                enabled: None,
                append: true,
                config: json!({}),
            },
        ];
        let resolved = apply_capability_settings(defaults(), &overrides);
        assert_eq!(
            resolved
                .iter()
                .filter(|c| c.capability_id() == "duckduckgo")
                .count(),
            3
        );
    }

    #[test]
    fn capabilities_array_roundtrip() {
        let overrides = vec![
            CapabilityOverride {
                capability_ref: MESSAGE_METADATA_CAPABILITY_ID.to_string(),
                enabled: Some(true),
                append: false,
                config: json!({ "fields": ["timestamp"] }),
            },
            CapabilityOverride::remove("duckduckgo"),
        ];

        let mut table = Table::new();
        table.insert("capabilities".to_string(), capabilities_to_toml(&overrides));

        let parsed = parse_capabilities_table(&table);
        assert_eq!(parsed, overrides);
    }

    #[test]
    fn legacy_map_form_is_converted_to_list() {
        let mut legacy = Table::new();
        let mut message = Table::new();
        message.insert(
            "fields".to_string(),
            TomlValue::Array(vec![TomlValue::String("timestamp".to_string())]),
        );
        legacy.insert(
            MESSAGE_METADATA_CAPABILITY_ID.to_string(),
            TomlValue::Table(message),
        );
        let mut duck = Table::new();
        duck.insert("enabled".to_string(), TomlValue::Boolean(false));
        legacy.insert("duckduckgo".to_string(), TomlValue::Table(duck));

        let mut table = Table::new();
        table.insert("capabilities".to_string(), TomlValue::Table(legacy));

        let parsed = parse_capabilities_table(&table);
        assert_eq!(parsed.len(), 2);
        assert!(
            parsed
                .iter()
                .any(|entry| entry.capability_ref == MESSAGE_METADATA_CAPABILITY_ID)
        );
        assert!(parsed.iter().any(|entry| entry.is_remove()));
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
}

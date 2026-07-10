//! Extension manifest (`extension.toml`) parsing.
//!
//! An extension is a directory with an `extension.toml` that declares
//! contributions to yolop subsystems — capabilities, MCP servers, providers,
//! UI, etc. Contributions are declarative JSON-shaped config merged at
//! runtime; no recompilation required.

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::path::Path;

pub const MANIFEST_FILE: &str = "extension.toml";

/// Parsed `extension.toml` for one installed extension.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ExtensionManifest {
    /// Stable identifier (directory name). Must be unique per scope.
    pub id: String,
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub publisher: Option<String>,
    #[serde(default)]
    pub contributions: ExtensionContributions,
}

/// Declarative contributions keyed by subsystem.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
pub struct ExtensionContributions {
    /// Per-capability config fragments, keyed by capability id (`lsp`, `mcp`, …).
    /// Merged into the harness like `[[capabilities]]` overrides.
    #[serde(default)]
    pub capabilities: BTreeMap<String, Value>,
    /// MCP server entries merged into the session MCP config (future).
    #[serde(default)]
    pub mcp: BTreeMap<String, Value>,
}

impl ExtensionManifest {
    pub fn from_toml_str(contents: &str) -> Result<Self> {
        let manifest: Self = toml::from_str(contents).context("parse extension.toml")?;
        manifest.validate()?;
        Ok(manifest)
    }

    pub fn from_file(path: &Path) -> Result<Self> {
        let contents = std::fs::read_to_string(path)
            .with_context(|| format!("read extension manifest {}", path.display()))?;
        Self::from_toml_str(&contents)
    }

    pub fn validate(&self) -> Result<()> {
        if self.id.trim().is_empty() {
            bail!("extension `id` must not be empty");
        }
        if self.name.trim().is_empty() {
            bail!("extension `name` must not be empty");
        }
        if self.version.trim().is_empty() {
            bail!("extension `version` must not be empty");
        }
        if self.id.contains('/') || self.id.contains('\\') {
            bail!("extension `id` must not contain path separators");
        }
        Ok(())
    }

    /// Whether this extension contributes anything yolop can apply.
    pub fn has_contributions(&self) -> bool {
        !self.contributions.capabilities.is_empty() || !self.contributions.mcp.is_empty()
    }
}

/// Deep-merge two JSON values. Objects recurse; other types are replaced.
pub fn deep_merge_json(base: &Value, over: &Value) -> Value {
    if over.is_null() {
        return base.clone();
    }
    match (base, over) {
        (Value::Object(base_map), Value::Object(over_map)) => {
            let mut merged = base_map.clone();
            for (key, over_val) in over_map {
                let merged_val = match merged.get(key) {
                    Some(base_val) if base_val.is_object() && over_val.is_object() => {
                        deep_merge_json(base_val, over_val)
                    }
                    _ => over_val.clone(),
                };
                merged.insert(key.clone(), merged_val);
            }
            Value::Object(merged)
        }
        (_, over) => over.clone(),
    }
}

/// Merge capability contributions from multiple extensions into one map.
pub fn merge_capability_contributions(
    extensions: &[&ExtensionManifest],
) -> BTreeMap<String, Value> {
    let mut merged: BTreeMap<String, Value> = BTreeMap::new();
    for manifest in extensions {
        for (cap_id, config) in &manifest.contributions.capabilities {
            let entry = merged
                .entry(cap_id.clone())
                .or_insert_with(|| Value::Object(Map::new()));
            *entry = deep_merge_json(entry, config);
        }
    }
    merged
}

/// Merge MCP contributions from extensions into a flat server map.
pub fn merge_mcp_contributions(extensions: &[&ExtensionManifest]) -> BTreeMap<String, Value> {
    let mut merged: BTreeMap<String, Value> = BTreeMap::new();
    for manifest in extensions {
        for (name, config) in &manifest.contributions.mcp {
            let entry = merged
                .entry(name.clone())
                .or_insert_with(|| Value::Object(Map::new()));
            *entry = deep_merge_json(entry, config);
        }
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_lsp_contribution() {
        let toml = r#"
id = "yaml-lsp"
name = "YAML Language Support"
version = "0.1.0"
description = "YAML via yaml-language-server"

[contributions.capabilities.lsp.servers.yaml]
command = "yaml-language-server"
args = ["--stdio"]
extensions = ["yaml", "yml"]
"#;
        let manifest = ExtensionManifest::from_toml_str(toml).unwrap();
        assert_eq!(manifest.id, "yaml-lsp");
        assert_eq!(manifest.contributions.capabilities.len(), 1);
        let lsp = manifest.contributions.capabilities.get("lsp").unwrap();
        assert_eq!(
            lsp,
            &json!({
                "servers": {
                    "yaml": {
                        "command": "yaml-language-server",
                        "args": ["--stdio"],
                        "extensions": ["yaml", "yml"]
                    }
                }
            })
        );
    }

    #[test]
    fn deep_merge_nested_servers() {
        let base = json!({
            "servers": {
                "rust": { "enabled": false }
            },
            "request_timeout_ms": 30000
        });
        let over = json!({
            "servers": {
                "yaml": {
                    "command": "yaml-language-server",
                    "extensions": ["yaml"]
                }
            }
        });
        let merged = deep_merge_json(&base, &over);
        assert_eq!(
            merged,
            json!({
                "servers": {
                    "rust": { "enabled": false },
                    "yaml": {
                        "command": "yaml-language-server",
                        "extensions": ["yaml"]
                    }
                },
                "request_timeout_ms": 30000
            })
        );
    }

    #[test]
    fn rejects_empty_id() {
        let toml = r#"
id = ""
name = "Bad"
version = "0.1.0"
"#;
        assert!(ExtensionManifest::from_toml_str(toml).is_err());
    }
}

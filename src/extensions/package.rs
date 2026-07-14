//! Extension package discovery and manifest parsing.
//!
//! One scope, global: `<config_dir>/yolop/extensions/<name>/plugin.json`
//! (`YOLOP_EXTENSIONS_DIR` overrides, for tests and dev). There is
//! deliberately no workspace scope — repos never carry yolop extensions
//! (see specs/extensions.md, "Packaging, install, trust").
//!
//! The manifest is the approval boundary (D4): it carries the full tool
//! definitions (name, description, parameters schema, policy) so every
//! contribution is inspectable — and clamps the handshake — without
//! executing the server binary.

use serde::Deserialize;
use serde_json::Value;
use std::path::{Path, PathBuf};

pub const MANIFEST_FILE: &str = "plugin.json";

/// `plugin.json`, upstream everruns-plugin dialect plus the `yolop` facet.
/// Lenient: unknown fields ignored everywhere.
#[derive(Debug, Clone, Deserialize)]
struct RawManifest {
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    version: Option<String>,
    yolop: RawFacet,
}

#[derive(Debug, Clone, Deserialize)]
struct RawFacet {
    #[serde(default)]
    protocol_version: Option<String>,
    #[serde(rename = "capabilityServer")]
    capability_server: ServerSpec,
    #[serde(default)]
    config_schema: Option<Value>,
    #[serde(default)]
    tools: Vec<ToolDefinition>,
    /// Permits a handshake system-prompt contribution.
    #[serde(default)]
    prompt: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServerSpec {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
}

/// A manifest-declared tool: the full definition the LLM sees, plus policy.
#[derive(Debug, Clone, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// JSON Schema for the tool's parameters.
    #[serde(default = "default_schema")]
    pub schema: Value,
    #[serde(default)]
    pub never_defer: bool,
}

fn default_schema() -> Value {
    serde_json::json!({ "type": "object" })
}

#[derive(Debug, Clone)]
pub struct ExtensionManifest {
    pub name: String,
    pub description: String,
    pub version: Option<String>,
    pub capability_server: ServerSpec,
    pub config_schema: Option<Value>,
    pub tools: Vec<ToolDefinition>,
    pub prompt: bool,
}

#[derive(Debug, Clone)]
pub struct ExtensionPackage {
    pub dir: PathBuf,
    pub manifest: ExtensionManifest,
}

/// Capability ref for an extension: `ext:<name>`.
pub fn extension_capability_id(name: &str) -> String {
    format!("ext:{name}")
}

pub fn parse_manifest(raw: &str) -> Result<ExtensionManifest, String> {
    let raw: RawManifest =
        serde_json::from_str(raw).map_err(|e| format!("invalid plugin.json: {e}"))?;
    let name = raw.name.trim().to_string();
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(format!(
            "invalid extension name `{name}`: use ascii letters, digits, `-`, `_`"
        ));
    }
    if let Some(version) = &raw.yolop.protocol_version
        && !super::protocol::version_compatible(version)
    {
        return Err(format!(
            "extension targets YEP {version}, incompatible with this yolop ({})",
            super::protocol::PROTOCOL_VERSION
        ));
    }
    if raw.yolop.capability_server.command.trim().is_empty() {
        return Err("yolop.capabilityServer.command must not be empty".into());
    }
    if raw.yolop.tools.is_empty() && !raw.yolop.prompt {
        return Err("extension declares no tools and no prompt; nothing to contribute".into());
    }
    Ok(ExtensionManifest {
        name,
        description: raw.description,
        version: raw.version,
        capability_server: raw.yolop.capability_server,
        config_schema: raw.yolop.config_schema,
        tools: raw.yolop.tools,
        prompt: raw.yolop.prompt,
    })
}

/// The global extensions directory. `None` when the platform has no config
/// dir and no override is set.
pub fn extensions_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("YOLOP_EXTENSIONS_DIR") {
        return Some(PathBuf::from(dir));
    }
    dirs::config_dir().map(|p| p.join("yolop").join("extensions"))
}

/// Discover installed packages. Malformed packages warn and are skipped;
/// a missing directory is silent. Never sinks startup.
pub fn discover_extensions(dir: &Path) -> Vec<ExtensionPackage> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut packages = Vec::new();
    for entry in entries.flatten() {
        let package_dir = entry.path();
        let manifest_path = package_dir.join(MANIFEST_FILE);
        if !manifest_path.is_file() {
            continue;
        }
        let raw = match std::fs::read_to_string(&manifest_path) {
            Ok(raw) => raw,
            Err(err) => {
                tracing::warn!(target: "yolop::ext", "skipping {}: {err}", manifest_path.display());
                continue;
            }
        };
        match parse_manifest(&raw) {
            Ok(manifest) => {
                // The directory name is the identity users enable by; a
                // mismatched manifest name is a packaging bug worth refusing.
                let dir_name = entry.file_name().to_string_lossy().to_string();
                if dir_name != manifest.name {
                    tracing::warn!(
                        target: "yolop::ext",
                        "skipping {}: directory `{dir_name}` != manifest name `{}`",
                        manifest_path.display(),
                        manifest.name
                    );
                    continue;
                }
                tracing::debug!(
                    target: "yolop::ext",
                    "discovered extension `{}` {}",
                    manifest.name,
                    manifest.version.as_deref().unwrap_or("(unversioned)")
                );
                packages.push(ExtensionPackage {
                    dir: package_dir,
                    manifest,
                });
            }
            Err(err) => {
                tracing::warn!(target: "yolop::ext", "skipping {}: {err}", manifest_path.display());
            }
        }
    }
    packages.sort_by(|a, b| a.manifest.name.cmp(&b.manifest.name));
    packages
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn manifest_json() -> Value {
        json!({
            "name": "echo",
            "description": "Echo test extension.",
            "version": "0.1.0",
            "future_top_level": true,
            "yolop": {
                "protocol_version": "1.0",
                "capabilityServer": { "command": "yolop-extension-echo" },
                "tools": [
                    { "name": "echo", "description": "Echo text back.",
                      "schema": { "type": "object", "properties": { "text": { "type": "string" } } },
                      "never_defer": true }
                ],
                "prompt": true
            }
        })
    }

    #[test]
    fn parses_manifest_and_ignores_unknown_fields() {
        let manifest = parse_manifest(&manifest_json().to_string()).expect("parse");
        assert_eq!(manifest.name, "echo");
        assert_eq!(manifest.tools.len(), 1);
        assert!(manifest.tools[0].never_defer);
        assert!(manifest.prompt);
        assert_eq!(extension_capability_id(&manifest.name), "ext:echo");
    }

    #[test]
    fn rejects_incompatible_protocol_and_empty_contribution() {
        let mut incompatible = manifest_json();
        incompatible["yolop"]["protocol_version"] = json!("2.0");
        assert!(
            parse_manifest(&incompatible.to_string())
                .unwrap_err()
                .contains("incompatible")
        );

        let mut empty = manifest_json();
        empty["yolop"]["tools"] = json!([]);
        empty["yolop"]["prompt"] = json!(false);
        assert!(
            parse_manifest(&empty.to_string())
                .unwrap_err()
                .contains("nothing to contribute")
        );
    }

    #[test]
    fn discovery_skips_malformed_and_mismatched_dirs() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let good = tmp.path().join("echo");
        std::fs::create_dir_all(&good).unwrap();
        std::fs::write(good.join(MANIFEST_FILE), manifest_json().to_string()).unwrap();

        let malformed = tmp.path().join("broken");
        std::fs::create_dir_all(&malformed).unwrap();
        std::fs::write(malformed.join(MANIFEST_FILE), "{not json").unwrap();

        // Manifest says `echo` but lives under `wrong-name`.
        let mismatched = tmp.path().join("wrong-name");
        std::fs::create_dir_all(&mismatched).unwrap();
        std::fs::write(mismatched.join(MANIFEST_FILE), manifest_json().to_string()).unwrap();

        let found = discover_extensions(tmp.path());
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].manifest.name, "echo");
    }
}

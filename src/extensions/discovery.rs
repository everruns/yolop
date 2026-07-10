//! Discover installed extensions from global and workspace scopes.

use super::manifest::{ExtensionManifest, MANIFEST_FILE};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// Where an extension was discovered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtensionScope {
    Global,
    Workspace,
}

impl ExtensionScope {
    pub fn label(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Workspace => "workspace",
        }
    }
}

/// One installed extension on disk.
#[derive(Debug, Clone)]
pub struct DiscoveredExtension {
    pub manifest: ExtensionManifest,
    pub root: PathBuf,
    pub scope: ExtensionScope,
}

/// Global extensions directory: `<config_dir>/yolop/extensions/`.
pub fn global_extensions_dir() -> Option<PathBuf> {
    dirs::config_dir().map(|p| p.join("yolop").join("extensions"))
}

/// Workspace extensions directory: `<workspace>/.yolop/extensions/`.
pub fn workspace_extensions_dir(workspace_root: &Path) -> PathBuf {
    workspace_root.join(".yolop").join("extensions")
}

/// Discover extensions from global then workspace scopes. Workspace entries
/// with the same `id` override global ones (later wins in contribution merge).
pub fn discover_extensions(workspace_root: &Path) -> Vec<DiscoveredExtension> {
    let mut by_id: std::collections::BTreeMap<String, DiscoveredExtension> =
        std::collections::BTreeMap::new();

    if let Some(global_dir) = global_extensions_dir() {
        for ext in discover_scope(&global_dir, ExtensionScope::Global) {
            by_id.insert(ext.manifest.id.clone(), ext);
        }
    }

    let workspace_dir = workspace_extensions_dir(workspace_root);
    for ext in discover_scope(&workspace_dir, ExtensionScope::Workspace) {
        by_id.insert(ext.manifest.id.clone(), ext);
    }

    by_id.into_values().collect()
}

fn discover_scope(dir: &Path, scope: ExtensionScope) -> Vec<DiscoveredExtension> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return Vec::new(),
    };

    let mut extensions = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let manifest_path = path.join(MANIFEST_FILE);
        if !manifest_path.is_file() {
            tracing::warn!(
                path = %path.display(),
                "extension directory missing {MANIFEST_FILE}; skipping"
            );
            continue;
        }
        match ExtensionManifest::from_file(&manifest_path) {
            Ok(manifest) => {
                if manifest.id != path.file_name().and_then(|n| n.to_str()).unwrap_or("") {
                    tracing::warn!(
                        path = %path.display(),
                        manifest_id = %manifest.id,
                        "extension directory name does not match manifest id; using manifest id"
                    );
                }
                extensions.push(DiscoveredExtension {
                    manifest,
                    root: path,
                    scope,
                });
            }
            Err(error) => {
                tracing::warn!(
                    path = %manifest_path.display(),
                    %error,
                    "ignoring malformed extension manifest"
                );
            }
        }
    }
    extensions.sort_by(|a, b| a.manifest.id.cmp(&b.manifest.id));
    extensions
}

/// Install target directory for an extension id.
pub fn install_dir(scope: ExtensionScope, workspace_root: &Path, id: &str) -> Result<PathBuf> {
    let base = match scope {
        ExtensionScope::Global => {
            global_extensions_dir().context("no platform config directory for global extensions")?
        }
        ExtensionScope::Workspace => workspace_extensions_dir(workspace_root),
    };
    Ok(base.join(id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write_extension(dir: &Path, id: &str, extra_toml: &str) {
        fs::create_dir_all(dir).unwrap();
        let manifest = format!(
            r#"
id = "{id}"
name = "Test {id}"
version = "0.0.1"

{extra_toml}
"#
        );
        fs::write(dir.join(MANIFEST_FILE), manifest).unwrap();
    }

    #[test]
    fn workspace_overrides_global_same_id() {
        let tmp = tempfile::tempdir().unwrap();
        let global = tmp.path().join("global");
        let workspace = tmp.path().join("ws");
        fs::create_dir_all(&global).unwrap();
        fs::create_dir_all(&workspace).unwrap();

        write_extension(
            &global.join("demo"),
            "demo",
            r#"
[contributions.capabilities.lsp.servers.yaml]
command = "global-server"
extensions = ["yaml"]
"#,
        );
        write_extension(
            &workspace.join(".yolop/extensions/demo"),
            "demo",
            r#"
[contributions.capabilities.lsp.servers.yaml]
command = "workspace-server"
extensions = ["yml"]
"#,
        );

        // Simulate discovery without patching global_extensions_dir
        let mut by_id = std::collections::BTreeMap::new();
        for ext in discover_scope(&global, ExtensionScope::Global) {
            by_id.insert(ext.manifest.id.clone(), ext);
        }
        for ext in discover_scope(
            &workspace.join(".yolop/extensions"),
            ExtensionScope::Workspace,
        ) {
            by_id.insert(ext.manifest.id.clone(), ext);
        }
        let exts: Vec<_> = by_id.into_values().collect();
        assert_eq!(exts.len(), 1);
        assert_eq!(exts[0].scope, ExtensionScope::Workspace);
        let lsp = exts[0]
            .manifest
            .contributions
            .capabilities
            .get("lsp")
            .unwrap();
        assert!(lsp.to_string().contains("workspace-server"));
    }
}

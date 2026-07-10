//! Apply extension contributions to the harness and MCP config.

use super::discovery::DiscoveredExtension;
use super::manifest::{deep_merge_json, merge_capability_contributions, merge_mcp_contributions};
use crate::capability_settings::CapabilityCatalog;
use everruns_core::AgentCapabilityConfig;
use everruns_core::ScopedMcpServers;

/// Merge extension capability contributions into the harness list.
///
/// Extensions auto-enable opt-in capabilities they contribute to (e.g. `lsp`).
/// Invalid contributions are warned and skipped.
pub fn apply_extension_capability_contributions(
    mut caps: Vec<AgentCapabilityConfig>,
    extensions: &[DiscoveredExtension],
    catalog: &CapabilityCatalog,
) -> Vec<AgentCapabilityConfig> {
    if extensions.is_empty() {
        return caps;
    }

    let manifests: Vec<_> = extensions.iter().map(|e| &e.manifest).collect();
    let contributions = merge_capability_contributions(&manifests);

    for (cap_id, config) in contributions {
        if let Err(error) = catalog.validate(&cap_id, &config) {
            tracing::warn!(
                capability = %cap_id,
                %error,
                "ignoring invalid extension capability contribution"
            );
            continue;
        }

        if let Some(existing) = caps.iter_mut().find(|c| c.capability_id() == cap_id) {
            existing.config = deep_merge_json(&existing.config, &config);
        } else {
            tracing::info!(
                capability = %cap_id,
                "extension auto-enabled capability"
            );
            caps.push(AgentCapabilityConfig::with_config(cap_id, config));
        }
    }

    caps
}

/// Merge extension MCP contributions into scoped MCP servers.
///
/// Extension MCP entries are added to the workspace scope (PoC). Malformed
/// entries are skipped with a warning.
pub fn apply_extension_mcp_contributions(
    mut servers: ScopedMcpServers,
    extensions: &[DiscoveredExtension],
) -> ScopedMcpServers {
    if extensions.is_empty() {
        return servers;
    }

    let manifests: Vec<_> = extensions.iter().map(|e| &e.manifest).collect();
    let contributions = merge_mcp_contributions(&manifests);

    for (name, config) in contributions {
        match serde_json::from_value::<everruns_core::ScopedMcpServer>(config.clone()) {
            Ok(server) => {
                servers.insert(name, server);
            }
            Err(error) => {
                tracing::warn!(
                    server = %name,
                    %error,
                    "ignoring invalid extension MCP contribution"
                );
            }
        }
    }

    servers
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capabilities::lsp::LspCapability;
    use crate::capability_settings::CapabilityCatalog;
    use crate::extensions::discovery::{DiscoveredExtension, ExtensionScope};
    use crate::extensions::manifest::ExtensionManifest;
    use crate::workspace_host::WorkspaceHost;
    use serde_json::json;
    use std::path::PathBuf;
    use std::sync::Arc;

    fn catalog_with_lsp(tmp: &tempfile::TempDir) -> CapabilityCatalog {
        let mut catalog = CapabilityCatalog::new();
        let root = tmp.path().to_path_buf();
        std::fs::create_dir_all(&root).expect("workspace dir");
        let host = Arc::new(
            WorkspaceHost::new(Arc::new(std::sync::RwLock::new(root.clone())), root)
                .expect("workspace host"),
        );
        catalog.register_arc(Arc::new(LspCapability::new(host)));
        catalog
    }

    fn ext_with_lsp_yaml() -> DiscoveredExtension {
        let manifest = ExtensionManifest::from_toml_str(
            r#"
id = "yaml-lsp"
name = "YAML"
version = "0.1.0"

[contributions.capabilities.lsp.servers.yaml]
command = "yaml-language-server"
args = ["--stdio"]
extensions = ["yaml", "yml"]
"#,
        )
        .unwrap();
        DiscoveredExtension {
            manifest,
            root: PathBuf::from("/ext/yaml-lsp"),
            scope: ExtensionScope::Global,
        }
    }

    #[test]
    fn auto_enables_lsp_and_merges_servers() {
        let tmp = tempfile::tempdir().unwrap();
        let catalog = catalog_with_lsp(&tmp);
        let defaults = vec![];
        let applied =
            apply_extension_capability_contributions(defaults, &[ext_with_lsp_yaml()], &catalog);
        assert_eq!(applied.len(), 1);
        assert_eq!(applied[0].capability_id(), "lsp");
        let servers = applied[0].config.get("servers").unwrap();
        assert!(servers.get("yaml").is_some());
        assert!(servers.get("rust").is_none());
    }

    #[test]
    fn merges_into_existing_lsp_config() {
        let tmp = tempfile::tempdir().unwrap();
        let catalog = catalog_with_lsp(&tmp);
        let defaults = vec![AgentCapabilityConfig::with_config(
            "lsp",
            json!({ "request_timeout_ms": 45000 }),
        )];
        let applied =
            apply_extension_capability_contributions(defaults, &[ext_with_lsp_yaml()], &catalog);
        assert_eq!(applied.len(), 1);
        assert_eq!(applied[0].config["request_timeout_ms"], 45000);
        assert!(applied[0].config["servers"]["yaml"].is_object());
    }
}

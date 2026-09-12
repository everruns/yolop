use crate::config::mcp::{McpConfigStore, McpServerSummary};
use crate::control::{ControlCapability, ControlResponse, ControlRoute};
use async_trait::async_trait;
use everruns_core::{Capability, CapabilityStatus, SystemPromptContext, Tool, ToolExecutionResult};
use serde_json::{Value, json};
use std::sync::Arc;

pub(crate) const MCP_VISIBLE_SERVERS: usize = 10;

pub(crate) const MCP_CAPABILITY_ID: &str = "mcp";

pub(crate) const MCP_CONTROL_ROUTE: ControlRoute = ControlRoute {
    resource: MCP_CAPABILITY_ID,
    cli_subcommand: "mcp",
    read_only_operations: &["list"],
    summary: "Inspect and manage MCP servers. Listing is served inline; every mutation runs through `yolop mcp ...`.",
};

pub(crate) struct McpCapability {
    pub(crate) store: Arc<McpConfigStore>,
}

pub(crate) fn render_mcp_prompt(servers: &[McpServerSummary]) -> String {
    let mut prompt = String::from("<capability id=\"mcp\">\nMCP servers provide external tools. ");
    if servers.is_empty() {
        prompt.push_str("No MCP servers are configured yet. ");
    } else {
        prompt.push_str("Configured servers (scope plus on/off state):\n");
        for server in servers.iter().take(MCP_VISIBLE_SERVERS) {
            let scope = format!("{:?}", server.scope).to_lowercase();
            let state = if server.enabled {
                "enabled"
            } else {
                "disabled"
            };
            prompt.push_str(&format!("- `{}` ({scope}, {state})\n", server.name));
        }
        let hidden = servers.len().saturating_sub(MCP_VISIBLE_SERVERS);
        if hidden > 0 {
            prompt.push_str(&format!(
                "... and {hidden} more. Run `yolop mcp list` for the full set.\n"
            ));
        }
    }
    prompt.push_str(
        "Discover the full set with the control tool (resource `mcp`, operation `list`) or `yolop mcp list`; details with `yolop mcp show <name>`. Manage with `yolop mcp login <name>` for OAuth, `yolop mcp enable <name>` / `yolop mcp disable <name>` to toggle, `yolop mcp add ...` to add, `yolop mcp remove <name>` to remove. Do not guess `.mcp.json` paths or environment keys; use `yolop mcp ...` instead.\n</capability>",
    );
    prompt
}

#[async_trait]
impl Capability for McpCapability {
    fn id(&self) -> &str {
        MCP_CAPABILITY_ID
    }
    fn name(&self) -> &str {
        "MCP"
    }
    fn description(&self) -> &str {
        "Manage global and workspace Model Context Protocol server configuration."
    }
    fn status(&self) -> CapabilityStatus {
        CapabilityStatus::Available
    }
    fn category(&self) -> Option<&str> {
        Some("Extensibility")
    }

    // MCP servers are invisible to tool search, so name them here the way the
    // skills capability names discoverable skills. The control route stays the
    // full discovery surface; this block keeps agents from guessing at
    // `.mcp.json` paths or environment keys.
    async fn system_prompt_contribution(&self, _ctx: &SystemPromptContext) -> Option<String> {
        let servers = self.store.effective().ok().map(|config| config.servers)?;
        Some(render_mcp_prompt(&servers))
    }

    fn system_prompt_preview(&self) -> Option<String> {
        let servers = self
            .store
            .effective()
            .map(|config| config.servers)
            .unwrap_or_default();
        Some(render_mcp_prompt(&servers))
    }

    fn tools(&self) -> Vec<Box<dyn Tool>> {
        // Fully CLI-driven (`yolop mcp ...` everywhere, `/mcp ...`
        // terminal-only reached through run_command). The read-only list is
        // served by the control route below; no model-invoked tools remain.
        Vec::new()
    }
}

#[async_trait]
impl ControlCapability for McpCapability {
    fn control_route(&self) -> ControlRoute {
        MCP_CONTROL_ROUTE
    }

    async fn execute_control(&self, action: &Value) -> ToolExecutionResult {
        let operation = action
            .get("operation")
            .and_then(Value::as_str)
            .unwrap_or("");
        if operation != "list" {
            return ToolExecutionResult::tool_error(format!(
                "unknown or write operation `{operation}`: manage MCP servers with `yolop mcp ...`"
            ));
        }
        match self.store.effective() {
            Ok(effective) => ToolExecutionResult::success(json!({
                "ok": true,
                "global_path": effective.global_path,
                "workspace_path": effective.workspace_path,
                "servers": effective.servers,
            })),
            Err(err) => ToolExecutionResult::tool_error(err),
        }
    }

    fn render_control(&self, _action: &Value, response: &ControlResponse) -> String {
        response.render_default()
    }
}

// MCP mutations live on the command path (`yolop mcp ...`, `/mcp ...`);
// this capability intentionally exposes no model-invoked tools.

#[cfg(test)]
mod tests {
    use super::*;

    fn test_store() -> (tempfile::TempDir, Arc<McpConfigStore>) {
        let tmp = tempfile::tempdir().expect("tmp");
        let store = Arc::new(McpConfigStore::new(
            tmp.path().join("settings.toml"),
            tmp.path().to_path_buf(),
        ));
        (tmp, store)
    }

    #[test]
    fn tools_exposes_no_model_tools() {
        let (_tmp, store) = test_store();
        let capability = McpCapability { store };
        assert!(capability.tools().is_empty());
    }

    #[test]
    fn control_route_points_at_mcp_cli() {
        let (_tmp, store) = test_store();
        let capability = McpCapability { store };
        let route = capability.control_route();
        assert_eq!(route.resource, "mcp");
        assert_eq!(route.cli_subcommand, "mcp");
        assert_eq!(route.read_only_operations, &["list"]);
    }

    #[tokio::test]
    async fn control_list_reports_effective_configuration() {
        let (_tmp, store) = test_store();
        let capability = McpCapability { store };
        let response = capability
            .execute_control(&json!({ "operation": "list" }))
            .await;
        let ToolExecutionResult::Success(value) = response else {
            panic!("expected JSON success, got {response:?}");
        };
        assert_eq!(value["ok"], true);
        assert!(value["servers"].is_array());
    }

    #[tokio::test]
    async fn control_rejects_mutations_toward_cli() {
        let (_tmp, store) = test_store();
        let capability = McpCapability { store };
        let response = capability
            .execute_control(&json!({ "operation": "remove", "name": "x" }))
            .await;
        let ToolExecutionResult::ToolError(message) = response else {
            panic!("expected error, got {response:?}");
        };
        assert!(message.contains("yolop mcp"), "{message}");
    }

    fn seed_server(store: &McpConfigStore, name: &str) {
        use crate::config::mcp::{McpConfigScope, McpServerEntry};
        use everruns_core::{McpServerTransportType, ScopedMcpServer};

        store
            .upsert(
                McpConfigScope::Global,
                name,
                McpServerEntry {
                    enabled: true,
                    server: ScopedMcpServer {
                        transport_type: McpServerTransportType::Http,
                        url: "https://example.com/mcp".to_string(),
                        ..Default::default()
                    },
                },
            )
            .expect("seed server");
    }

    #[tokio::test]
    async fn system_prompt_lists_servers_and_discovery() {
        let (_tmp, store) = test_store();
        seed_server(&store, "linear");
        let capability = McpCapability { store };
        let prompt = capability
            .system_prompt_contribution(&SystemPromptContext::without_file_store(
                everruns_provider::typed_id::SessionId::new(),
            ))
            .await
            .expect("mcp prompt");
        assert!(prompt.contains("linear"), "{prompt}");
        assert!(prompt.contains("yolop mcp list"), "{prompt}");
        assert!(prompt.contains("yolop mcp login"), "{prompt}");
        assert!(prompt.contains("yolop mcp enable"), "{prompt}");
    }

    #[tokio::test]
    async fn system_prompt_truncates_long_server_lists() {
        let (_tmp, store) = test_store();
        for i in 0..12 {
            seed_server(&store, &format!("server-{i:02}"));
        }
        let capability = McpCapability { store };
        let prompt = capability
            .system_prompt_contribution(&SystemPromptContext::without_file_store(
                everruns_provider::typed_id::SessionId::new(),
            ))
            .await
            .expect("mcp prompt");
        assert!(prompt.contains("server-00"), "{prompt}");
        assert!(!prompt.contains("server-11"), "{prompt}");
        assert!(prompt.contains("more"), "{prompt}");
        assert!(prompt.contains("yolop mcp list"), "{prompt}");
    }

    #[test]
    fn system_prompt_preview_mentions_discovery_when_empty() {
        let (_tmp, store) = test_store();
        let capability = McpCapability { store };
        let prompt = capability.system_prompt_preview().expect("mcp preview");
        assert!(prompt.contains("yolop mcp list"), "{prompt}");
        assert!(prompt.contains("yolop mcp show"), "{prompt}");
    }
}

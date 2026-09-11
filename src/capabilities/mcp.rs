use crate::config::mcp::McpConfigStore;
use crate::control::{ControlCapability, ControlResponse, ControlRoute};
use async_trait::async_trait;
use everruns_core::{Capability, CapabilityStatus, Tool, ToolExecutionResult};
use serde_json::{Value, json};
use std::sync::Arc;

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

    // No system-prompt contribution: everything lives on the control route
    // (`yolop mcp ...`), documented there.

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
}

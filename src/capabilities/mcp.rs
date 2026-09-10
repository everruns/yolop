use crate::capabilities::narration::stable_labeled;
use crate::config::mcp::McpConfigStore;
use async_trait::async_trait;
use everruns_core::tool_narration::ToolNarrationPhase;
use everruns_core::{Capability, CapabilityStatus};
use everruns_core::{Tool, ToolExecutionResult};
use everruns_provider::ToolCall;
use serde_json::{Value, json};
use std::sync::Arc;

pub(crate) const MCP_CAPABILITY_ID: &str = "mcp";

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

    // No system-prompt contribution: mutations live on the command path
    // (`yolop mcp ...`, `/mcp ...` via run_command), documented there.

    fn tools(&self) -> Vec<Box<dyn Tool>> {
        // Mutations are CLI-only (`yolop mcp ...` and `/mcp ...` reached through
        // run_command). The model gets the read-only list; nothing here competes
        // with the command path, so there is no second reload story to forget.
        vec![Box::new(ListMcpServersTool {
            store: self.store.clone(),
        })]
    }
}

struct ListMcpServersTool {
    store: Arc<McpConfigStore>,
}

#[async_trait]
impl Tool for ListMcpServersTool {
    fn narrate(
        &self,
        _tool_call: &ToolCall,
        phase: ToolNarrationPhase,
        _locale: Option<&str>,
        _ctx: everruns_core::tool_narration::ToolNarrationContext<'_>,
    ) -> Option<String> {
        Some(stable_labeled("List MCP servers", None, phase))
    }
    fn name(&self) -> &str {
        "list_mcp_servers"
    }
    fn display_name(&self) -> Option<&str> {
        Some("List MCP servers")
    }
    fn description(&self) -> &str {
        "List global and workspace MCP server configuration, including enabled state and override source."
    }
    fn parameters_schema(&self) -> Value {
        json!({ "type": "object", "properties": {}, "additionalProperties": false })
    }
    async fn execute(&self, _arguments: Value) -> ToolExecutionResult {
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
}

// MCP mutations live on the command path (`yolop mcp ...`, `/mcp ...`);
// this capability intentionally exposes no mutation tools.

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
    fn tools_exposes_only_read_operations() {
        let (_tmp, store) = test_store();
        let capability = McpCapability { store };
        let names: Vec<_> = capability
            .tools()
            .iter()
            .map(|tool| tool.name().to_string())
            .collect();
        assert_eq!(names, vec!["list_mcp_servers".to_string()]);
    }

    #[tokio::test]
    async fn list_tool_reports_effective_configuration() {
        let (_tmp, store) = test_store();
        let tool = ListMcpServersTool {
            store: store.clone(),
        };
        let result = tool.execute(json!({})).await;
        let everruns_core::ToolExecutionResult::Success(value) = result else {
            panic!("expected JSON success, got {result:?}");
        };
        assert_eq!(value["ok"], true);
        assert!(value["servers"].is_array());
    }
}

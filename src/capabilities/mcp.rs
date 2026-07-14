use crate::capabilities::narration::stable_labeled;
use crate::mcp_config::{McpConfigScope, McpConfigStore, McpServerEntry};
use async_trait::async_trait;
use everruns_core::capabilities::{Capability, CapabilityStatus, SystemPromptContext};
use everruns_core::tool_narration::ToolNarrationPhase;
use everruns_core::tool_types::ToolCall;
use everruns_core::tools::{Tool, ToolExecutionResult};
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

    async fn system_prompt_contribution(&self, _ctx: &SystemPromptContext) -> Option<String> {
        Some(
            "<capability id=\"mcp\">\n\
              Manage global and workspace MCP servers with `list_mcp_servers`, \
              `upsert_mcp_server`, `remove_mcp_server`, and `set_mcp_server_enabled`. \
              Global servers live in settings.toml under `[mcp.servers.<name>]`; workspace \
              servers live in `.mcp.json` and override global servers by name. Config changes \
              take effect on the next `/mcp reload` (or a new session); use `enabled=false` to \
              deactivate without deleting.\n\
              </capability>"
                .to_string(),
        )
    }

    fn system_prompt_preview(&self) -> Option<String> {
        Some("<capability id=\"mcp\">Manage global/workspace MCP servers with list/upsert/remove/enable tools.</capability>".to_string())
    }

    fn tools(&self) -> Vec<Box<dyn Tool>> {
        vec![
            Box::new(ListMcpServersTool {
                store: self.store.clone(),
            }),
            Box::new(UpsertMcpServerTool {
                store: self.store.clone(),
            }),
            Box::new(RemoveMcpServerTool {
                store: self.store.clone(),
            }),
            Box::new(SetMcpServerEnabledTool {
                store: self.store.clone(),
            }),
        ]
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

struct UpsertMcpServerTool {
    store: Arc<McpConfigStore>,
}

#[async_trait]
impl Tool for UpsertMcpServerTool {
    fn narrate(
        &self,
        _tool_call: &ToolCall,
        phase: ToolNarrationPhase,
        _locale: Option<&str>,
        _ctx: everruns_core::tool_narration::ToolNarrationContext<'_>,
    ) -> Option<String> {
        Some(stable_labeled("Upsert MCP server", None, phase))
    }
    fn name(&self) -> &str {
        "upsert_mcp_server"
    }
    fn display_name(&self) -> Option<&str> {
        Some("Upsert MCP server")
    }
    fn description(&self) -> &str {
        "Create or replace one global or workspace MCP server. Changes take effect on the next `/mcp reload` or a new session."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "scope": { "type": "string", "enum": ["global", "workspace"], "default": "global" },
                "name": { "type": "string", "minLength": 1 },
                "server": {
                    "type": "object",
                    "description": "MCP server config. Supports remote HTTP servers and OAuth-capable metadata such as auth_mode='oauth', oauth_provider_id, headers, and enabled=false.",
                    "additionalProperties": true
                }
            },
            "required": ["name", "server"],
            "additionalProperties": false
        })
    }
    async fn execute(&self, arguments: Value) -> ToolExecutionResult {
        let scope = parse_scope(arguments.get("scope"));
        let name = match arguments
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(name) => name.to_string(),
            None => return ToolExecutionResult::tool_error("name is required"),
        };
        let server_value = match arguments.get("server") {
            Some(value) => value.clone(),
            None => return ToolExecutionResult::tool_error("server is required"),
        };
        let entry: McpServerEntry = match serde_json::from_value(server_value) {
            Ok(entry) => entry,
            Err(err) => {
                return ToolExecutionResult::tool_error(format!(
                    "invalid MCP server config: {err}"
                ));
            }
        };
        match self.store.upsert(scope, &name, entry) {
            Ok(summary) => ToolExecutionResult::success(json!({ "ok": true, "server": summary })),
            Err(err) => ToolExecutionResult::tool_error(err),
        }
    }
}

struct RemoveMcpServerTool {
    store: Arc<McpConfigStore>,
}

#[async_trait]
impl Tool for RemoveMcpServerTool {
    fn narrate(
        &self,
        _tool_call: &ToolCall,
        phase: ToolNarrationPhase,
        _locale: Option<&str>,
        _ctx: everruns_core::tool_narration::ToolNarrationContext<'_>,
    ) -> Option<String> {
        Some(stable_labeled("Remove MCP server", None, phase))
    }
    fn name(&self) -> &str {
        "remove_mcp_server"
    }
    fn display_name(&self) -> Option<&str> {
        Some("Remove MCP server")
    }
    fn description(&self) -> &str {
        "Remove one MCP server from global settings or workspace .mcp.json."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "scope": { "type": "string", "enum": ["global", "workspace"], "default": "global" },
                "name": { "type": "string", "minLength": 1 }
            },
            "required": ["name"],
            "additionalProperties": false
        })
    }
    async fn execute(&self, arguments: Value) -> ToolExecutionResult {
        let scope = parse_scope(arguments.get("scope"));
        let name = match arguments
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(name) => name,
            None => return ToolExecutionResult::tool_error("name is required"),
        };
        match self.store.remove(scope, name) {
            Ok(removed) => ToolExecutionResult::success(json!({ "ok": true, "removed": removed })),
            Err(err) => ToolExecutionResult::tool_error(err),
        }
    }
}

struct SetMcpServerEnabledTool {
    store: Arc<McpConfigStore>,
}

#[async_trait]
impl Tool for SetMcpServerEnabledTool {
    fn narrate(
        &self,
        _tool_call: &ToolCall,
        phase: ToolNarrationPhase,
        _locale: Option<&str>,
        _ctx: everruns_core::tool_narration::ToolNarrationContext<'_>,
    ) -> Option<String> {
        Some(stable_labeled("Set MCP server enabled", None, phase))
    }
    fn name(&self) -> &str {
        "set_mcp_server_enabled"
    }
    fn display_name(&self) -> Option<&str> {
        Some("Set MCP server enabled")
    }
    fn description(&self) -> &str {
        "Enable or disable one MCP server without deleting it. Changes take effect on the next `/mcp reload` or a new session."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "scope": { "type": "string", "enum": ["global", "workspace"], "default": "global" },
                "name": { "type": "string", "minLength": 1 },
                "enabled": { "type": "boolean" }
            },
            "required": ["name", "enabled"],
            "additionalProperties": false
        })
    }
    async fn execute(&self, arguments: Value) -> ToolExecutionResult {
        let scope = parse_scope(arguments.get("scope"));
        let name = match arguments
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(name) => name,
            None => return ToolExecutionResult::tool_error("name is required"),
        };
        let enabled = match arguments.get("enabled").and_then(Value::as_bool) {
            Some(enabled) => enabled,
            None => return ToolExecutionResult::tool_error("enabled is required"),
        };
        match self.store.set_enabled(scope, name, enabled) {
            Ok(summary) => ToolExecutionResult::success(json!({ "ok": true, "server": summary })),
            Err(err) => ToolExecutionResult::tool_error(err),
        }
    }
}

fn parse_scope(value: Option<&Value>) -> McpConfigScope {
    match value.and_then(Value::as_str) {
        Some("workspace") | Some("local") => McpConfigScope::Workspace,
        _ => McpConfigScope::Global,
    }
}

use crate::capabilities::narration::stable_labeled;
use crate::config::mcp::{McpConfigScope, McpConfigStore, McpServerEntry};
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
    pub(crate) allow_literal_credentials: bool,
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

    // No system-prompt contribution: the four tool descriptions already name the
    // verbs and the `/mcp reload` timing, and the file layout moved into
    // `upsert_mcp_server`'s description where it is needed to fill in `scope`.

    fn tools(&self) -> Vec<Box<dyn Tool>> {
        vec![
            Box::new(ListMcpServersTool {
                store: self.store.clone(),
            }),
            Box::new(UpsertMcpServerTool {
                store: self.store.clone(),
                allow_literal_credentials: self.allow_literal_credentials,
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
    allow_literal_credentials: bool,
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
        "Create or replace one global or workspace MCP server. Global servers live in \
         settings.toml under `[mcp.servers.<name>]`; workspace servers live in `.mcp.json` and \
         override global servers by name. Changes take effect on the next `/mcp reload` or a \
         new session."
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
        if !self.allow_literal_credentials
            && let Some(field) = literal_credential_field(&server_value)
        {
            return ToolExecutionResult::tool_error(format!(
                "ACP cannot enter credentials securely through model tools; replace literal `{field}` with an environment placeholder such as `${{MCP_TOKEN}}`, or use `/mcp login`"
            ));
        }
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

fn literal_credential_field(server: &Value) -> Option<String> {
    for container in ["headers", "env"] {
        let Some(fields) = server.get(container).and_then(Value::as_object) else {
            continue;
        };
        for (name, value) in fields {
            let Some(value) = value.as_str() else {
                continue;
            };
            if credential_field_name(name) && !credential_value_uses_placeholder(name, value) {
                return Some(format!("{container}.{name}"));
            }
        }
    }
    None
}

fn credential_field_name(name: &str) -> bool {
    let normalized = name.to_ascii_lowercase().replace('_', "-");
    matches!(
        normalized.as_str(),
        "authorization"
            | "proxy-authorization"
            | "cookie"
            | "set-cookie"
            | "api-key"
            | "x-api-key"
            | "x-auth-token"
    ) || normalized.contains("token")
        || normalized.contains("secret")
        || normalized.contains("api-key")
        || normalized.contains("apikey")
}

fn credential_value_uses_placeholder(name: &str, value: &str) -> bool {
    let value = value.trim();
    if is_env_placeholder(value) {
        return true;
    }
    let normalized = name.to_ascii_lowercase().replace('_', "-");
    if matches!(normalized.as_str(), "authorization" | "proxy-authorization")
        && let Some((scheme, credential)) = value.split_once(char::is_whitespace)
    {
        return matches!(scheme.to_ascii_lowercase().as_str(), "bearer" | "basic")
            && is_env_placeholder(credential.trim());
    }
    false
}

fn is_env_placeholder(value: &str) -> bool {
    value
        .strip_prefix("${")
        .and_then(|value| value.strip_suffix('}'))
        .is_some_and(|name| {
            !name.is_empty()
                && name
                    .chars()
                    .all(|character| character == '_' || character.is_ascii_alphanumeric())
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn upsert_tool(root: &std::path::Path, allow_literal_credentials: bool) -> UpsertMcpServerTool {
        UpsertMcpServerTool {
            store: Arc::new(McpConfigStore::new(
                root.join("settings.toml"),
                root.to_path_buf(),
            )),
            allow_literal_credentials,
        }
    }

    #[tokio::test]
    async fn acp_rejects_literal_mcp_credentials() {
        let tmp = tempfile::tempdir().expect("tmp");
        let result = upsert_tool(tmp.path(), false)
            .execute(json!({
                "name": "docs",
                "server": {
                    "type": "http",
                    "url": "https://example.com/mcp",
                    "headers": { "Authorization": "Bearer secret" }
                }
            }))
            .await;

        match result {
            ToolExecutionResult::ToolError(message) => {
                assert!(message.contains("ACP cannot enter credentials securely"));
                assert!(message.contains("headers.Authorization"));
            }
            other => panic!("expected credential rejection, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn acp_accepts_mcp_environment_placeholders() {
        let tmp = tempfile::tempdir().expect("tmp");
        let result = upsert_tool(tmp.path(), false)
            .execute(json!({
                "name": "docs",
                "server": {
                    "type": "http",
                    "url": "https://example.com/mcp",
                    "headers": { "Authorization": "Bearer ${DOCS_TOKEN}" }
                }
            }))
            .await;

        assert!(result.is_success(), "{result:?}");
    }

    #[tokio::test]
    async fn acp_rejects_mixed_literal_and_placeholder_credentials() {
        let tmp = tempfile::tempdir().expect("tmp");
        let result = upsert_tool(tmp.path(), false)
            .execute(json!({
                "name": "docs",
                "server": {
                    "type": "http",
                    "url": "https://example.com/mcp",
                    "headers": {
                        "Authorization": "Bearer literal-${DOCS_TOKEN}"
                    }
                }
            }))
            .await;

        assert!(result.is_error(), "{result:?}");
    }
}

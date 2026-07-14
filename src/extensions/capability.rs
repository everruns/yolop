//! `ExtensionCapability` — the generic adapter that lets one installed
//! extension package fulfil the everruns `Capability` contract. Static
//! answers (identity, config schema, tool definitions) come from the
//! manifest; execution and the prompt contribution are proxied to the
//! package's capability server over YEP (see `manager.rs`).

use super::manager::{DEFAULT_REQUEST_TIMEOUT_MS, ExtensionProcess, ExtensionProcessSpec};
use super::package::{ExtensionPackage, ToolDefinition, extension_capability_id};
use async_trait::async_trait;
use everruns_core::capabilities::{Capability, SystemPromptContext};
use everruns_core::tools::{Tool, ToolExecutionResult};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Per-extension config keys yolop owns (everything else is passed to the
/// server verbatim in `initialize.config`).
const CONFIG_REQUEST_TIMEOUT_MS: &str = "request_timeout_ms";

pub struct ExtensionCapability {
    id: String,
    package: ExtensionPackage,
    workspace_root: PathBuf,
    /// Process shared by all tool instances so the server persists across
    /// turns; rebuilt (killing the old server) when the config changes —
    /// the same cache-by-config pattern as `LspCapability::manager_for`.
    process: Mutex<Option<(Value, Arc<ExtensionProcess>)>>,
}

impl ExtensionCapability {
    pub fn new(package: ExtensionPackage, workspace_root: PathBuf) -> Self {
        Self {
            id: extension_capability_id(&package.manifest.name),
            package,
            workspace_root,
            process: Mutex::new(None),
        }
    }

    fn process_for(&self, config: &Value) -> Arc<ExtensionProcess> {
        let mut slot = self.process.lock().expect("extension process lock");
        if let Some((cached_config, process)) = slot.as_ref()
            && cached_config == config
        {
            return process.clone();
        }
        let timeout_ms = config
            .get(CONFIG_REQUEST_TIMEOUT_MS)
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_REQUEST_TIMEOUT_MS)
            .clamp(1_000, 600_000);
        let process = Arc::new(ExtensionProcess::new(ExtensionProcessSpec {
            manifest: self.package.manifest.clone(),
            package_dir: self.package.dir.clone(),
            workspace_root: self.workspace_root.clone(),
            config: config.clone(),
            request_timeout: Duration::from_millis(timeout_ms),
        }));
        *slot = Some((config.clone(), process.clone()));
        process
    }

    /// Tool names this extension asks to keep fully loaded (never deferred
    /// behind `tool_search`), bounded by the spec's per-extension budget.
    pub fn never_defer_tools(&self) -> Vec<String> {
        const NEVER_DEFER_BUDGET: usize = 8;
        self.package
            .manifest
            .tools
            .iter()
            .filter(|tool| tool.never_defer)
            .take(NEVER_DEFER_BUDGET)
            .map(|tool| tool.name.clone())
            .collect()
    }

    /// Manifest-declared MCP servers this extension contributes (D1: an
    /// extension *provides* MCP servers, which yolop consumes through its own
    /// client). Keyed `<extension>__<server>` so two extensions can't collide
    /// on a logical server name and `/mcp` shows provenance. Manifest-declared
    /// means the approved transport shape is exactly what runs.
    pub fn contributed_mcp_servers(&self) -> everruns_core::ScopedMcpServers {
        use super::package::McpTransport;
        use everruns_core::mcp_server::{McpServerTransportType, ScopedMcpServer};
        let mut servers = everruns_core::ScopedMcpServers::new();
        for (name, spec) in &self.package.manifest.mcp_servers {
            let scoped = match spec.transport {
                McpTransport::Stdio => ScopedMcpServer {
                    transport_type: McpServerTransportType::Stdio,
                    command: spec.command.clone(),
                    args: spec.args.clone(),
                    env: spec.env.clone(),
                    ..Default::default()
                },
                McpTransport::Http => ScopedMcpServer {
                    transport_type: McpServerTransportType::Http,
                    url: spec.url.clone().unwrap_or_default(),
                    headers: spec.headers.clone(),
                    ..Default::default()
                },
            };
            servers.insert(format!("{}__{name}", self.package.manifest.name), scoped);
        }
        servers
    }
}

#[async_trait]
impl Capability for ExtensionCapability {
    fn id(&self) -> &str {
        &self.id
    }

    fn name(&self) -> &str {
        &self.package.manifest.name
    }

    fn description(&self) -> &str {
        &self.package.manifest.description
    }

    fn category(&self) -> Option<&str> {
        Some("Extensions")
    }

    fn config_schema(&self) -> Option<Value> {
        self.package.manifest.config_schema.clone()
    }

    fn validate_config(&self, config: &Value) -> Result<(), String> {
        // Structural gate only; full JSON Schema validation is a follow-up.
        // The server re-validates semantically at `initialize`.
        if config.is_null() || config.is_object() {
            Ok(())
        } else {
            Err(format!(
                "config for `{}` must be a table of keys, got {config}",
                self.id
            ))
        }
    }

    fn mcp_servers(&self) -> everruns_core::ScopedMcpServers {
        self.contributed_mcp_servers()
    }

    async fn system_prompt_contribution(&self, _ctx: &SystemPromptContext) -> Option<String> {
        self.system_prompt_contribution_with_config(_ctx, &Value::Null)
            .await
    }

    async fn system_prompt_contribution_with_config(
        &self,
        _ctx: &SystemPromptContext,
        config: &Value,
    ) -> Option<String> {
        if !self.package.manifest.prompt {
            return None;
        }
        // Prompt facet ⇒ eager spawn: the contribution is needed before the
        // first turn, so first prompt assembly starts the server.
        let text = self.process_for(config).prompt_contribution().await?;
        Some(format!(
            "<capability id=\"{}\">\n{}\n</capability>",
            self.id, text
        ))
    }

    fn tools(&self) -> Vec<Box<dyn Tool>> {
        self.tools_with_config(&Value::Null)
    }

    fn tools_with_config(&self, config: &Value) -> Vec<Box<dyn Tool>> {
        let process = self.process_for(config);
        self.package
            .manifest
            .tools
            .iter()
            .map(|definition| {
                Box::new(ExtensionTool {
                    definition: definition.clone(),
                    process: process.clone(),
                }) as Box<dyn Tool>
            })
            .collect()
    }
}

struct ExtensionTool {
    definition: ToolDefinition,
    process: Arc<ExtensionProcess>,
}

#[async_trait]
impl Tool for ExtensionTool {
    fn name(&self) -> &str {
        &self.definition.name
    }

    fn description(&self) -> &str {
        &self.definition.description
    }

    fn parameters_schema(&self) -> Value {
        self.definition.schema.clone()
    }

    async fn execute(&self, arguments: Value) -> ToolExecutionResult {
        // The YEP request id correlates streamed updates; the agent-loop
        // tool_call_id is not visible at this seam, so pass the tool name —
        // servers echo it in observability output only.
        match self
            .process
            .call_tool(&self.definition.name, &self.definition.name, arguments)
            .await
        {
            Ok(result) => ToolExecutionResult::Success(result),
            // Server-reported and transport errors are both actionable for
            // the model (bad args, missing binary, crashed server) and carry
            // no secrets: surface them as tool errors, not internal ones.
            Err(err) => ToolExecutionResult::ToolError(err.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extensions::package::ExtensionPackage;
    use crate::extensions::package::parse_manifest;
    use everruns_core::mcp_server::McpServerTransportType;
    use serde_json::json;

    #[test]
    fn contributed_mcp_servers_are_prefixed_and_shaped() {
        let manifest = parse_manifest(
            &json!({
                "name": "docs", "description": "Docs.",
                "yolop": {
                    "protocol_version": "1.0",
                    "capabilityServer": { "command": "x" },
                    "mcpServers": {
                        "search": { "type": "http", "url": "https://d/mcp",
                                    "headers": { "A": "b" } },
                        "local": { "command": "svc", "args": ["--stdio"] }
                    }
                }
            })
            .to_string(),
        )
        .expect("parse");
        let cap = ExtensionCapability::new(
            ExtensionPackage {
                dir: std::env::temp_dir(),
                manifest,
            },
            std::env::temp_dir(),
        );
        let servers = cap.contributed_mcp_servers();
        // Prefixed with the extension name for provenance + collision safety.
        let http = servers.get("docs__search").expect("http server");
        assert_eq!(http.transport_type, McpServerTransportType::Http);
        assert_eq!(http.url, "https://d/mcp");
        let stdio = servers.get("docs__local").expect("stdio server");
        assert_eq!(stdio.transport_type, McpServerTransportType::Stdio);
        assert_eq!(stdio.command.as_deref(), Some("svc"));
    }
}

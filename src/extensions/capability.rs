//! `ExtensionCapability` — the generic adapter that lets one installed
//! extension package fulfil the everruns `Capability` contract. Static
//! answers (identity, config schema, tool definitions) come from the
//! manifest; execution and the prompt contribution are proxied to the
//! package's capability server over YEP (see `manager.rs`).

use super::client::{AskSink, StatusSink};
use super::manager::{
    DEFAULT_REQUEST_TIMEOUT_MS, ExtensionProcess, ExtensionProcessSpec, LiveProcessRegistry,
};
use super::package::{ExtensionPackage, ToolDefinition, extension_capability_id};
use crate::capabilities::EnvironmentContextRegistry;
use crate::capabilities::narration::stable_labeled;
use async_trait::async_trait;
use everruns_core::command::{
    CommandArg, CommandDescriptor, CommandExecutionContext, CommandResult, CommandSource,
    ExecuteCommandRequest,
};
use everruns_core::tool_narration::ToolNarrationPhase;
use everruns_core::{Capability, SystemPromptContext};
use everruns_core::{Tool, ToolExecutionResult};
use everruns_provider::ToolCall;
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
    /// Where the server's `status/changed` pushes go. Forwarded to the process
    /// only when the manifest declares `status` (the D4 opt-in).
    status_sink: Option<StatusSink>,
    /// Handles the server's `ui/ask` requests. Forwarded to the process only
    /// when the manifest declares `ui_ask` (the D4 opt-in).
    ask_sink: Option<AskSink>,
    /// Shared live-process registry so `reload_extension` can restart this
    /// extension's server mid-session; `None` outside the runtime (tests).
    live_processes: Option<LiveProcessRegistry>,
    /// Per-extension secret store; supplies the injected env for `secret`
    /// config fields. `None` outside the runtime (tests without secrets).
    secrets: Option<super::secrets::ExtensionSecrets>,
    environment_context: Option<EnvironmentContextRegistry>,
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
            status_sink: None,
            ask_sink: None,
            live_processes: None,
            secrets: None,
            environment_context: None,
            process: Mutex::new(None),
        }
    }

    /// Wire the per-extension secret store, so `secret` config fields are
    /// injected into the server's environment at spawn.
    pub fn with_secrets(mut self, secrets: super::secrets::ExtensionSecrets) -> Self {
        self.secrets = Some(secrets);
        self
    }

    /// Wire the shared live-process registry so this extension's server can be
    /// reloaded mid-session via `reload_extension`.
    pub fn with_process_registry(mut self, registry: LiveProcessRegistry) -> Self {
        self.live_processes = Some(registry);
        self
    }

    pub(crate) fn with_environment_context(mut self, registry: EnvironmentContextRegistry) -> Self {
        self.environment_context = Some(registry);
        self
    }

    /// Wire a status-bar sink. The sink is used only if the manifest declares
    /// `status`; a non-declaring extension can never write the status bar even
    /// if given a sink.
    pub fn with_status_sink(mut self, sink: Option<StatusSink>) -> Self {
        self.status_sink = sink;
        self
    }

    /// Wire a `ui/ask` handler. Used only if the manifest declares `ui_ask`.
    pub fn with_ask_sink(mut self, sink: Option<AskSink>) -> Self {
        self.ask_sink = sink;
        self
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
        let manifest = &self.package.manifest;
        // Env injection: `secret` fields from the credential store, plus
        // non-secret fields that declare an `env` mapping, sourced from config.
        let mut env = self
            .secrets
            .as_ref()
            .map(|s| s.env_overrides(manifest))
            .unwrap_or_default();
        for field in manifest.config_fields() {
            if field.secret {
                continue;
            }
            if let (Some(env_name), Some(value)) = (
                field.env.as_ref(),
                config.get(&field.name).and_then(Value::as_str),
            ) {
                env.insert(env_name.clone(), value.to_string());
            }
        }
        // Cache by the original config (below); pass a secret-stripped copy to
        // the server so a `secret` key never rides `initialize.config`.
        let spec_config = strip_secret_keys(config, manifest);
        let process = Arc::new(ExtensionProcess::new(ExtensionProcessSpec {
            manifest: self.package.manifest.clone(),
            package_dir: self.package.dir.clone(),
            workspace_root: self.workspace_root.clone(),
            config: spec_config,
            env,
            request_timeout: Duration::from_millis(timeout_ms),
            status_sink: self
                .package
                .manifest
                .status
                .then(|| self.status_sink.clone())
                .flatten(),
            ask_sink: self
                .package
                .manifest
                .ui_ask
                .then(|| self.ask_sink.clone())
                .flatten(),
        }));
        // Publish the live handle so `reload_extension` can reach this server.
        if let Some(registry) = &self.live_processes {
            registry.register(&self.package.manifest.name, &process);
        }
        *slot = Some((config.clone(), process.clone()));
        process
    }

    /// The live-server process to forward `trace/event` notifications to,
    /// built with `config` (so it shares the per-config process cache with the
    /// tool/hook/prompt facets), iff this extension declares the `trace` facet.
    /// `None` otherwise — a non-declaring extension never sees the event stream
    /// (D4). The runtime pairs this with a session event subscription to drive
    /// forwarding (see `extensions::trace`).
    pub fn trace_process(&self, config: &Value) -> Option<Arc<ExtensionProcess>> {
        self.package
            .manifest
            .trace
            .then(|| self.process_for(config))
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
        use everruns_core::{McpServerTransportType, ScopedMcpServer};
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
        let manifest = &self.package.manifest;
        if !manifest.prompt && !manifest.dynamic_prompt {
            return None;
        }
        // Prompt facet ⇒ eager spawn: the contribution is needed before the
        // first turn, so first prompt assembly starts the server. A dynamic
        // prompt is recomputed per turn (`prompt/contribution`); it falls
        // back to the static handshake prompt on failure.
        let process = self.process_for(config);
        let text = if manifest.dynamic_prompt {
            match process.dynamic_prompt().await {
                Some(text) => Some(text),
                None => process.prompt_contribution().await,
            }
        } else {
            process.prompt_contribution().await
        };
        if let Some(registry) = &self.environment_context {
            let key = format!("extension_{}", manifest.name);
            if let Some(text) = text {
                registry.set(key, text);
            } else {
                registry.remove(&key);
            }
            None
        } else {
            text.map(|text| format!("<capability id=\"{}\">\n{}\n</capability>", self.id, text))
        }
    }

    fn pre_tool_use_hooks_with_config(
        &self,
        config: &Value,
    ) -> Vec<Arc<dyn everruns_core::tool_hooks::PreToolUseHook>> {
        use super::hooks::ExtensionPreHook;
        use super::package::HookEvent;
        let manifest = &self.package.manifest;
        if manifest.hooks.is_empty() {
            return Vec::new();
        }
        let process = self.process_for(config);
        manifest
            .hooks
            .iter()
            .filter(|sub| sub.event == HookEvent::PreToolUse)
            .map(|sub| {
                Arc::new(ExtensionPreHook {
                    ext_name: manifest.name.clone(),
                    process: process.clone(),
                    sub: sub.clone(),
                }) as Arc<dyn everruns_core::tool_hooks::PreToolUseHook>
            })
            .collect()
    }

    fn post_tool_exec_hooks_with_config(
        &self,
        config: &Value,
    ) -> Vec<Arc<dyn everruns_core::tool_hooks::PostToolExecHook>> {
        use super::hooks::ExtensionPostHook;
        use super::package::HookEvent;
        let manifest = &self.package.manifest;
        if manifest.hooks.is_empty() {
            return Vec::new();
        }
        let process = self.process_for(config);
        manifest
            .hooks
            .iter()
            .filter(|sub| sub.event == HookEvent::PostToolUse)
            .map(|sub| {
                Arc::new(ExtensionPostHook {
                    ext_name: manifest.name.clone(),
                    process: process.clone(),
                    sub: sub.clone(),
                }) as Arc<dyn everruns_core::tool_hooks::PostToolExecHook>
            })
            .collect()
    }

    fn commands(&self) -> Vec<CommandDescriptor> {
        let ext = &self.package.manifest.name;
        self.package
            .manifest
            .commands
            .iter()
            .map(|c| CommandDescriptor {
                // Namespaced `<ext>:<name>` so extension commands can never
                // collide with or shadow built-in slash commands.
                name: format!("{ext}:{}", c.name),
                description: c.description.clone(),
                source: CommandSource::System,
                args: c
                    .args
                    .iter()
                    .map(|a| CommandArg {
                        name: a.name.clone(),
                        description: if a.description.is_empty() {
                            a.name.clone()
                        } else {
                            a.description.clone()
                        },
                        required: a.required,
                        suggestions: Vec::new(),
                    })
                    .collect(),
            })
            .collect()
    }

    async fn execute_command(
        &self,
        request: &ExecuteCommandRequest,
        _ctx: &CommandExecutionContext,
    ) -> everruns_provider::error::Result<CommandResult> {
        let prefix = format!("{}:", self.package.manifest.name);
        let name = request.name.strip_prefix(&prefix).unwrap_or(&request.name);
        if !self
            .package
            .manifest
            .commands
            .iter()
            .any(|c| c.name == name)
        {
            return Err(everruns_provider::error::AgentLoopError::config(format!(
                "extension `{}` does not provide command `{name}`",
                self.package.manifest.name
            )));
        }
        let arguments = request.arguments.as_deref().unwrap_or("");
        let process = self.process_for(&Value::Null);
        Ok(match process.execute_command(name, arguments).await {
            Ok(result) => CommandResult {
                success: result.success,
                message: result.message,
                error_code: None,
                error_fields: None,
            },
            Err(err) => CommandResult {
                success: false,
                message: format!("command failed: {err}"),
                error_code: None,
                error_fields: None,
            },
        })
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

/// Remove any `secret`-declared key from a config object so a credential never
/// travels via `initialize.config` (secrets are injected as env instead). A
/// non-object config is returned unchanged.
fn strip_secret_keys(config: &Value, manifest: &super::package::ExtensionManifest) -> Value {
    let secret_names: Vec<String> = manifest
        .secret_fields()
        .into_iter()
        .map(|f| f.name)
        .collect();
    if secret_names.is_empty() {
        return config.clone();
    }
    match config {
        Value::Object(map) => {
            let mut map = map.clone();
            for name in &secret_names {
                map.remove(name);
            }
            Value::Object(map)
        }
        other => other.clone(),
    }
}

struct ExtensionTool {
    definition: ToolDefinition,
    process: Arc<ExtensionProcess>,
}

#[async_trait]
impl Tool for ExtensionTool {
    fn narrate(
        &self,
        _tool_call: &ToolCall,
        phase: ToolNarrationPhase,
        locale: Option<&str>,
        _ctx: everruns_core::tool_narration::ToolNarrationContext<'_>,
    ) -> Option<String> {
        let _ = locale;
        let mut label = self.definition.name.replace(['_', '-'], " ");
        if let Some(first) = label.get_mut(..1) {
            first.make_ascii_uppercase();
        }
        Some(stable_labeled(&label, None, phase))
    }

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
    use everruns_core::McpServerTransportType;
    use serde_json::json;

    #[test]
    fn extension_tool_narration_uses_manifest_tool_name() {
        let manifest = parse_manifest(
            &json!({
                "name": "docs", "description": "Docs.",
                "yolop": {
                    "protocol_version": "1.0",
                    "capabilityServer": { "command": "x" },
                    "tools": [{
                        "name": "search_docs",
                        "description": "Search documentation.",
                        "inputSchema": { "type": "object" }
                    }]
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
        let tool = cap.tools().remove(0);
        let call = ToolCall {
            id: "call-1".into(),
            name: "search_docs".into(),
            arguments: json!({ "query": "narration" }),
        };

        assert_eq!(
            tool.narrate(
                &call,
                ToolNarrationPhase::Started,
                None,
                everruns_core::tool_narration::ToolNarrationContext::default(),
            ),
            Some("Search docs".into())
        );
    }

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

//! `run_command`: the agent's door into the session's command registry.
//!
//! Slash commands are contributed by capabilities and listed by
//! `runtime.list_commands`, one registry per session (see
//! [`crate::capabilities::client_commands`] for the terminal-side ones and
//! `knowledge/specs/commands.md` for the contract). This capability gives the
//! model the same door: `run_command` resolves a name against that live
//! registry and executes it through `runtime.execute_command`, exactly as
//! typing the slash command would. It keeps no allowlist, so a command becomes
//! agent-reachable the moment a capability, extension, or MCP server registers
//! it.
//!
//! Registered on every host. The registry differs per host (the TUI adds
//! terminal commands ACP and `--print` never see), but the mechanism does not:
//! whatever a host advertises, the agent can run. The optional [`HostUi`] port
//! is the one host-specific detail: when a terminal is present, the informational
//! terminal commands (`/mcp`, `/tools`, `/help`, `/cwd`) are dispatched through
//! it so the tool result carries the transcript lines the host printed, instead
//! of the empty `CommandResult` those commands return by design.

use crate::capabilities::client_commands::ui_command_for;
use crate::capabilities::narration::stable_labeled;
use crate::tui::host_ui::HostUi;
use async_trait::async_trait;
use everruns_core::command::{CommandDescriptor, CommandResult, CommandSource};
use everruns_core::tool_narration::{ToolNarrationPhase, arg_str, truncate};
use everruns_core::{Capability, CapabilityStatus};
use everruns_core::{Tool, ToolExecutionResult};
use everruns_provider::ToolCall;
use serde_json::{Value, json};
use std::sync::Arc;

pub(crate) const AGENT_COMMANDS_CAPABILITY_ID: &str = "yolop_agent_commands";

pub(crate) const AGENT_COMMANDS_PROMPT: &str = r#"<capability id="yolop_agent_commands">
`run_command` runs any slash command this session registers, not a fixed subset:
`/setup status|login|reauthenticate <provider>`, `/background`, `/undo`,
`/rewind`, `/goal`, and whatever else the host registers. Use `command: help`
for the live list; an unknown name returns the available ones. Prefer running a
command over telling the user to type it. Skill commands activate by prompt, so
follow the skill instead. The result carries the command's own output.
</capability>"#;

/// Registry port for `run_command`: list the commands this session actually has
/// and execute one through the runtime, the same path a typed slash command
/// takes. Injected rather than hard-coded so the tool never keeps its own
/// allowlist, and so commands contributed later (extensions, MCP, upstream
/// capabilities) are reachable the moment they are registered.
#[async_trait]
pub(crate) trait CommandDispatch: Send + Sync {
    async fn list(&self) -> everruns_provider::error::Result<Vec<CommandDescriptor>>;

    async fn execute(
        &self,
        name: &str,
        arguments: Option<String>,
    ) -> everruns_provider::error::Result<CommandResult>;
}

pub(crate) struct AgentCommandsCapability {
    dispatch: Arc<dyn CommandDispatch>,
    /// Present only on hosts with a terminal transcript to read back.
    ui: Option<Arc<dyn HostUi>>,
}

impl AgentCommandsCapability {
    pub(crate) fn new(dispatch: Arc<dyn CommandDispatch>, ui: Option<Arc<dyn HostUi>>) -> Self {
        Self { dispatch, ui }
    }
}

#[async_trait]
impl Capability for AgentCommandsCapability {
    fn id(&self) -> &str {
        AGENT_COMMANDS_CAPABILITY_ID
    }
    fn name(&self) -> &str {
        "Agent Commands"
    }
    fn description(&self) -> &str {
        "Runs this session's registered slash commands on the model's behalf."
    }
    fn status(&self) -> CapabilityStatus {
        CapabilityStatus::Available
    }
    fn category(&self) -> Option<&str> {
        Some("Examples")
    }
    fn system_prompt_addition(&self) -> Option<&str> {
        Some(AGENT_COMMANDS_PROMPT)
    }

    fn tools(&self) -> Vec<Box<dyn Tool>> {
        vec![Box::new(RunCommandTool {
            dispatch: self.dispatch.clone(),
            ui: self.ui.clone(),
        })]
    }
}

struct RunCommandTool {
    dispatch: Arc<dyn CommandDispatch>,
    ui: Option<Arc<dyn HostUi>>,
}

impl RunCommandTool {
    /// Run `name` through the live registry, mirroring the host's typed-slash
    /// dispatch: unknown names list what is available, `Skill` commands stay
    /// prompt-activated, and missing required arguments are reported instead of
    /// sent on.
    async fn dispatch_registry_command(
        &self,
        name: &str,
        arg: Option<String>,
    ) -> ToolExecutionResult {
        let commands = match self.dispatch.list().await {
            Ok(commands) => commands,
            Err(error) => {
                return ToolExecutionResult::tool_error(format!("list yolop commands: {error}"));
            }
        };
        let available: Vec<&str> = commands.iter().map(|c| c.name.as_str()).collect();
        let Some(descriptor) = commands.iter().find(|c| c.name == name) else {
            // `help` is the documented discovery call, so answer it from the
            // registry on hosts that have no `/help` command of their own.
            if name == "help" {
                return ToolExecutionResult::success(json!({
                    "success": true,
                    "command": "/help",
                    "message": format!("available commands: {}", available.join(", "))
                }));
            }
            return ToolExecutionResult::tool_error(format!(
                "unknown yolop command: /{name}; available: {}",
                available.join(", ")
            ));
        };
        if descriptor.source == CommandSource::Skill {
            return ToolExecutionResult::tool_error(format!(
                "/{name} is a skill command: it activates by prompt, not by run_command. \
                 Follow the skill's instructions directly."
            ));
        }
        let missing: Vec<&str> = descriptor
            .args
            .iter()
            .filter(|a| a.required)
            .map(|a| a.name.as_str())
            .collect();
        if arg.is_none() && !missing.is_empty() {
            return ToolExecutionResult::tool_error(format!(
                "/{name} requires: {}",
                missing.join(", ")
            ));
        }

        let rendered = rendered_command(name, arg.as_deref());
        match self.dispatch.execute(name, arg).await {
            Ok(result) if result.success => {
                let message = if result.message.is_empty() {
                    format!("command applied: {rendered}")
                } else {
                    result.message
                };
                ToolExecutionResult::success(json!({
                    "success": true,
                    "command": rendered,
                    "message": message
                }))
            }
            Ok(result) if result.message.is_empty() => {
                ToolExecutionResult::tool_error(format!("{rendered} failed"))
            }
            Ok(result) => ToolExecutionResult::tool_error(result.message),
            Err(error) => ToolExecutionResult::tool_error(format!("{rendered} failed: {error}")),
        }
    }

    /// Ask the terminal host to apply an informational client command and hand
    /// back the lines it printed. Only reachable when a `HostUi` is present.
    async fn request_from_host(
        &self,
        ui: &Arc<dyn HostUi>,
        name: &str,
        arg: Option<String>,
    ) -> ToolExecutionResult {
        let Some(command) = ui_command_for(name, arg.clone()) else {
            return self.dispatch_registry_command(name, arg).await;
        };
        let rendered = rendered_command(name, arg.as_deref());
        let reply_rx = ui.request(command);
        let message = match tokio::time::timeout(std::time::Duration::from_secs(15), reply_rx).await
        {
            Ok(Ok(messages)) if !messages.is_empty() => messages.join("\n"),
            Ok(_) => format!("command applied: {rendered}"),
            Err(_) => format!(
                "command queued for the interactive terminal host ({rendered}); \
                 host did not return output in time"
            ),
        };
        ToolExecutionResult::success(json!({
            "success": true,
            "command": rendered,
            "message": message
        }))
    }
}

fn rendered_command(name: &str, arg: Option<&str>) -> String {
    match arg {
        Some(arg) => format!("/{name} {arg}"),
        None => format!("/{name}"),
    }
}

/// Whether `run_command` should wait for host transcript lines. These commands
/// print to the transcript and return an empty `CommandResult`, so the registry
/// path alone would tell the agent nothing.
fn command_awaits_host_reply(name: &str) -> bool {
    matches!(name, "mcp" | "tools" | "help" | "cwd")
}

#[async_trait]
impl Tool for RunCommandTool {
    fn narrate(
        &self,
        tool_call: &ToolCall,
        phase: ToolNarrationPhase,
        locale: Option<&str>,
        _ctx: everruns_core::tool_narration::ToolNarrationContext<'_>,
    ) -> Option<String> {
        let _ = locale;
        let command = arg_str(&tool_call.arguments, &["command"]).map(|value| truncate(value, 48));
        Some(stable_labeled("Run command", command, phase))
    }

    fn name(&self) -> &str {
        "run_command"
    }

    fn display_name(&self) -> Option<&str> {
        Some("Yolop command")
    }

    fn description(&self) -> &str {
        "Execute any slash command this session registers, on behalf of a natural-language user \
         request: `setup` (`command: setup`, `args: [reauthenticate, codex_browser]`), \
         `background`, `undo`, `redo`, `rewind`, `goal`, and in the terminal also help, tools, \
         mcp, cwd, status, model, effort, clear, and quit. Use `command: help` to list the live \
         command set; an unknown name returns the available ones. Accepts command names with or \
         without the leading slash; `exit` is an alias for `quit`. Skill commands activate by \
         prompt and `shell` is typed-only, so neither runs here."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                // Deliberately not an enum: the command set is per session and
                // grows with capabilities, extensions, and MCP servers, so the
                // live registry validates the name at execution time.
                "command": {
                    "type": "string",
                    "description": "Slash command name, with or without the leading slash, \
                                    e.g. `setup`, `background`, `mcp`, `model`. Use `help` to list them."
                },
                "args": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Ordered command arguments, e.g. [`reload`] for /mcp, \
                                    [`openai/gpt-5.4`] for /model, or [`reauthenticate`, `codex_browser`] for /setup."
                }
            },
            "required": ["command"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, arguments: Value) -> ToolExecutionResult {
        let raw = match arguments.get("command").and_then(Value::as_str) {
            Some(raw) if !raw.trim().is_empty() => raw.trim(),
            _ => return ToolExecutionResult::tool_error("'command' is required"),
        };
        let stripped = raw.trim_start_matches('/');
        let name = if stripped == "exit" { "quit" } else { stripped };
        if name == "shell" {
            return ToolExecutionResult::tool_error(
                "shell commands must be typed directly as !shell <command> or /shell <command>",
            );
        }
        let args = match arguments.get("args") {
            None => Vec::new(),
            Some(Value::Array(args)) => {
                let mut parsed = Vec::with_capacity(args.len());
                for value in args {
                    let Some(value) = value.as_str() else {
                        return ToolExecutionResult::tool_error("'args' must contain only strings");
                    };
                    parsed.push(value.to_string());
                }
                parsed
            }
            Some(_) => {
                return ToolExecutionResult::tool_error("'args' must be an array of strings");
            }
        };
        let arg = (!args.is_empty()).then(|| args.join(" "));

        // Informational terminal commands need the host's transcript lines back
        // so the agent can act on `/mcp` / `/tools` conversationally. Everything
        // else, including the side-effecting terminal commands, goes through the
        // registry, which routes it to the same capability handler.
        match self.ui.as_ref() {
            Some(ui) if command_awaits_host_reply(name) => {
                self.request_from_host(ui, name, arg).await
            }
            _ => self.dispatch_registry_command(name, arg).await,
        }
    }
}

/// A [`CommandDispatch`] that knows no commands. Hosts always inject the real
/// registry; this keeps unit tests honest about there being nothing else to run.
#[cfg(test)]
pub(crate) struct EmptyCommandDispatch;

#[cfg(test)]
#[async_trait]
impl CommandDispatch for EmptyCommandDispatch {
    async fn list(&self) -> everruns_provider::error::Result<Vec<CommandDescriptor>> {
        Ok(Vec::new())
    }

    async fn execute(
        &self,
        name: &str,
        _arguments: Option<String>,
    ) -> everruns_provider::error::Result<CommandResult> {
        Err(everruns_provider::error::AgentLoopError::config(format!(
            "no command registry: /{name}"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::host_ui::{RecordingUi, UiCommand};
    use everruns_core::command::CommandArg;
    use std::sync::Mutex;

    /// Registry stand-in: records what was executed and answers from a fixed
    /// descriptor list, standing in for `runtime.list_commands`.
    struct RecordingDispatch {
        commands: Vec<CommandDescriptor>,
        executed: Mutex<Vec<(String, Option<String>)>>,
    }

    impl RecordingDispatch {
        fn new(commands: Vec<CommandDescriptor>) -> Arc<Self> {
            Arc::new(Self {
                commands,
                executed: Mutex::new(Vec::new()),
            })
        }

        /// A registry shaped like a non-terminal host: `/setup` and `/goal` are
        /// runtime commands, `/ship` is a skill.
        fn registry() -> Arc<Self> {
            Self::new(vec![
                descriptor("setup", &[optional("action")]),
                descriptor("goal", &[required("condition")]),
                descriptor("quit", &[]),
                CommandDescriptor {
                    source: CommandSource::Skill,
                    ..descriptor("ship", &[])
                },
            ])
        }

        fn executed(&self) -> Vec<(String, Option<String>)> {
            self.executed.lock().expect("executed lock").clone()
        }
    }

    #[async_trait]
    impl CommandDispatch for RecordingDispatch {
        async fn list(&self) -> everruns_provider::error::Result<Vec<CommandDescriptor>> {
            Ok(self.commands.clone())
        }

        async fn execute(
            &self,
            name: &str,
            arguments: Option<String>,
        ) -> everruns_provider::error::Result<CommandResult> {
            self.executed
                .lock()
                .expect("executed lock")
                .push((name.to_string(), arguments.clone()));
            Ok(CommandResult {
                success: true,
                message: format!("/{name} ran with {}", arguments.unwrap_or_default()),
                error_code: None,
                error_fields: None,
            })
        }
    }

    fn descriptor(name: &str, args: &[CommandArg]) -> CommandDescriptor {
        CommandDescriptor {
            name: name.to_string(),
            description: name.to_string(),
            source: CommandSource::System,
            args: args.to_vec(),
        }
    }

    fn arg(name: &str, required: bool) -> CommandArg {
        CommandArg {
            name: name.to_string(),
            description: name.to_string(),
            required,
            suggestions: Vec::new(),
        }
    }

    fn optional(name: &str) -> CommandArg {
        arg(name, false)
    }

    fn required(name: &str) -> CommandArg {
        arg(name, true)
    }

    /// A non-terminal host (ACP, `--print`): no `HostUi`, registry only.
    fn headless_tool(dispatch: Arc<RecordingDispatch>) -> RunCommandTool {
        RunCommandTool { dispatch, ui: None }
    }

    /// A terminal host: registry plus the transcript read-back port.
    fn terminal_tool(dispatch: Arc<RecordingDispatch>, ui: Arc<RecordingUi>) -> RunCommandTool {
        RunCommandTool {
            dispatch,
            ui: Some(ui),
        }
    }

    fn message(result: &ToolExecutionResult) -> String {
        match result {
            ToolExecutionResult::Success(value) => value
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            other => panic!("expected Success, got {other:?}"),
        }
    }

    #[test]
    fn prompt_offers_the_whole_registry_to_every_host() {
        let capability = AgentCommandsCapability::new(Arc::new(EmptyCommandDispatch), None)
            .system_prompt_addition()
            .expect("prompt")
            .to_string();

        assert!(capability.contains("run_command"));
        assert!(capability.contains("runs any slash command this session registers"));
        assert!(capability.contains("reauthenticate <provider>"));
        // The prompt wraps this guidance across two lines, check each side.
        assert!(capability.contains("Use `command: help`"));
        assert!(capability.contains("for the live list"));
    }

    #[test]
    fn capability_contributes_run_command_on_every_host() {
        for ui in [
            None,
            Some(Arc::new(RecordingUi::default()) as Arc<dyn HostUi>),
        ] {
            let capability = AgentCommandsCapability::new(Arc::new(EmptyCommandDispatch), ui);
            let names: Vec<String> = capability
                .tools()
                .iter()
                .map(|tool| tool.name().to_string())
                .collect();
            assert_eq!(names, vec!["run_command".to_string()]);
        }
    }

    /// The schema must not pin a command allowlist: the live registry decides
    /// what exists, so any registered command can be named.
    #[test]
    fn run_command_schema_does_not_fix_a_command_allowlist() {
        let schema = headless_tool(RecordingDispatch::registry()).parameters_schema();

        assert!(
            schema["properties"]["command"].get("enum").is_none(),
            "schema: {schema}"
        );
        assert_eq!(schema["properties"]["command"]["type"], json!("string"));
    }

    #[test]
    fn run_command_narration_includes_command() {
        let tool = headless_tool(RecordingDispatch::registry());
        let tool_call = ToolCall {
            id: "call-1".to_string(),
            name: "run_command".to_string(),
            arguments: json!({ "command": "/model" }),
        };

        assert_eq!(
            tool.narrate(
                &tool_call,
                ToolNarrationPhase::Started,
                None,
                everruns_core::tool_narration::ToolNarrationContext::default(),
            ),
            Some("Run command: /model".to_string())
        );
    }

    /// The reported gap: `/setup reauthenticate <provider>` was unreachable
    /// because `run_command` carried its own client-command allowlist.
    #[tokio::test]
    async fn run_command_dispatches_registry_commands() {
        let dispatch = RecordingDispatch::registry();
        let tool = headless_tool(dispatch.clone());

        let result = tool
            .execute(json!({
                "command": "/setup",
                "args": ["reauthenticate", "codex_browser"]
            }))
            .await;

        assert!(result.is_success(), "tool result: {result:?}");
        assert_eq!(
            dispatch.executed(),
            vec![(
                "setup".to_string(),
                Some("reauthenticate codex_browser".to_string())
            )]
        );
        assert_eq!(
            message(&result),
            "/setup ran with reauthenticate codex_browser"
        );
    }

    /// ACP and `--print` have no `HostUi`, so every command, including a
    /// terminal one the host happens to register, goes through the registry.
    #[tokio::test]
    async fn run_command_without_a_host_ui_uses_the_registry() {
        let dispatch = RecordingDispatch::registry();
        let tool = headless_tool(dispatch.clone());

        let result = tool.execute(json!({ "command": "/exit" })).await;

        assert!(result.is_success(), "tool result: {result:?}");
        assert_eq!(dispatch.executed(), vec![("quit".to_string(), None)]);
    }

    /// `help` is the documented discovery call and must answer even on a host
    /// whose registry has no `/help` command.
    #[tokio::test]
    async fn run_command_help_lists_the_live_registry() {
        let tool = headless_tool(RecordingDispatch::registry());

        let result = tool.execute(json!({ "command": "help" })).await;

        assert!(result.is_success(), "tool result: {result:?}");
        let message = message(&result);
        assert!(message.contains("setup"), "message: {message}");
        assert!(message.contains("goal"), "message: {message}");
    }

    #[tokio::test]
    async fn run_command_reports_the_live_command_set_for_unknown_names() {
        let dispatch = RecordingDispatch::registry();
        let tool = headless_tool(dispatch.clone());

        let result = tool.execute(json!({ "command": "nope" })).await;

        assert!(result.is_error(), "tool result: {result:?}");
        let rendered = format!("{result:?}");
        assert!(rendered.contains("setup"), "error should list: {rendered}");
        assert!(dispatch.executed().is_empty());
    }

    /// Skill commands are prompts, not runtime effects; running one from a tool
    /// would re-enter the turn instead of activating the skill.
    #[tokio::test]
    async fn run_command_refuses_skill_commands() {
        let dispatch = RecordingDispatch::registry();
        let tool = headless_tool(dispatch.clone());

        let result = tool.execute(json!({ "command": "ship" })).await;

        assert!(result.is_error(), "tool result: {result:?}");
        assert!(dispatch.executed().is_empty());
    }

    #[tokio::test]
    async fn run_command_reports_missing_required_arguments() {
        let dispatch = RecordingDispatch::registry();
        let tool = headless_tool(dispatch.clone());

        let result = tool.execute(json!({ "command": "goal" })).await;

        assert!(result.is_error(), "tool result: {result:?}");
        assert!(dispatch.executed().is_empty());
    }

    #[tokio::test]
    async fn run_command_rejects_shell_dispatch() {
        let dispatch = RecordingDispatch::registry();
        let tool = headless_tool(dispatch.clone());

        let result = tool
            .execute(json!({
                "command": "shell",
                "args": ["echo", "should-not-run"]
            }))
            .await;

        assert!(result.is_error(), "tool result: {result:?}");
        assert!(dispatch.executed().is_empty());
    }

    /// In the terminal, `/mcp` must come back with the host's own listing so the
    /// agent can act on it (login, reload) instead of inventing a manager window.
    #[tokio::test]
    async fn run_command_mcp_returns_host_output_in_the_terminal() {
        let ui = Arc::new(RecordingUi::default());
        let dispatch = RecordingDispatch::registry();
        let tool = terminal_tool(dispatch.clone(), ui.clone());

        let result = tool.execute(json!({ "command": "mcp", "args": [] })).await;

        assert!(result.is_success(), "tool result: {result:?}");
        let message = message(&result);
        assert!(
            !message.contains("queued for the interactive terminal host"),
            "agent must receive the real /mcp listing: {message}"
        );
        assert!(
            message.contains("MCP") || message.contains("mcp") || message.contains("usage"),
            "tool result should carry the host /mcp response: {message}"
        );
        assert_eq!(ui.take(), vec![UiCommand::ManageMcp { arg: None }]);
        assert!(
            dispatch.executed().is_empty(),
            "informational terminal commands go through the host, not the registry"
        );
    }

    #[tokio::test]
    async fn run_command_dispatches_mcp_reload_through_the_host() {
        let ui = Arc::new(RecordingUi::default());
        let tool = terminal_tool(RecordingDispatch::registry(), ui.clone());

        let result = tool
            .execute(json!({ "command": "mcp", "args": ["reload"] }))
            .await;

        assert!(result.is_success(), "tool result: {result:?}");
        assert_eq!(
            ui.take(),
            vec![UiCommand::ManageMcp {
                arg: Some("reload".into()),
            }]
        );
    }

    /// Side-effecting terminal commands take the registry path, which routes
    /// them to the client capability and on to the same `UiCommand`.
    #[tokio::test]
    async fn run_command_routes_terminal_side_effects_through_the_registry() {
        let ui = Arc::new(RecordingUi::default());
        let dispatch = RecordingDispatch::registry();
        let tool = terminal_tool(dispatch.clone(), ui.clone());

        let result = tool.execute(json!({ "command": "/quit" })).await;

        assert!(result.is_success(), "tool result: {result:?}");
        assert_eq!(dispatch.executed(), vec![("quit".to_string(), None)]);
        assert!(ui.take().is_empty());
    }
}

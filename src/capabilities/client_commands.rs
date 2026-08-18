//! Client-executed slash commands for the TUI host.
//!
//! `help`, `tools`, `mcp`, `cwd`, `status`, `model`, `effort`, `clear`, `/shell`, and
//! `quit` act on the terminal, not the agent runtime. They are declared here as ordinary
//! capability commands so they share the single command registry — palette,
//! `/help`, and completion all read `runtime.list_commands` — and dispatched
//! through the injected [`HostUi`] port: `execute_command` translates each
//! command into a [`UiCommand`] that the terminal event loop applies. The
//! runtime never performs the effect; it only routes the invocation.
//!
//! Because the effect lives entirely in the host, this capability is only
//! registered for the TUI (see [`crate::runtime::BuildOptions`]); ACP and
//! `--print` hosts, which have no overlay/transcript to drive, omit it.
//!
//! The model-facing `run_command` tool is not limited to the commands declared
//! here. Client commands keep the [`HostUi`] path (so `/mcp` and `/tools`
//! return their transcript lines); every other command is looked up in the live
//! registry through [`CommandDispatch`] and executed by the runtime, exactly as
//! typing the slash command would. Anything a user can type, the agent can run,
//! from one registry, with no second allowlist to keep in sync.

use crate::capabilities::narration::stable_labeled;
use crate::tui::host_ui::{HostUi, UiCommand};
use async_trait::async_trait;
use everruns_core::command::{
    CommandArg, CommandDescriptor, CommandExecutionContext, CommandResult, CommandSource,
    ExecuteCommandRequest,
};
use everruns_core::tool_narration::{ToolNarrationPhase, arg_str, truncate};
use everruns_core::{Capability, CapabilityStatus};
use everruns_core::{Tool, ToolExecutionResult};
use everruns_provider::ToolCall;
use serde_json::{Value, json};
use std::sync::Arc;

pub(crate) const CLIENT_COMMANDS_CAPABILITY_ID: &str = "yolop_client_commands";

pub(crate) const CLIENT_COMMANDS_PROMPT: &str = r#"<capability id="yolop_client_commands">
`run_command` runs any slash command this session registers, not a fixed subset:
`/help`, `/tools`, `/mcp`, `/cwd`, `/status [compact|expanded|toggle]`,
`/model [id]`, `/effort [level]`, `/clear`, `/quit` (`/exit` alias), plus the
rest of the registry, e.g. `/setup status|login|reauthenticate <provider>`,
`/background`, `/undo`, `/rewind`. Use `command: help` for the live list; an
unknown name returns the available ones. Skill commands activate by prompt, and
`/shell` is typed-only (use `bash`). When the user asks in prose, for example
"exit", "clear the screen", "show tools", "switch model", "restart MCP",
"reconnect MCP", or "refresh MCP" (`/mcp reload`), "log in to an MCP server"
(`/mcp login <name>`), or "re-authenticate my provider", call `run_command` with
the command and argument array; do not merely tell the user to type it, and do
not invent a manager window. The tool result includes the host's response text
(server lists, tool lists, OAuth URLs).
Use `set_status` for a concise live description of meaningful turn progress.
The contribution is cleared automatically when the turn finishes; send an
empty value to clear it earlier.
</capability>"#;

/// Registry port for `run_command`: list the commands this session actually has
/// and execute one through the runtime, the same path a typed slash command
/// takes. Injected rather than hard-coded so the agent-facing tool never keeps
/// its own allowlist, and so commands contributed later (extensions, MCP,
/// upstream capabilities) are reachable the moment they are registered.
#[async_trait]
pub(crate) trait CommandDispatch: Send + Sync {
    async fn list(&self) -> everruns_provider::error::Result<Vec<CommandDescriptor>>;

    async fn execute(
        &self,
        name: &str,
        arguments: Option<String>,
    ) -> everruns_provider::error::Result<CommandResult>;
}

pub(crate) struct ClientCommandsCapability {
    ui: Arc<dyn HostUi>,
    dispatch: Arc<dyn CommandDispatch>,
}

impl ClientCommandsCapability {
    pub(crate) fn new(ui: Arc<dyn HostUi>, dispatch: Arc<dyn CommandDispatch>) -> Self {
        Self { ui, dispatch }
    }
}

#[async_trait]
impl Capability for ClientCommandsCapability {
    fn id(&self) -> &str {
        CLIENT_COMMANDS_CAPABILITY_ID
    }
    fn name(&self) -> &str {
        "Client Commands"
    }
    fn description(&self) -> &str {
        "Terminal-side commands (help, tools, mcp, cwd, status, model, effort, clear, shell, quit)."
    }
    fn status(&self) -> CapabilityStatus {
        CapabilityStatus::Available
    }
    fn category(&self) -> Option<&str> {
        Some("Examples")
    }
    fn system_prompt_addition(&self) -> Option<&str> {
        Some(CLIENT_COMMANDS_PROMPT)
    }

    fn commands(&self) -> Vec<CommandDescriptor> {
        command_descriptors()
    }

    fn tools(&self) -> Vec<Box<dyn Tool>> {
        vec![
            Box::new(RunCommandTool {
                ui: self.ui.clone(),
                dispatch: self.dispatch.clone(),
            }),
            Box::new(SetStatusTool {
                ui: self.ui.clone(),
            }),
        ]
    }

    async fn execute_command(
        &self,
        request: &ExecuteCommandRequest,
        _ctx: &CommandExecutionContext,
    ) -> everruns_provider::error::Result<CommandResult> {
        let arg = request
            .arguments
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let command = ui_command_for(&request.name, arg).ok_or_else(|| {
            everruns_provider::error::AgentLoopError::config(format!(
                "{} cannot execute /{}",
                self.id(),
                request.name
            ))
        })?;
        self.ui.send(command);
        // The host event loop applies the effect; nothing to render inline.
        Ok(CommandResult {
            success: true,
            message: String::new(),
            error_code: None,
            error_fields: None,
        })
    }
}

fn command_descriptors() -> Vec<CommandDescriptor> {
    vec![
        cmd("help", "show commands", &[]),
        cmd("tools", "list available tools", &[]),
        cmd(
            "mcp",
            "list/reload/login or enable/disable/remove MCP servers live",
            &[opt("action")],
        ),
        cmd("cwd", "show workspace root", &[]),
        cmd(
            "status",
            "toggle compact or expanded session status",
            &[opt_with_suggestions(
                "layout",
                &["compact", "expanded", "toggle"],
            )],
        ),
        cmd("model", "show or switch model", &[opt("id")]),
        cmd("effort", "show or set reasoning effort", &[opt("level")]),
        cmd("clear", "clear transcript", &[]),
        cmd(
            "shell",
            "run shell command from workspace root",
            &[required("command")],
        ),
        cmd("quit", "exit", &[]),
    ]
}

fn ui_command_for(name: &str, arg: Option<String>) -> Option<UiCommand> {
    match name {
        "help" => Some(UiCommand::ShowHelp),
        "tools" => Some(UiCommand::ShowTools),
        "mcp" => Some(UiCommand::ManageMcp { arg }),
        "cwd" => Some(UiCommand::ShowCwd),
        "status" => Some(UiCommand::SetStatusLayout { arg }),
        "clear" => Some(UiCommand::ClearTranscript),
        "shell" => Some(UiCommand::RunShell {
            command: arg.unwrap_or_default(),
        }),
        "quit" => Some(UiCommand::Quit),
        "model" => Some(UiCommand::OpenModelOverlay { arg }),
        "effort" => Some(UiCommand::OpenEffortOverlay { arg }),
        _ => None,
    }
}

struct RunCommandTool {
    ui: Arc<dyn HostUi>,
    dispatch: Arc<dyn CommandDispatch>,
}

struct SetStatusTool {
    ui: Arc<dyn HostUi>,
}

#[async_trait]
impl Tool for SetStatusTool {
    fn narrate(
        &self,
        tool_call: &ToolCall,
        phase: ToolNarrationPhase,
        locale: Option<&str>,
        _ctx: everruns_core::tool_narration::ToolNarrationContext<'_>,
    ) -> Option<String> {
        let _ = locale;
        let status = arg_str(&tool_call.arguments, &["status"]).map(|value| truncate(value, 48));
        Some(stable_labeled("Update status", status, phase))
    }

    fn name(&self) -> &str {
        "set_status"
    }

    fn display_name(&self) -> Option<&str> {
        Some("Status")
    }

    fn description(&self) -> &str {
        "Set a concise, turn-scoped status shown in the interactive TUI. \
         Use an empty status to clear it before the turn finishes."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "status": {
                    "type": "string",
                    "description": "Concise live progress, at most 120 characters. Empty clears it."
                }
            },
            "required": ["status"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, arguments: Value) -> ToolExecutionResult {
        let Some(raw) = arguments.get("status").and_then(Value::as_str) else {
            return ToolExecutionResult::tool_error("'status' is required");
        };
        let status = raw.trim();
        if status.chars().count() > 120 {
            return ToolExecutionResult::tool_error("status must be at most 120 characters");
        }
        if status.chars().any(char::is_control) {
            return ToolExecutionResult::tool_error("status must be a single printable line");
        }
        self.ui.send(UiCommand::SetAgentStatus {
            status: status.to_string(),
        });
        ToolExecutionResult::success(json!({
            "success": true,
            "status": status
        }))
    }
}

impl RunCommandTool {
    /// Run a non-terminal command through the live registry, mirroring the
    /// host's typed-slash dispatch: unknown names list what is available,
    /// `Skill` commands stay prompt-activated, and missing required arguments
    /// are reported instead of sent on.
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
        let Some(descriptor) = commands.iter().find(|c| c.name == name) else {
            let available: Vec<&str> = commands.iter().map(|c| c.name.as_str()).collect();
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

        let rendered = match &arg {
            Some(arg) => format!("/{name} {arg}"),
            None => format!("/{name}"),
        };
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
        "Execute any registered yolop slash command on behalf of a natural-language user request: \
         the terminal ones (help, tools, mcp, cwd, status, model, effort, clear, quit) and every \
         other command this session registers, such as `setup` (`command: setup`, \
         `args: [reauthenticate, codex_browser]`), `background`, `undo`, `redo`, `rewind`, and `goal`. \
         Use `command: help` to list the live command set; an unknown name returns the available ones. \
         Accepts command names with or without the leading slash; `exit` is an alias for `quit`. \
         Skill commands activate by prompt and `shell` is typed-only, so neither runs here."
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
                                    e.g. `mcp`, `model`, `setup`, `background`. Use `help` to list them."
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

        let Some(command) = ui_command_for(name, arg.clone()) else {
            // Not a terminal-side command: run it through the session's own
            // registry so anything the user could type is also reachable here.
            return self.dispatch_registry_command(name, arg).await;
        };

        let rendered = match &arg {
            Some(arg) => format!("/{name} {arg}"),
            None => format!("/{name}"),
        };

        // Informational commands need the host's transcript lines back so the
        // agent can act on `/mcp` / `/tools` conversationally. Side-effect
        // commands (quit/clear/overlays) only need to be queued — awaiting a
        // reply would hang any host that is not draining the UI channel
        // (scripted tests, brief races at shutdown).
        let message = if command_awaits_host_reply(name) {
            let reply_rx = self.ui.request(command);
            match tokio::time::timeout(std::time::Duration::from_secs(15), reply_rx).await {
                Ok(Ok(messages)) if !messages.is_empty() => messages.join("\n"),
                Ok(Ok(_)) => format!("command applied: {rendered}"),
                Ok(Err(_)) => format!("command applied: {rendered}"),
                Err(_) => format!(
                    "command queued for the interactive terminal host ({rendered}); \
                     host did not return output in time"
                ),
            }
        } else {
            self.ui.send(command);
            format!("command applied: {rendered}")
        };

        ToolExecutionResult::success(json!({
            "success": true,
            "command": rendered,
            "message": message
        }))
    }
}

/// Whether `run_command` should wait for host transcript lines.
fn command_awaits_host_reply(name: &str) -> bool {
    matches!(name, "mcp" | "tools" | "help" | "cwd")
}

fn cmd(name: &str, description: &str, args: &[CommandArg]) -> CommandDescriptor {
    CommandDescriptor {
        name: name.to_string(),
        description: description.to_string(),
        source: CommandSource::System,
        args: args.to_vec(),
    }
}

fn opt(name: &str) -> CommandArg {
    arg(name, false)
}

fn opt_with_suggestions(name: &str, suggestions: &[&str]) -> CommandArg {
    CommandArg {
        suggestions: suggestions.iter().map(|s| (*s).to_string()).collect(),
        ..arg(name, false)
    }
}

fn required(name: &str) -> CommandArg {
    arg(name, true)
}

fn arg(name: &str, required: bool) -> CommandArg {
    CommandArg {
        name: name.to_string(),
        description: name.to_string(),
        required,
        suggestions: Vec::new(),
    }
}

/// A [`CommandDispatch`] that knows no commands. Hosts always inject the real
/// registry; this keeps unit tests that only need the descriptor list honest
/// about there being nothing else to run.
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
    use std::sync::{Arc, Mutex};
    use tokio::sync::oneshot;

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

        fn with_setup() -> Arc<Self> {
            Self::new(vec![
                cmd("setup", "configure providers", &[opt("action")]),
                cmd(
                    "goal",
                    "run until a condition holds",
                    &[required("condition")],
                ),
                CommandDescriptor {
                    source: CommandSource::Skill,
                    ..cmd("ship", "shipping workflow", &[])
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
                message: format!("setup: reauthenticated {}", arguments.unwrap_or_default()),
                error_code: None,
                error_fields: None,
            })
        }
    }

    fn run_command_tool(ui: Arc<RecordingUi>) -> RunCommandTool {
        RunCommandTool {
            ui,
            dispatch: Arc::new(EmptyCommandDispatch),
        }
    }

    #[derive(Default)]
    struct RecordingUi {
        commands: Mutex<Vec<UiCommand>>,
    }

    impl RecordingUi {
        fn take(&self) -> Vec<UiCommand> {
            std::mem::take(&mut *self.commands.lock().expect("commands lock"))
        }
    }

    impl HostUi for RecordingUi {
        fn send(&self, command: UiCommand) {
            self.commands.lock().expect("commands lock").push(command);
        }

        fn request(&self, command: UiCommand) -> oneshot::Receiver<Vec<String>> {
            self.send(command.clone());
            let (tx, rx) = oneshot::channel();
            // Unit tests without a live App still need a non-blocking reply.
            // Integration coverage for real host text lives in the TUI suite.
            let message = match &command {
                UiCommand::ManageMcp { .. } => {
                    vec![
                        "active MCP servers: none".into(),
                        "usage: /mcp [reload | login <name> | enable|disable|remove <name> [global|workspace]]"
                            .into(),
                    ]
                }
                UiCommand::ShowTools => vec!["tools: bash".into()],
                UiCommand::ShowHelp => vec!["commands:".into()],
                UiCommand::ShowCwd => vec!["workspace root: /tmp".into()],
                _ => Vec::new(),
            };
            let _ = tx.send(message);
            rx
        }
    }

    #[test]
    fn prompt_tells_model_to_run_natural_language_commands() {
        let ui = Arc::new(RecordingUi::default());
        let capability = ClientCommandsCapability::new(ui, Arc::new(EmptyCommandDispatch));
        let prompt = capability.system_prompt_addition().expect("prompt");

        assert!(prompt.contains("run_command"));
        // The prompt must not fence the model into a subset of the registry.
        assert!(prompt.contains("runs any slash command this session registers"));
        assert!(prompt.contains("reauthenticate <provider>"));
        assert!(prompt.contains("`command: help` for the live list"));
        assert!(!prompt.contains("only use `run_command` for this listed"));
        assert!(prompt.contains("/quit"));
        assert!(prompt.contains("/exit"));
        assert!(prompt.contains("\"restart MCP\""));
        assert!(prompt.contains("\"reconnect MCP\""));
        assert!(prompt.contains("\"refresh MCP\" (`/mcp reload`)"));
        assert!(prompt.contains("set_status"));
    }

    #[test]
    fn run_command_narration_includes_command() {
        use everruns_core::tool_narration::ToolNarrationPhase;
        use everruns_provider::ToolCall;

        let tool = run_command_tool(Arc::new(RecordingUi::default()));
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

    #[test]
    fn set_status_narration_includes_truncated_status() {
        use everruns_core::tool_narration::ToolNarrationPhase;
        use everruns_provider::ToolCall;

        let tool = SetStatusTool {
            ui: Arc::new(RecordingUi::default()),
        };
        let tool_call = ToolCall {
            id: "call-1".to_string(),
            name: "set_status".to_string(),
            arguments: json!({ "status": "Auditing tool narration" }),
        };

        assert_eq!(
            tool.narrate(
                &tool_call,
                ToolNarrationPhase::Started,
                None,
                everruns_core::tool_narration::ToolNarrationContext::default(),
            ),
            Some("Update status: Auditing tool narration".to_string())
        );
    }

    /// The schema must not pin a command allowlist: the live registry decides
    /// what exists, so any registered command can be named.
    #[test]
    fn run_command_schema_does_not_fix_a_command_allowlist() {
        let tool = run_command_tool(Arc::new(RecordingUi::default()));
        let schema = tool.parameters_schema();

        assert!(
            schema["properties"]["command"].get("enum").is_none(),
            "schema: {schema}"
        );
        assert_eq!(schema["properties"]["command"]["type"], json!("string"));
    }

    #[tokio::test]
    async fn run_command_exit_alias_queues_quit() {
        let ui = Arc::new(RecordingUi::default());
        let tool = run_command_tool(ui.clone());

        let result = tool.execute(json!({ "command": "/exit" })).await;

        assert!(result.is_success(), "tool result: {result:?}");
        assert_eq!(ui.take(), vec![UiCommand::Quit]);
    }

    #[tokio::test]
    async fn set_status_queues_turn_scoped_status_update() {
        let ui = Arc::new(RecordingUi::default());
        let tool = SetStatusTool { ui: ui.clone() };

        let result = tool.execute(json!({ "status": "running tests 3/8" })).await;

        assert!(result.is_success(), "tool result: {result:?}");
        assert_eq!(
            ui.take(),
            vec![UiCommand::SetAgentStatus {
                status: "running tests 3/8".to_string()
            }]
        );
    }

    #[tokio::test]
    async fn set_status_rejects_values_over_the_ui_limit() {
        let ui = Arc::new(RecordingUi::default());
        let tool = SetStatusTool { ui: ui.clone() };

        let result = tool.execute(json!({ "status": "x".repeat(121) })).await;

        assert!(result.is_error(), "tool result: {result:?}");
        assert!(ui.take().is_empty());
    }

    #[tokio::test]
    async fn set_status_rejects_terminal_control_characters() {
        let ui = Arc::new(RecordingUi::default());
        let tool = SetStatusTool { ui: ui.clone() };

        let result = tool.execute(json!({ "status": "tests\u{1b}[2J" })).await;

        assert!(result.is_error(), "tool result: {result:?}");
        assert!(ui.take().is_empty());
    }

    #[tokio::test]
    async fn run_command_rejects_shell_dispatch() {
        let ui = Arc::new(RecordingUi::default());
        let tool = run_command_tool(ui.clone());

        let result = tool
            .execute(json!({
                "command": "shell",
                "args": ["echo", "should-not-run"]
            }))
            .await;

        assert!(result.is_error(), "tool result: {result:?}");
        assert_eq!(ui.take(), Vec::<UiCommand>::new());
    }

    #[tokio::test]
    async fn run_command_preserves_model_argument() {
        let ui = Arc::new(RecordingUi::default());
        let tool = run_command_tool(ui.clone());

        let result = tool
            .execute(json!({
                "command": "model",
                "args": ["openai/gpt-5.4"]
            }))
            .await;

        assert!(result.is_success(), "tool result: {result:?}");
        assert_eq!(
            ui.take(),
            vec![UiCommand::OpenModelOverlay {
                arg: Some("openai/gpt-5.4".to_string())
            }]
        );
    }

    #[tokio::test]
    async fn run_command_preserves_status_layout_argument() {
        let ui = Arc::new(RecordingUi::default());
        let tool = run_command_tool(ui.clone());

        let result = tool
            .execute(json!({
                "command": "status",
                "args": ["expanded"]
            }))
            .await;

        assert!(result.is_success(), "tool result: {result:?}");
        assert_eq!(
            ui.take(),
            vec![UiCommand::SetStatusLayout {
                arg: Some("expanded".to_string())
            }]
        );
    }

    /// MCP reload is directly callable from conversation; the host receives the
    /// same typed command as interactive `/mcp reload` and returns its report.
    #[tokio::test]
    async fn run_command_dispatches_mcp_reload() {
        let ui = Arc::new(RecordingUi::default());
        let tool = run_command_tool(ui.clone());

        let result = tool
            .execute(json!({ "command": "mcp", "args": ["reload"] }))
            .await;

        assert!(result.is_success(), "tool should succeed: {result:?}");
        assert_eq!(
            ui.take(),
            vec![UiCommand::ManageMcp {
                arg: Some("reload".into()),
            }]
        );
    }

    /// The reported gap: `/setup reauthenticate <provider>` was unreachable
    /// from a turn because `run_command` carried its own client-command
    /// allowlist. It now dispatches through the session's command registry.
    #[tokio::test]
    async fn run_command_dispatches_registry_commands() {
        let ui = Arc::new(RecordingUi::default());
        let dispatch = RecordingDispatch::with_setup();
        let tool = RunCommandTool {
            ui: ui.clone(),
            dispatch: dispatch.clone(),
        };

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
        // Registry commands answer with the runtime's own message.
        let payload = match result {
            ToolExecutionResult::Success(value) => value,
            other => panic!("expected Success, got {other:?}"),
        };
        assert_eq!(
            payload.get("message").and_then(Value::as_str),
            Some("setup: reauthenticated reauthenticate codex_browser")
        );
        assert!(ui.take().is_empty(), "registry commands bypass the host UI");
    }

    #[tokio::test]
    async fn run_command_reports_the_live_command_set_for_unknown_names() {
        let dispatch = RecordingDispatch::with_setup();
        let tool = RunCommandTool {
            ui: Arc::new(RecordingUi::default()),
            dispatch: dispatch.clone(),
        };

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
        let dispatch = RecordingDispatch::with_setup();
        let tool = RunCommandTool {
            ui: Arc::new(RecordingUi::default()),
            dispatch: dispatch.clone(),
        };

        let result = tool.execute(json!({ "command": "ship" })).await;

        assert!(result.is_error(), "tool result: {result:?}");
        assert!(dispatch.executed().is_empty());
    }

    #[tokio::test]
    async fn run_command_reports_missing_required_arguments() {
        let dispatch = RecordingDispatch::with_setup();
        let tool = RunCommandTool {
            ui: Arc::new(RecordingUi::default()),
            dispatch: dispatch.clone(),
        };

        let result = tool.execute(json!({ "command": "goal" })).await;

        assert!(result.is_error(), "tool result: {result:?}");
        assert!(dispatch.executed().is_empty());
    }

    /// `run_command` for `/mcp` returns the host listing so the agent
    /// can act conversationally (login/reload) without inventing a manager window.
    #[tokio::test]
    async fn repro_run_command_mcp_returns_host_output() {
        let ui = Arc::new(RecordingUi::default());
        let tool = run_command_tool(ui.clone());

        let result = tool.execute(json!({ "command": "mcp", "args": [] })).await;
        assert!(result.is_success(), "tool should succeed: {result:?}");

        let payload = match result {
            ToolExecutionResult::Success(value) => value,
            other => panic!("expected Success, got {other:?}"),
        };
        let message = payload
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_default();

        assert!(
            !message.contains("queued for the interactive terminal host"),
            "agent must receive the real /mcp listing, not a fire-and-forget queue ack.\n\
             got message={message:?}; queued={:?}",
            ui.take()
        );
        assert!(
            message.contains("MCP") || message.contains("mcp") || message.contains("usage"),
            "tool result should carry the host /mcp response for conversational control.\n\
             got message={message:?}"
        );
    }
}

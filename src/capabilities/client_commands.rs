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

use crate::capabilities::narration::stable_labeled;
use crate::tui::host_ui::{HostUi, UiCommand};
use async_trait::async_trait;
use everruns_core::capabilities::{Capability, CapabilityStatus};
use everruns_core::command::{
    CommandArg, CommandDescriptor, CommandExecutionContext, CommandResult, CommandSource,
    ExecuteCommandRequest,
};
use everruns_core::tool_narration::{ToolNarrationPhase, arg_str, truncate};
use everruns_core::tool_types::ToolCall;
use everruns_core::tools::{Tool, ToolExecutionResult};
use serde_json::{Value, json};
use std::sync::Arc;

pub(crate) const CLIENT_COMMANDS_CAPABILITY_ID: &str = "yolop_client_commands";

pub(crate) const CLIENT_COMMANDS_PROMPT: &str = r#"<capability id="yolop_client_commands">
For natural-language requests, `run_command` can perform these TUI client
commands: `/help`, `/tools`, `/mcp`, `/cwd`, `/status [compact|expanded|toggle]`,
`/model [id]`, `/effort [level]`, `/clear`, and `/quit` (`/exit` is an alias).
The TUI may expose other slash commands, but only use `run_command` for this listed
client-command set. When the user asks for one of these terminal
actions — for example "exit", "clear the screen", "show tools", "switch model",
"restart MCP", "reconnect MCP", or "refresh MCP" (`/mcp reload`), "log in to an MCP server"
(`/mcp login <name>`), or "expand the status bar" —
call `run_command` with the command and argument array; do not merely tell the
user to type the slash command, and do not invent a manager window. The tool
result includes the host's response text (server lists, tool lists, OAuth URLs).
Use `set_status` for a concise live description of meaningful turn progress.
The contribution is cleared automatically when the turn finishes; send an
empty value to clear it earlier.
</capability>"#;

pub(crate) struct ClientCommandsCapability {
    ui: Arc<dyn HostUi>,
}

impl ClientCommandsCapability {
    pub(crate) fn new(ui: Arc<dyn HostUi>) -> Self {
        Self { ui }
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
    ) -> everruns_core::Result<CommandResult> {
        let arg = request
            .arguments
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let command = ui_command_for(&request.name, arg).ok_or_else(|| {
            everruns_core::AgentLoopError::config(format!(
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
        "Execute an interactive yolop slash command on behalf of a natural-language user request. \
         Use this when the user asks to exit, clear the transcript, show help/tools/MCP/cwd, \
         reload MCP servers (`command: mcp`, `args: [reload]`), show or change the status \
         layout, or open/switch model or reasoning effort. Accepts command names with or without the leading \
         slash; `exit` is accepted as an alias for `quit`."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "Slash command name, with or without the leading slash.",
                    "enum": [
                        "help", "tools", "mcp", "cwd", "status", "model", "effort", "clear", "quit", "exit",
                        "/help", "/tools", "/mcp", "/cwd", "/status", "/model", "/effort", "/clear", "/quit", "/exit"
                    ]
                },
                "args": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Ordered command arguments, e.g. [`reload`] for /mcp or [`openai/gpt-5.4`] for /model."
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
            return ToolExecutionResult::tool_error(format!("unknown yolop command: /{stripped}"));
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tokio::sync::oneshot;

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
        let capability = ClientCommandsCapability::new(ui);
        let prompt = capability.system_prompt_addition().expect("prompt");

        assert!(prompt.contains("run_command"));
        assert!(prompt.contains("TUI client"));
        // The prompt wraps this guidance across two lines — check each side.
        assert!(prompt.contains("only use `run_command` for this listed"));
        assert!(prompt.contains("client-command set"));
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
        use everruns_core::tool_types::ToolCall;

        let tool = RunCommandTool {
            ui: Arc::new(RecordingUi::default()),
        };
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
        use everruns_core::tool_types::ToolCall;

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

    #[test]
    fn run_command_schema_accepts_slashed_aliases() {
        let tool = RunCommandTool {
            ui: Arc::new(RecordingUi::default()),
        };
        let schema = tool.parameters_schema();
        let variants = schema["properties"]["command"]["enum"]
            .as_array()
            .expect("command enum");

        assert!(variants.contains(&json!("exit")));
        assert!(variants.contains(&json!("/exit")));
        assert!(variants.contains(&json!("quit")));
        assert!(variants.contains(&json!("/quit")));
    }

    #[tokio::test]
    async fn run_command_exit_alias_queues_quit() {
        let ui = Arc::new(RecordingUi::default());
        let tool = RunCommandTool { ui: ui.clone() };

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
        let tool = RunCommandTool { ui: ui.clone() };

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
        let tool = RunCommandTool { ui: ui.clone() };

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
        let tool = RunCommandTool { ui: ui.clone() };

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
        let tool = RunCommandTool { ui: ui.clone() };

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

    /// `run_command` for `/mcp` returns the host listing so the agent
    /// can act conversationally (login/reload) without inventing a manager window.
    #[tokio::test]
    async fn repro_run_command_mcp_returns_host_output() {
        let ui = Arc::new(RecordingUi::default());
        let tool = RunCommandTool { ui: ui.clone() };

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

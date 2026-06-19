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
use crate::host_ui::{HostUi, UiCommand};
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

const CLIENT_COMMANDS_PROMPT: &str = r#"<capability id="yolop_client_commands">
For natural-language requests, `run_yolop_command` can perform these TUI client
commands: `/help`, `/tools`, `/mcp`, `/cwd`, `/status [compact|expanded|toggle]`,
`/model [id]`, `/effort [level]`, `/clear`, and `/quit` (`/exit` is an alias).
The TUI may expose other slash commands, but only use `run_yolop_command` for this listed
client-command set. When the user asks for one of these terminal
actions — for example "exit", "clear the screen", "show tools", "switch model",
or "expand the status bar" — call `run_yolop_command`; do not merely tell the
user to type the slash command.
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
        vec![Box::new(RunYolopCommandTool {
            ui: self.ui.clone(),
        })]
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
        cmd("mcp", "list configured MCP servers", &[]),
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
        "mcp" => Some(UiCommand::ShowMcp),
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

struct RunYolopCommandTool {
    ui: Arc<dyn HostUi>,
}

#[async_trait]
impl Tool for RunYolopCommandTool {
    fn narrate(
        &self,
        tool_call: &ToolCall,
        phase: ToolNarrationPhase,
        locale: Option<&str>,
    ) -> Option<String> {
        let _ = locale;
        let command = arg_str(&tool_call.arguments, &["command"]).map(|value| truncate(value, 48));
        Some(stable_labeled("Run command", command, phase))
    }

    fn name(&self) -> &str {
        "run_yolop_command"
    }

    fn display_name(&self) -> Option<&str> {
        Some("Yolop command")
    }

    fn description(&self) -> &str {
        "Execute an interactive yolop slash command on behalf of a natural-language user request. \
         Use this when the user asks to exit, clear the transcript, show help/tools/MCP/cwd, \
         show or change the status layout, or open/switch model or reasoning effort. Accepts command names with or without the leading \
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
                "arguments": {
                    "type": "string",
                    "description": "Optional command arguments, e.g. a model id for /model or level for /effort."
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
        let arg = arguments
            .get("arguments")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);

        let Some(command) = ui_command_for(name, arg.clone()) else {
            return ToolExecutionResult::tool_error(format!("unknown yolop command: /{stripped}"));
        };

        self.ui.send(command);
        let rendered = match &arg {
            Some(arg) => format!("/{name} {arg}"),
            None => format!("/{name}"),
        };
        ToolExecutionResult::success(json!({
            "success": true,
            "command": rendered,
            "message": "command queued for the interactive terminal host"
        }))
    }
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
    }

    #[test]
    fn prompt_tells_model_to_run_natural_language_commands() {
        let ui = Arc::new(RecordingUi::default());
        let capability = ClientCommandsCapability::new(ui);
        let prompt = capability.system_prompt_addition().expect("prompt");

        assert!(prompt.contains("run_yolop_command"));
        assert!(prompt.contains("TUI client"));
        // The prompt wraps this guidance across two lines — check each side.
        assert!(prompt.contains("only use `run_yolop_command` for this listed"));
        assert!(prompt.contains("client-command set"));
        assert!(prompt.contains("/quit"));
        assert!(prompt.contains("/exit"));
    }

    #[test]
    fn run_yolop_command_narration_includes_command() {
        use everruns_core::tool_narration::ToolNarrationPhase;
        use everruns_core::tool_types::ToolCall;

        let tool = RunYolopCommandTool {
            ui: Arc::new(RecordingUi::default()),
        };
        let tool_call = ToolCall {
            id: "call-1".to_string(),
            name: "run_yolop_command".to_string(),
            arguments: json!({ "command": "/model" }),
        };

        assert_eq!(
            tool.narrate(&tool_call, ToolNarrationPhase::Started, None),
            Some("Run command: /model".to_string())
        );
    }

    #[test]
    fn run_yolop_command_schema_accepts_slashed_aliases() {
        let tool = RunYolopCommandTool {
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
    async fn run_yolop_command_exit_alias_queues_quit() {
        let ui = Arc::new(RecordingUi::default());
        let tool = RunYolopCommandTool { ui: ui.clone() };

        let result = tool.execute(json!({ "command": "/exit" })).await;

        assert!(result.is_success(), "tool result: {result:?}");
        assert_eq!(ui.take(), vec![UiCommand::Quit]);
    }

    #[tokio::test]
    async fn run_yolop_command_rejects_shell_dispatch() {
        let ui = Arc::new(RecordingUi::default());
        let tool = RunYolopCommandTool { ui: ui.clone() };

        let result = tool
            .execute(json!({
                "command": "shell",
                "arguments": "echo should-not-run"
            }))
            .await;

        assert!(result.is_error(), "tool result: {result:?}");
        assert_eq!(ui.take(), Vec::<UiCommand>::new());
    }

    #[tokio::test]
    async fn run_yolop_command_preserves_model_argument() {
        let ui = Arc::new(RecordingUi::default());
        let tool = RunYolopCommandTool { ui: ui.clone() };

        let result = tool
            .execute(json!({
                "command": "model",
                "arguments": "openai/gpt-5.4"
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
    async fn run_yolop_command_preserves_status_layout_argument() {
        let ui = Arc::new(RecordingUi::default());
        let tool = RunYolopCommandTool { ui: ui.clone() };

        let result = tool
            .execute(json!({
                "command": "status",
                "arguments": "expanded"
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
}

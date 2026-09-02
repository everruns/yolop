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
//! The model reaches these commands the same way it reaches every other one,
//! through the `run_command` tool in
//! [`crate::capabilities::agent_commands`], which resolves names against the
//! live registry rather than any list kept here.

use crate::tui::host_ui::{HostUi, UiCommand};
use async_trait::async_trait;
use everruns_core::command::{
    CommandArg, CommandDescriptor, CommandExecutionContext, CommandResult, CommandSource,
    ExecuteCommandRequest,
};
use everruns_core::{Capability, CapabilityStatus};
use std::sync::Arc;

pub(crate) const CLIENT_COMMANDS_CAPABILITY_ID: &str = "yolop_client_commands";

pub(crate) const CLIENT_COMMANDS_PROMPT: &str = r#"<capability id="yolop_client_commands">
Terminal commands, all runnable with `run_command`: `/help`, `/tools`, `/mcp`,
`/cwd`, `/status [compact|expanded|toggle]`, `/model [id]`, `/effort [level]`,
`/clear`, `/quit` (`/exit` alias). Run them on prose requests such as "exit",
"clear the screen", "show tools", "switch model", "restart MCP", "reconnect MCP",
or "refresh MCP" (`/mcp reload`), "log in to an MCP server" (`/mcp login <name>`);
never invent a manager window. `/shell` is typed-only; use `bash`.
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

pub(crate) fn ui_command_for(name: &str, arg: Option<String>) -> Option<UiCommand> {
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
    use crate::tui::host_ui::RecordingUi;
    use everruns_provider::typed_id::SessionId;

    #[test]
    fn prompt_covers_the_terminal_commands() {
        let ui = Arc::new(RecordingUi::default());
        let capability = ClientCommandsCapability::new(ui);
        let prompt = capability.system_prompt_addition().expect("prompt");

        assert!(prompt.contains("run_command"));
        assert!(prompt.contains("/quit"));
        assert!(prompt.contains("/exit"));
        assert!(prompt.contains("\"restart MCP\""));
        assert!(prompt.contains("\"reconnect MCP\""));
        assert!(prompt.contains("\"refresh MCP\" (`/mcp reload`)"));
    }

    /// The terminal commands are ordinary registry entries; `run_command` finds
    /// them there rather than in a list this capability hands the model.
    #[test]
    fn capability_contributes_terminal_commands() {
        let capability = ClientCommandsCapability::new(Arc::new(RecordingUi::default()));

        let commands: Vec<String> = capability
            .commands()
            .into_iter()
            .map(|command| command.name)
            .collect();
        assert!(commands.contains(&"quit".to_string()), "{commands:?}");
        assert!(commands.contains(&"mcp".to_string()), "{commands:?}");
        assert!(capability.tools().is_empty());
    }

    /// Executing a client command emits its `UiCommand` and returns an empty
    /// result: the host, not the runtime, performs the effect.
    #[tokio::test]
    async fn execute_command_emits_the_ui_command() {
        let ui = Arc::new(RecordingUi::default());
        let capability = ClientCommandsCapability::new(ui.clone());

        let result = capability
            .execute_command(
                &ExecuteCommandRequest {
                    name: "model".to_string(),
                    arguments: Some("openai/gpt-5.4".to_string()),
                    controls: None,
                },
                &CommandExecutionContext::without_host(SessionId::new()),
            )
            .await
            .expect("execute /model");

        assert!(result.success);
        assert!(result.message.is_empty());
        assert_eq!(
            ui.take(),
            vec![UiCommand::OpenModelOverlay {
                arg: Some("openai/gpt-5.4".to_string())
            }]
        );
    }

    #[tokio::test]
    async fn execute_command_emits_mcp_reload() {
        let ui = Arc::new(RecordingUi::default());
        let capability = ClientCommandsCapability::new(ui.clone());

        let result = capability
            .execute_command(
                &ExecuteCommandRequest {
                    name: "mcp".to_string(),
                    arguments: Some("reload".to_string()),
                    controls: None,
                },
                &CommandExecutionContext::without_host(SessionId::new()),
            )
            .await
            .expect("execute /mcp reload");

        assert!(result.success);
        assert!(result.message.is_empty());
        assert_eq!(
            ui.take(),
            vec![UiCommand::ManageMcp {
                arg: Some("reload".to_string())
            }]
        );
    }

    #[tokio::test]
    async fn execute_command_rejects_names_this_capability_does_not_own() {
        let ui = Arc::new(RecordingUi::default());
        let capability = ClientCommandsCapability::new(ui.clone());

        let error = capability
            .execute_command(
                &ExecuteCommandRequest {
                    name: "setup".to_string(),
                    arguments: None,
                    controls: None,
                },
                &CommandExecutionContext::without_host(SessionId::new()),
            )
            .await
            .expect_err("/setup belongs to another capability");

        assert!(error.to_string().contains("setup"), "error: {error}");
        assert!(ui.take().is_empty());
    }
}

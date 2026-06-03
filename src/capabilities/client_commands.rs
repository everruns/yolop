//! Client-executed slash commands for the TUI host.
//!
//! `help`, `tools`, `cwd`, `model`, `effort`, `clear`, and `quit` act on the
//! terminal, not the agent runtime. They are declared here as ordinary
//! capability commands so they share the single command registry — palette,
//! `/help`, and completion all read `runtime.list_commands` — and dispatched
//! through the injected [`HostUi`] port: `execute_command` translates each
//! command into a [`UiCommand`] that the terminal event loop applies. The
//! runtime never performs the effect; it only routes the invocation.
//!
//! Because the effect lives entirely in the host, this capability is only
//! registered for the TUI (see [`crate::runtime::BuildOptions`]); ACP and
//! `--print` hosts, which have no overlay/transcript to drive, omit it.

use crate::host_ui::{HostUi, UiCommand};
use async_trait::async_trait;
use everruns_core::capabilities::{Capability, CapabilityStatus};
use everruns_core::command::{
    CommandArg, CommandDescriptor, CommandExecutionContext, CommandResult, CommandSource,
    ExecuteCommandRequest,
};
use std::sync::Arc;

pub(crate) const CLIENT_COMMANDS_CAPABILITY_ID: &str = "yolop_client_commands";

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
        "Terminal-side slash commands (help, tools, cwd, model, effort, clear, quit)."
    }
    fn status(&self) -> CapabilityStatus {
        CapabilityStatus::Available
    }
    fn category(&self) -> Option<&str> {
        Some("Examples")
    }
    fn system_prompt_addition(&self) -> Option<&str> {
        None
    }

    fn commands(&self) -> Vec<CommandDescriptor> {
        vec![
            cmd("help", "show commands", &[]),
            cmd("tools", "list available tools", &[]),
            cmd("cwd", "show workspace root", &[]),
            cmd("model", "show or switch model", &[opt("id")]),
            cmd("effort", "show or set reasoning effort", &[opt("level")]),
            cmd("clear", "clear transcript", &[]),
            cmd("quit", "exit", &[]),
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
        let command = match request.name.as_str() {
            "help" => UiCommand::ShowHelp,
            "tools" => UiCommand::ShowTools,
            "cwd" => UiCommand::ShowCwd,
            "clear" => UiCommand::ClearTranscript,
            "quit" => UiCommand::Quit,
            "model" => UiCommand::OpenModelOverlay { arg },
            "effort" => UiCommand::OpenEffortOverlay { arg },
            other => {
                return Err(everruns_core::AgentLoopError::config(format!(
                    "{} cannot execute /{other}",
                    self.id()
                )));
            }
        };
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

fn cmd(name: &str, description: &str, args: &[CommandArg]) -> CommandDescriptor {
    CommandDescriptor {
        name: name.to_string(),
        description: description.to_string(),
        source: CommandSource::System,
        args: args.to_vec(),
    }
}

fn opt(name: &str) -> CommandArg {
    CommandArg {
        name: name.to_string(),
        description: name.to_string(),
        required: false,
        suggestions: Vec::new(),
    }
}

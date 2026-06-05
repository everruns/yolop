//! Host UI port for client-executed slash commands.
//!
//! A handful of slash commands act on the terminal client itself — clear the
//! transcript, open the model/effort overlay, print local info, quit — rather
//! than on the agent runtime. They are nonetheless ordinary capabilities (see
//! [`crate::capabilities::ClientCommandsCapability`]) so that every command
//! lives in one registry. Their `execute_command` runs on the runtime's task
//! and cannot touch the TUI directly, so it instead calls this port, which
//! forwards a typed [`UiCommand`] to the terminal event loop over a channel.
//! The loop is the only thing that can perform the effect, so it applies it.

use tokio::sync::mpsc;

/// A request from a client-executed capability command to the terminal host.
/// Variants name *what* should happen; the host decides *how* to render it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UiCommand {
    /// Print the command palette / input help.
    ShowHelp,
    /// Print the list of available tools.
    ShowTools,
    /// Print the configured MCP servers.
    ShowMcp,
    /// Print the workspace root.
    ShowCwd,
    /// Clear the transcript buffer.
    ClearTranscript,
    /// Exit the application.
    Quit,
    /// Open the interactive model picker. `arg` pre-seeds the selection.
    OpenModelOverlay { arg: Option<String> },
    /// Open the interactive reasoning-effort picker. `arg` pre-seeds it.
    OpenEffortOverlay { arg: Option<String> },
}

/// What a client-executed command can ask the host UI to do. Implemented per
/// host (the TUI's [`TuiHandle`]); a host that cannot honor client commands
/// simply never registers the capability that depends on this port.
pub trait HostUi: Send + Sync {
    fn send(&self, command: UiCommand);
}

/// TUI implementation of [`HostUi`]: every call is a non-blocking channel
/// send. The matching receiver is drained by the `App` event loop.
pub struct TuiHandle {
    tx: mpsc::UnboundedSender<UiCommand>,
}

impl TuiHandle {
    pub fn new(tx: mpsc::UnboundedSender<UiCommand>) -> Self {
        Self { tx }
    }
}

impl HostUi for TuiHandle {
    fn send(&self, command: UiCommand) {
        // The receiver lives as long as the `App`; a failed send only happens
        // during shutdown, where dropping the command is the correct behavior.
        let _ = self.tx.send(command);
    }
}

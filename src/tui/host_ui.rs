//! Host UI port for client-executed slash commands.
//!
//! A handful of slash commands act on the terminal client itself — clear the
//! transcript, open the model/effort overlay, run a local shell command, print
//! local info, quit — rather than on the agent runtime. They are nonetheless
//! ordinary capabilities (see
//! [`crate::capabilities::ClientCommandsCapability`]) so that every command
//! lives in one registry. Their `execute_command` runs on the runtime's task
//! and cannot touch the TUI directly, so it instead calls this port, which
//! forwards a typed [`UiCommand`] to the terminal event loop over a channel.
//! The loop is the only thing that can perform the effect, so it applies it.
//!
//! Agent-facing tools ([`crate::capabilities::client_commands::RunCommandTool`])
//! use [`HostUi::request`] so the host can return the transcript lines it
//! produced — making `/mcp` and `/tools` conversational instead of
//! fire-and-forget.

use tokio::sync::{mpsc, oneshot};

/// Marker phrase shared by the transcript line the App writes and the
/// `enable_extension` result note, so the client and the agent describe the
/// same situation and cannot drift apart.
pub(crate) const EXTENSION_INSTALLED_MID_SESSION: &str = "installed after this session started";

/// A server→host `ui/ask`: prompt the user and deliver their answer via
/// `reply`. Carried on its own channel rather than as a [`UiCommand`] variant
/// because the oneshot sender can't derive `Clone`/`Eq`.
pub struct AskRequest {
    pub prompt: String,
    pub placeholder: Option<String>,
    /// Mask input and never echo the answer (credentials).
    pub secret: bool,
    /// When non-empty, present a selector over these options.
    pub options: Vec<String>,
    pub reply: oneshot::Sender<AskAnswer>,
}

/// The user's answer to a [`AskRequest`] (empty + `cancelled` on dismiss).
#[derive(Clone, Debug, Default)]
pub struct AskAnswer {
    pub answer: String,
    pub cancelled: bool,
}

/// A request from a client-executed capability command to the terminal host.
/// Variants name *what* should happen; the host decides *how* to render it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UiCommand {
    /// Print the command palette / input help.
    ShowHelp,
    /// Print the list of available tools.
    ShowTools,
    /// Inspect or update configured MCP servers.
    ManageMcp { arg: Option<String> },
    /// Print the workspace root.
    ShowCwd,
    /// Change the inline session status layout.
    SetStatusLayout { arg: Option<String> },
    /// Clear the transcript buffer.
    ClearTranscript,
    /// Run a shell command from the workspace root.
    RunShell { command: String },
    /// Exit the application.
    Quit,
    /// Open the interactive model picker. `arg` pre-seeds the selection.
    OpenModelOverlay { arg: Option<String> },
    /// Open the interactive reasoning-effort picker. `arg` pre-seeds it.
    OpenEffortOverlay { arg: Option<String> },
    /// Set (or clear when empty) the agent's turn-scoped status contribution.
    SetAgentStatus { status: String },
    /// Set (or, when `status` is empty, clear) an extension's status-bar field.
    /// Pushed by an extension server's `status/changed` notification.
    SetExtensionStatus { ext: String, status: String },
    /// Activate (`activate = true`) or deactivate an extension capability on the
    /// live session so it takes effect this session, not just next. Pushed by
    /// `enable_extension`/`disable_extension` after the settings write.
    SetExtensionActive {
        capability_id: String,
        name: String,
        activate: bool,
    },
}

/// Envelope on the host UI channel: a [`UiCommand`] plus an optional reply
/// oneshot that receives the system transcript lines the host produced while
/// applying it. Fire-and-forget callers leave `reply` as `None`.
pub struct UiRequest {
    pub command: UiCommand,
    pub reply: Option<oneshot::Sender<Vec<String>>>,
}

impl UiRequest {
    /// Queue `command` with no reply (slash-command and extension pushes).
    pub fn fire(command: UiCommand) -> Self {
        Self {
            command,
            reply: None,
        }
    }
}

/// What a client-executed command can ask the host UI to do. Implemented per
/// host (the TUI's [`TuiHandle`]); a host that cannot honor client commands
/// simply never registers the capability that depends on this port.
pub trait HostUi: Send + Sync {
    /// Fire-and-forget: apply `command` on the next event-loop tick.
    fn send(&self, command: UiCommand);

    /// Apply `command` and return the system transcript lines the host produced.
    /// Used by `run_command` so the agent sees `/mcp` / `/tools` output.
    fn request(&self, command: UiCommand) -> oneshot::Receiver<Vec<String>>;
}

/// TUI implementation of [`HostUi`]: every call is a non-blocking channel
/// send. The matching receiver is drained by the `App` event loop.
pub struct TuiHandle {
    tx: mpsc::UnboundedSender<UiRequest>,
}

impl TuiHandle {
    pub fn new(tx: mpsc::UnboundedSender<UiRequest>) -> Self {
        Self { tx }
    }
}

impl HostUi for TuiHandle {
    fn send(&self, command: UiCommand) {
        // The receiver lives as long as the `App`; a failed send only happens
        // during shutdown, where dropping the command is the correct behavior.
        let _ = self.tx.send(UiRequest::fire(command));
    }

    fn request(&self, command: UiCommand) -> oneshot::Receiver<Vec<String>> {
        let (reply_tx, reply_rx) = oneshot::channel();
        let _ = self.tx.send(UiRequest {
            command,
            reply: Some(reply_tx),
        });
        reply_rx
    }
}

/// Recording [`HostUi`] for unit tests: captures queued commands and answers
/// `request` with representative transcript lines, so capabilities that drive
/// the host can be tested without a live `App`. Integration coverage for the
/// real host text lives in the TUI suite.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct RecordingUi {
    commands: std::sync::Mutex<Vec<UiCommand>>,
}

#[cfg(test)]
impl RecordingUi {
    pub(crate) fn take(&self) -> Vec<UiCommand> {
        std::mem::take(&mut *self.commands.lock().expect("commands lock"))
    }
}

#[cfg(test)]
impl HostUi for RecordingUi {
    fn send(&self, command: UiCommand) {
        self.commands.lock().expect("commands lock").push(command);
    }

    fn request(&self, command: UiCommand) -> oneshot::Receiver<Vec<String>> {
        self.send(command.clone());
        let (tx, rx) = oneshot::channel();
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

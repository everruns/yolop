// TUI app state and event loop.
// Decision: keep the TUI surface tiny. Transcript output is inserted into the
// native terminal scrollback; ratatui owns only a short inline composer at the
// bottom, with approvals handled through the same status delimiter.

use crate::approval::ApprovalRequest;
use crate::runtime::{BuiltRuntime, ModelState, RuntimeHandles, StartupInfo};
use anyhow::Result;
use crossterm::event::{
    self, Event as CrosstermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
};
use everruns_core::command::{CommandDescriptor, CommandSource};
use everruns_core::events::{Event as RuntimeEvent, EventData, ToolCompletedData};
use everruns_core::message::{ContentPart, Message, MessageRole};
use everruns_core::typed_id::SessionId;
use ratatui::Terminal;
use ratatui::backend::Backend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};
use ratatui_textarea::{CursorMove, TextArea, WrapMode};
use serde_json::Value;
use std::collections::HashSet;
use std::time::Duration;
use tokio::sync::broadcast;
use tokio::sync::{mpsc, oneshot};

#[derive(Clone, Debug)]
pub enum Author {
    User,
    Assistant,
    Tool,
    ToolDetail,
    Diff,
    System,
}

impl Author {
    pub fn label(&self) -> &'static str {
        match self {
            Author::User => "you",
            Author::Assistant => "agent",
            Author::Tool => "tool",
            Author::ToolDetail => "",
            Author::Diff => "diff",
            Author::System => "system",
        }
    }
    pub fn color(&self) -> Color {
        match self {
            Author::User => ACCENT_BLUE,
            Author::Assistant => ACCENT_GOLD,
            Author::Tool => TEXT_MUTED,
            Author::ToolDetail => TEXT_MUTED,
            Author::Diff => ACCENT_BLUE,
            Author::System => TEXT_DIM,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ChatLine {
    pub author: Author,
    pub text: String,
}

type ApprovalRx = mpsc::UnboundedReceiver<(ApprovalRequest, oneshot::Sender<bool>)>;

struct PendingApproval {
    responder: oneshot::Sender<bool>,
}

#[derive(Clone, Copy)]
struct CommandSpec {
    name: &'static str,
    usage: &'static str,
    description: &'static str,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CommandSuggestion {
    completion: String,
    label: String,
}

const COMMANDS: &[CommandSpec] = &[
    CommandSpec {
        name: "help",
        usage: "/help",
        description: "show commands",
    },
    CommandSpec {
        name: "tools",
        usage: "/tools",
        description: "list available tools",
    },
    CommandSpec {
        name: "cwd",
        usage: "/cwd",
        description: "show workspace root",
    },
    CommandSpec {
        name: "clear",
        usage: "/clear",
        description: "clear transcript",
    },
    CommandSpec {
        name: "quit",
        usage: "/quit",
        description: "exit",
    },
];

pub const COMPOSER_VIEWPORT_HEIGHT: u16 = 5;
const ACCENT_BLUE: Color = Color::Rgb(45, 91, 158);
const ACCENT_GOLD: Color = Color::Rgb(126, 94, 19);
const TEXT_PRIMARY: Color = Color::Rgb(230, 230, 232);
const TEXT_MUTED: Color = Color::Rgb(140, 140, 145);
const TEXT_DIM: Color = Color::Rgb(72, 72, 78);
const CODE_BG: Color = Color::Rgb(18, 18, 20);

pub struct App {
    handles: RuntimeHandles,
    startup: StartupInfo,
    model: ModelState,
    pub lines: Vec<ChatLine>,
    printed_lines: usize,
    pub input: TextArea<'static>,
    pub busy: bool,
    pub should_quit: bool,
    ctrl_c_exit: bool,
    busy_frame: u64,
    turn_activity: Option<String>,
    /// Live tail of streaming assistant text (and other delta events).
    /// Cleared on turn completion; never enters the persistent transcript.
    stream_preview: Option<StreamPreview>,
    rx: Option<mpsc::UnboundedReceiver<TurnEvent>>,
    approval_rx: ApprovalRx,
    pending: Option<PendingApproval>,
    /// Active setup step, if any. The wizard captures plain-text input from
    /// the composer and dispatches internal `/setup` actions at each step.
    setup: Option<SetupStep>,
}

#[derive(Clone, Debug)]
enum SetupStep {
    ChooseAction,
    PickProvider,
    PickTokenProvider,
    EnterToken {
        provider: String,
        default_model: String,
        activate: bool,
    },
    PickModel {
        provider: String,
        default_model: String,
    },
}

struct ProviderOption {
    name: &'static str,
    label: &'static str,
    hint: &'static str,
}

const PROVIDER_OPTIONS: &[ProviderOption] = &[
    ProviderOption {
        name: "openai",
        label: "OpenAI",
        hint: "recommended",
    },
    ProviderOption {
        name: "anthropic",
        label: "Anthropic",
        hint: "Claude",
    },
    ProviderOption {
        name: "google",
        label: "Google Gemini",
        hint: "Gemini models",
    },
    ProviderOption {
        name: "openrouter",
        label: "OpenRouter",
        hint: "many hosted models",
    },
    ProviderOption {
        name: "ollama",
        label: "Ollama local",
        hint: "local OpenAI-compatible server",
    },
    ProviderOption {
        name: "llmsim",
        label: "Offline demo mode",
        hint: "no API key",
    },
];

/// Owned snapshot of the App fields the pure-render chrome helpers
/// (command suggestions, stream preview, separators, session status)
/// consume. Extracted from `App` so those helpers can be exercised by
/// unit tests against `ratatui::backend::TestBackend` without standing
/// up a full runtime.
///
/// Owned rather than borrowed because building it does not need to
/// borrow `App` for the duration of a draw: `draw_input` needs `&mut
/// App` for the input field's `Widget` impl, and a borrowed `ViewState`
/// would block that within a single `draw()`. The per-frame clone cost
/// is dominated by `String`-sized fields and is negligible compared to
/// the chrome render itself.
#[derive(Clone, Debug)]
pub(crate) struct ViewState {
    pub stream_preview: Option<StreamPreview>,
    pub command_suggestions: Vec<CommandSuggestion>,
    /// `true` while an approval prompt is waiting on user input. The
    /// renderer only needs the bit, not the responder channel.
    pub approval_pending: bool,
    pub busy: bool,
    pub busy_frame: u64,
    pub turn_activity: Option<String>,
    pub model_label: String,
    pub workspace_root: std::path::PathBuf,
    pub session_id: SessionId,
    pub lines_count: usize,
}

/// What kind of delta is currently being streamed. Only the assistant
/// output is finalized into the transcript at end-of-turn (via the
/// message store); thinking and tool output are display-only.
#[derive(Clone, Debug)]
pub struct StreamPreview {
    pub kind: StreamKind,
    pub text: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamKind {
    Assistant,
    Thinking,
    Tool,
}

impl StreamKind {
    fn label(self) -> &'static str {
        match self {
            StreamKind::Assistant => "agent",
            StreamKind::Thinking => "thinking",
            StreamKind::Tool => "tool",
        }
    }

    fn color(self) -> Color {
        match self {
            StreamKind::Assistant => ACCENT_GOLD,
            StreamKind::Thinking => TEXT_MUTED,
            StreamKind::Tool => TEXT_MUTED,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ActivityStatus {
    pub text: String,
    fallback: bool,
}

#[derive(Debug)]
pub(crate) enum TurnEvent {
    Lines(Vec<ChatLine>),
    Activity(ActivityStatus),
    /// Replace the live streaming preview shown above the input.
    /// `None` clears the preview.
    Stream(Option<StreamPreview>),
    Done,
    Failed(String),
}

impl App {
    pub fn new(runtime: BuiltRuntime, approval_rx: ApprovalRx) -> Self {
        let should_setup = runtime.startup.setup_recommended;
        let mut app = Self {
            handles: runtime.handles,
            startup: runtime.startup,
            model: runtime.model,
            lines: Vec::new(),
            printed_lines: 0,
            input: new_input_area(vec![String::new()]),
            busy: false,
            should_quit: false,
            ctrl_c_exit: false,
            busy_frame: 0,
            turn_activity: None,
            stream_preview: None,
            rx: None,
            approval_rx,
            pending: None,
            setup: None,
        };
        app.emit_system_banner();
        if should_setup {
            app.start_first_run_setup();
        }
        app
    }

    pub fn should_show_resume_hint(&self) -> bool {
        self.ctrl_c_exit
    }

    pub fn session_id(&self) -> SessionId {
        self.handles.session_id
    }

    /// Snapshot the renderer-relevant fields into a `ViewState`. Called
    /// once per frame; the clones are dominated by small `String`s.
    pub(crate) fn view_state(&self) -> ViewState {
        ViewState {
            stream_preview: self.stream_preview.clone(),
            command_suggestions: if !self.busy && self.pending.is_none() {
                self.suggestions()
            } else {
                Vec::new()
            },
            approval_pending: self.pending.is_some(),
            busy: self.busy,
            busy_frame: self.busy_frame,
            turn_activity: self.turn_activity.clone(),
            model_label: self.model.provider_label(),
            workspace_root: self.startup.workspace_root.clone(),
            session_id: self.handles.session_id,
            lines_count: self.lines.len(),
        }
    }

    fn emit_system_banner(&mut self) {
        self.push_system(format!(
            "workspace: {}",
            self.startup.workspace_root.display()
        ));
        self.push_system(format!("model: {}", self.model.provider_label()));
        self.push_system(format!("tools: {}", self.startup.tool_names.join(", ")));
        if !self.startup.capability_commands.is_empty() {
            let names: Vec<String> = self
                .startup
                .capability_commands
                .iter()
                .map(|c| format!("/{}", c.name))
                .collect();
            self.push_system(format!("commands: {}", names.join(", ")));
        }
        self.push_system("type /help for commands, Esc or Ctrl-D to exit; approvals: y / n".into());
    }

    fn push_user(&mut self, text: String) {
        self.lines.push(ChatLine {
            author: Author::User,
            text,
        });
    }
    fn push_system(&mut self, text: String) {
        self.lines.push(ChatLine {
            author: Author::System,
            text,
        });
    }

    pub async fn run<B>(&mut self, terminal: &mut Terminal<B>) -> Result<()>
    where
        B: Backend,
        B::Error: std::error::Error + Send + Sync + 'static,
    {
        self.emit_replayed_transcript().await;
        loop {
            if self.busy {
                self.busy_frame = self.busy_frame.wrapping_add(1);
            }
            self.flush_transcript(terminal)?;
            terminal.draw(|f| draw(f, self))?;

            // 1) drain background turn events
            if let Some(rx) = self.rx.as_mut() {
                match rx.try_recv() {
                    Ok(TurnEvent::Lines(lines)) => {
                        self.lines.extend(lines);
                        continue;
                    }
                    Ok(TurnEvent::Activity(activity)) => {
                        if !activity.fallback || self.turn_activity.is_none() {
                            self.turn_activity = Some(activity.text);
                        }
                        continue;
                    }
                    Ok(TurnEvent::Stream(preview)) => {
                        self.stream_preview = preview;
                        continue;
                    }
                    Ok(TurnEvent::Done) => {
                        self.busy = false;
                        self.busy_frame = 0;
                        self.turn_activity = None;
                        self.stream_preview = None;
                        self.rx = None;
                        continue;
                    }
                    Ok(TurnEvent::Failed(err)) => {
                        self.busy = false;
                        self.busy_frame = 0;
                        self.turn_activity = None;
                        self.stream_preview = None;
                        self.rx = None;
                        self.push_system(format!("turn failed: {err}"));
                        continue;
                    }
                    Err(mpsc::error::TryRecvError::Empty) => {}
                    Err(mpsc::error::TryRecvError::Disconnected) => {
                        self.busy = false;
                        self.turn_activity = None;
                        self.stream_preview = None;
                        self.rx = None;
                    }
                }
            }

            // 2) drain pending approval requests
            if self.pending.is_none()
                && let Ok((req, responder)) = self.approval_rx.try_recv()
            {
                let header = format!("approval needed: {}", req.headline());
                self.push_system(header);
                let detail = req.detail();
                for line in detail.lines().take(40) {
                    self.lines.push(ChatLine {
                        author: Author::Diff,
                        text: line.to_string(),
                    });
                }
                self.pending = Some(PendingApproval { responder });
            }

            // 3) keystrokes. Mouse wheel/drag stays native terminal behavior
            // because the transcript lives in scrollback, not in this viewport.
            let mut poll_timeout = Duration::from_millis(80);
            while event::poll(poll_timeout)? {
                poll_timeout = Duration::ZERO;
                if let CrosstermEvent::Key(key) = event::read()? {
                    if key.kind == KeyEventKind::Release {
                        continue;
                    }
                    self.handle_key(key).await;
                }
                if self.should_quit {
                    break;
                }
            }
            if self.should_quit {
                // If we exit with an outstanding approval, deny it so the tool
                // task unblocks and the runtime can record a tool error.
                if let Some(p) = self.pending.take() {
                    let _ = p.responder.send(false);
                }
                break;
            }
        }
        Ok(())
    }

    async fn emit_replayed_transcript(&mut self) {
        if self.startup.replayed_events == 0 {
            return;
        }

        let events = match self.handles.runtime.events().await {
            Ok(events) => events,
            Err(err) => {
                self.push_system(format!("load replayed transcript: {err}"));
                return;
            }
        };
        let replayed_lines = events
            .iter()
            .take(self.startup.replayed_events)
            .flat_map(lines_for_replayed_event)
            .collect::<Vec<_>>();
        self.lines.extend(replayed_lines);
    }

    fn flush_transcript<B>(&mut self, terminal: &mut Terminal<B>) -> Result<()>
    where
        B: Backend,
        B::Error: std::error::Error + Send + Sync + 'static,
    {
        if self.printed_lines >= self.lines.len() {
            return Ok(());
        }

        let width = terminal.size()?.width.saturating_sub(2).max(20) as usize;
        let mut rendered: Vec<Line<'static>> = Vec::new();
        for (index, chat) in self.lines[self.printed_lines..].iter().enumerate() {
            append_chat_lines(&mut rendered, chat, width);
            let absolute = self.printed_lines + index;
            if should_insert_chat_gap(
                &chat.author,
                self.lines.get(absolute + 1).map(|line| &line.author),
            ) {
                rendered.push(Line::from(""));
            }
        }
        if rendered.is_empty() {
            self.printed_lines = self.lines.len();
            return Ok(());
        }

        for chunk in rendered.chunks(u16::MAX as usize) {
            terminal.insert_before(chunk.len() as u16, |buf| {
                Paragraph::new(chunk.to_vec()).render(buf.area, buf);
            })?;
        }
        self.printed_lines = self.lines.len();
        Ok(())
    }

    async fn handle_key(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('c') => {
                    self.ctrl_c_exit = true;
                    self.should_quit = true;
                    return;
                }
                KeyCode::Char('d') => {
                    self.should_quit = true;
                    return;
                }
                _ => {}
            }
        }

        // Approval mode: only y / n / Esc.
        if self.pending.is_some() {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    if let Some(p) = self.pending.take() {
                        let _ = p.responder.send(true);
                        self.push_system("approved".into());
                    }
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                    if let Some(p) = self.pending.take() {
                        let _ = p.responder.send(false);
                        self.push_system("denied".into());
                    }
                }
                _ => {}
            }
            return;
        }

        if matches!(key.code, KeyCode::Esc) {
            self.should_quit = true;
            return;
        }

        if self.busy {
            // Block only input editing while a turn is running.
            return;
        }
        match key.code {
            KeyCode::Enter
                if !key
                    .modifiers
                    .intersects(KeyModifiers::ALT | KeyModifiers::SHIFT) =>
            {
                self.submit_input().await;
            }
            KeyCode::Tab => {
                if let Some(suggestion) = self.suggestions().first() {
                    self.set_input_text(suggestion.completion.clone());
                } else {
                    let _ = self.input.input(key);
                }
            }
            _ => {
                let _ = self.input.input(key);
            }
        }
    }

    fn suggestions(&self) -> Vec<CommandSuggestion> {
        command_suggestions(self.suggestion_input(), &self.startup.capability_commands)
    }

    fn suggestion_input(&self) -> &str {
        self.input
            .lines()
            .first()
            .map(String::as_str)
            .unwrap_or_default()
    }

    fn input_text(&self) -> String {
        self.input.lines().join("\n")
    }

    fn set_input_text(&mut self, text: String) {
        self.input = new_input_area(vec![text]);
        self.input.move_cursor(CursorMove::End);
    }

    fn reset_input(&mut self) {
        self.input = new_input_area(vec![String::new()]);
    }

    fn input_height(&self) -> u16 {
        1
    }

    async fn submit_input(&mut self) {
        let raw = self.input_text();
        self.reset_input();
        let text = raw.trim().to_string();
        if let Some(rest) = text.strip_prefix('/') {
            self.handle_command(rest).await;
            return;
        }
        // While setup is active, plain-text input is the wizard answer (empty
        // included — "skip" for token/model steps).
        if self.setup.is_some() {
            self.advance_setup(&text).await;
            return;
        }
        if text.is_empty() {
            return;
        }
        self.push_user(text.clone());
        self.start_turn(text);
    }

    async fn handle_command(&mut self, cmd: &str) {
        let cmd = cmd.trim();
        let mut parts = cmd.splitn(2, char::is_whitespace);
        let head = parts.next().unwrap_or_default();
        let arg = parts.next().unwrap_or_default().trim();
        match head {
            "help" => {
                self.push_system(
                    COMMANDS
                        .iter()
                        .map(|cmd| cmd.usage)
                        .collect::<Vec<_>>()
                        .join(" · "),
                );
                if !self.startup.capability_commands.is_empty() {
                    let caps = self
                        .startup
                        .capability_commands
                        .iter()
                        .map(capability_command_usage)
                        .collect::<Vec<_>>()
                        .join(" · ");
                    self.push_system(format!("commands: {caps}"));
                }
                self.push_system(
                    "input: ←/→ edit · Alt/Shift-Enter newline · scroll: use the terminal scrollback"
                        .into(),
                );
                self.push_system("approvals: y allow · n / Esc deny · exit: Esc / Ctrl-D".into());
            }
            "tools" => {
                self.push_system(format!("tools: {}", self.startup.tool_names.join(", ")));
            }
            "cwd" => {
                self.push_system(format!(
                    "workspace root: {}",
                    self.startup.workspace_root.display()
                ));
            }
            "clear" => {
                self.lines.clear();
                self.printed_lines = 0;
                self.emit_system_banner();
            }
            "quit" | "exit" => self.should_quit = true,
            other => {
                if let Some(descriptor) = self
                    .startup
                    .capability_commands
                    .iter()
                    .find(|c| c.name == other)
                    .cloned()
                {
                    self.invoke_capability_command(descriptor, arg.to_string())
                        .await;
                } else {
                    self.push_system(format!("unknown command: /{other}"));
                }
            }
        }
    }

    /// Dispatch a capability-provided slash command.
    ///
    /// `System` commands execute through `runtime.execute_command` — the
    /// capability's own handler runs and the result is rendered inline. This
    /// is the path `/setup` now takes. `Skill` commands match the web UI's
    /// behavior: the literal `/name args` text is sent as a chat message so
    /// the LLM activates the skill.
    async fn invoke_capability_command(&mut self, descriptor: CommandDescriptor, args: String) {
        let trimmed = args.trim();
        let required_missing = descriptor
            .args
            .iter()
            .any(|a| a.required && trimmed.is_empty());
        if required_missing {
            let needed: Vec<&str> = descriptor
                .args
                .iter()
                .filter(|a| a.required)
                .map(|a| a.name.as_str())
                .collect();
            self.push_system(format!(
                "/{} requires: {}",
                descriptor.name,
                needed.join(", ")
            ));
            return;
        }

        match descriptor.source {
            CommandSource::System => {
                if descriptor.name == "setup" && trimmed.is_empty() {
                    self.start_setup();
                    return;
                }

                let request = everruns_core::command::ExecuteCommandRequest {
                    name: descriptor.name.clone(),
                    arguments: if trimmed.is_empty() {
                        None
                    } else {
                        Some(trimmed.to_string())
                    },
                    controls: None,
                };
                let result = self
                    .handles
                    .runtime
                    .execute_command(self.handles.session_id, request)
                    .await;
                match result {
                    Ok(result) => {
                        let prefix = if result.success { "" } else { "error: " };
                        self.push_system(format!("{prefix}{}", result.message));
                    }
                    Err(err) => self.push_system(format!("/{} failed: {err}", descriptor.name)),
                }
            }
            CommandSource::Skill => {
                let text = if trimmed.is_empty() {
                    format!("/{}", descriptor.name)
                } else {
                    format!("/{} {trimmed}", descriptor.name)
                };
                self.push_user(text.clone());
                self.start_turn(text);
            }
        }
    }

    fn start_setup(&mut self) {
        self.setup = Some(SetupStep::ChooseAction);
        self.push_system(format!("current setup: {}", self.model.provider_label()));
        self.push_system("what do you want to change?".into());
        self.push_system("1 provider, API key, and model".into());
        self.push_system("2 API key".into());
        self.push_system("3 model".into());
        self.push_system("4 use offline demo mode".into());
        self.push_system("5 done".into());
    }

    fn start_first_run_setup(&mut self) {
        self.push_system("setup: choose a provider to get started".into());
        self.ask_provider();
    }

    fn ask_provider(&mut self) {
        self.setup = Some(SetupStep::PickProvider);
        self.push_system("choose a provider:".into());
        for (idx, option) in PROVIDER_OPTIONS.iter().enumerate() {
            self.push_system(format!("{} {} — {}", idx + 1, option.label, option.hint));
        }
    }

    fn ask_token_provider(&mut self) {
        self.setup = Some(SetupStep::PickTokenProvider);
        self.push_system("API key for which provider?".into());
        for (idx, option) in PROVIDER_OPTIONS
            .iter()
            .filter(|option| option.name != "llmsim")
            .enumerate()
        {
            self.push_system(format!("{} {}", idx + 1, option.label));
        }
    }

    fn current_provider_name(&self) -> String {
        self.model
            .provider_label()
            .split('/')
            .next()
            .unwrap_or("openai")
            .trim()
            .to_string()
    }

    fn provider_label(name: &str) -> &'static str {
        PROVIDER_OPTIONS
            .iter()
            .find(|option| option.name == name)
            .map(|option| option.label)
            .unwrap_or("provider")
    }

    fn parse_provider_choice(input: &str) -> Option<&'static str> {
        let value = input.trim().to_ascii_lowercase();
        if value.is_empty() {
            return None;
        }
        if let Ok(index) = value.parse::<usize>() {
            return PROVIDER_OPTIONS
                .get(index.saturating_sub(1))
                .map(|p| p.name);
        }
        PROVIDER_OPTIONS
            .iter()
            .find(|option| {
                option.name == value
                    || option.label.to_ascii_lowercase() == value
                    || (option.name == "llmsim"
                        && matches!(value.as_str(), "offline" | "demo" | "offline demo"))
            })
            .map(|option| option.name)
    }

    /// Drive the setup wizard one step forward. State mutations go through the
    /// single `/setup` capability so provider, token, and model setup have one
    /// user-facing command.
    async fn advance_setup(&mut self, input: &str) {
        let Some(step) = self.setup.clone() else {
            return;
        };
        match step {
            SetupStep::ChooseAction => {
                let choice = input.trim().to_ascii_lowercase();
                match choice.as_str() {
                    "" | "1" | "provider" | "providers" | "all" => {
                        self.ask_provider();
                    }
                    "2" | "key" | "api key" | "token" => {
                        self.ask_token_provider();
                    }
                    "3" | "model" => {
                        let provider = self.current_provider_name();
                        if provider == "llmsim" {
                            self.push_system(
                                "offline mode uses the fixed llmsim model; choose provider first to use another model"
                                    .into(),
                            );
                            self.ask_provider();
                            return;
                        }
                        let current = self.model.provider_label();
                        self.push_system(format!(
                            "model for {} (Enter to keep `{current}`, or paste a model id)",
                            Self::provider_label(&provider)
                        ));
                        self.setup = Some(SetupStep::PickModel {
                            provider,
                            default_model: current,
                        });
                    }
                    "4" | "offline" | "llmsim" | "demo" => {
                        let _ = self
                            .run_system_command("setup", Some("provider llmsim"))
                            .await;
                        self.push_system("setup complete — using offline demo mode".into());
                        self.setup = None;
                    }
                    "5" | "done" | "cancel" | "quit" => {
                        self.push_system("setup unchanged".into());
                        self.setup = None;
                    }
                    _ => {
                        self.push_system(
                            "choose 1 provider, 2 API key, 3 model, 4 offline, or 5 done".into(),
                        );
                    }
                }
            }
            SetupStep::PickProvider => {
                let Some(name) = Self::parse_provider_choice(input) else {
                    self.push_system("choose a provider by number or name".into());
                    self.ask_provider();
                    return;
                };
                if name == "llmsim" {
                    // Offline simulator: no token, no model picker.
                    let _ = self
                        .run_system_command("setup", Some("provider llmsim"))
                        .await;
                    self.push_system("setup complete — using offline demo mode".into());
                    self.setup = None;
                    return;
                }
                let default_model = crate::runtime::ProviderChoice::default_for_provider_name(name)
                    .map(|p| p.label())
                    .unwrap_or_else(|_| name.to_string());
                self.push_system(format!(
                    "API key for {} (paste it, or press Enter to use env vars)",
                    Self::provider_label(name)
                ));
                self.setup = Some(SetupStep::EnterToken {
                    provider: name.to_string(),
                    default_model,
                    activate: true,
                });
            }
            SetupStep::PickTokenProvider => {
                let Some(provider) = Self::parse_provider_choice(input) else {
                    self.push_system("choose a provider by number or name".into());
                    self.ask_token_provider();
                    return;
                };
                if provider == "llmsim" {
                    self.push_system("offline demo mode does not use an API key".into());
                    self.ask_token_provider();
                    return;
                }
                let default_model = self.model.provider_label();
                self.push_system(format!(
                    "API key for {} (paste it, or type clear to remove the saved key)",
                    Self::provider_label(provider)
                ));
                self.setup = Some(SetupStep::EnterToken {
                    provider: provider.to_string(),
                    default_model,
                    activate: false,
                });
            }
            SetupStep::EnterToken {
                provider,
                default_model,
                activate,
            } => {
                let token = input.trim();
                if token.eq_ignore_ascii_case("clear") {
                    let arg = format!("token {provider} clear");
                    let _ = self.run_system_command("setup", Some(&arg)).await;
                    self.push_system("setup complete — API key cleared".into());
                    self.setup = None;
                    return;
                } else if !token.is_empty() {
                    // Don't echo the token; successful internal setup
                    // commands stay out of the transcript.
                    let arg = format!("token {provider} {token}");
                    let _ = self.run_system_command("setup", Some(&arg)).await;
                } else if activate {
                    self.push_system(format!(
                        "no token entered — relying on env vars for {provider}"
                    ));
                } else {
                    self.push_system(format!("setup unchanged — using env vars for {provider}"));
                }
                if !activate {
                    if !token.is_empty() {
                        self.push_system("setup complete — API key updated".into());
                    }
                    self.setup = None;
                    return;
                }
                // Now flip the runtime to the chosen provider. With the
                // token just saved (or env var present), this picks up the
                // credential transparently.
                let arg = format!("provider {provider}");
                let switched = self.run_system_command("setup", Some(&arg)).await;
                if !switched {
                    self.push_system(format!(
                        "couldn't activate {provider} — paste a token now, or press Enter after setting the provider env var"
                    ));
                    self.setup = Some(SetupStep::EnterToken {
                        provider,
                        default_model,
                        activate,
                    });
                    return;
                }
                self.push_system(format!(
                    "model for {} (Enter to keep `{default_model}`, or paste a model id)",
                    Self::provider_label(&provider)
                ));
                self.setup = Some(SetupStep::PickModel {
                    provider,
                    default_model,
                });
            }
            SetupStep::PickModel {
                provider,
                default_model,
            } => {
                let model = input.trim();
                if !model.is_empty() {
                    let spec = if model.contains('/') {
                        model.to_string()
                    } else {
                        format!("{provider}/{model}")
                    };
                    let arg = format!("model {spec}");
                    let _ = self.run_system_command("setup", Some(&arg)).await;
                }
                self.push_system(format!(
                    "setup complete — provider {provider}, model {}. run /setup to change it",
                    if model.is_empty() {
                        default_model.as_str()
                    } else {
                        model
                    }
                ));
                self.setup = None;
            }
        }
    }

    /// Execute an internal System slash command for the setup wizard. Returns
    /// whether the capability reported success. Successful mutations stay
    /// quiet so the user only sees the wizard's prompts and completion lines.
    async fn run_system_command(&mut self, name: &str, arg: Option<&str>) -> bool {
        let request = everruns_core::command::ExecuteCommandRequest {
            name: name.to_string(),
            arguments: arg.map(str::to_string),
            controls: None,
        };
        match self
            .handles
            .runtime
            .execute_command(self.handles.session_id, request)
            .await
        {
            Ok(result) if result.success => true,
            Ok(result) => {
                self.push_system(format!("error: {}", result.message));
                false
            }
            Err(err) => {
                self.push_system(format!("/{name} failed: {err}"));
                false
            }
        }
    }

    fn start_turn(&mut self, prompt: String) {
        let handles = self.handles.clone();
        let model = self.model.clone();
        let (tx, rx) = mpsc::unbounded_channel::<TurnEvent>();
        self.rx = Some(rx);
        self.busy = true;
        self.turn_activity = None;
        self.stream_preview = None;

        // Subscribe BEFORE spawning the turn so we don't miss the first
        // few events (turn.started, reason.started). The broadcast only
        // delivers events emitted after subscribe().
        let mut live = handles.events.subscribe();

        tokio::spawn(async move {
            let session_id = handles.session_id;
            let before = match handles.runtime.messages(session_id).await {
                Ok(m) => m.len(),
                Err(e) => {
                    let _ = tx.send(TurnEvent::Failed(format!("load history: {e}")));
                    let _ = tx.send(TurnEvent::Done);
                    return;
                }
            };
            let events_before = match handles.runtime.events().await {
                Ok(e) => e.len(),
                Err(_) => 0,
            };

            let input = model.input_message(prompt);
            let runtime = handles.runtime.clone();
            let turn = tokio::spawn(async move { runtime.run_turn(session_id, input).await });

            let mut emitted_events = HashSet::new();
            let mut delta_router = DeltaRouter::default();
            loop {
                tokio::select! {
                    biased;
                    recv = live.recv() => match recv {
                        Ok(event) => {
                            if event.session_id != session_id {
                                continue;
                            }
                            handle_live_event(
                                &event,
                                &mut emitted_events,
                                &mut delta_router,
                                &tx,
                            );
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => {
                            // Receiver overflow: catch up from the canonical
                            // event vec so we don't lose persistent events.
                            // Resubscribe to restart from the current head.
                            live = handles.events.subscribe();
                            catch_up_events(
                                &handles,
                                events_before,
                                &mut emitted_events,
                                &tx,
                            )
                            .await;
                        }
                        Err(broadcast::error::RecvError::Closed) => break,
                    },
                    _ = tokio::time::sleep(Duration::from_millis(200)) => {
                        if turn.is_finished() {
                            break;
                        }
                    }
                }
            }

            // Drain any tail events emitted between the last broadcast
            // poll and the turn's actual completion.
            catch_up_events(&handles, events_before, &mut emitted_events, &tx).await;
            // Clear any in-flight streaming preview before we finalize.
            let _ = tx.send(TurnEvent::Stream(None));

            let result = match turn.await {
                Ok(result) => result,
                Err(e) => {
                    let _ = tx.send(TurnEvent::Failed(format!("turn task: {e}")));
                    let _ = tx.send(TurnEvent::Done);
                    return;
                }
            };
            let response = match result {
                Ok(r) => r,
                Err(e) => {
                    let _ = tx.send(TurnEvent::Failed(format!("{e}")));
                    let _ = tx.send(TurnEvent::Done);
                    return;
                }
            };

            let messages = handles
                .runtime
                .messages(session_id)
                .await
                .unwrap_or_default();

            let mut out = Vec::new();
            // Assistant text from the turn.
            for msg in messages.iter().skip(before) {
                if msg.role == MessageRole::Agent
                    && !msg.has_tool_calls()
                    && let Some(text) = msg.text()
                {
                    let trimmed = text.trim();
                    if !trimmed.is_empty() {
                        out.push(ChatLine {
                            author: Author::Assistant,
                            text: trimmed.to_string(),
                        });
                    }
                }
            }
            if out.is_empty() && !response.response.is_empty() {
                out.push(ChatLine {
                    author: Author::Assistant,
                    text: response.response,
                });
            }
            if !response.success
                && let Some(err) = response.error
            {
                out.push(ChatLine {
                    author: Author::System,
                    text: format!("turn error: {err}"),
                });
            }
            let _ = tx.send(TurnEvent::Lines(out));
            let _ = tx.send(TurnEvent::Done);
        });
    }
}

/// Tracks the most recently active delta stream so we can drop the
/// preview as soon as a matching `*.completed` arrives. Per-turn state
/// — one `DeltaRouter` per `start_turn` invocation.
#[derive(Default)]
pub(crate) struct DeltaRouter {
    last_assistant_turn: Option<everruns_core::typed_id::TurnId>,
    last_thinking_turn: Option<everruns_core::typed_id::TurnId>,
    last_tool_call: Option<String>,
}

pub(crate) fn handle_live_event(
    event: &RuntimeEvent,
    emitted_events: &mut HashSet<String>,
    router: &mut DeltaRouter,
    tx: &mpsc::UnboundedSender<TurnEvent>,
) {
    if !emitted_events.insert(event.id.to_string()) {
        return;
    }

    match &event.data {
        EventData::OutputMessageDelta(data) => {
            router.last_assistant_turn = Some(data.turn_id);
            let _ = tx.send(TurnEvent::Stream(Some(StreamPreview {
                kind: StreamKind::Assistant,
                text: data.accumulated.clone(),
            })));
            return;
        }
        EventData::OutputMessageCompleted(_) | EventData::OutputMessageReplaced(_)
            if router.last_assistant_turn.is_some() =>
        {
            router.last_assistant_turn = None;
            let _ = tx.send(TurnEvent::Stream(None));
        }
        EventData::ReasonThinkingDelta(data) => {
            router.last_thinking_turn = Some(data.turn_id);
            let _ = tx.send(TurnEvent::Stream(Some(StreamPreview {
                kind: StreamKind::Thinking,
                text: data.accumulated.clone(),
            })));
            return;
        }
        EventData::ReasonThinkingCompleted(_) if router.last_thinking_turn.is_some() => {
            router.last_thinking_turn = None;
            let _ = tx.send(TurnEvent::Stream(None));
        }
        EventData::ToolOutputDelta(data) => {
            router.last_tool_call = Some(data.tool_call_id.clone());
            let text = format!(
                "{} [{}] {}",
                data.tool_name,
                data.stream,
                data.delta.trim_end()
            );
            let _ = tx.send(TurnEvent::Stream(Some(StreamPreview {
                kind: StreamKind::Tool,
                text,
            })));
            return;
        }
        EventData::ToolCompleted(data)
            if router.last_tool_call.as_deref() == Some(data.tool_call_id.as_str()) =>
        {
            router.last_tool_call = None;
            let _ = tx.send(TurnEvent::Stream(None));
        }
        _ => {}
    }

    if let Some(activity) = status_for_event(event) {
        let _ = tx.send(TurnEvent::Activity(activity));
    }
    let lines = lines_for_event(event);
    if !lines.is_empty() {
        let _ = tx.send(TurnEvent::Lines(lines));
    }
}

/// Drain any persisted events (from `runtime.events()`) that the broadcast
/// receiver may have missed — used after a `Lagged` recv error and once
/// more at end-of-turn so the transcript is never missing tool/reason
/// completion lines.
async fn catch_up_events(
    handles: &RuntimeHandles,
    events_before: usize,
    emitted_events: &mut HashSet<String>,
    tx: &mpsc::UnboundedSender<TurnEvent>,
) {
    let events = handles.runtime.events().await.unwrap_or_default();
    let mut lines = Vec::new();
    for event in events.iter().skip(events_before) {
        let event_id = event.id.to_string();
        if !emitted_events.insert(event_id) {
            continue;
        }
        if let Some(activity) = status_for_event(event) {
            let _ = tx.send(TurnEvent::Activity(activity));
        }
        lines.extend(lines_for_event(event));
    }
    if !lines.is_empty() {
        let _ = tx.send(TurnEvent::Lines(lines));
    }
}

pub fn lines_for_event(event: &RuntimeEvent) -> Vec<ChatLine> {
    match &event.data {
        EventData::ReasonStarted(_) => Vec::new(),
        EventData::ReasonCompleted(data) => {
            // Render pre-tool-call narration. When there are no tool calls
            // the final assistant message will arrive via the message loop
            // shortly, so we'd duplicate it; keep the has_tool_calls gate.
            if data.success && data.has_tool_calls {
                let mut lines = Vec::new();
                if let Some(text) = data
                    .text_preview
                    .as_deref()
                    .map(str::trim)
                    .filter(|text| !text.is_empty())
                {
                    lines.push(ChatLine {
                        author: Author::Assistant,
                        text: text.to_string(),
                    });
                }
                lines
            } else {
                Vec::new()
            }
        }
        EventData::ReasonItem(data) => data
            .summary
            .iter()
            .filter_map(|segment| {
                let trimmed = segment.trim();
                (!trimmed.is_empty()).then(|| ChatLine {
                    author: Author::Assistant,
                    text: trimmed.to_string(),
                })
            })
            .collect(),
        EventData::OutputMessageCompleted(_) => Vec::new(),
        EventData::ToolCompleted(data) => {
            if data.tool_name == "write_todos" {
                return todo_lines_for_result(data);
            }
            let marker = if data.success { "✓" } else { "✗" };
            let label = data
                .narration
                .as_deref()
                .or(data.display_name.as_deref())
                .unwrap_or(data.tool_name.as_str());
            let summary = summarize_tool_result(data);
            let mut lines = vec![ChatLine {
                author: Author::Tool,
                text: if summary.is_empty() {
                    format!("{marker} {label}")
                } else {
                    format!("{marker} {label}  {summary}")
                },
            }];
            if data.tool_name == "edit_file"
                && let Some(diff) = extract_field(data, "diff")
            {
                for line in diff.lines().take(40) {
                    lines.push(ChatLine {
                        author: Author::Diff,
                        text: line.to_string(),
                    });
                }
            }
            lines
        }
        _ => Vec::new(),
    }
}

fn lines_for_replayed_event(event: &RuntimeEvent) -> Vec<ChatLine> {
    match &event.data {
        EventData::InputMessage(data) => message_line(Author::User, &data.message)
            .into_iter()
            .collect(),
        EventData::OutputMessageCompleted(data) => {
            if data.message.role == MessageRole::Agent {
                message_line(Author::Assistant, &data.message)
                    .into_iter()
                    .collect()
            } else {
                Vec::new()
            }
        }
        _ => lines_for_event(event),
    }
}

fn message_line(author: Author, message: &Message) -> Option<ChatLine> {
    let text = message.text()?;
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    Some(ChatLine {
        author,
        text: text.to_string(),
    })
}

pub fn status_for_event(event: &RuntimeEvent) -> Option<ActivityStatus> {
    match &event.data {
        EventData::ReasonStarted(_) => Some(fallback_status("thinking")),
        EventData::ReasonCompleted(data) => {
            if !data.success {
                let err = data.error.as_deref().unwrap_or("reasoning failed");
                return Some(activity_status(format!(
                    "reasoning failed: {}",
                    first_line(err, 100)
                )));
            }
            data.has_tool_calls
                .then(|| activity_status(format!("planned {} tool call(s)", data.tool_call_count)))
        }
        EventData::ActStarted(data) => data
            .headline
            .clone()
            .or_else(|| Some(format!("running {} tool(s)", data.tool_calls.len())))
            .map(activity_status),
        EventData::ActCompleted(data) => data
            .headline
            .clone()
            .or_else(|| {
                Some(format!(
                    "tools finished: {} ok, {} failed",
                    data.success_count, data.error_count
                ))
            })
            .map(activity_status),
        EventData::ToolStarted(data) => Some(activity_status(format!(
            "→ {}",
            data.narration
                .as_deref()
                .or(data.display_name.as_deref())
                .unwrap_or(data.tool_call.name.as_str())
        ))),
        EventData::ToolProgress(data) => Some(activity_status(format!(
            "… {}: {}",
            data.display_name
                .as_deref()
                .unwrap_or(data.tool_name.as_str()),
            first_line(&data.message, 100)
        ))),
        EventData::ToolCallRequested(data) => Some(activity_status(format!(
            "waiting for {} client tool result(s)",
            data.tool_calls.len()
        ))),
        EventData::OutputMessageStarted(data) => {
            let iteration = data.iteration.unwrap_or(1);
            Some(activity_status(format!(
                "iteration {iteration}: writing response"
            )))
        }
        EventData::ReasonThinkingStarted(_) => Some(fallback_status("thinking deeply")),
        EventData::TurnCancelled(_) => Some(activity_status("turn cancelled")),
        EventData::TurnFailed(data) => Some(activity_status(format!(
            "turn failed: {}",
            first_line(&data.error, 100)
        ))),
        _ => None,
    }
}

fn activity_status(text: impl Into<String>) -> ActivityStatus {
    ActivityStatus {
        text: text.into(),
        fallback: false,
    }
}

fn fallback_status(text: impl Into<String>) -> ActivityStatus {
    ActivityStatus {
        text: text.into(),
        fallback: true,
    }
}

fn command_suggestions(
    input: &str,
    capability_commands: &[CommandDescriptor],
) -> Vec<CommandSuggestion> {
    let Some(rest) = input.strip_prefix('/') else {
        return Vec::new();
    };

    // If the user already typed a command name and a space, surface the
    // first-arg suggestions declared by the matching capability. This is
    // fully declarative — the capability populates `CommandArg::suggestions`
    // when it builds its `CommandDescriptor`, so the UI never has to call
    // back into the capability between keystrokes.
    if let Some((head, arg_prefix)) = rest.split_once(' ')
        && let Some(descriptor) = capability_commands.iter().find(|c| c.name == head)
        && let Some(arg) = descriptor.args.first()
        && !arg.suggestions.is_empty()
    {
        let prefix = arg_prefix.trim_start();
        return arg
            .suggestions
            .iter()
            .filter(|s| s.starts_with(prefix))
            .take(8)
            .map(|s| CommandSuggestion {
                completion: format!("/{} {s}", descriptor.name),
                label: format!("/{} {s}    {}", descriptor.name, descriptor.description),
            })
            .collect();
    }

    let mut out: Vec<CommandSuggestion> = COMMANDS
        .iter()
        .filter(|cmd| cmd.name.starts_with(rest))
        .map(|cmd| CommandSuggestion {
            completion: cmd
                .usage
                .split_whitespace()
                .next()
                .unwrap_or(cmd.usage)
                .to_string(),
            label: format!("{}    {}", cmd.usage, cmd.description),
        })
        .collect();

    // Capability-provided commands. Names that collide with a built-in CLI
    // command are skipped (built-in wins) so the local handler keeps running.
    let builtin_names: std::collections::HashSet<&str> = COMMANDS.iter().map(|c| c.name).collect();
    for descriptor in capability_commands {
        if !descriptor.name.starts_with(rest) {
            continue;
        }
        if builtin_names.contains(descriptor.name.as_str()) {
            continue;
        }
        let usage = capability_command_usage(descriptor);
        // If the command takes args, leave a trailing space so the user can
        // start typing immediately after accepting the suggestion.
        let completion = if descriptor.args.is_empty() {
            format!("/{}", descriptor.name)
        } else {
            format!("/{} ", descriptor.name)
        };
        out.push(CommandSuggestion {
            completion,
            label: format!("{usage}    {}", descriptor.description),
        });
    }

    out.truncate(8);
    out
}

fn capability_command_usage(descriptor: &CommandDescriptor) -> String {
    if descriptor.args.is_empty() {
        format!("/{}", descriptor.name)
    } else {
        let args = descriptor
            .args
            .iter()
            .map(|a| {
                if a.required {
                    format!("<{}>", a.name)
                } else {
                    format!("[{}]", a.name)
                }
            })
            .collect::<Vec<_>>()
            .join(" ");
        format!("/{} {args}", descriptor.name)
    }
}

fn new_input_area(lines: Vec<String>) -> TextArea<'static> {
    let mut input = TextArea::new(lines);
    input.set_wrap_mode(WrapMode::Word);
    input.set_style(Style::default().fg(TEXT_PRIMARY));
    input.set_cursor_line_style(Style::default());
    input.set_cursor_style(Style::default().add_modifier(Modifier::REVERSED));
    input
}

// ---------- helpers for surfacing tool results ----------

fn result_value(data: &ToolCompletedData) -> Option<Value> {
    let parts = data.result.as_ref()?;
    for part in parts {
        if let ContentPart::Text(t) = part
            && let Ok(v) = serde_json::from_str::<Value>(&t.text)
        {
            return Some(v);
        }
    }
    None
}

fn extract_field(data: &ToolCompletedData, field: &str) -> Option<String> {
    let v = result_value(data)?;
    v.get(field).and_then(|s| s.as_str()).map(str::to_string)
}

const MAX_RENDERED_TODOS: usize = 20;
const MAX_TODO_TEXT_CHARS: usize = 160;

fn truncate_chars(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let truncated: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{truncated}…")
    } else {
        truncated
    }
}

fn todo_lines_for_result(data: &ToolCompletedData) -> Vec<ChatLine> {
    let Some(v) = result_value(data) else {
        return vec![ChatLine {
            author: Author::Tool,
            text: format!(
                "✓ {}",
                data.display_name.as_deref().unwrap_or("Write Todos")
            ),
        }];
    };
    let Some(todos) = v.get("todos").and_then(Value::as_array) else {
        return vec![ChatLine {
            author: Author::Tool,
            text: summarize_tool_result(data),
        }];
    };

    let total = todos.len();
    let completed = todos
        .iter()
        .filter(|todo| todo.get("status").and_then(Value::as_str) == Some("completed"))
        .count();
    let summary = format!("{completed} of {total} todos completed");
    let mut rendered_todos = Vec::new();
    for todo in todos.iter().take(MAX_RENDERED_TODOS) {
        let status = todo
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("pending");
        let content = todo.get("content").and_then(Value::as_str).unwrap_or("");
        let active_form = todo
            .get("activeForm")
            .and_then(Value::as_str)
            .unwrap_or(content);
        let (icon, text) = match status {
            "completed" => ("✓", content),
            "in_progress" => ("›", active_form),
            _ => ("○", content),
        };
        rendered_todos.push(format!(
            "{icon} {}",
            truncate_chars(text, MAX_TODO_TEXT_CHARS)
        ));
    }

    let mut lines = if rendered_todos.len() <= 3 {
        let inline_todos = rendered_todos.join("  ");
        vec![ChatLine {
            author: Author::Tool,
            text: if inline_todos.is_empty() {
                summary
            } else {
                format!("{summary}  {inline_todos}")
            },
        }]
    } else {
        let mut lines = vec![ChatLine {
            author: Author::Tool,
            text: summary,
        }];
        lines.extend(rendered_todos.into_iter().map(|text| ChatLine {
            author: Author::ToolDetail,
            text,
        }));
        lines
    };

    let omitted = total.saturating_sub(MAX_RENDERED_TODOS);
    if omitted > 0 {
        lines.push(ChatLine {
            author: Author::ToolDetail,
            text: format!("… {omitted} more todo(s) omitted"),
        });
    }

    if let Some(warning) = v.get("warning").and_then(Value::as_str) {
        lines.push(ChatLine {
            author: Author::ToolDetail,
            text: format!("warning: {}", truncate_chars(warning, MAX_TODO_TEXT_CHARS)),
        });
    }

    lines
}

/// One-line summary of a tool result, used in the transcript and `--print` output.
pub fn summarize_tool_result(data: &ToolCompletedData) -> String {
    let Some(v) = result_value(data) else {
        if let Some(err) = &data.error {
            return format!("error: {}", first_line(err, 120));
        }
        return String::new();
    };
    // Field names match the built-in `session_file_system` capability's
    // result shapes. See crates/core/src/capabilities/file_system.rs.
    match data.tool_name.as_str() {
        "write_todos" => {
            let completed = v.get("completed").and_then(Value::as_u64).unwrap_or(0);
            let total = v.get("total_tasks").and_then(Value::as_u64).unwrap_or(0);
            format!("{completed}/{total} completed")
        }
        "read_file" => {
            let path = v.get("path").and_then(Value::as_str).unwrap_or("");
            let total = v.get("total_lines").and_then(Value::as_u64).unwrap_or(0);
            let shown = v.get("lines_shown");
            let start = shown
                .and_then(|s| s.get("start"))
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let end = shown
                .and_then(|s| s.get("end"))
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let count = end.saturating_sub(start.saturating_sub(1));
            format!("{path} ({count}/{total} lines)")
        }
        "write_file" => {
            let path = v.get("path").and_then(Value::as_str).unwrap_or("");
            let bytes = v.get("size_bytes").and_then(Value::as_u64).unwrap_or(0);
            format!("{path} ({bytes} bytes)")
        }
        "edit_file" => {
            let path = v.get("path").and_then(Value::as_str).unwrap_or("");
            let n = v.get("applied_edits").and_then(Value::as_u64).unwrap_or(0);
            format!("{path} ({n} edit(s))")
        }
        "list_directory" => {
            let path = v.get("path").and_then(Value::as_str).unwrap_or("");
            let n = v.get("count").and_then(Value::as_u64).unwrap_or(0);
            format!("{path} ({n} entries)")
        }
        "grep_files" => {
            let pattern = v.get("pattern").and_then(Value::as_str).unwrap_or("");
            let n = v.get("match_count").and_then(Value::as_u64).unwrap_or(0);
            format!("/{pattern}/ ({n} match(es))")
        }
        "delete_file" => {
            let path = v.get("path").and_then(Value::as_str).unwrap_or("");
            format!("{path} (deleted)")
        }
        "stat_file" => {
            let path = v.get("path").and_then(Value::as_str).unwrap_or("");
            let size = v.get("size_bytes").and_then(Value::as_u64).unwrap_or(0);
            format!("{path} ({size} bytes)")
        }
        "bash" => {
            let cmd = v
                .get("command")
                .and_then(Value::as_str)
                .map(|c| first_line(c, 80))
                .unwrap_or_default();
            let code = v
                .get("exit_code")
                .and_then(Value::as_i64)
                .map(|c| c.to_string())
                .unwrap_or_else(|| "?".into());
            format!("`{cmd}` exit={code}")
        }
        _ => String::new(),
    }
}

fn first_line(s: &str, max: usize) -> String {
    let l = s.lines().next().unwrap_or("");
    if l.len() > max {
        format!("{}…", &l[..max])
    } else {
        l.to_string()
    }
}

// ---------- rendering ----------

fn draw(f: &mut ratatui::Frame, app: &mut App) {
    let input_height = app.input_height();
    let area = f.area();
    let state = app.view_state();

    // Chrome renders the non-input rows; we then layer the input field
    // on top into the chrome-reserved input slot.
    let input_rect = draw_chrome(f, area, input_height, &state);
    draw_input(f, input_rect, app);
}

/// Render the non-input chrome (command suggestions / stream preview,
/// message separator, status separator, session status) into `area` using `state`, and
/// return the `Rect` where the caller should render the input widget
/// (which needs `&mut` and so cannot be driven through `ViewState`).
///
/// Snapshot tests call this against a `TestBackend` and ignore the
/// returned input rect — the buffer's other rows are what they assert
/// against.
pub(crate) fn draw_chrome(
    f: &mut ratatui::Frame,
    area: Rect,
    input_height: u16,
    state: &ViewState,
) -> Rect {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),            // suggestions / stream preview
            Constraint::Length(1),            // message separator
            Constraint::Length(input_height), // input (left to the caller)
            Constraint::Length(1),            // status separator
            Constraint::Length(1),            // session status
        ])
        .split(area);

    if state.command_suggestions.is_empty() {
        draw_stream_preview(f, chunks[0], state);
    } else {
        draw_suggestions(f, chunks[0], &state.command_suggestions);
    }
    draw_message_separator(f, chunks[1], state);
    draw_status_separator(f, chunks[3]);
    draw_session_status(f, chunks[4], state);

    chunks[2]
}

fn draw_suggestions(f: &mut ratatui::Frame, area: Rect, suggestions: &[CommandSuggestion]) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    f.render_widget(
        Paragraph::new(suggestion_preview_line(suggestions, area.width)),
        area,
    );
}

fn suggestion_preview_line(suggestions: &[CommandSuggestion], width: u16) -> Line<'static> {
    let body = suggestions
        .iter()
        .map(|suggestion| suggestion.label.as_str())
        .collect::<Vec<_>>()
        .join("  ·  ");
    let prefix = "Tab ";
    let max_body = (width as usize)
        .saturating_sub(prefix.chars().count() + 1)
        .max(8);
    Line::from(vec![
        Span::styled(
            prefix,
            Style::default()
                .fg(ACCENT_BLUE)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            truncate_end_chars(&body, max_body),
            Style::default().fg(TEXT_MUTED),
        ),
    ])
}

fn draw_stream_preview(f: &mut ratatui::Frame, area: Rect, state: &ViewState) {
    if area.height == 0 {
        return;
    }
    let Some(preview) = state.stream_preview.as_ref() else {
        return;
    };
    let inner_width = area.width as usize;
    if inner_width == 0 {
        return;
    }
    // Show the most recent line of the accumulated stream so the eye
    // tracks the live tail rather than the start of a long response.
    let tail = preview
        .text
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("");
    let label = preview.kind.label();
    let prefix = format!("{label} › ");
    let prefix_w = prefix.chars().count();
    let max_text = inner_width.saturating_sub(prefix_w + 1).max(8);
    let truncated = truncate_tail_chars(tail, max_text);
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                prefix,
                Style::default()
                    .fg(preview.kind.color())
                    .add_modifier(Modifier::DIM),
            ),
            Span::styled(truncated, Style::default().fg(TEXT_MUTED)),
        ])),
        area,
    );
}

/// Keep the last `max_chars` of `text`. Streaming preview reads better
/// when the cursor (tail of the stream) is what's visible.
fn truncate_tail_chars(text: &str, max_chars: usize) -> String {
    let count = text.chars().count();
    if count <= max_chars {
        return text.to_string();
    }
    let skip = count - max_chars.saturating_sub(1);
    let mut out = String::with_capacity(max_chars);
    out.push('…');
    out.extend(text.chars().skip(skip));
    out
}

fn truncate_end_chars(text: &str, max_chars: usize) -> String {
    let count = text.chars().count();
    if count <= max_chars {
        return text.to_string();
    }
    if max_chars == 0 {
        return String::new();
    }
    if max_chars == 1 {
        return "…".to_string();
    }
    let mut out = String::with_capacity(max_chars);
    out.extend(text.chars().take(max_chars - 1));
    out.push('…');
    out
}

fn should_insert_chat_gap(current: &Author, next: Option<&Author>) -> bool {
    let Some(next) = next else {
        return false;
    };

    !matches!(
        (current, next),
        (&Author::Tool, &Author::Tool)
            | (&Author::Tool, &Author::ToolDetail)
            | (&Author::ToolDetail, &Author::Tool)
            | (&Author::ToolDetail, &Author::ToolDetail)
    )
}

fn append_chat_lines<'a>(lines: &mut Vec<Line<'a>>, chat: &ChatLine, inner_width: usize) {
    if matches!(chat.author, Author::ToolDetail) {
        append_wrapped_plain(
            lines,
            "           ",
            Style::default().fg(TEXT_MUTED),
            &chat.text,
            inner_width,
        );
        return;
    }

    let header_text = format!("{} › ", chat.author.label());
    let header_style = Style::default()
        .fg(chat.author.color())
        .add_modifier(Modifier::BOLD);
    if matches!(chat.author, Author::Assistant) {
        append_markdown_lines(lines, &header_text, header_style, &chat.text, inner_width);
    } else {
        append_wrapped_plain(lines, &header_text, header_style, &chat.text, inner_width);
    }
}

fn append_wrapped_plain<'a>(
    lines: &mut Vec<Line<'a>>,
    first_prefix: &str,
    prefix_style: Style,
    text: &str,
    inner_width: usize,
) {
    let continuation = " ".repeat(first_prefix.chars().count());
    let wrap_width = inner_width
        .saturating_sub(first_prefix.chars().count())
        .max(20);
    let mut emitted = false;
    for raw in text.lines() {
        let wrapped = textwrap::wrap(raw, wrap_width);
        if wrapped.is_empty() {
            let prefix = if emitted {
                continuation.as_str()
            } else {
                first_prefix
            };
            lines.push(Line::from(vec![Span::styled(
                prefix.to_string(),
                prefix_style,
            )]));
            emitted = true;
            continue;
        }
        for piece in wrapped {
            let prefix = if emitted {
                continuation.as_str()
            } else {
                first_prefix
            };
            lines.push(Line::from(vec![
                Span::styled(prefix.to_string(), prefix_style),
                Span::raw(piece.into_owned()),
            ]));
            emitted = true;
        }
    }
    if !emitted {
        lines.push(Line::from(vec![Span::styled(
            first_prefix.to_string(),
            prefix_style,
        )]));
    }
}

fn append_markdown_lines<'a>(
    lines: &mut Vec<Line<'a>>,
    first_prefix: &str,
    prefix_style: Style,
    text: &str,
    inner_width: usize,
) {
    let continuation = " ".repeat(first_prefix.chars().count());
    let wrap_width = inner_width
        .saturating_sub(first_prefix.chars().count())
        .max(20);
    let mut first = true;
    let mut in_code = false;

    for raw in text.lines() {
        let trimmed = raw.trim_end();
        if let Some(lang) = trimmed.trim_start().strip_prefix("```") {
            in_code = !in_code;
            let code_lang = lang.trim();
            let label = if in_code {
                if code_lang.is_empty() {
                    "code".to_string()
                } else {
                    format!("code: {code_lang}")
                }
            } else {
                String::new()
            };
            push_markdown_line(
                lines,
                first_prefix,
                &continuation,
                prefix_style,
                &mut first,
                vec![Span::styled(
                    label,
                    Style::default().fg(TEXT_DIM).add_modifier(Modifier::ITALIC),
                )],
            );
            continue;
        }

        let content_spans = if in_code {
            markdown_code_spans(trimmed)
        } else {
            markdown_text_spans(trimmed)
        };
        let plain = spans_plain_text(&content_spans);
        let wrapped = textwrap::wrap(&plain, wrap_width);
        if wrapped.is_empty() {
            push_markdown_line(
                lines,
                first_prefix,
                &continuation,
                prefix_style,
                &mut first,
                vec![],
            );
            continue;
        }
        if content_spans.len() == 1 {
            let style = content_spans[0].style;
            for piece in wrapped {
                push_markdown_line(
                    lines,
                    first_prefix,
                    &continuation,
                    prefix_style,
                    &mut first,
                    vec![Span::styled(piece.into_owned(), style)],
                );
            }
        } else {
            push_markdown_line(
                lines,
                first_prefix,
                &continuation,
                prefix_style,
                &mut first,
                content_spans,
            );
        }
    }
}

fn push_markdown_line<'a>(
    lines: &mut Vec<Line<'a>>,
    first_prefix: &str,
    continuation: &str,
    prefix_style: Style,
    first: &mut bool,
    mut spans: Vec<Span<'a>>,
) {
    let prefix = if *first { first_prefix } else { continuation };
    let mut line_spans = vec![Span::styled(prefix.to_string(), prefix_style)];
    line_spans.append(&mut spans);
    lines.push(Line::from(line_spans));
    *first = false;
}

fn markdown_text_spans(text: &str) -> Vec<Span<'static>> {
    let trimmed = text.trim_start();
    if trimmed.starts_with('#') {
        let heading = trimmed.trim_start_matches('#').trim_start();
        return vec![Span::styled(
            heading.to_string(),
            Style::default()
                .fg(TEXT_PRIMARY)
                .add_modifier(Modifier::BOLD),
        )];
    }
    if let Some(rest) = trimmed.strip_prefix("> ") {
        return vec![
            Span::styled("| ", Style::default().fg(ACCENT_BLUE)),
            Span::styled(rest.to_string(), Style::default().fg(TEXT_MUTED)),
        ];
    }
    if let Some(rest) = trimmed
        .strip_prefix("- ")
        .or_else(|| trimmed.strip_prefix("* "))
    {
        return vec![
            Span::styled("- ", Style::default().fg(ACCENT_GOLD)),
            Span::raw(rest.to_string()),
        ];
    }
    if let Some((marker, rest)) = numbered_marker(trimmed) {
        return vec![
            Span::styled(marker, Style::default().fg(ACCENT_GOLD)),
            Span::raw(rest.to_string()),
        ];
    }
    inline_code_spans(text)
}

fn markdown_code_spans(text: &str) -> Vec<Span<'static>> {
    let mut spans = vec![Span::styled("    ", Style::default().fg(TEXT_DIM))];
    spans.extend(simple_code_highlight(text));
    spans
}

fn inline_code_spans(text: &str) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut rest = text;
    let mut code = false;
    while let Some((before, after_tick)) = rest.split_once('`') {
        if !before.is_empty() {
            spans.push(Span::raw(before.to_string()));
        }
        if let Some((inside, after)) = after_tick.split_once('`') {
            spans.push(Span::styled(
                inside.to_string(),
                Style::default().fg(TEXT_PRIMARY).bg(CODE_BG),
            ));
            rest = after;
            code = true;
        } else {
            spans.push(Span::raw("`".to_string()));
            rest = after_tick;
            break;
        }
    }
    if !rest.is_empty() {
        spans.push(Span::raw(rest.to_string()));
    }
    if spans.is_empty() || !code {
        vec![Span::raw(text.to_string())]
    } else {
        spans
    }
}

fn simple_code_highlight(text: &str) -> Vec<Span<'static>> {
    let keywords = [
        "async", "await", "const", "enum", "fn", "impl", "let", "match", "pub", "return", "struct",
        "use",
    ];
    let mut spans = Vec::new();
    let mut token = String::new();
    for ch in text.chars() {
        if ch.is_alphanumeric() || ch == '_' {
            token.push(ch);
            continue;
        }
        if !token.is_empty() {
            let style = if keywords.contains(&token.as_str()) {
                Style::default()
                    .fg(ACCENT_GOLD)
                    .add_modifier(Modifier::BOLD)
            } else if token.chars().all(|c| c.is_ascii_digit()) {
                Style::default().fg(TEXT_MUTED)
            } else {
                Style::default().fg(ACCENT_BLUE)
            };
            spans.push(Span::styled(std::mem::take(&mut token), style));
        }
        spans.push(Span::styled(ch.to_string(), Style::default().fg(TEXT_DIM)));
    }
    if !token.is_empty() {
        let style = if keywords.contains(&token.as_str()) {
            Style::default()
                .fg(ACCENT_GOLD)
                .add_modifier(Modifier::BOLD)
        } else if token.chars().all(|c| c.is_ascii_digit()) {
            Style::default().fg(TEXT_MUTED)
        } else {
            Style::default().fg(ACCENT_BLUE)
        };
        spans.push(Span::styled(token, style));
    }
    spans
}

fn numbered_marker(text: &str) -> Option<(String, &str)> {
    let dot = text.find(". ")?;
    if dot == 0 || !text[..dot].chars().all(|ch| ch.is_ascii_digit()) {
        return None;
    }
    Some((text[..dot + 2].to_string(), &text[dot + 2..]))
}

fn spans_plain_text(spans: &[Span]) -> String {
    spans.iter().map(|span| span.content.as_ref()).collect()
}

fn inset_x(area: Rect, pad: u16) -> Rect {
    let total = pad.saturating_mul(2);
    if area.width <= total {
        return area;
    }
    Rect {
        x: area.x.saturating_add(pad),
        width: area.width.saturating_sub(total),
        ..area
    }
}

fn line_width(line: &Line) -> usize {
    line.spans
        .iter()
        .map(|span| span.content.chars().count())
        .sum()
}

fn separator_line(mut title: Line<'static>, width: u16, style: Style) -> Line<'static> {
    let fill_width = (width as usize).saturating_sub(line_width(&title));
    title
        .spans
        .push(Span::styled("─".repeat(fill_width), style));
    title
}

fn draw_separator(f: &mut ratatui::Frame, area: Rect, title: Line<'static>, style: Style) {
    if area.height == 0 {
        return;
    }
    f.render_widget(
        Paragraph::new(separator_line(title, area.width, style)),
        area,
    );
}

fn draw_input(f: &mut ratatui::Frame, area: Rect, app: &mut App) {
    let area = inset_x(area, 0);
    let prompt_width = area.width.min(2);
    let prompt_area = Rect {
        width: prompt_width,
        ..area
    };
    let input_area = Rect {
        x: area.x.saturating_add(prompt_width),
        width: area.width.saturating_sub(prompt_width),
        ..area
    };
    f.render_widget(
        Paragraph::new(Span::styled(
            "> ",
            Style::default()
                .fg(ACCENT_BLUE)
                .add_modifier(Modifier::BOLD),
        )),
        prompt_area,
    );
    app.input.set_block(ratatui::widgets::Block::default());
    f.render_widget(&app.input, input_area);
    draw_input_cursor(f, input_area, app);
}

fn draw_input_cursor(f: &mut ratatui::Frame, area: Rect, app: &App) {
    if app.pending.is_some() || app.busy {
        return;
    }

    let inner_width = area.width;
    let inner_height = area.height;
    if inner_width == 0 || inner_height == 0 {
        return;
    }

    let cursor = app.input.screen_cursor();
    let x = area
        .x
        .saturating_add((cursor.col as u16).min(inner_width.saturating_sub(1)));
    let y = area
        .y
        .saturating_add((cursor.row as u16).min(inner_height.saturating_sub(1)));
    f.set_cursor_position((x, y));
}

fn message_separator_title(state: &ViewState) -> Line<'static> {
    if state.approval_pending {
        return Line::from(vec![
            Span::styled("─── ", Style::default().fg(ACCENT_GOLD)),
            Span::styled(
                "approval pending - press y / n ",
                Style::default()
                    .fg(ACCENT_GOLD)
                    .add_modifier(Modifier::BOLD),
            ),
        ]);
    }
    if state.busy {
        return thinking_title(
            state.busy_frame,
            state.turn_activity.as_deref().unwrap_or("thinking"),
        );
    }
    Line::from(vec![
        Span::styled("─── ", Style::default().fg(ACCENT_BLUE)),
        Span::styled(
            "(Enter to send, Alt/Shift-Enter for newline) ",
            Style::default().fg(TEXT_MUTED),
        ),
    ])
}

fn thinking_title(frame: u64, activity: &str) -> Line<'static> {
    const SPINNER: [&str; 4] = ["-", "\\", "|", "/"];
    let spinner = SPINNER[((frame / 2) as usize) % SPINNER.len()];
    let text = format!("{activity}...");
    let text_style = Style::default().fg(TEXT_MUTED).add_modifier(Modifier::BOLD);
    let spans = vec![
        Span::styled("─── ", Style::default().fg(ACCENT_BLUE)),
        Span::styled(spinner.to_string(), Style::default().fg(ACCENT_GOLD)),
        Span::raw(" "),
        Span::styled(text, text_style),
        Span::styled(" (input disabled) ", Style::default().fg(TEXT_DIM)),
    ];
    Line::from(spans)
}

fn draw_message_separator(f: &mut ratatui::Frame, area: Rect, state: &ViewState) {
    draw_separator(
        f,
        area,
        message_separator_title(state),
        Style::default().fg(ACCENT_BLUE),
    );
}

fn draw_status_separator(f: &mut ratatui::Frame, area: Rect) {
    draw_separator(f, area, Line::from(""), Style::default().fg(ACCENT_GOLD));
}

fn draw_session_status(f: &mut ratatui::Frame, area: Rect, state: &ViewState) {
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" ", Style::default().fg(TEXT_MUTED)),
            Span::styled(state.model_label.clone(), Style::default().fg(TEXT_MUTED)),
            Span::styled("  ·  ", Style::default().fg(TEXT_DIM)),
            Span::styled(
                display_path(&state.workspace_root),
                Style::default().fg(TEXT_MUTED),
            ),
            Span::styled("  ·  ", Style::default().fg(TEXT_DIM)),
            Span::styled(
                format!("{} msgs", state.lines_count),
                Style::default().fg(TEXT_MUTED),
            ),
            Span::styled("  ·  session ", Style::default().fg(TEXT_DIM)),
            Span::styled(
                state.session_id.to_string(),
                Style::default().fg(TEXT_MUTED),
            ),
            Span::styled(" ", Style::default().fg(TEXT_MUTED)),
        ])),
        area,
    );
}

fn display_path(path: &std::path::Path) -> String {
    if let Ok(home) = std::env::var("HOME") {
        let home = std::path::Path::new(&home);
        if let Ok(rest) = path.strip_prefix(home) {
            if rest.as_os_str().is_empty() {
                return "~".to_string();
            }
            return format!("~/{}", rest.display());
        }
    }
    path.display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use everruns_core::events::{
        EventContext, InputMessageData, OutputMessageCompletedData, OutputMessageStartedData,
        ReasonCompletedData,
    };
    use everruns_core::message::Message;
    use everruns_core::tool_types::ToolCall;
    use everruns_core::{SessionId, TurnId};

    use everruns_core::command::{CommandArg, CommandDescriptor, CommandSource};

    fn setup_capability_command() -> CommandDescriptor {
        CommandDescriptor {
            name: "setup".to_string(),
            description: "Configure provider, API key, and model.".to_string(),
            source: CommandSource::System,
            args: vec![],
        }
    }

    fn command_with_arg_suggestions() -> CommandDescriptor {
        CommandDescriptor {
            name: "pick".to_string(),
            description: "Pick a value.".to_string(),
            source: CommandSource::System,
            args: vec![CommandArg {
                name: "value".to_string(),
                description: "value".to_string(),
                required: false,
                suggestions: vec![
                    "alpha-one".to_string(),
                    "alpha-two".to_string(),
                    "beta-one".to_string(),
                ],
            }],
        }
    }

    #[test]
    fn command_suggestions_list_commands_for_slash() {
        let caps = vec![setup_capability_command()];
        let suggestions = command_suggestions("/", &caps);

        assert!(suggestions.iter().any(|s| s.completion == "/help"));
        assert!(
            suggestions
                .iter()
                .any(|s| s.completion == "/setup" || s.completion == "/setup "),
            "capability-provided /setup should appear in suggestions: {suggestions:?}"
        );
    }

    #[test]
    fn suggestion_preview_line_shows_command_dropdown() {
        let caps = vec![setup_capability_command()];
        let suggestions = command_suggestions("/s", &caps);
        let rendered = line_text(&suggestion_preview_line(&suggestions, 96));

        assert!(rendered.starts_with("Tab /setup"));
        assert!(rendered.contains("/setup"));
    }

    #[test]
    fn suggestion_preview_line_keeps_first_match_when_truncated() {
        let caps = vec![setup_capability_command()];
        let suggestions = command_suggestions("/", &caps);
        let rendered = line_text(&suggestion_preview_line(&suggestions, 18));

        assert!(rendered.starts_with("Tab /help"));
        assert!(rendered.ends_with('…'));
    }

    #[test]
    fn command_suggestions_filter_first_arg_by_prefix() {
        // After `/pick <prefix>`, the suggestion source must be the arg's
        // declared `suggestions` — read straight from the descriptor with
        // no extra plumbing.
        let caps = vec![command_with_arg_suggestions()];
        let suggestions = command_suggestions("/pick alpha-", &caps);

        assert_eq!(
            suggestions
                .iter()
                .map(|s| s.completion.as_str())
                .collect::<Vec<_>>(),
            vec!["/pick alpha-one", "/pick alpha-two"]
        );
    }

    #[test]
    fn command_suggestions_no_arg_suggestions_means_free_form() {
        // A capability command whose first arg has no suggestions returns an
        // empty list once the user types past the command name — the renderer
        // should fall back to plain text entry instead of fabricating items.
        let caps = vec![CommandDescriptor {
            name: "echo".to_string(),
            description: "echo".to_string(),
            source: CommandSource::System,
            args: vec![CommandArg {
                name: "text".to_string(),
                description: "text".to_string(),
                required: true,
                suggestions: vec![],
            }],
        }];

        let suggestions = command_suggestions("/echo hello", &caps);
        assert!(suggestions.is_empty(), "got: {suggestions:?}");
    }

    #[test]
    fn capability_commands_appear_in_suggestions() {
        let caps = vec![CommandDescriptor {
            name: "btw".to_string(),
            description: "Ask a side question.".to_string(),
            source: CommandSource::System,
            args: vec![CommandArg {
                name: "question".to_string(),
                description: "the question".to_string(),
                required: true,
                suggestions: vec![],
            }],
        }];

        let suggestions = command_suggestions("/b", &caps);

        let btw = suggestions
            .iter()
            .find(|s| s.completion == "/btw ")
            .expect("capability command surfaced in suggestions");
        assert!(btw.label.starts_with("/btw <question>"));
    }

    #[test]
    fn builtin_commands_win_over_capability_with_same_name() {
        // A capability that accidentally declares /help must not shadow the
        // built-in handler: the built-in suggestion (no trailing space, no
        // args) should be the only one returned for that name.
        let caps = vec![CommandDescriptor {
            name: "help".to_string(),
            description: "shadow help".to_string(),
            source: CommandSource::System,
            args: vec![],
        }];

        let suggestions = command_suggestions("/help", &caps);

        let help_entries: Vec<_> = suggestions
            .iter()
            .filter(|s| s.completion.starts_with("/help"))
            .collect();
        assert_eq!(help_entries.len(), 1);
        assert_eq!(help_entries[0].completion, "/help");
    }

    #[test]
    fn input_area_supports_multiline_and_cursor_editing() {
        let mut input = new_input_area(vec![String::new()]);

        for key in [
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::empty()),
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::empty()),
            KeyEvent::new(KeyCode::Left, KeyModifiers::empty()),
            KeyEvent::new(KeyCode::Char('b'), KeyModifiers::empty()),
            KeyEvent::new(KeyCode::Right, KeyModifiers::empty()),
            KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT),
            KeyEvent::new(KeyCode::Char('d'), KeyModifiers::empty()),
        ] {
            let _ = input.input(key);
        }

        assert_eq!(input.lines(), ["abc", "d"]);
    }

    #[test]
    fn replayed_events_render_user_assistant_and_tool_lines() {
        let session_id = SessionId::new();
        let user_event = RuntimeEvent::new(
            session_id,
            EventContext::empty(),
            InputMessageData::new(Message::user("What changed?")),
        );
        let assistant_event = RuntimeEvent::new(
            session_id,
            EventContext::empty(),
            OutputMessageCompletedData::new(Message::assistant("I updated the renderer.")),
        );
        let mut tool_data = ToolCompletedData::success(
            "call_bash".to_string(),
            "bash".to_string(),
            vec![ContentPart::text(
                serde_json::json!({
                    "command": "cargo test",
                    "exit_code": 0
                })
                .to_string(),
            )],
            None,
        );
        tool_data.narration = Some("Ran tests".to_string());
        let tool_event = RuntimeEvent::new(session_id, EventContext::empty(), tool_data);

        let lines = [user_event, assistant_event, tool_event]
            .iter()
            .flat_map(lines_for_replayed_event)
            .map(|line| (line.author, line.text))
            .collect::<Vec<_>>();

        assert!(matches!(lines[0].0, Author::User));
        assert_eq!(lines[0].1, "What changed?");
        assert!(matches!(lines[1].0, Author::Assistant));
        assert_eq!(lines[1].1, "I updated the renderer.");
        assert!(matches!(lines[2].0, Author::Tool));
        assert!(lines[2].1.contains("Ran tests"));
    }

    #[test]
    fn lines_for_event_surfaces_tool_call_monologue() {
        let event = RuntimeEvent::new(
            SessionId::new(),
            EventContext::empty(),
            ReasonCompletedData::success("I'll check the manifests first.", true, 2, None, None),
        );

        let lines = lines_for_event(&event);

        assert_eq!(lines.len(), 1);
        assert!(matches!(lines[0].author, Author::Assistant));
        assert_eq!(lines[0].text, "I'll check the manifests first.");
        assert_eq!(
            status_for_event(&event)
                .map(|status| status.text)
                .as_deref(),
            Some("planned 2 tool call(s)")
        );
    }

    #[test]
    fn lines_for_event_renders_reason_item_summary_segments() {
        use everruns_core::events::ReasonItemData;

        let event = RuntimeEvent::new(
            SessionId::new(),
            EventContext::empty(),
            ReasonItemData {
                turn_id: TurnId::new(),
                provider: "openai".to_string(),
                model: Some("gpt-5".to_string()),
                item_id: "rs_abc".to_string(),
                encrypted_content: Some("opaque".to_string()),
                summary: vec![
                    "Considering file layout".to_string(),
                    "".to_string(),
                    "  Plan the read order  ".to_string(),
                ],
                token_count: None,
            },
        );

        let lines = lines_for_event(&event);

        assert_eq!(lines.len(), 2, "blank summary segments are dropped");
        assert!(matches!(lines[0].author, Author::Assistant));
        assert_eq!(lines[0].text, "Considering file layout");
        assert_eq!(lines[1].text, "Plan the read order");
    }

    #[test]
    fn lines_for_event_hides_output_message_thinking() {
        let mut message = everruns_core::Message::assistant_with_tools(
            "",
            vec![ToolCall {
                id: "call_read".to_string(),
                name: "read_file".to_string(),
                arguments: serde_json::json!({ "path": "/workspace/Cargo.toml" }),
            }],
        );
        message.thinking = Some(
            "**Inspecting package files**\n\nI should read the package manifest first.".to_string(),
        );
        let event = RuntimeEvent::new(
            SessionId::new(),
            EventContext::empty(),
            OutputMessageCompletedData::new(message),
        );

        let lines = lines_for_event(&event);

        assert!(lines.is_empty(), "thinking must not be rendered: {lines:?}");
    }

    #[test]
    fn status_for_event_labels_output_iteration() {
        let event = RuntimeEvent::new(
            SessionId::new(),
            EventContext::empty(),
            OutputMessageStartedData {
                turn_id: TurnId::new(),
                model: None,
                iteration: Some(3),
            },
        );

        assert!(lines_for_event(&event).is_empty());
        assert_eq!(
            status_for_event(&event)
                .map(|status| status.text)
                .as_deref(),
            Some("iteration 3: writing response")
        );
    }

    #[test]
    fn lines_for_event_renders_short_write_todos_inline() {
        let event = RuntimeEvent::new(
            SessionId::new(),
            EventContext::empty(),
            ToolCompletedData::success(
                "call_todos".to_string(),
                "write_todos".to_string(),
                vec![ContentPart::text(
                    serde_json::json!({
                        "success": true,
                        "total_tasks": 3,
                        "pending": 1,
                        "in_progress": 1,
                        "completed": 1,
                        "todos": [
                            {
                                "content": "Read current CLI renderer",
                                "activeForm": "Reading current CLI renderer",
                                "status": "completed"
                            },
                            {
                                "content": "Render todos in transcript",
                                "activeForm": "Rendering todos in transcript",
                                "status": "in_progress"
                            },
                            {
                                "content": "Run focused tests",
                                "activeForm": "Running focused tests",
                                "status": "pending"
                            }
                        ]
                    })
                    .to_string(),
                )],
                None,
            ),
        );

        let lines = lines_for_event(&event)
            .into_iter()
            .map(|line| (line.author, line.text))
            .collect::<Vec<_>>();

        assert!(matches!(lines[0].0, Author::Tool));
        assert_eq!(
            lines[0].1,
            "1 of 3 todos completed  ✓ Read current CLI renderer  › Rendering todos in transcript  ○ Run focused tests"
        );
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn lines_for_event_renders_long_write_todos_as_rows() {
        let event = RuntimeEvent::new(
            SessionId::new(),
            EventContext::empty(),
            ToolCompletedData::success(
                "call_todos".to_string(),
                "write_todos".to_string(),
                vec![ContentPart::text(
                    serde_json::json!({
                        "success": true,
                        "total_tasks": 4,
                        "pending": 2,
                        "in_progress": 1,
                        "completed": 1,
                        "todos": [
                            {
                                "content": "Read current CLI renderer",
                                "activeForm": "Reading current CLI renderer",
                                "status": "completed"
                            },
                            {
                                "content": "Render todos in transcript",
                                "activeForm": "Rendering todos in transcript",
                                "status": "in_progress"
                            },
                            {
                                "content": "Run focused tests",
                                "activeForm": "Running focused tests",
                                "status": "pending"
                            },
                            {
                                "content": "Summarize changes",
                                "activeForm": "Summarizing changes",
                                "status": "pending"
                            }
                        ]
                    })
                    .to_string(),
                )],
                None,
            ),
        );

        let lines = lines_for_event(&event)
            .into_iter()
            .map(|line| (line.author, line.text))
            .collect::<Vec<_>>();

        assert!(matches!(lines[0].0, Author::Tool));
        assert_eq!(lines[0].1, "1 of 4 todos completed");
        assert!(
            lines
                .iter()
                .any(|(author, line)| matches!(author, Author::ToolDetail)
                    && line == "✓ Read current CLI renderer")
        );
        assert!(
            lines
                .iter()
                .any(|(author, line)| matches!(author, Author::ToolDetail)
                    && line == "› Rendering todos in transcript")
        );
        assert!(
            lines
                .iter()
                .any(|(author, line)| matches!(author, Author::ToolDetail)
                    && line == "○ Run focused tests")
        );
    }

    #[test]
    fn lines_for_event_limits_write_todo_rows_and_truncates_text() {
        let total = MAX_RENDERED_TODOS + 5;
        let long_text = "x".repeat(MAX_TODO_TEXT_CHARS + 60);
        let todos = (0..total)
            .map(|_| {
                serde_json::json!({
                    "content": &long_text,
                    "activeForm": &long_text,
                    "status": "pending"
                })
            })
            .collect::<Vec<_>>();
        let event = RuntimeEvent::new(
            SessionId::new(),
            EventContext::empty(),
            ToolCompletedData::success(
                "call_todos".to_string(),
                "write_todos".to_string(),
                vec![ContentPart::text(
                    serde_json::json!({
                        "success": true,
                        "todos": todos,
                        "warning": "w".repeat(MAX_TODO_TEXT_CHARS + 60)
                    })
                    .to_string(),
                )],
                None,
            ),
        );

        let lines = lines_for_event(&event);
        let detail_lines = lines
            .iter()
            .filter(|line| matches!(line.author, Author::ToolDetail))
            .map(|line| line.text.as_str())
            .collect::<Vec<_>>();

        let omitted = total - MAX_RENDERED_TODOS;
        assert_eq!(
            detail_lines
                .iter()
                .filter(|line| line.starts_with("○ "))
                .count(),
            MAX_RENDERED_TODOS
        );
        assert!(
            detail_lines
                .iter()
                .any(|line| *line == format!("… {omitted} more todo(s) omitted"))
        );
        assert!(
            detail_lines
                .iter()
                .any(|line| line.starts_with("warning: "))
        );
        assert!(
            detail_lines
                .iter()
                .filter(|line| line.starts_with("○ "))
                .all(|line| line.ends_with('…'))
        );
    }

    #[test]
    fn handle_live_event_routes_assistant_delta_to_stream_preview() {
        use everruns_core::events::{OutputMessageDeltaData, ToolOutputDeltaData};
        use everruns_core::typed_id::TurnId;

        let (tx, mut rx) = mpsc::unbounded_channel::<TurnEvent>();
        let mut emitted = HashSet::new();
        let mut router = DeltaRouter::default();
        let turn_id = TurnId::new();

        let delta_event = RuntimeEvent::new(
            SessionId::new(),
            EventContext::empty(),
            OutputMessageDeltaData {
                turn_id,
                delta: "Hel".to_string(),
                accumulated: "Hel".to_string(),
            },
        );
        handle_live_event(&delta_event, &mut emitted, &mut router, &tx);

        let more = RuntimeEvent::new(
            SessionId::new(),
            EventContext::empty(),
            OutputMessageDeltaData {
                turn_id,
                delta: "lo, world".to_string(),
                accumulated: "Hello, world".to_string(),
            },
        );
        handle_live_event(&more, &mut emitted, &mut router, &tx);

        let completed = RuntimeEvent::new(
            SessionId::new(),
            EventContext::empty(),
            OutputMessageCompletedData::new(Message::assistant("Hello, world")),
        );
        handle_live_event(&completed, &mut emitted, &mut router, &tx);

        // Tool delta event surfaces a separate preview kind.
        let tool_delta = RuntimeEvent::new(
            SessionId::new(),
            EventContext::empty(),
            ToolOutputDeltaData {
                tool_call_id: "call-99".to_string(),
                tool_name: "bash".to_string(),
                delta: "compiling...\n".to_string(),
                stream: "stdout".to_string(),
            },
        );
        handle_live_event(&tool_delta, &mut emitted, &mut router, &tx);

        let mut previews = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let TurnEvent::Stream(preview) = event {
                previews.push(preview);
            }
        }

        // Expect: first delta → Assistant preview, second delta → Assistant
        // preview with accumulated text, completed → None, tool delta → Tool preview.
        assert_eq!(previews.len(), 4);
        match &previews[0] {
            Some(p) => {
                assert_eq!(p.kind, StreamKind::Assistant);
                assert_eq!(p.text, "Hel");
            }
            None => panic!("expected first preview to be Some"),
        }
        match &previews[1] {
            Some(p) => {
                assert_eq!(p.kind, StreamKind::Assistant);
                assert_eq!(p.text, "Hello, world");
            }
            None => panic!("expected second preview to be Some"),
        }
        assert!(previews[2].is_none(), "completed must clear preview");
        match &previews[3] {
            Some(p) => {
                assert_eq!(p.kind, StreamKind::Tool);
                assert!(
                    p.text.contains("bash") && p.text.contains("compiling"),
                    "tool preview text: {:?}",
                    p.text
                );
            }
            None => panic!("expected tool delta to surface preview"),
        }
    }

    #[test]
    fn handle_live_event_deduplicates_by_event_id() {
        let (tx, mut rx) = mpsc::unbounded_channel::<TurnEvent>();
        let mut emitted = HashSet::new();
        let mut router = DeltaRouter::default();

        let event = RuntimeEvent::new(
            SessionId::new(),
            EventContext::empty(),
            ReasonCompletedData::success("plan", true, 1, None, None),
        );
        handle_live_event(&event, &mut emitted, &mut router, &tx);
        handle_live_event(&event, &mut emitted, &mut router, &tx);

        let mut count = 0;
        while rx.try_recv().is_ok() {
            count += 1;
        }
        assert_eq!(
            count, 2,
            "first dispatch yields Activity + Lines; second is suppressed"
        );
    }

    #[test]
    fn truncate_tail_keeps_visible_cursor() {
        assert_eq!(truncate_tail_chars("hello", 10), "hello");
        let out = truncate_tail_chars("0123456789abcdef", 8);
        assert!(out.starts_with('…'), "expected ellipsis prefix: {out:?}");
        assert!(
            out.ends_with("cdef"),
            "expected tail of the text to survive: {out:?}"
        );
    }

    #[test]
    fn truncate_end_handles_tiny_limits() {
        assert_eq!(truncate_end_chars("hello", 0), "");
        assert_eq!(truncate_end_chars("hello", 1), "…");
        assert_eq!(truncate_end_chars("hello", 99), "hello");
    }

    fn line_text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>()
    }

    #[test]
    fn should_not_insert_chat_gap_inside_tool_blocks() {
        assert!(!should_insert_chat_gap(&Author::Tool, Some(&Author::Tool)));
        assert!(!should_insert_chat_gap(
            &Author::Tool,
            Some(&Author::ToolDetail)
        ));
        assert!(!should_insert_chat_gap(
            &Author::ToolDetail,
            Some(&Author::Tool)
        ));
        assert!(!should_insert_chat_gap(
            &Author::ToolDetail,
            Some(&Author::ToolDetail)
        ));
        assert!(should_insert_chat_gap(
            &Author::ToolDetail,
            Some(&Author::Assistant)
        ));
        assert!(!should_insert_chat_gap(&Author::ToolDetail, None));
    }

    // ====================================================================
    // ViewState + draw_chrome snapshot tests.
    //
    // These render the non-input chrome (stream preview, message
    // separator, status separator, session status) into a TestBackend
    // buffer and assert on its textual contents. The point is to lock
    // down what each UI mode (idle / busy / approval-pending / streaming)
    // looks like end-to-end on the screen, without spinning up a runtime.
    // ====================================================================

    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn view_state_idle() -> ViewState {
        ViewState {
            stream_preview: None,
            command_suggestions: Vec::new(),
            approval_pending: false,
            busy: false,
            busy_frame: 0,
            turn_activity: None,
            model_label: "openai/gpt-5.5".to_string(),
            workspace_root: std::path::PathBuf::from("/tmp/ws"),
            session_id: SessionId::from_seed(770001),
            lines_count: 3,
        }
    }

    /// Render `draw_chrome` into a TestBackend and collect the buffer
    /// rows as plain strings (style information dropped). Width and
    /// height are minimums; if the chrome layout would need more space
    /// it will be silently clipped, which is fine for substring asserts.
    fn render_chrome_lines(state: &ViewState, width: u16, height: u16) -> Vec<String> {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|f| {
                // Production input slot height is 1; tests don't
                // exercise the input row so the value only matters
                // because it shifts the status rows by that amount.
                let _input_rect = draw_chrome(f, f.area(), 1, state);
            })
            .expect("draw");
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                let mut row = String::with_capacity(buffer.area.width as usize);
                for x in 0..buffer.area.width {
                    let cell = &buffer[(x, y)];
                    row.push_str(cell.symbol());
                }
                row.trim_end().to_string()
            })
            .collect()
    }

    struct TestApp {
        app: App,
        _workspace: tempfile::TempDir,
        _sessions: tempfile::TempDir,
    }

    async fn app_with_llmsim() -> TestApp {
        let workspace = tempfile::tempdir().expect("workspace tempdir");
        let sessions = tempfile::tempdir().expect("sessions tempdir");
        let settings = std::sync::Arc::new(crate::settings::SettingsStore::open(
            sessions.path().join("settings.toml"),
        ));
        let runtime = crate::runtime::build_with_options(
            workspace.path().to_path_buf(),
            crate::runtime::ProviderChoice::Sim,
            crate::approval::ApprovalGate::auto(),
            None,
            sessions.path().to_path_buf(),
            settings,
            crate::runtime::BuildOptions::default(),
        )
        .await
        .expect("build llmsim runtime");
        let (_tx, rx) = mpsc::unbounded_channel();
        TestApp {
            app: App::new(runtime, rx),
            _workspace: workspace,
            _sessions: sessions,
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn app_slash_input_renders_command_suggestions_end_to_end() {
        let mut fixture = app_with_llmsim().await;
        let app = &mut fixture.app;

        app.handle_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::empty()))
            .await;

        let state = app.view_state();
        assert!(
            state
                .command_suggestions
                .iter()
                .any(|suggestion| suggestion.completion == "/help"),
            "expected /help suggestion in view state: {:?}",
            state.command_suggestions
        );
        let rows = render_chrome_lines(&state, 80, 5);
        assert!(
            rows[0].contains("Tab /help"),
            "slash input should render command suggestions in chrome row: {:?}",
            rows
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn setup_command_starts_guided_wizard() {
        let mut fixture = app_with_llmsim().await;
        let app = &mut fixture.app;
        app.setup = None;
        app.lines.clear();

        app.handle_command("setup").await;

        assert!(matches!(app.setup, Some(SetupStep::ChooseAction)));
        assert!(
            app.lines
                .iter()
                .any(|line| line.text.starts_with("current setup: "))
        );
        assert!(
            !app.lines
                .iter()
                .any(|line| line.text.starts_with("setup: provider=")),
            "plain /setup should not print backend status before the wizard"
        );
        assert!(
            app.lines
                .iter()
                .any(|line| line.text == "1 provider, API key, and model")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn setup_menu_opens_numbered_provider_picker() {
        let mut fixture = app_with_llmsim().await;
        let app = &mut fixture.app;
        app.lines.clear();
        app.setup = Some(SetupStep::ChooseAction);

        app.advance_setup("1").await;

        assert!(matches!(app.setup, Some(SetupStep::PickProvider)));
        assert!(
            app.lines
                .iter()
                .any(|line| line.text == "choose a provider:")
        );
        assert!(
            app.lines
                .iter()
                .any(|line| line.text == "1 OpenAI — recommended")
        );
        assert!(
            app.lines
                .iter()
                .any(|line| line.text == "6 Offline demo mode — no API key")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn setup_api_key_only_blank_input_reports_no_change() {
        let mut fixture = app_with_llmsim().await;
        let app = &mut fixture.app;
        app.lines.clear();
        app.setup = Some(SetupStep::ChooseAction);

        app.advance_setup("2").await;
        assert!(matches!(app.setup, Some(SetupStep::PickTokenProvider)));
        app.advance_setup("1").await;
        assert!(matches!(
            app.setup,
            Some(SetupStep::EnterToken {
                ref provider,
                activate: false,
                ..
            }) if provider == "openai"
        ));
        app.advance_setup("").await;

        assert!(app.setup.is_none());
        assert!(
            app.lines
                .iter()
                .any(|line| line.text == "setup unchanged — using env vars for openai")
        );
        assert!(
            !app.lines
                .iter()
                .any(|line| line.text == "setup complete — API key updated")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn setup_api_key_only_update_hides_internal_success_line() {
        let mut fixture = app_with_llmsim().await;
        let app = &mut fixture.app;
        app.lines.clear();
        app.setup = Some(SetupStep::ChooseAction);

        app.advance_setup("2").await;
        app.advance_setup("1").await;
        app.advance_setup("test-token").await;

        assert!(app.setup.is_none());
        assert!(
            app.lines
                .iter()
                .any(|line| line.text == "setup complete — API key updated")
        );
        assert!(
            !app.lines
                .iter()
                .any(|line| line.text.starts_with("setup token stored for")),
            "wizard should hide internal setup command success output"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn setup_wizard_can_select_offline_provider() {
        let mut fixture = app_with_llmsim().await;
        let app = &mut fixture.app;
        app.lines.clear();
        app.setup = Some(SetupStep::PickProvider);

        app.advance_setup("llmsim").await;

        assert!(app.setup.is_none());
        assert_eq!(app.model.provider_label(), "llmsim/llmsim-yolop");
        assert!(
            app.lines
                .iter()
                .any(|line| line.text == "setup complete — using offline demo mode")
        );
        assert!(
            !app.lines
                .iter()
                .any(|line| line.text.starts_with("setup provider changed:")),
            "wizard should hide internal setup command success output"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn app_view_state_hides_command_suggestions_when_input_disabled() {
        let mut fixture = app_with_llmsim().await;
        let app = &mut fixture.app;

        app.handle_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::empty()))
            .await;
        assert!(
            !app.view_state().command_suggestions.is_empty(),
            "slash input should produce suggestions before input is disabled"
        );

        app.busy = true;
        assert!(
            app.view_state().command_suggestions.is_empty(),
            "busy turns should not render suggestions"
        );

        app.busy = false;
        let (responder, _rx) = oneshot::channel();
        app.pending = Some(PendingApproval { responder });
        assert!(
            app.view_state().command_suggestions.is_empty(),
            "approval prompts should not render suggestions"
        );
    }

    #[test]
    fn chrome_command_suggestions_override_stream_preview_row() {
        let state = ViewState {
            stream_preview: Some(StreamPreview {
                kind: StreamKind::Assistant,
                text: "streaming response".to_string(),
            }),
            command_suggestions: vec![CommandSuggestion {
                completion: "/help".to_string(),
                label: "/help    show commands".to_string(),
            }],
            ..view_state_idle()
        };
        let rows = render_chrome_lines(&state, 80, 5);
        assert!(
            rows[0].contains("Tab /help"),
            "suggestions should render in the top chrome row: {:?}",
            rows
        );
        assert!(
            !rows[0].contains("agent"),
            "command suggestions should replace the stream preview row: {}",
            rows[0]
        );
    }

    #[test]
    fn draw_suggestions_ignores_empty_areas() {
        let suggestions = vec![CommandSuggestion {
            completion: "/help".to_string(),
            label: "/help    show commands".to_string(),
        }];
        let backend = TestBackend::new(4, 1);
        let mut terminal = Terminal::new(backend).expect("terminal");

        terminal
            .draw(|f| {
                draw_suggestions(
                    f,
                    Rect {
                        x: 0,
                        y: 0,
                        width: 0,
                        height: 1,
                    },
                    &suggestions,
                );
                draw_suggestions(
                    f,
                    Rect {
                        x: 0,
                        y: 0,
                        width: 4,
                        height: 0,
                    },
                    &suggestions,
                );
            })
            .expect("draw");

        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(0, 0)].symbol(), " ");
    }

    #[test]
    fn chrome_idle_shows_enter_to_send_hint() {
        let state = view_state_idle();
        let rows = render_chrome_lines(&state, 80, 5);
        // Row 1 = message separator. Idle mode shows the keystroke hint.
        assert!(
            rows[1].contains("Enter to send"),
            "idle separator missing Enter hint: rows={rows:?}"
        );
        assert!(
            !rows[1].contains("approval pending"),
            "idle separator should not mention approval: {}",
            rows[1]
        );
    }

    #[test]
    fn chrome_approval_pending_overrides_busy_separator() {
        // Even when `busy` is true, the approval prompt takes priority
        // — the user must answer y/n before the loop can resume.
        let state = ViewState {
            approval_pending: true,
            busy: true,
            busy_frame: 12,
            ..view_state_idle()
        };
        let rows = render_chrome_lines(&state, 80, 5);
        assert!(
            rows[1].contains("approval pending"),
            "expected approval separator, got: {}",
            rows[1]
        );
        assert!(
            rows[1].contains("y / n"),
            "approval separator must surface the key bindings: {}",
            rows[1]
        );
    }

    #[test]
    fn chrome_busy_shows_thinking_spinner_and_activity() {
        let state = ViewState {
            busy: true,
            busy_frame: 4,
            turn_activity: Some("reading files".to_string()),
            ..view_state_idle()
        };
        let rows = render_chrome_lines(&state, 80, 5);
        assert!(
            rows[1].contains("reading files"),
            "busy separator should display turn activity: {}",
            rows[1]
        );
        assert!(
            rows[1].contains("input disabled"),
            "busy separator should signal input is disabled: {}",
            rows[1]
        );
    }

    #[test]
    fn chrome_busy_falls_back_to_thinking_when_activity_unset() {
        let state = ViewState {
            busy: true,
            ..view_state_idle()
        };
        let rows = render_chrome_lines(&state, 80, 5);
        assert!(
            rows[1].contains("thinking"),
            "busy separator without activity should show 'thinking': {}",
            rows[1]
        );
    }

    #[test]
    fn chrome_renders_stream_preview_tail_with_kind_label() {
        let state = ViewState {
            stream_preview: Some(StreamPreview {
                kind: StreamKind::Assistant,
                text: "first line\nsecond line tail".to_string(),
            }),
            ..view_state_idle()
        };
        let rows = render_chrome_lines(&state, 80, 5);
        // The preview shows the latest non-blank tail line of the stream
        // prefixed by the kind label.
        assert!(
            rows[0].contains("agent"),
            "stream preview should show kind label 'agent': {}",
            rows[0]
        );
        assert!(
            rows[0].contains("second line tail"),
            "stream preview should show the tail, not the head: {}",
            rows[0]
        );
        assert!(
            !rows[0].contains("first line"),
            "stream preview should not show earlier lines: {}",
            rows[0]
        );
    }

    #[test]
    fn chrome_stream_preview_thinking_uses_thinking_label() {
        let state = ViewState {
            stream_preview: Some(StreamPreview {
                kind: StreamKind::Thinking,
                text: "weighing options".to_string(),
            }),
            ..view_state_idle()
        };
        let rows = render_chrome_lines(&state, 80, 5);
        assert!(
            rows[0].contains("thinking"),
            "thinking-kind preview should use 'thinking' label: {}",
            rows[0]
        );
    }

    #[test]
    fn chrome_session_status_shows_model_workspace_msgs_and_session() {
        let state = ViewState {
            model_label: "anthropic/claude-sonnet-4-5".to_string(),
            workspace_root: std::path::PathBuf::from("/tmp/some-workspace"),
            session_id: SessionId::from_seed(99887766),
            lines_count: 42,
            ..view_state_idle()
        };
        let rows = render_chrome_lines(&state, 120, 5);
        let status = &rows[4];
        assert!(
            status.contains("anthropic/claude-sonnet-4-5"),
            "status should include model label: {status}"
        );
        assert!(
            status.contains("/tmp/some-workspace"),
            "status should include workspace path: {status}"
        );
        assert!(
            status.contains("42 msgs"),
            "status should include message count: {status}"
        );
        assert!(
            status.contains("session "),
            "status should include 'session' label: {status}"
        );
        let session_id_str = state.session_id.to_string();
        assert!(
            status.contains(&session_id_str),
            "status should include the session id ({session_id_str}): {status}"
        );
    }

    #[test]
    fn chrome_session_status_collapses_home_prefix_with_tilde() {
        // `display_path` rewrites $HOME-prefixed paths to start with '~'.
        // Save / restore the env var so this test doesn't leak.
        // SAFETY: env mutation in tests is racy across threads. cargo
        // test by default runs tests in parallel; we accept the tiny
        // window of cross-test contamination here because the assertion
        // is on the rendered output of THIS state alone, and a parallel
        // mutation can only widen the substring we check for, not narrow
        // it. (`display_path` returns plain `path.display()` if $HOME
        // doesn't prefix; tilde replacement is opt-in per path.)
        let prior = std::env::var("HOME").ok();
        unsafe {
            std::env::set_var("HOME", "/tmp/fake-home");
        }
        let state = ViewState {
            workspace_root: std::path::PathBuf::from("/tmp/fake-home/projects/yolop"),
            ..view_state_idle()
        };
        let rows = render_chrome_lines(&state, 120, 5);
        let status = &rows[4];
        assert!(
            status.contains("~/projects/yolop"),
            "status should collapse $HOME to ~: {status}"
        );
        unsafe {
            match prior {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
        }
    }

    #[test]
    fn chrome_stream_preview_row_is_empty_when_none() {
        let state = view_state_idle();
        let rows = render_chrome_lines(&state, 80, 5);
        assert!(
            rows[0].is_empty(),
            "stream preview row should be empty when no preview is set: {:?}",
            rows[0]
        );
    }
}

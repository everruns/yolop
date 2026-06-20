// TUI app state and event loop.
// Decision: keep the TUI surface tiny. Transcript output is inserted into the
// native terminal scrollback; ratatui owns only a short inline composer at the
// bottom.

use crate::goal::{GOAL_EVALUATE_ARG, GoalStore, parse_evaluation_response};
use crate::host_ui::UiCommand;
use crate::runtime::{BuiltRuntime, ModelState, StartupInfo};
use crate::session::Session;
use crate::worktree::WorktreeManager;
use anyhow::Result;
use crossterm::event::{
    self, Event as CrosstermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton,
    MouseEvent, MouseEventKind,
};
use everruns_core::command::{CommandDescriptor, CommandSource};
use everruns_core::message::ContentPart;
use everruns_core::typed_id::SessionId;
use ratatui::Terminal;
use ratatui::backend::Backend;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Widget};
use ratatui_textarea::{CursorMove, TextArea, WrapMode};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

mod markdown_table;
mod render;
mod setup;
mod viewport;

// Re-export the moved free items so the rest of the crate (and the test module)
// can keep referring to them as `crate::app::*`. `setup` exposes only `impl App`
// methods, so it needs no re-export. The rendering module is named `render`
// rather than `draw` so it does not collide with the free `draw` fn it exports.
// The view-model types and runtime-event translation live in `crate::transcript`
// (the single boundary that interprets `everruns_core` events); re-export them
// here so the TUI's own submodules keep referring to them as `crate::app::*`.
pub(crate) use self::{render::*, viewport::*};
pub(crate) use crate::transcript::*;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CommandSuggestion {
    completion: String,
    label: String,
}

pub const COMPOSER_VIEWPORT_HEIGHT: u16 = 18;
/// Consecutive failed event-loop iterations tolerated before the terminal is
/// considered gone and the error becomes fatal. The slowest failure mode (an
/// unanswered cursor-position query) blocks ~2s per attempt inside crossterm,
/// so this bounds a permanently dead terminal to ~10s before exit while
/// letting a briefly unresponsive emulator recover.
const MAX_TERMINAL_IO_FAILURES: usize = 5;
const MAX_INPUT_HEIGHT: u16 = 12;
const EXPANDED_STATUS_ROWS: u16 = 4;
const RECENT_TRANSCRIPT_SOURCE_LINES: usize = 80;
const RECENT_TRANSCRIPT_MAX_TEXT_BYTES: usize = 4_000;
const ACCENT_BLUE: Color = Color::Rgb(45, 91, 158);
const ACCENT_GOLD: Color = Color::Rgb(126, 94, 19);
const TEXT_PRIMARY: Color = Color::Rgb(230, 230, 232);
const TEXT_MUTED: Color = Color::Rgb(140, 140, 145);
const TEXT_DIM: Color = Color::Rgb(72, 72, 78);
const DIFF_ADD: Color = Color::Rgb(132, 166, 142);
const DIFF_DELETE: Color = Color::Rgb(180, 132, 136);
const DIFF_META: Color = Color::Rgb(108, 132, 188);
const CODE_BG: Color = Color::Rgb(18, 18, 20);
const PANEL_BG: Color = Color::Rgb(28, 28, 34);

pub struct App {
    session: Session,
    startup: StartupInfo,
    model: ModelState,
    pub lines: Vec<ChatLine>,
    printed_lines: usize,
    pub input: TextArea<'static>,
    pub busy: bool,
    pub should_quit: bool,
    ctrl_c_exit: bool,
    /// First Ctrl+C armed exit; a second press quits.
    ctrl_c_pending_exit: bool,
    /// First Esc during a busy turn armed cancellation; a second press cancels.
    esc_pending_cancel: bool,
    busy_frame: u64,
    turn_activity: Option<String>,
    /// Live tail of streaming assistant text (and other delta events).
    /// Cleared on turn completion; never enters the persistent transcript.
    stream_preview: Option<StreamPreview>,
    rx: Option<mpsc::UnboundedReceiver<TurnEvent>>,
    turn_cancel: Option<oneshot::Sender<()>>,
    /// Active setup overlay, if any. The overlay owns its own keyboard
    /// handling so provider, token, and model setup never echo through the
    /// normal chat composer.
    setup: Option<SetupStep>,
    status_layout: StatusLayout,
    session_tokens: Option<u64>,
    /// Terminal-side commands emitted by `ClientCommandsCapability` (via
    /// `runtime.execute_command`). Drained in the event loop; see
    /// [`App::apply_ui_command`].
    ui_rx: mpsc::UnboundedReceiver<UiCommand>,
    /// Settings store shared with the runtime (same instance
    /// `SetupCapability` writes). Used to resolve credentials when querying
    /// provider models APIs and to show per-provider connection status in
    /// the setup overlay.
    settings: Arc<crate::settings::SettingsStore>,
    /// Models discovered from each provider's models API, keyed by provider
    /// name. Once populated, replaces the curated fallback list in the
    /// model picker.
    model_catalog: HashMap<String, ModelPickerCatalog>,
    /// Providers with an in-flight models API fetch.
    model_fetches_in_flight: HashSet<String>,
    /// Disabled in unit tests so opening the picker never spawns real
    /// network requests.
    model_discovery_enabled: bool,
    models_tx: mpsc::UnboundedSender<ModelDiscovery>,
    models_rx: mpsc::UnboundedReceiver<ModelDiscovery>,
    /// This session's background task registry, polled each frame for the
    /// status-bar task count. Same registry the `background` capability owns.
    background: Arc<crate::capabilities::BackgroundRegistry>,
    /// Open background-tasks panel overlay, holding its scroll offset (in lines).
    /// `None` when closed. Toggled with Ctrl+B; read-only view of the registry.
    background_panel: Option<usize>,
    goal_store: Arc<GoalStore>,
    worktree: Arc<WorktreeManager>,
    /// Images from `--image` / `-i` on the CLI, consumed on the first turn.
    pending_images: Vec<ContentPart>,
    /// Large paste placeholders mapped to their full clipboard/terminal payloads.
    pending_pastes: Vec<(String, String)>,
}

/// Result of one background models API fetch. `Ok(None)` means the provider
/// does not support listing; the picker keeps the curated fallback list.
pub(crate) struct ModelDiscovery {
    provider: String,
    result: Result<Option<ModelPickerCatalog>, String>,
}

/// One provider's model picker list: selectable rows plus how many leading
/// rows belong in the recommended section (before the "more models" divider).
#[derive(Clone, Debug)]
pub(crate) struct ModelPickerCatalog {
    pub options: Vec<ModelOption>,
    pub recommended_count: usize,
}

/// State of the first-run / `/setup` overlay. This enum *is* the overlay's
/// state machine; its transitions (key handling, provider/model discovery,
/// persistence) live in [`setup`]. Rendering lives in [`render`].
#[derive(Clone, Debug)]
pub(crate) enum SetupStep {
    Provider {
        selected: usize,
    },
    /// Endpoint base URL for the generic OpenAI-compatible provider.
    BaseUrlInput {
        value: String,
        error: Option<String>,
    },
    Credential {
        provider: String,
        selected: usize,
        error: Option<String>,
    },
    TokenInput {
        provider: String,
        token: String,
        error: Option<String>,
    },
    PickModel {
        provider: String,
        selected: usize,
        custom: Option<String>,
        error: Option<String>,
    },
    PickEffort {
        selected: usize,
        error: Option<String>,
    },
}

pub(crate) struct ProviderOption {
    name: &'static str,
    label: &'static str,
    hint: &'static str,
}

const PROVIDER_OPTIONS: &[ProviderOption] = &[
    ProviderOption {
        name: "openai",
        label: "OpenAI",
        hint: "GPT models",
    },
    ProviderOption {
        name: "codex",
        label: "Codex subscription",
        hint: "ChatGPT Plus/Pro login",
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
        name: "custom",
        label: "Custom endpoint",
        hint: "any OpenAI-compatible URL",
    },
    ProviderOption {
        name: "llmsim",
        label: "Offline demo mode",
        hint: "canned offline responses",
    },
];

pub(crate) struct CredentialOption {
    id: CredentialAction,
    label: String,
    hint: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CredentialAction {
    UseEnv,
    BrowserLogin,
    DeviceLogin,
    PasteKey,
    Skip,
    ClearSaved,
}

#[derive(Clone, Debug)]
pub(crate) struct ModelOption {
    spec: Option<String>,
    label: String,
    hint: String,
}

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
    pub busy: bool,
    pub busy_frame: u64,
    pub turn_activity: Option<String>,
    pub model_id: String,
    pub provider_name: String,
    pub reasoning_effort: Option<String>,
    pub session_id: SessionId,
    pub lines_count: usize,
    pub session_tokens: Option<u64>,
    pub status_layout: StatusLayout,
    pub hooks_summary: String,
    /// Current soft-approval level (`protective` / `normal` / `off`), shown
    /// in the session status bar so the paranoia level is always visible.
    pub approval_mode: String,
    /// `(running, total)` background task counts, shown in the status bar only
    /// when there is at least one task this session. `None` hides the segment.
    pub background: Option<(usize, usize)>,
    /// Short label when a `/goal` loop is active (for example `◎ goal`).
    pub goal_indicator: Option<String>,
}

/// What kind of delta is currently being streamed. Only the assistant
/// output is finalized into the transcript at end-of-turn (via the
/// message store); thinking and tool output are display-only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StatusLayout {
    Compact,
    Expanded,
}

impl StatusLayout {
    pub(crate) fn row_count(self) -> u16 {
        match self {
            Self::Compact => 1,
            Self::Expanded => EXPANDED_STATUS_ROWS,
        }
    }
}

impl App {
    pub fn new(runtime: BuiltRuntime, pending_images: Vec<ContentPart>) -> Self {
        let should_setup = runtime.startup.setup_recommended;
        let goal_store = runtime.goal_store.clone();
        let session_id = runtime.handles.session_id;
        let session = Session::new(runtime.handles, runtime.model.clone());
        let (models_tx, models_rx) = mpsc::unbounded_channel::<ModelDiscovery>();
        let mut app = Self {
            session,
            startup: runtime.startup,
            model: runtime.model,
            lines: Vec::new(),
            printed_lines: 0,
            input: new_input_area(vec![String::new()]),
            busy: false,
            should_quit: false,
            ctrl_c_exit: false,
            ctrl_c_pending_exit: false,
            esc_pending_cancel: false,
            busy_frame: 0,
            turn_activity: None,
            stream_preview: None,
            rx: None,
            turn_cancel: None,
            setup: None,
            status_layout: StatusLayout::Compact,
            session_tokens: None,
            ui_rx: runtime.ui_rx,
            settings: runtime.settings,
            model_catalog: HashMap::new(),
            model_fetches_in_flight: HashSet::new(),
            model_discovery_enabled: true,
            models_tx,
            models_rx,
            background: runtime.background,
            background_panel: None,
            goal_store,
            worktree: runtime.worktree,
            pending_images,
            pending_pastes: Vec::new(),
        };
        app.emit_system_banner();
        if should_setup {
            app.start_first_run_setup();
        } else if app.goal_store.is_paused(session_id)
            && let Some(condition) = app.goal_store.active_condition(session_id)
        {
            app.push_system(format!(
                "restored paused goal: {condition} (run /goal resume to continue)"
            ));
        } else if app.goal_store.take_pending_turn(session_id)
            && let Some(condition) = app.goal_store.active_condition(session_id)
        {
            app.push_system(format!("restored active goal: {condition}"));
            app.push_user(condition.clone());
            app.start_turn(condition);
        }
        app
    }

    pub fn should_show_resume_hint(&self) -> bool {
        self.ctrl_c_exit
    }

    pub fn session_id(&self) -> SessionId {
        self.session.session_id()
    }

    /// Snapshot the renderer-relevant fields into a `ViewState`. Called
    /// once per frame; the clones are dominated by small `String`s.
    pub(crate) fn view_state(&self) -> ViewState {
        ViewState {
            stream_preview: self.stream_preview.clone(),
            command_suggestions: if !self.busy && self.setup.is_none() {
                self.suggestions()
            } else {
                Vec::new()
            },
            busy: self.busy,
            busy_frame: self.busy_frame,
            turn_activity: self.turn_activity.clone(),
            model_id: self.model.model_id(),
            provider_name: self.model.provider_name(),
            reasoning_effort: self.model.reasoning_effort(),
            session_id: self.session.session_id(),
            lines_count: self.lines.len(),
            session_tokens: self.session_tokens,
            status_layout: self.status_layout,
            hooks_summary: self.startup.hook_summary(),
            approval_mode: self
                .settings
                .snapshot()
                .approval_mode()
                .as_str()
                .to_string(),
            background: match self.background.counts() {
                (_, 0) => None,
                counts => Some(counts),
            },
            goal_indicator: self.goal_indicator(),
        }
    }

    fn goal_indicator(&self) -> Option<String> {
        if !self.goal_store.is_active(self.session.session_id()) {
            return None;
        }
        let turns = self
            .goal_store
            .status(self.session.session_id(), self.session_tokens)
            .active
            .map(|active| (active.evaluated_turns, active.paused))
            .unwrap_or((0, false));
        if turns.1 {
            Some(format!("◎ goal paused ({})", turns.0))
        } else {
            Some(format!("◎ goal ({})", turns.0))
        }
    }

    fn emit_system_banner(&mut self) {
        self.push_system(format!(
            "workspace: {}",
            self.startup.workspace_root.display()
        ));
        if let Some(line) = self.startup.worktree_line.clone() {
            if let Some(repo) = &self.startup.repo_root {
                self.push_system(format!("repo: {}", repo.display()));
            }
            self.push_system(line);
        }
        self.push_system(format!("model: {}", self.model.provider_label()));
        self.push_system(format!("tools: {}", self.startup.tool_names.join(", ")));
        if !self.startup.capability_commands.is_empty() {
            let names: Vec<String> = self
                .startup
                .capability_commands
                .iter()
                .map(capability_command_display_usage)
                .collect();
            self.push_system(format!("commands: {}", names.join(", ")));
        }
        self.push_system(
            "type /help for commands, Ctrl+V to paste an image, large pastes attach as placeholders, press Ctrl-C twice (or Ctrl-D) to exit"
                .into(),
        );
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
        // Terminal I/O fails transiently in the wild, so one failed loop
        // iteration must not end the session. The motivating case:
        // xterm.js-backed hosts (ttyd, vhs recordings) resize the PTY
        // mid-session, ratatui re-anchors the inline viewport by querying
        // the cursor position (`CSI 6n`), and crossterm abandons that query
        // after 2s if the emulator is too busy (reflowing scrollback,
        // screencasting) to answer in time. Propagating that error killed
        // the TUI right as turns completed, while tmux — which answers the
        // query itself, instantly — never showed the bug.
        //
        // Retrying is safe because a failed iteration loses nothing and is
        // re-attempted next frame once the terminal catches up. Worst case
        // it leaves cosmetic artifacts: `flush_transcript` only advances
        // `printed_lines` after every chunk landed, so a flush interrupted
        // mid-way re-inserts its lines on retry (briefly duplicated
        // scrollback during a terminal hiccup), and the spinner skips the
        // frames spent failing. Only a run of consecutive failures
        // (terminal actually gone, e.g. PTY closed) is fatal.
        let mut io_failures = 0usize;
        loop {
            if self.busy {
                self.busy_frame = self.busy_frame.wrapping_add(1);
            }
            match self.run_loop_iteration(terminal).await {
                Ok(()) => io_failures = 0,
                Err(err) => {
                    io_failures += 1;
                    if io_failures >= MAX_TERMINAL_IO_FAILURES {
                        return Err(err);
                    }
                    tracing::warn!(
                        "terminal i/o failed ({io_failures}/{MAX_TERMINAL_IO_FAILURES}): {err:#}"
                    );
                }
            }
            if self.should_quit {
                return Ok(());
            }
        }
    }

    /// One iteration of the event loop: render, then drain at most one
    /// class of pending work (turn events, UI commands, keystrokes).
    ///
    /// Invariant: every terminal read/write the TUI performs belongs in
    /// here (or below), never directly in [`App::run`], so it is covered
    /// by `run`'s retry policy. A bare `?` on terminal I/O outside this
    /// function reintroduces the bug where one slow terminal reply exits
    /// the whole session.
    async fn run_loop_iteration<B>(&mut self, terminal: &mut Terminal<B>) -> Result<()>
    where
        B: Backend,
        B::Error: std::error::Error + Send + Sync + 'static,
    {
        self.flush_transcript(terminal)?;
        // Ratatui keeps the inline viewport row fixed across vertical grows and
        // resets it to the top on horizontal shrinks. Re-anchor before each draw
        // so the composer stays pinned to the terminal bottom after resize.
        terminal.autoresize()?;
        maybe_reanchor_inline_viewport(terminal)?;
        terminal.draw(|f| draw(f, self))?;

        // 1) drain background turn events
        if let Some(rx) = self.rx.as_mut() {
            match rx.try_recv() {
                Ok(TurnEvent::Lines(lines)) => {
                    self.lines.extend(lines);
                    return Ok(());
                }
                Ok(TurnEvent::Activity(activity)) => {
                    if !activity.fallback || self.turn_activity.is_none() {
                        self.turn_activity = Some(activity.text);
                    }
                    return Ok(());
                }
                Ok(TurnEvent::Stream(preview)) => {
                    self.stream_preview = preview;
                    return Ok(());
                }
                Ok(TurnEvent::Tokens(tokens)) => {
                    self.session_tokens =
                        Some(self.session_tokens.unwrap_or(0).saturating_add(tokens));
                    return Ok(());
                }
                Ok(TurnEvent::Done) => {
                    self.finish_busy();
                    self.after_turn_goal_check().await;
                    return Ok(());
                }
                Ok(TurnEvent::Failed(err)) => {
                    self.finish_busy();
                    self.push_system(format!("turn failed: {err}"));
                    return Ok(());
                }
                Err(mpsc::error::TryRecvError::Empty) => {}
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    self.finish_busy();
                }
            }
        }

        // 2) drain terminal-side commands emitted by capabilities. Apply
        // every queued command before re-rendering so a burst (or a future
        // capability that emits more than one) doesn't cost a full
        // flush/draw per command, matching the test dispatch helper.
        let mut applied_ui_command = false;
        while let Ok(command) = self.ui_rx.try_recv() {
            self.apply_ui_command(command);
            applied_ui_command = true;
        }
        if applied_ui_command {
            return Ok(());
        }

        // 3) drain finished models API fetches so an open picker refreshes.
        let mut applied_model_discovery = false;
        while let Ok(discovery) = self.models_rx.try_recv() {
            self.apply_model_discovery(discovery);
            applied_model_discovery = true;
        }
        if applied_model_discovery {
            return Ok(());
        }

        // 3b) proactive wake: when background tasks finish while the session is
        // idle, auto-start a turn so the agent reacts without a user prompt.
        if self.maybe_wake_for_background() {
            return Ok(());
        }

        // 4) direct terminal input.
        let mut poll_timeout = Duration::from_millis(80);
        while event::poll(poll_timeout)? {
            poll_timeout = Duration::ZERO;
            match event::read()? {
                CrosstermEvent::Key(key) => {
                    if key.kind == KeyEventKind::Release {
                        continue;
                    }
                    if key.code == KeyCode::Esc && self.handle_escape_prefixed_enter().await? {
                        continue;
                    }
                    self.handle_key(key).await;
                }
                CrosstermEvent::Mouse(mouse) => {
                    let area = terminal.get_frame().area();
                    if self.handle_mouse(mouse, area) {
                        return Ok(());
                    }
                }
                CrosstermEvent::Paste(pasted) => {
                    if self.setup.is_some() {
                        self.handle_setup_paste(pasted).await;
                    } else {
                        self.handle_paste(pasted);
                    }
                }
                _ => {}
            }
            if self.should_quit {
                break;
            }
        }
        Ok(())
    }

    async fn handle_escape_prefixed_enter(&mut self) -> Result<bool> {
        if !event::poll(Duration::from_millis(25))? {
            return Ok(false);
        }

        match event::read()? {
            CrosstermEvent::Key(next) if next.kind == KeyEventKind::Release => Ok(false),
            CrosstermEvent::Key(next) if next.code == KeyCode::Enter => {
                self.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT))
                    .await;
                Ok(true)
            }
            CrosstermEvent::Key(next) if next.code == KeyCode::Esc => {
                self.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()))
                    .await;
                self.handle_key(next).await;
                Ok(true)
            }
            CrosstermEvent::Key(next) => {
                let mut alt = next;
                alt.modifiers.insert(KeyModifiers::ALT);
                self.handle_key(alt).await;
                Ok(true)
            }
            _ => {
                self.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()))
                    .await;
                Ok(true)
            }
        }
    }

    async fn emit_replayed_transcript(&mut self) {
        if self.startup.replayed_events == 0 {
            return;
        }

        match self
            .session
            .replayed_lines(self.startup.replayed_events)
            .await
        {
            Ok(replayed_lines) => self.lines.extend(replayed_lines),
            Err(err) => self.push_system(format!("load replayed transcript: {err}")),
        }
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
                    self.handle_ctrl_c();
                    return;
                }
                KeyCode::Char('d') => {
                    self.should_quit = true;
                    return;
                }
                KeyCode::Char('b') => {
                    // Clear the armed single-Ctrl+C exit ourselves, since this
                    // branch returns before the shared reset below.
                    self.ctrl_c_pending_exit = false;
                    self.toggle_background_panel();
                    return;
                }
                KeyCode::Char('v') => {
                    self.try_paste_clipboard();
                    return;
                }
                _ => {}
            }
        }

        self.ctrl_c_pending_exit = false;

        // The background panel overlay captures navigation keys (even mid-turn,
        // so you can watch tasks while a turn runs).
        if self.background_panel.is_some() {
            self.handle_background_panel_key(key);
            return;
        }

        if self.busy {
            // Block only input editing while a turn is running.
            self.handle_busy_key(key);
            return;
        }
        if self.setup.is_some() {
            self.handle_setup_key(key).await;
            return;
        }
        match key.code {
            KeyCode::Enter if key.modifiers == KeyModifiers::SHIFT => {
                self.input.insert_newline();
            }
            KeyCode::Enter => {
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
                let _ = self.input.input(normalize_printable_key(key));
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
        self.pending_pastes.clear();
    }

    fn input_height(&self, input_width: u16) -> u16 {
        wrapped_input_visual_lines(&self.input, input_width).clamp(1, MAX_INPUT_HEIGHT as usize)
            as u16
    }

    fn handle_paste(&mut self, pasted: String) {
        if self.busy || self.setup.is_some() || self.background_panel.is_some() {
            return;
        }

        let pasted = crate::paste_attachment::normalize_pasted_text(&pasted);
        if pasted.is_empty() {
            return;
        }

        if pasted.len() > crate::paste_attachment::MAX_PASTE_ATTACHMENT_BYTES {
            self.push_system(format!(
                "paste too large (max {} KiB)",
                crate::paste_attachment::MAX_PASTE_ATTACHMENT_BYTES / 1024
            ));
            return;
        }

        if crate::paste_attachment::is_large_paste(&pasted) {
            let char_count = pasted.chars().count();
            let placeholder = crate::paste_attachment::next_large_paste_placeholder(
                char_count,
                &self.pending_pastes,
            );
            self.input.insert_str(&placeholder);
            self.pending_pastes.push((placeholder, pasted));
        } else {
            self.input.insert_str(&pasted);
        }
    }

    fn try_paste_clipboard(&mut self) {
        if self.busy || self.setup.is_some() || self.background_panel.is_some() {
            return;
        }
        match crate::clipboard_paste::paste_image_content_part() {
            Ok((part, info)) => {
                self.pending_images.push(part);
                let index = self.pending_images.len();
                self.push_system(format!(
                    "attached clipboard image #{index} ({}x{} PNG)",
                    info.width, info.height
                ));
            }
            Err(crate::clipboard_paste::PasteImageError::NoImage(_)) => {
                if let Ok(text) = crate::clipboard_paste::paste_clipboard_text() {
                    self.handle_paste(text);
                }
            }
            Err(err) => {
                tracing::debug!("clipboard image paste failed: {err}");
                self.push_system(format!("clipboard image paste failed: {err}"));
            }
        }
    }

    async fn submit_input(&mut self) {
        let raw = self.input_text();
        crate::paste_attachment::prune_pending_pastes(&raw, &mut self.pending_pastes);
        let pending_pastes = std::mem::take(&mut self.pending_pastes);
        let expanded = crate::paste_attachment::expand_pending_pastes(&raw, &pending_pastes);
        self.reset_input();
        let text = expanded.trim().to_string();
        let display_text = raw.trim().to_string();
        if let Some(command) = parse_bang_shell_command(&text) {
            if command.is_empty() {
                self.push_system("usage: !<command>".into());
            } else {
                self.handle_shell_alias(command.to_string()).await;
            }
            return;
        }
        if let Some(rest) = text.strip_prefix('/') {
            self.handle_command(rest).await;
            return;
        }
        if text.is_empty() && self.pending_images.is_empty() {
            return;
        }
        let image_count = self.pending_images.len();
        let display = crate::image_input::user_display_text(&display_text, image_count);
        self.push_user(display);
        self.start_turn(text);
    }

    /// Open the background-tasks panel (read-only) or close it if already open.
    /// Suppressed while the setup overlay is up so two modals never stack.
    fn toggle_background_panel(&mut self) {
        if self.background_panel.is_some() {
            self.background_panel = None;
        } else if self.setup.is_none() {
            self.background_panel = Some(0);
        }
    }

    /// Navigation for the open background panel: Esc/q closes, Up/Down scroll.
    fn handle_background_panel_key(&mut self, key: KeyEvent) {
        let Some(offset) = self.background_panel else {
            return;
        };
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.background_panel = None,
            KeyCode::Up | KeyCode::Char('k') => {
                self.background_panel = Some(offset.saturating_sub(1));
            }
            KeyCode::Down | KeyCode::Char('j') => {
                // Clamp so scrolling can't run past the last line of content.
                let max = self
                    .background
                    .render_task_list()
                    .lines()
                    .count()
                    .saturating_sub(1);
                self.background_panel = Some(offset.saturating_add(1).min(max));
            }
            _ => {}
        }
    }

    /// Proactive wake (Phase 4): when background tasks reach a terminal state
    /// while the session is idle, automatically start a turn so the agent reacts
    /// to the result (reads output, continues, or reports) without waiting for a
    /// user prompt. Returns true if it started a turn. Only fires when idle, so
    /// it never interrupts an in-flight turn; each task wakes at most once.
    fn maybe_wake_for_background(&mut self) -> bool {
        if self.busy || self.rx.is_some() || self.setup.is_some() {
            return false;
        }
        let finished = self.background.drain_finished_for_wake();
        if finished.is_empty() {
            return false;
        }
        // Honor the `proactive_wake` setting: when off, surface a notice but do
        // not auto-start a turn (the tasks are already drained, so this reports
        // each finished task exactly once without waking).
        if !self.settings.snapshot().proactive_wake_enabled() {
            let ids = finished
                .iter()
                .map(|t| t.id.clone())
                .collect::<Vec<_>>()
                .join(", ");
            self.push_system(format!(
                "✓ background task finished ({ids}) — see /background (proactive wake off)"
            ));
            return false;
        }
        let ids = finished
            .iter()
            .map(|t| t.id.clone())
            .collect::<Vec<_>>()
            .join(", ");
        self.push_system(format!(
            "↻ background task finished ({ids}) — waking agent to review"
        ));
        let prompt = crate::capabilities::background::wake_prompt(&finished);
        self.start_turn(prompt);
        true
    }

    /// Dispatch a slash command. Every command — including the terminal-side
    /// ones (help/tools/mcp/cwd/model/effort/clear/shell/quit) — is now a capability
    /// command, so this is a single uniform lookup against the registry. The
    /// terminal-side commands take effect via `UiCommand`s their capability
    /// emits while executing (drained in the event loop); see
    /// [`App::apply_ui_command`].
    async fn handle_command(&mut self, cmd: &str) {
        let cmd = cmd.trim();
        let mut parts = cmd.splitn(2, char::is_whitespace);
        let head = parts.next().unwrap_or_default();
        let arg = parts.next().unwrap_or_default().trim();
        // `/exit` is an accepted alias for the declared `/quit`.
        let name = if head == "exit" { "quit" } else { head };

        if let Some(descriptor) = self
            .startup
            .capability_commands
            .iter()
            .find(|c| c.name == name)
            .cloned()
        {
            self.invoke_capability_command(descriptor, arg.to_string())
                .await;
        } else {
            self.push_system(format!("unknown command: /{head}"));
        }
    }

    /// Dispatch the TUI's `!shell <command>` alias through the same capability
    /// descriptor as `/shell`, so registration, required-arg validation, and
    /// host gating keep one source of truth.
    async fn handle_shell_alias(&mut self, command: String) {
        if let Some(descriptor) = self
            .startup
            .capability_commands
            .iter()
            .find(|c| c.name == "shell")
            .cloned()
        {
            self.invoke_capability_command(descriptor, command).await;
        } else {
            self.push_system("unknown command: !shell".into());
        }
    }

    /// Apply a terminal-side command emitted by a capability. This is the only
    /// place the host interprets the `UiCommand` vocabulary; capabilities
    /// declare commands and request effects, the host performs them.
    fn apply_ui_command(&mut self, command: UiCommand) {
        match command {
            UiCommand::ShowHelp => self.show_help(),
            UiCommand::ShowTools => {
                self.push_system(format!("tools: {}", self.startup.tool_names.join(", ")));
            }
            UiCommand::ShowMcp => {
                if self.startup.mcp_server_names.is_empty() {
                    let global = crate::mcp_config::global_mcp_config_path()
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| "the yolop config dir".to_string());
                    self.push_system(format!(
                        "no MCP servers configured (add them to .mcp.json in the workspace \
                         root or {global})"
                    ));
                } else {
                    self.push_system(format!(
                        "MCP servers: {}",
                        self.startup.mcp_server_names.join(", ")
                    ));
                }
            }
            UiCommand::ShowCwd => {
                self.push_system(format!(
                    "workspace root: {}",
                    self.startup.workspace_root.display()
                ));
            }
            UiCommand::SetStatusLayout { arg } => self.set_status_layout(arg.as_deref()),
            UiCommand::ClearTranscript => {
                self.lines.clear();
                self.printed_lines = 0;
                self.goal_store.clear_active(self.session.session_id());
                self.emit_system_banner();
            }
            UiCommand::RunShell { command } => self.start_shell_command(command),
            UiCommand::Quit => self.should_quit = true,
            UiCommand::OpenModelOverlay { arg } => match arg {
                Some(arg) => self.start_model_setup_with_arg(&arg),
                None => self.start_model_setup(),
            },
            UiCommand::OpenEffortOverlay { arg } => {
                self.start_effort_setup(arg.as_deref().unwrap_or(""))
            }
        }
    }

    fn show_help(&mut self) {
        if !self.startup.capability_commands.is_empty() {
            let command_lines: Vec<String> = self
                .startup
                .capability_commands
                .iter()
                .map(help_command_line)
                .collect();
            self.push_system("commands:".into());
            for line in command_lines {
                self.push_system(line);
            }
        }
        self.push_system("shortcuts:".into());
        for line in help_shortcut_lines() {
            self.push_system(line.to_string());
        }
        self.push_system(
            "more: /tools · /mcp · /yolop skill · type naturally for terminal actions".into(),
        );
    }

    fn set_status_layout(&mut self, raw: Option<&str>) {
        let layout = match raw.map(str::trim).filter(|s| !s.is_empty()) {
            None | Some("toggle") => match self.status_layout {
                StatusLayout::Compact => StatusLayout::Expanded,
                StatusLayout::Expanded => StatusLayout::Compact,
            },
            Some("compact") => StatusLayout::Compact,
            Some("expanded") => StatusLayout::Expanded,
            Some(other) => {
                self.push_system(format!(
                    "usage: /status [compact|expanded|toggle] (unknown layout: {other})"
                ));
                return;
            }
        };
        self.status_layout = layout;
    }

    fn toggle_status_layout(&mut self) {
        self.status_layout = match self.status_layout {
            StatusLayout::Compact => StatusLayout::Expanded,
            StatusLayout::Expanded => StatusLayout::Compact,
        };
    }

    fn handle_mouse(&mut self, mouse: MouseEvent, terminal_area: Rect) -> bool {
        if !matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
            return false;
        }
        if self.mouse_is_on_status(mouse, terminal_area) {
            self.toggle_status_layout();
            return true;
        }
        false
    }

    fn mouse_is_on_status(&self, mouse: MouseEvent, terminal_area: Rect) -> bool {
        let input_width = terminal_area.width.saturating_sub(2);
        let state = self.view_state();
        let layout = app_layout_for_frame(
            terminal_area,
            self.input_height(input_width),
            self.status_layout,
            chrome_preview_visible(&state),
        );
        if layout.chrome.session_status.height == 0 {
            return false;
        }
        let status_rect = layout.chrome.session_status;
        mouse.row >= status_rect.y
            && mouse.row < status_rect.y.saturating_add(status_rect.height)
            && mouse.column >= status_rect.x
            && mouse.column < status_rect.x.saturating_add(status_rect.width)
    }

    fn handle_ctrl_c(&mut self) {
        if !self.busy && !self.input_text().trim().is_empty() {
            self.reset_input();
            self.ctrl_c_pending_exit = false;
            return;
        }

        if self.ctrl_c_pending_exit {
            self.ctrl_c_exit = true;
            self.should_quit = true;
            return;
        }

        self.ctrl_c_pending_exit = true;
        self.push_system("Press Ctrl+C again to exit".into());
    }

    fn handle_busy_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc if self.esc_pending_cancel => self.cancel_current_turn(),
            KeyCode::Esc if self.turn_cancel.is_some() => {
                self.esc_pending_cancel = true;
                self.push_system("Press Esc again to cancel current turn".into());
            }
            _ => {
                self.esc_pending_cancel = false;
            }
        }
    }

    fn cancel_current_turn(&mut self) {
        self.esc_pending_cancel = false;
        if self.goal_store.is_active(self.session.session_id()) {
            match self.goal_store.pause_active(self.session.session_id()) {
                Ok(message) => self.push_system(message),
                Err(err) => self.push_system(format!("goal pause failed: {err}")),
            }
        }
        if let Some(cancel) = self.turn_cancel.take() {
            let _ = cancel.send(());
            self.turn_activity = Some("cancelling".into());
            self.stream_preview = None;
        }
    }

    fn finish_busy(&mut self) {
        self.busy = false;
        self.busy_frame = 0;
        self.turn_activity = None;
        self.stream_preview = None;
        self.rx = None;
        self.turn_cancel = None;
        self.esc_pending_cancel = false;
    }

    fn maybe_start_goal_turn(&mut self) {
        let session_id = self.session.session_id();
        if !self.goal_store.take_pending_turn(session_id) {
            return;
        }
        let Some(condition) = self.goal_store.active_condition(session_id) else {
            return;
        };
        self.push_user(condition.clone());
        self.start_turn(condition);
    }

    async fn after_turn_goal_check(&mut self) {
        let session_id = self.session.session_id();
        if !self.goal_store.is_active(session_id) {
            return;
        }
        if self.goal_store.is_paused(session_id) {
            return;
        }
        let result = match self
            .session
            .execute_command("goal", Some(GOAL_EVALUATE_ARG.to_string()))
            .await
        {
            Ok(result) => result,
            Err(err) => {
                self.push_system(format!("goal evaluation failed: {err}"));
                return;
            }
        };
        if !result.success {
            self.push_system(format!("goal evaluation failed: {}", result.message));
            return;
        }
        let evaluation = match parse_evaluation_response(&result.message) {
            Ok(evaluation) => evaluation,
            Err(err) => {
                self.push_system(format!("goal evaluation failed: {err}"));
                return;
            }
        };
        if evaluation.met {
            self.push_system(format!("goal achieved: {}", evaluation.reason));
            return;
        }
        self.push_system(format!("goal: {}", evaluation.reason));
        let Some(prompt) = self.goal_store.continuation_prompt(session_id) else {
            return;
        };
        self.push_user(prompt.clone());
        self.start_turn(prompt);
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

                let arguments = (!trimmed.is_empty()).then(|| trimmed.to_string());
                let result = self
                    .session
                    .execute_command(&descriptor.name, arguments)
                    .await;
                match result {
                    Ok(result) => {
                        // Client-executed commands (help/clear/model/…) apply
                        // their effect via a `UiCommand` and return an empty
                        // message; don't render a blank line for those.
                        if !result.message.is_empty() {
                            let prefix = if result.success { "" } else { "error: " };
                            self.push_system(format!("{prefix}{}", result.message));
                        }
                        if descriptor.name == "goal" && result.success {
                            self.maybe_start_goal_turn();
                        }
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

    fn start_shell_command(&mut self, command: String) {
        let workspace_root = self.startup.workspace_root.clone();
        self.push_user(format!("!shell {command}"));
        let handle = self.session.run_shell(command, workspace_root);
        self.begin_turn(handle, Some("running shell command".into()));
    }

    /// Wire a freshly started [`crate::session::TurnHandle`] into the event loop:
    /// store its receiver and cancel trigger and flip into the busy state.
    fn begin_turn(&mut self, handle: crate::session::TurnHandle, activity: Option<String>) {
        self.rx = Some(handle.events);
        self.turn_cancel = Some(handle.cancel);
        self.esc_pending_cancel = false;
        self.busy = true;
        self.turn_activity = activity;
        self.stream_preview = None;
    }

    fn start_turn(&mut self, prompt: String) {
        match self.worktree.ensure_before_turn(&prompt) {
            Ok(true) => {
                self.startup.workspace_root = self.worktree.active_root();
                if let Some(line) = self.worktree.status_line() {
                    self.push_system(line);
                }
            }
            Ok(false) => {
                self.startup.workspace_root = self.worktree.active_root();
            }
            Err(err) => self.push_system(format!("worktree: {err}")),
        }

        let images = std::mem::take(&mut self.pending_images);
        let handle = self.session.run_turn(prompt, images);
        self.begin_turn(handle, None);
    }
}

fn parse_bang_shell_command(input: &str) -> Option<&str> {
    let rest = input.trim().strip_prefix('!')?;
    let rest = rest
        .trim_start()
        .strip_prefix("shell")
        .and_then(|tail| {
            tail.chars()
                .next()
                .is_none_or(char::is_whitespace)
                .then_some(tail)
        })
        .unwrap_or(rest);
    if rest.is_empty() {
        return Some("");
    }
    Some(rest.trim())
}

fn command_suggestions(
    input: &str,
    capability_commands: &[CommandDescriptor],
) -> Vec<CommandSuggestion> {
    if let Some(rest) = input.strip_prefix('!') {
        return bang_command_suggestions(rest, capability_commands);
    }
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

    // Every command is capability-provided now (the TUI's terminal-side
    // commands come from `ClientCommandsCapability`), so there is a single
    // source of truth to filter and present.
    let mut out: Vec<CommandSuggestion> = Vec::new();
    for descriptor in capability_commands {
        if !descriptor.name.starts_with(rest) {
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

    // Keep the dropdown bounded but large enough to show every built-in
    // command (10 client commands + capability commands like /setup) for a
    // bare `/`, so none is hidden behind the cap.
    out.truncate(12);
    out
}

fn bang_command_suggestions(
    rest: &str,
    capability_commands: &[CommandDescriptor],
) -> Vec<CommandSuggestion> {
    let Some(descriptor) = capability_commands.iter().find(|c| c.name == "shell") else {
        return Vec::new();
    };
    if rest.contains(char::is_whitespace) || !descriptor.name.starts_with(rest) {
        return Vec::new();
    }
    vec![CommandSuggestion {
        completion: "!shell ".to_string(),
        label: format!(
            "{}    {}",
            capability_command_display_usage(descriptor),
            descriptor.description
        ),
    }]
}

fn capability_command_display_usage(descriptor: &CommandDescriptor) -> String {
    capability_command_usage_with_prefix(
        descriptor,
        if descriptor.name == "shell" { "!" } else { "/" },
    )
}

fn capability_command_usage(descriptor: &CommandDescriptor) -> String {
    capability_command_usage_with_prefix(descriptor, "/")
}

fn help_command_line(descriptor: &CommandDescriptor) -> String {
    let usage = if descriptor.name == "quit" {
        "/quit (/exit)".to_string()
    } else {
        capability_command_usage(descriptor)
    };
    format!("  {} — {}", usage, descriptor.description)
}

fn help_shortcut_lines() -> [&'static str; 5] {
    [
        "  Enter send · Shift-Enter newline · Tab complete slash command · ←/→ edit",
        "  Ctrl+V paste image/text · Ctrl+B background tasks · !<cmd> shell alias",
        "  exit: Ctrl-C twice / Ctrl-D",
        "  cancel turn: Esc twice (while model is busy)",
        "  scroll: terminal scrollback (no in-app page keys)",
    ]
}

fn capability_command_usage_with_prefix(descriptor: &CommandDescriptor, prefix: &str) -> String {
    if descriptor.args.is_empty() {
        format!("{prefix}{}", descriptor.name)
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
        format!("{prefix}{} {args}", descriptor.name)
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

/// Visual rows the composer needs at `width`, including soft-wrapped logical lines.
fn wrapped_input_visual_lines(input: &TextArea<'_>, width: u16) -> usize {
    let width = width.max(1);
    let mut scratch = input.clone();
    let area = Rect {
        x: 0,
        y: 0,
        width,
        height: MAX_INPUT_HEIGHT,
    };
    let mut buf = Buffer::empty(area);
    Widget::render(&scratch, area, &mut buf);
    scratch.move_cursor(CursorMove::End);
    scratch.screen_cursor().row + 1
}

fn normalize_printable_key(mut key: KeyEvent) -> KeyEvent {
    if !key.modifiers.contains(KeyModifiers::SHIFT)
        || key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
    {
        return key;
    }

    let KeyCode::Char(ch) = key.code else {
        return key;
    };
    let Some(ch) = shifted_char(ch) else {
        return key;
    };

    key.code = KeyCode::Char(ch);
    key.modifiers.remove(KeyModifiers::SHIFT);
    key
}

fn shifted_char(ch: char) -> Option<char> {
    let shifted = match ch {
        'a'..='z' => ch.to_ascii_uppercase(),
        'A'..='Z' | ' ' => ch,
        '`' => '~',
        '1' => '!',
        '2' => '@',
        '3' => '#',
        '4' => '$',
        '5' => '%',
        '6' => '^',
        '7' => '&',
        '8' => '*',
        '9' => '(',
        '0' => ')',
        '-' => '_',
        '=' => '+',
        '[' => '{',
        ']' => '}',
        '\\' => '|',
        ';' => ':',
        '\'' => '"',
        ',' => '<',
        '.' => '>',
        '/' => '?',
        '~' | '!' | '@' | '#' | '$' | '%' | '^' | '&' | '*' | '(' | ')' | '_' | '+' | '{' | '}'
        | '|' | ':' | '"' | '<' | '>' | '?' => ch,
        _ => return None,
    };
    Some(shifted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capabilities::model_discovery::DiscoveredProviderModel;
    use everruns_core::events::{
        Event as RuntimeEvent, EventContext, InputMessageData, OutputMessageCompletedData,
        OutputMessageStartedData, ReasonCompletedData, ToolCompletedData,
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

    /// The terminal-side command descriptors as declared by
    /// `ClientCommandsCapability` (help/tools/cwd/model/effort/clear/shell/quit).
    /// These now flow through the same registry as every other command, so
    /// suggestion tests source them the same way the running TUI does.
    fn client_command_descriptors() -> Vec<CommandDescriptor> {
        use everruns_core::capabilities::Capability;
        struct NoopUi;
        impl crate::host_ui::HostUi for NoopUi {
            fn send(&self, _: crate::host_ui::UiCommand) {}
        }
        crate::capabilities::client_commands::ClientCommandsCapability::new(std::sync::Arc::new(
            NoopUi,
        ))
        .commands()
    }

    /// Client commands plus a representative capability command, in the order
    /// the TUI would see them at startup.
    fn caps_with_client_commands(extra: Vec<CommandDescriptor>) -> Vec<CommandDescriptor> {
        let mut caps = client_command_descriptors();
        caps.extend(extra);
        caps
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
        let caps = caps_with_client_commands(vec![setup_capability_command()]);
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
        let caps = caps_with_client_commands(vec![setup_capability_command()]);
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
    fn bang_shell_parser_accepts_shell_alias_and_bare_command() {
        assert_eq!(parse_bang_shell_command("!shell"), Some(""));
        assert_eq!(parse_bang_shell_command("  !shell   pwd  "), Some("pwd"));
        assert_eq!(
            parse_bang_shell_command("!shell	printf hi"),
            Some("printf hi")
        );
        assert_eq!(
            parse_bang_shell_command("!printf shell-ok"),
            Some("printf shell-ok")
        );
        assert_eq!(
            parse_bang_shell_command("!shellshock echo yes"),
            Some("shellshock echo yes")
        );
        assert_eq!(parse_bang_shell_command("!"), Some(""));
    }

    #[test]
    fn bang_shell_suggestions_use_client_command_registry() {
        let caps = caps_with_client_commands(vec![setup_capability_command()]);
        let suggestions = command_suggestions("!s", &caps);

        assert_eq!(suggestions.len(), 1, "got: {suggestions:?}");
        assert_eq!(suggestions[0].completion, "!shell ");
        assert!(suggestions[0].label.starts_with("!shell <command>"));
        assert!(command_suggestions("!shell echo", &caps).is_empty());
        assert!(command_suggestions("!s", &[setup_capability_command()]).is_empty());
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
    fn suggestions_come_solely_from_the_command_registry() {
        // There are no hard-coded built-ins anymore: every command — including
        // /help — is a capability command (the TUI's come from
        // `ClientCommandsCapability`). So suggestions reflect exactly the
        // descriptor list, one entry per declared command.
        let caps = vec![CommandDescriptor {
            name: "help".to_string(),
            description: "show commands".to_string(),
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
    fn newline_shortcut_hint_uses_shift_enter_only() {
        assert_eq!(newline_shortcut_hint(), "Shift-Enter");
    }

    #[test]
    fn chrome_height_reserves_four_expanded_status_rows() {
        assert_eq!(chrome_height(1, StatusLayout::Compact, false), 4);
        assert_eq!(chrome_height(1, StatusLayout::Compact, true), 5);
        assert_eq!(chrome_height(1, StatusLayout::Expanded, false), 7);
        assert_eq!(chrome_height(1, StatusLayout::Expanded, true), 8);
        assert_eq!(chrome_height(3, StatusLayout::Compact, false), 5);
        assert_eq!(chrome_height(3, StatusLayout::Expanded, false), 8);
        assert_eq!(chrome_height(4, StatusLayout::Compact, false), 6);
        assert_eq!(chrome_height(4, StatusLayout::Expanded, false), 9);
    }

    #[test]
    fn chrome_dimensions_clamp_input_to_visible_frame() {
        assert_eq!(
            chrome_dimensions(7, MAX_INPUT_HEIGHT, StatusLayout::Expanded, false),
            (7, 1)
        );
        assert_eq!(
            chrome_dimensions(5, MAX_INPUT_HEIGHT, StatusLayout::Compact, false),
            (5, 3)
        );
        assert_eq!(
            chrome_dimensions(0, MAX_INPUT_HEIGHT, StatusLayout::Compact, false),
            (0, 0)
        );
    }

    fn rect_inside(parent: Rect, child: Rect) -> bool {
        let parent_right = parent.x as u32 + parent.width as u32;
        let parent_bottom = parent.y as u32 + parent.height as u32;
        let child_right = child.x as u32 + child.width as u32;
        let child_bottom = child.y as u32 + child.height as u32;
        child.x >= parent.x
            && child.y >= parent.y
            && child_right <= parent_right
            && child_bottom <= parent_bottom
    }

    #[test]
    fn app_layout_rectangles_stay_inside_frame_across_sizes() {
        let widths = [0, 1, 2, 4, 8, 16, 40, 120];
        let heights = [0, 1, 2, 3, 4, 5, 7, 12, 24, 60];
        let desired_inputs = [0, 1, 2, 3, MAX_INPUT_HEIGHT, MAX_INPUT_HEIGHT + 8];
        for status_layout in [StatusLayout::Compact, StatusLayout::Expanded] {
            for width in widths {
                for height in heights {
                    for desired_input in desired_inputs {
                        let frame = Rect {
                            x: 2,
                            y: 3,
                            width,
                            height,
                        };
                        let layout =
                            app_layout_for_frame(frame, desired_input, status_layout, false);
                        assert_eq!(layout.frame, frame);
                        assert!(
                            rect_inside(frame, layout.transcript),
                            "transcript escaped frame: {layout:?}"
                        );
                        assert!(
                            rect_inside(frame, layout.chrome.area),
                            "chrome escaped frame: {layout:?}"
                        );
                        assert!(
                            rect_inside(layout.chrome.area, layout.chrome.preview),
                            "preview escaped chrome: {layout:?}"
                        );
                        assert!(
                            rect_inside(layout.chrome.area, layout.chrome.message_separator),
                            "message separator escaped chrome: {layout:?}"
                        );
                        assert!(
                            rect_inside(layout.chrome.area, layout.chrome.input),
                            "input escaped chrome: {layout:?}"
                        );
                        assert!(
                            rect_inside(layout.chrome.area, layout.chrome.status_separator),
                            "status separator escaped chrome: {layout:?}"
                        );
                        assert!(
                            rect_inside(layout.chrome.area, layout.chrome.session_status),
                            "session status escaped chrome: {layout:?}"
                        );
                        assert!(
                            layout.chrome.area.height <= frame.height,
                            "chrome taller than frame: {layout:?}"
                        );
                        assert!(
                            layout.chrome.input_height <= MAX_INPUT_HEIGHT,
                            "input height exceeded cap: {layout:?}"
                        );
                    }
                }
            }
        }
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
        assert!(matches!(lines[0].author, Author::Narration));
        assert_eq!(lines[0].text, "I'll check the manifests first.");
        assert_eq!(lines[0].author.label(), "note");
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
        assert!(matches!(lines[0].author, Author::Narration));
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
    fn handle_live_event_renders_write_todos_from_started_args_when_result_is_counts_only() {
        use everruns_core::events::ToolStartedData;
        use everruns_core::tool_types::ToolCall;

        let (tx, mut rx) = mpsc::unbounded_channel::<TurnEvent>();
        let mut emitted = HashSet::new();
        let mut router = DeltaRouter::default();
        let session_id = SessionId::new();

        let started = RuntimeEvent::new(
            session_id,
            EventContext::empty(),
            ToolStartedData {
                tool_call: ToolCall {
                    id: "call_todos".to_string(),
                    name: "write_todos".to_string(),
                    arguments: serde_json::json!({
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
                    }),
                },
                tool_call_fingerprint: None,
                display_name: Some("Write Todos".to_string()),
                narration: None,
            },
        );
        let completed = RuntimeEvent::new(
            session_id,
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
                        "completed": 1
                    })
                    .to_string(),
                )],
                None,
            ),
        );

        handle_live_event(&started, &mut emitted, &mut router, &tx);
        handle_live_event(&completed, &mut emitted, &mut router, &tx);

        let mut lines = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let TurnEvent::Lines(batch) = event {
                lines.extend(batch.into_iter().map(|line| line.text));
            }
        }

        assert!(lines.iter().all(|line| line != "✓ Write Todos"));
        assert_eq!(
            lines,
            vec![
                "1 of 3 todos completed  ✓ Read current CLI renderer  › Rendering todos in transcript  ○ Run focused tests"
            ]
        );
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
    fn handle_live_event_hides_thinking_delta_from_stream_preview() {
        use everruns_core::events::ReasonThinkingDeltaData;
        use everruns_core::typed_id::TurnId;

        let (tx, mut rx) = mpsc::unbounded_channel::<TurnEvent>();
        let mut emitted = HashSet::new();
        let mut router = DeltaRouter::default();

        let event = RuntimeEvent::new(
            SessionId::new(),
            EventContext::empty(),
            ReasonThinkingDeltaData {
                turn_id: TurnId::new(),
                delta: "private chain".to_string(),
                accumulated: "private chain".to_string(),
            },
        );
        handle_live_event(&event, &mut emitted, &mut router, &tx);

        while let Ok(event) = rx.try_recv() {
            assert!(
                !matches!(event, TurnEvent::Stream(_)),
                "private thinking must not create a stream preview: {event:?}"
            );
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
    fn handle_live_event_emits_known_token_counts() {
        use everruns_core::events::ReasonItemData;

        let (tx, mut rx) = mpsc::unbounded_channel::<TurnEvent>();
        let mut emitted = HashSet::new();
        let mut router = DeltaRouter::default();

        let event = RuntimeEvent::new(
            SessionId::new(),
            EventContext::empty(),
            ReasonItemData {
                turn_id: TurnId::new(),
                provider: "openai".to_string(),
                model: Some("gpt-5".to_string()),
                item_id: "rs_tokens".to_string(),
                encrypted_content: None,
                summary: Vec::new(),
                token_count: Some(120),
            },
        );
        handle_live_event(&event, &mut emitted, &mut router, &tx);

        let mut tokens = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let TurnEvent::Tokens(count) = event {
                tokens.push(count);
            }
        }
        assert_eq!(tokens, vec![120]);
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

    #[test]
    fn first_line_truncates_on_char_boundaries() {
        // Regression: byte-index slicing panicked when `max` landed inside a
        // multi-byte code point. "héllo" — the limit of 2 must split between
        // 'h' and 'é' (a 2-byte char), not mid-codepoint.
        assert_eq!(first_line("héllo", 2), "hé…");
        // Only the first line is kept, and short non-ASCII text is untouched.
        assert_eq!(first_line("résumé\nsecond", 99), "résumé");
    }

    fn line_text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>()
    }

    fn line_content_color(line: &Line<'_>) -> Option<Color> {
        line.spans.get(1).and_then(|span| span.style.fg)
    }

    #[test]
    fn diff_lines_style_adds_deletes_and_metadata() {
        let mut lines = Vec::new();
        append_chat_lines(
            &mut lines,
            &ChatLine {
                author: Author::Diff,
                text: "--- /workspace/src/app.rs (before)\n+++ /workspace/src/app.rs (after)\n@@ -1 +1 @@\n-old\n+new\n unchanged".to_string(),
            },
            96,
        );

        assert_eq!(line_content_color(&lines[0]), Some(DIFF_DELETE));
        assert_eq!(line_content_color(&lines[1]), Some(DIFF_ADD));
        assert_eq!(line_content_color(&lines[2]), Some(DIFF_META));
        assert_eq!(line_content_color(&lines[3]), Some(DIFF_DELETE));
        assert_eq!(line_content_color(&lines[4]), Some(DIFF_ADD));
        assert_eq!(line_content_color(&lines[5]), Some(TEXT_PRIMARY));
    }

    #[test]
    fn narration_lines_use_note_label_and_muted_text() {
        let mut lines = Vec::new();
        append_chat_lines(
            &mut lines,
            &ChatLine {
                author: Author::Narration,
                text: "Considering installation steps".to_string(),
            },
            96,
        );

        assert_eq!(
            line_text(&lines[0]),
            "note › Considering installation steps"
        );
        assert_eq!(lines[0].spans[0].style.fg, Some(TEXT_MUTED));
        assert_eq!(line_content_color(&lines[0]), Some(TEXT_MUTED));
    }

    #[test]
    fn markdown_table_renders_columns_within_width() {
        let width = 48;
        let mut lines = Vec::new();
        append_chat_lines(
            &mut lines,
            &ChatLine {
                author: Author::Assistant,
                text: "| Name | Value |\n| --- | --- |\n| foo | bar |".to_string(),
            },
            width,
        );

        let body = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(
            body.contains("Name") && body.contains("foo"),
            "table content should survive rendering: {body}"
        );
        assert!(
            lines.iter().all(|line| line_width(line) <= width),
            "table rows should fit width {width}: {lines:?}"
        );
        assert!(
            lines
                .iter()
                .flat_map(|line| line.spans.iter())
                .any(|span| span.style.fg == Some(TEXT_DIM)),
            "table borders should use dim styling: {lines:?}"
        );
    }

    #[test]
    fn markdown_table_reflows_when_terminal_width_changes() {
        let text = "| Key | Notes |\n| --- | --- |\n| resize | should reflow columns cleanly |"
            .to_string();
        let chat = ChatLine {
            author: Author::Assistant,
            text,
        };

        let mut narrow = Vec::new();
        append_chat_lines(&mut narrow, &chat, 28);
        let mut wide = Vec::new();
        append_chat_lines(&mut wide, &chat, 80);

        assert_ne!(
            narrow.iter().map(line_text).collect::<Vec<_>>(),
            wide.iter().map(line_text).collect::<Vec<_>>(),
            "table layout should change with width"
        );
        for (width, rendered) in [(28, &narrow), (80, &wide)] {
            assert!(
                rendered.iter().all(|line| line_width(line) <= width),
                "table should fit width {width}: {rendered:?}"
            );
        }
    }

    #[test]
    fn markdown_table_inside_code_fence_stays_literal() {
        let mut lines = Vec::new();
        append_chat_lines(
            &mut lines,
            &ChatLine {
                author: Author::Assistant,
                text: "```\n| not | a table |\n| --- | --- |\n```".to_string(),
            },
            60,
        );

        let body = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(
            !body.contains('╭') && !body.contains('╮'),
            "fenced pipe rows should not be rendered as a table: {body}"
        );
        assert!(
            body.contains("| not | a table |"),
            "literal pipes preserved"
        );
    }

    #[test]
    fn markdown_lines_wrap_styled_content_to_available_width() {
        let width = 32;
        let mut lines = Vec::new();
        append_chat_lines(
            &mut lines,
            &ChatLine {
                author: Author::Assistant,
                text: "Use `very-long-command-name` before continuing with the next operation."
                    .to_string(),
            },
            width,
        );

        assert!(
            lines.len() > 1,
            "styled markdown should wrap into multiple rows: {lines:?}"
        );
        assert!(
            lines.iter().all(|line| line_width(line) <= width),
            "all rendered rows should fit width {width}: {lines:?}"
        );
        assert!(
            lines
                .iter()
                .flat_map(|line| line.spans.iter())
                .any(|span| span.style.bg == Some(CODE_BG)),
            "wrapped inline-code spans should keep code styling: {lines:?}"
        );
    }

    #[test]
    fn wrapped_plain_lines_do_not_use_wider_than_view_floor() {
        let width = 14;
        let mut lines = Vec::new();
        append_chat_lines(
            &mut lines,
            &ChatLine {
                author: Author::User,
                text: "supercalifragilistic".to_string(),
            },
            width,
        );

        assert!(
            lines.iter().all(|line| line_width(line) <= width),
            "hard-wrapped rows should fit narrow width {width}: {lines:?}"
        );
    }

    #[test]
    fn rendered_chat_lines_fit_available_width_across_authors() {
        let chats = [
            ChatLine {
                author: Author::User,
                text: "this is a deliberately long user prompt with one-super-long-token".into(),
            },
            ChatLine {
                author: Author::Assistant,
                text: "| Col A | Col B |\n| --- | --- |\n| alpha | beta |".into(),
            },
            ChatLine {
                author: Author::Assistant,
                text: "Use `cargo test --all-features` before resizing the terminal again.".into(),
            },
            ChatLine {
                author: Author::Narration,
                text: "reviewing layout constraints before drawing bottom chrome".into(),
            },
            ChatLine {
                author: Author::Diff,
                text: "+changed line with a very long path /workspace/src/app/render.rs".into(),
            },
            ChatLine {
                author: Author::ToolDetail,
                text: "stdout: output with a long uninterrupted token abcdefghijklmnopqrstuvwxyz"
                    .into(),
            },
        ];

        for width in [12, 16, 24, 40, 80] {
            for chat in &chats {
                let mut lines = Vec::new();
                append_chat_lines(&mut lines, chat, width);
                assert!(
                    lines.iter().all(|line| line_width(line) <= width),
                    "rendered line escaped width {width} for {chat:?}: {lines:?}"
                );
            }
        }
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
    // down what each UI mode (idle / busy / streaming)
    // looks like end-to-end on the screen, without spinning up a runtime.
    // ====================================================================

    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn view_state_idle() -> ViewState {
        ViewState {
            stream_preview: None,
            command_suggestions: Vec::new(),
            busy: false,
            busy_frame: 0,
            turn_activity: None,
            model_id: "gpt-5.5".to_string(),
            provider_name: "openai".to_string(),
            reasoning_effort: Some("medium".to_string()),
            session_id: SessionId::from_seed(770001),
            lines_count: 3,
            session_tokens: None,
            status_layout: StatusLayout::Compact,
            hooks_summary: "none".to_string(),
            approval_mode: "normal".to_string(),
            background: None,
            goal_indicator: None,
        }
    }

    #[test]
    fn status_bar_shows_background_task_count() {
        let state = ViewState {
            status_layout: StatusLayout::Expanded,
            background: Some((1, 2)),
            ..view_state_idle()
        };
        let lines = render_chrome_lines(&state, 120, 8).join("\n");
        assert!(
            lines.contains("bg"),
            "status should show bg segment: {lines}"
        );
        assert!(
            lines.contains("1▸/2"),
            "status should show running/total: {lines}"
        );
    }

    #[test]
    fn status_bar_hides_background_segment_when_no_tasks() {
        let state = ViewState {
            status_layout: StatusLayout::Expanded,
            background: None,
            ..view_state_idle()
        };
        let lines = render_chrome_lines(&state, 120, 8).join("\n");
        assert!(
            !lines.contains("▸"),
            "no background segment expected when there are no tasks: {lines}"
        );
    }

    /// Render `draw_chrome` into a TestBackend and collect the buffer
    /// rows as plain strings (style information dropped). Width and
    /// height are minimums; if the chrome layout would need more space
    /// it will be silently clipped, which is fine for substring asserts.
    fn render_chrome_lines(state: &ViewState, width: u16, height: u16) -> Vec<String> {
        render_chrome_lines_with_input_height(state, width, height, 1)
    }

    fn render_chrome_lines_with_input_height(
        state: &ViewState,
        width: u16,
        height: u16,
        input_height: u16,
    ) -> Vec<String> {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|f| {
                let _input_rect = draw_chrome(f, f.area(), input_height, state);
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

    fn render_app_lines(app: &mut App, width: u16, height: u16) -> Vec<String> {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal.draw(|f| draw(f, app)).expect("draw");
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

    fn setup_overlay_text(app: &App) -> Vec<String> {
        setup_overlay_content(app)
            .0
            .iter()
            .map(|line| spans_plain_text(&line.spans))
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
            None,
            sessions.path().to_path_buf(),
            settings,
            crate::runtime::BuildOptions {
                client_commands: true,
                ..crate::runtime::BuildOptions::default()
            },
        )
        .await
        .expect("build llmsim runtime");
        let mut app = App::new(runtime, vec![]);
        // Never let unit tests hit real provider models APIs.
        app.model_discovery_enabled = false;
        TestApp {
            app,
            _workspace: workspace,
            _sessions: sessions,
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn proactive_wake_starts_turn_when_background_task_finishes() {
        let mut test = app_with_llmsim().await;
        let app = &mut test.app;
        // The setup/onboarding overlay opens when no provider credentials are
        // configured (the case in CI, which has no API keys). Wake is correctly
        // suppressed while it's open, so clear it to exercise the idle path.
        app.setup = None;

        // Idle with no tasks: nothing to wake for.
        assert!(!app.maybe_wake_for_background());

        // Start a quick background script and wait for it to finish.
        let record = app.background.spawn_script(None, "echo hi".to_string());
        for _ in 0..100 {
            if app.background.counts() == (0, 1) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(
            app.background.counts(),
            (0, 1),
            "background script should finish"
        );

        // Idle + a freshly-finished task ⇒ a turn is auto-started.
        assert!(
            app.maybe_wake_for_background(),
            "a finished background task should wake the agent"
        );
        assert!(app.busy, "proactive wake must start a turn");
        assert!(
            app.lines
                .iter()
                .any(|l| l.text.contains(&record.id) && l.text.contains("waking")),
            "a notice explaining the auto-wake should be shown"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn proactive_wake_suppressed_during_setup_overlay() {
        let mut test = app_with_llmsim().await;
        let app = &mut test.app;
        // With the setup overlay open, a finished task must NOT auto-start a turn.
        app.setup = None;
        let record = app.background.spawn_script(None, "echo hi".to_string());
        for _ in 0..100 {
            if app.background.counts() == (0, 1) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        // Re-open the overlay, then confirm wake is suppressed and the task is
        // left undrained (so it can still wake once the overlay closes).
        app.setup = Some(SetupStep::Provider { selected: 0 });
        assert!(!app.maybe_wake_for_background(), "no wake during setup");
        assert!(!app.busy);
        app.setup = None;
        assert!(
            app.maybe_wake_for_background(),
            "wake should fire once the overlay closes — the task wasn't consumed"
        );
        assert!(app.background.get(&record.id).is_some());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn proactive_wake_disabled_by_setting_does_not_start_turn() {
        let mut test = app_with_llmsim().await;
        let app = &mut test.app;
        app.setup = None;
        app.settings
            .set_proactive_wake(false)
            .expect("disable wake");

        let record = app.background.spawn_script(None, "echo hi".to_string());
        for _ in 0..100 {
            if app.background.counts() == (0, 1) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        // Setting off: no turn, but the completion is still surfaced once.
        assert!(!app.maybe_wake_for_background());
        assert!(!app.busy, "wake setting off must not start a turn");
        assert!(
            app.lines.iter().any(|l| l.text.contains(&record.id)),
            "finished task should still be surfaced as a notice"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn background_panel_toggle_scroll_and_close() {
        let mut test = app_with_llmsim().await;
        let app = &mut test.app;
        app.setup = None;
        assert!(app.background_panel.is_none());

        app.toggle_background_panel();
        assert_eq!(app.background_panel, Some(0));

        // With no tasks the body is a single line, so Down can't scroll past it.
        app.handle_background_panel_key(KeyEvent::new(KeyCode::Down, KeyModifiers::empty()));
        assert_eq!(app.background_panel, Some(0));

        app.handle_background_panel_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()));
        assert!(app.background_panel.is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ctrl_b_clears_armed_ctrl_c_exit() {
        let mut test = app_with_llmsim().await;
        let app = &mut test.app;
        app.setup = None;
        app.handle_ctrl_c(); // first Ctrl+C arms the pending exit
        assert!(app.ctrl_c_pending_exit);

        app.handle_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL))
            .await;
        assert!(
            !app.ctrl_c_pending_exit,
            "Ctrl+B must disarm the pending Ctrl+C exit"
        );
        assert_eq!(app.background_panel, Some(0));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn background_panel_not_opened_over_setup_overlay() {
        let mut test = app_with_llmsim().await;
        let app = &mut test.app;
        app.setup = Some(SetupStep::Provider { selected: 0 });
        app.toggle_background_panel();
        assert!(
            app.background_panel.is_none(),
            "panel must not stack on top of the setup overlay"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn background_panel_renders_task_rows() {
        let mut test = app_with_llmsim().await;
        let app = &mut test.app;
        app.setup = None;
        app.background
            .spawn_script(Some("ci".into()), "echo hi".into());
        app.background_panel = Some(0);

        let lines = render_app_lines(app, 120, 20).join("\n");
        assert!(lines.contains("Background tasks"), "panel header: {lines}");
        assert!(
            lines.contains("script"),
            "panel should list the task: {lines}"
        );
    }

    #[test]
    fn background_panel_lines_header_and_scroll() {
        let body = "row-a\nrow-b\nrow-c\nrow-d";
        let lines = render::background_panel_lines(body, 1, 3);
        assert!(lines[0].contains("Background tasks"));
        // height 3 ⇒ 1 header + 2 body rows, starting at offset 1.
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[1], "row-b");
        assert_eq!(lines[2], "row-c");
        // A zero-height rect yields no lines (not even the header).
        assert!(render::background_panel_lines(body, 0, 0).is_empty());
    }

    impl App {
        /// Test-only: dispatch a slash command and pump any `UiCommand`s it
        /// emits, mirroring what the event loop does between frames. Needed
        /// because terminal-side commands now take effect asynchronously via
        /// the host UI channel rather than synchronously inside
        /// `handle_command`.
        async fn dispatch_command_for_test(&mut self, cmd: &str) {
            self.handle_command(cmd).await;
            self.pump_ui_commands_for_test();
        }

        fn pump_ui_commands_for_test(&mut self) {
            while let Ok(command) = self.ui_rx.try_recv() {
                self.apply_ui_command(command);
            }
        }

        /// Drain turn events the way [`App::run_loop_iteration`] does until
        /// the background turn finishes or the deadline passes.
        async fn pump_turn_until_idle_for_test(&mut self) {
            use std::time::Duration;

            let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
            while self.busy {
                if tokio::time::Instant::now() >= deadline {
                    panic!(
                        "turn did not complete within 15s: busy={} lines={:?}",
                        self.busy, self.lines
                    );
                }
                if let Some(rx) = self.rx.as_mut() {
                    match rx.try_recv() {
                        Ok(TurnEvent::Lines(lines)) => self.lines.extend(lines),
                        Ok(TurnEvent::Activity(activity)) => {
                            if !activity.fallback || self.turn_activity.is_none() {
                                self.turn_activity = Some(activity.text);
                            }
                        }
                        Ok(TurnEvent::Stream(preview)) => self.stream_preview = preview,
                        Ok(TurnEvent::Tokens(tokens)) => {
                            self.session_tokens =
                                Some(self.session_tokens.unwrap_or(0).saturating_add(tokens));
                        }
                        Ok(TurnEvent::Done) => {
                            self.finish_busy();
                        }
                        Ok(TurnEvent::Failed(err)) => {
                            self.finish_busy();
                            self.push_system(format!("turn failed: {err}"));
                        }
                        Err(mpsc::error::TryRecvError::Empty) => {}
                        Err(mpsc::error::TryRecvError::Disconnected) => {
                            self.finish_busy();
                        }
                    }
                }
                if self.busy {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            }
        }

        /// Mirror [`App::run`]'s startup replay without standing up a terminal.
        async fn replay_transcript_for_test(&mut self) {
            self.emit_replayed_transcript().await;
        }
    }

    async fn llmsim_settings(
        sessions: &tempfile::TempDir,
    ) -> std::sync::Arc<crate::settings::SettingsStore> {
        let settings_path = sessions.path().join("settings.toml");
        std::fs::write(settings_path, "provider = \"llmsim\"\n").expect("write settings");
        std::sync::Arc::new(crate::settings::SettingsStore::open(
            sessions.path().join("settings.toml"),
        ))
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn enter_submit_completes_llmsim_turn_in_transcript() {
        let mut fixture = app_with_llmsim().await;
        let app = &mut fixture.app;
        app.setup = None;
        app.lines.clear();

        for ch in "hello turn".chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::empty()))
                .await;
        }
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()))
            .await;

        assert!(app.busy, "submit should start a background turn");
        app.pump_turn_until_idle_for_test().await;

        assert!(
            app.lines
                .iter()
                .any(|line| matches!(line.author, Author::User) && line.text == "hello turn"),
            "user prompt should land in the transcript: {:?}",
            app.lines
        );
        assert!(
            app.lines.iter().any(|line| {
                matches!(line.author, Author::Assistant) && line.text.contains("offline mode")
            }),
            "assistant reply should finalize into the transcript: {:?}",
            app.lines
        );
        assert!(!app.busy);
        assert!(app.stream_preview.is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bang_shell_input_runs_shell_from_workspace_without_model_turn() {
        let mut fixture = app_with_llmsim().await;
        let app = &mut fixture.app;
        app.setup = None;
        app.lines.clear();
        app.set_input_text(
            "!shell printf shell-output > shell-marker.txt && cat shell-marker.txt".into(),
        );

        app.submit_input().await;
        app.pump_ui_commands_for_test();

        assert!(
            app.busy,
            "!shell should run as a bounded background command"
        );
        app.pump_turn_until_idle_for_test().await;

        let marker = app.startup.workspace_root.join("shell-marker.txt");
        assert_eq!(
            std::fs::read_to_string(marker).expect("shell marker"),
            "shell-output"
        );
        assert!(
            app.lines.iter().any(|line| {
                matches!(line.author, Author::User)
                    && line
                        .text
                        .starts_with("!shell printf shell-output > shell-marker.txt")
            }),
            "the submitted shell command should be echoed in the transcript: {:?}",
            app.lines
        );
        assert!(
            app.lines
                .iter()
                .any(|line| matches!(line.author, Author::ToolDetail)
                    && line.text.contains("shell-output")),
            "shell stdout should render in the transcript: {:?}",
            app.lines
        );
        assert!(
            !app.lines
                .iter()
                .any(|line| matches!(line.author, Author::Assistant)),
            "!shell should not start a model turn: {:?}",
            app.lines
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bang_input_runs_shell_without_model_turn() {
        let mut fixture = app_with_llmsim().await;
        let app = &mut fixture.app;
        app.setup = None;
        app.lines.clear();
        app.set_input_text("!printf bare-output".into());

        app.submit_input().await;
        app.pump_ui_commands_for_test();

        assert!(app.busy, "! should run as a bounded background command");
        app.pump_turn_until_idle_for_test().await;

        assert!(
            app.lines
                .iter()
                .any(|line| matches!(line.author, Author::ToolDetail)
                    && line.text.contains("bare-output")),
            "shell stdout should render in the transcript: {:?}",
            app.lines
        );
        assert!(
            !app.lines
                .iter()
                .any(|line| matches!(line.author, Author::Assistant)),
            "! should not start a model turn: {:?}",
            app.lines
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn resume_replays_prior_turn_into_transcript() {
        let workspace = tempfile::tempdir().expect("workspace tempdir");
        let sessions = tempfile::tempdir().expect("sessions tempdir");
        let settings = llmsim_settings(&sessions).await;

        let first = crate::runtime::build_with_options(
            workspace.path().to_path_buf(),
            crate::runtime::ProviderChoice::Sim,
            None,
            sessions.path().to_path_buf(),
            settings.clone(),
            crate::runtime::BuildOptions {
                client_commands: true,
                ..crate::runtime::BuildOptions::default()
            },
        )
        .await
        .expect("build first runtime");
        let session_id = first.handles.session_id;
        let prompt = "prior turn";
        let input = first.model.input_message(prompt.to_string());
        first
            .handles
            .runtime
            .run_turn(session_id, input)
            .await
            .expect("first turn");
        drop(first);

        let mut resumed = None;
        for _ in 0..20 {
            match crate::runtime::build_with_options(
                workspace.path().to_path_buf(),
                crate::runtime::ProviderChoice::Sim,
                Some(session_id),
                sessions.path().to_path_buf(),
                settings.clone(),
                crate::runtime::BuildOptions {
                    client_commands: true,
                    ..crate::runtime::BuildOptions::default()
                },
            )
            .await
            {
                Ok(runtime) => {
                    resumed = Some(runtime);
                    break;
                }
                Err(err) if err.to_string().contains("another yolop process") => {
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
                Err(err) => panic!("build resumed runtime: {err}"),
            }
        }
        let resumed = resumed.expect("build resumed runtime after releasing first log lock");
        assert!(
            resumed.startup.replayed_events > 0,
            "resume should report replayed events"
        );

        let mut app = App::new(resumed, vec![]);
        app.setup = None;
        app.lines.clear();
        app.replay_transcript_for_test().await;

        assert!(
            app.lines
                .iter()
                .any(|line| matches!(line.author, Author::User) && line.text == prompt),
            "replayed transcript should include the prior user prompt: {:?}",
            app.lines
        );
        assert!(
            app.lines.iter().any(|line| {
                matches!(line.author, Author::Assistant) && line.text.contains("offline mode")
            }),
            "replayed transcript should include the prior assistant reply: {:?}",
            app.lines
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn startup_banner_names_ctrl_c_and_ctrl_d_as_exit_keys() {
        let fixture = app_with_llmsim().await;

        assert!(
            fixture
                .app
                .lines
                .iter()
                .any(|line| line.text.contains("Ctrl+V to paste an image")),
            "startup banner should mention image paste: {:?}",
            fixture.app.lines
        );
        assert!(
            fixture
                .app
                .lines
                .iter()
                .any(|line| line.text.contains("press Ctrl-C twice (or Ctrl-D) to exit")),
            "startup banner should name Ctrl-C/Ctrl-D exits: {:?}",
            fixture.app.lines
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn large_paste_attaches_placeholder_and_expands_on_submit() {
        use crate::paste_attachment::LARGE_PASTE_CHAR_THRESHOLD;

        let mut fixture = app_with_llmsim().await;
        let app = &mut fixture.app;
        app.setup = None;
        app.lines.clear();

        let payload = format!("paste-marker-{}", "x".repeat(LARGE_PASTE_CHAR_THRESHOLD));
        app.handle_paste(payload.clone());
        let placeholder = app.input_text();
        assert!(
            placeholder.contains("[Pasted Content"),
            "large paste should insert a placeholder: {placeholder:?}"
        );
        assert_eq!(app.pending_pastes.len(), 1);

        app.submit_input().await;

        assert!(
            app.busy,
            "submit should start a turn with the expanded paste"
        );
        assert!(
            app.lines.iter().any(|line| {
                matches!(line.author, Author::User) && line.text.contains("[Pasted Content")
            }),
            "transcript should show the compact placeholder: {:?}",
            app.lines
        );
        assert!(
            !app.lines.iter().any(|line| {
                matches!(line.author, Author::User) && line.text.contains("paste-marker-")
            }),
            "transcript should not inline the full pasted payload: {:?}",
            app.lines
        );
        assert!(app.pending_pastes.is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn startup_banner_is_kept_out_of_inline_viewport() {
        let mut fixture = app_with_llmsim().await;
        let rows = render_app_lines(&mut fixture.app, 96, COMPOSER_VIEWPORT_HEIGHT);

        assert!(
            !rows.iter().any(|row| row.contains("workspace:")),
            "startup transcript should render through scrollback, not the inline viewport: {rows:?}"
        );
        assert!(
            !rows.iter().any(|row| row.contains("type /help")),
            "startup transcript should not be mirrored above the composer: {rows:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn inline_viewport_shows_recent_transcript_lines() {
        let mut fixture = app_with_llmsim().await;
        let app = &mut fixture.app;
        app.setup = None;
        app.lines.clear();
        app.push_user("What changed last time?".into());
        app.lines.push(ChatLine {
            author: Author::Assistant,
            text: "The renderer now mirrors resumed history.".into(),
        });

        let rows = render_app_lines(app, 96, COMPOSER_VIEWPORT_HEIGHT);

        assert!(
            rows.iter()
                .any(|row| row.contains("What changed last time?")),
            "inline viewport should show recent user transcript: {rows:?}"
        );
        assert!(
            rows.iter()
                .any(|row| row.contains("mirrors resumed history")),
            "inline viewport should show recent assistant transcript: {rows:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn inline_viewport_bottom_aligns_recent_transcript_tail() {
        let mut fixture = app_with_llmsim().await;
        let app = &mut fixture.app;
        app.setup = None;
        app.lines.clear();
        app.lines.push(ChatLine {
            author: Author::Assistant,
            text: "Latest answer tail".into(),
        });

        let rows = render_app_lines(app, 96, COMPOSER_VIEWPORT_HEIGHT);
        let answer_row = rows
            .iter()
            .position(|row| row.contains("Latest answer tail"))
            .expect("recent answer rendered");
        let separator_row = rows
            .iter()
            .position(|row| row.contains("Enter to send"))
            .expect("message separator rendered");

        assert_eq!(
            answer_row + 1,
            separator_row,
            "recent answer should sit above the input chrome without a large blank gap: {rows:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn inline_viewport_does_not_mirror_flushed_transcript_lines() {
        let mut fixture = app_with_llmsim().await;
        let app = &mut fixture.app;
        app.setup = None;
        app.lines.clear();
        app.push_user("Do something".into());
        app.lines.push(ChatLine {
            author: Author::Assistant,
            text: "Done.".into(),
        });
        app.printed_lines = app.lines.len();

        let rows = render_app_lines(app, 96, COMPOSER_VIEWPORT_HEIGHT);

        assert!(
            !rows.iter().any(|row| row.contains("Do something")),
            "flushed user transcript should stay in scrollback only: {rows:?}"
        );
        assert!(
            !rows.iter().any(|row| row.contains("Done.")),
            "flushed assistant transcript should stay in scrollback only: {rows:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn inline_viewport_uses_recent_transcript_tail() {
        let mut fixture = app_with_llmsim().await;
        let app = &mut fixture.app;
        app.setup = None;
        app.lines.clear();
        for index in 0..20 {
            app.push_user(format!("old line {index}"));
        }
        app.lines.push(ChatLine {
            author: Author::Assistant,
            text: "newest resumed line".into(),
        });

        let rows = render_app_lines(app, 96, COMPOSER_VIEWPORT_HEIGHT);

        assert!(
            !rows.iter().any(|row| row.contains("old line 0")),
            "inline viewport should drop old transcript head: {rows:?}"
        );
        assert!(
            rows.iter().any(|row| row.contains("newest resumed line")),
            "inline viewport should keep transcript tail: {rows:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn inline_viewport_bounds_large_recent_entries() {
        let bounded = bounded_recent_chat_line(&ChatLine {
            author: Author::Assistant,
            text: format!(
                "{} visible-tail",
                "hidden-head ".repeat(RECENT_TRANSCRIPT_MAX_TEXT_BYTES)
            ),
        });

        assert!(
            bounded.text.len() <= RECENT_TRANSCRIPT_MAX_TEXT_BYTES,
            "bounded text should fit the inline render budget"
        );
        assert!(bounded.text.starts_with('…'), "bounded text: {bounded:?}");
        assert!(
            bounded.text.ends_with("visible-tail"),
            "recent transcript should keep the tail: {bounded:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn inline_viewport_stops_rendering_after_visible_tail_is_full() {
        let mut fixture = app_with_llmsim().await;
        let app = &mut fixture.app;
        app.setup = None;
        app.lines.clear();
        app.lines.push(ChatLine {
            author: Author::Assistant,
            text: "older invisible line".repeat(200),
        });
        for index in 0..20 {
            app.push_user(format!("new line {index}"));
        }

        let rows = render_app_lines(app, 96, COMPOSER_VIEWPORT_HEIGHT);

        assert!(
            !rows.iter().any(|row| row.contains("older invisible")),
            "inline viewport should avoid rendering invisible older entries: {rows:?}"
        );
        assert!(
            rows.iter().any(|row| row.contains("new line 19")),
            "inline viewport should keep newest entries: {rows:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn resumed_session_renders_replayed_history_in_inline_viewport() {
        let workspace = tempfile::tempdir().expect("workspace tempdir");
        let sessions = tempfile::tempdir().expect("sessions tempdir");
        let session_id = SessionId::from_seed(321987);
        let session_dir = crate::session_log::session_dir_path(sessions.path(), session_id);
        std::fs::create_dir_all(&session_dir).expect("session dir");
        let log_path = crate::session_log::session_log_path(&session_dir);
        let events = [
            RuntimeEvent::new(
                session_id,
                EventContext::empty(),
                InputMessageData::new(Message::user("previous question")),
            ),
            RuntimeEvent::new(
                session_id,
                EventContext::empty(),
                OutputMessageCompletedData::new(Message::assistant("previous answer")),
            ),
        ];
        let jsonl = events
            .iter()
            .map(|event| serde_json::to_string(event).expect("serialize event"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&log_path, format!("{jsonl}\n")).expect("session log");

        let settings = std::sync::Arc::new(crate::settings::SettingsStore::open(
            sessions.path().join("settings.toml"),
        ));
        let runtime = crate::runtime::build_with_options(
            workspace.path().to_path_buf(),
            crate::runtime::ProviderChoice::Sim,
            Some(session_id),
            sessions.path().to_path_buf(),
            settings,
            crate::runtime::BuildOptions {
                client_commands: true,
                ..crate::runtime::BuildOptions::default()
            },
        )
        .await
        .expect("build resumed runtime");
        let mut app = App::new(runtime, vec![]);
        app.setup = None;

        app.emit_replayed_transcript().await;
        let rows = render_app_lines(&mut app, 96, COMPOSER_VIEWPORT_HEIGHT);

        assert!(
            rows.iter().any(|row| row.contains("previous question")),
            "inline viewport should show replayed user message: {rows:?}"
        );
        assert!(
            rows.iter().any(|row| row.contains("previous answer")),
            "inline viewport should show replayed assistant message: {rows:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn help_command_lists_commands_shortcuts_and_exit_keys() {
        let mut fixture = app_with_llmsim().await;
        let app = &mut fixture.app;
        app.lines.clear();

        app.dispatch_command_for_test("help").await;

        let help_lines: Vec<_> = app.lines.iter().map(|line| line.text.as_str()).collect();
        assert!(
            help_lines.contains(&"commands:"),
            "help should introduce the command list: {help_lines:?}"
        );
        assert!(
            help_lines.iter().any(|line| line.starts_with("  /help —")),
            "help should list /help with a description: {help_lines:?}"
        );
        assert!(
            help_lines.contains(&"shortcuts:"),
            "help should introduce keyboard shortcuts: {help_lines:?}"
        );
        assert!(
            help_lines
                .iter()
                .any(|line| line.contains("exit: Ctrl-C twice / Ctrl-D")),
            "help output should name Ctrl-C/Ctrl-D exits: {help_lines:?}"
        );
        assert!(
            help_lines.iter().any(|line| line.contains("cancel turn")),
            "help output should label Esc as cancel turn: {help_lines:?}"
        );
        assert!(
            help_lines.iter().any(|line| line.contains("/exit")),
            "help output should mention /exit alias for /quit: {help_lines:?}"
        );
        assert!(
            help_lines
                .iter()
                .any(|line| line.contains("Ctrl+V paste image/text")),
            "help output should mention image paste: {help_lines:?}"
        );
        assert!(
            help_lines.iter().any(|line| line.contains("/yolop skill")),
            "help output should point at the yolop skill: {help_lines:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn first_ctrl_c_prompts_for_second_press_to_exit() {
        let mut fixture = app_with_llmsim().await;
        let app = &mut fixture.app;
        app.setup = None;

        app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
            .await;

        assert!(!app.should_quit, "first Ctrl-C should not quit immediately");
        assert!(app.ctrl_c_pending_exit, "first Ctrl-C should arm exit");
        assert!(
            app.lines
                .iter()
                .any(|line| { line.text.contains("Press Ctrl+C again to exit") }),
            "first Ctrl-C should invite a second press: {:?}",
            app.lines
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn second_ctrl_c_exits_after_prompt() {
        let mut fixture = app_with_llmsim().await;
        let app = &mut fixture.app;
        app.setup = None;
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);

        app.handle_key(ctrl_c).await;
        app.handle_key(ctrl_c).await;

        assert!(app.should_quit, "second Ctrl-C should quit");
        assert!(app.ctrl_c_exit, "second Ctrl-C should count as Ctrl-C exit");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ctrl_c_clears_nonempty_input_without_exiting() {
        let mut fixture = app_with_llmsim().await;
        let app = &mut fixture.app;
        app.setup = None;
        app.set_input_text("draft prompt".into());

        app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
            .await;

        assert!(!app.should_quit, "Ctrl-C with draft input should not quit");
        assert!(
            app.input_text().trim().is_empty(),
            "Ctrl-C should clear draft input"
        );
        assert!(
            !app.ctrl_c_pending_exit,
            "clearing input should not arm exit"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn typing_after_first_ctrl_c_disarms_exit_prompt() {
        let mut fixture = app_with_llmsim().await;
        let app = &mut fixture.app;
        app.setup = None;

        app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
            .await;
        app.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::empty()))
            .await;

        assert!(
            !app.ctrl_c_pending_exit,
            "typing should disarm the pending exit prompt"
        );
        assert!(!app.should_quit);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn first_esc_while_busy_prompts_for_second_press_to_cancel() {
        let mut fixture = app_with_llmsim().await;
        let app = &mut fixture.app;
        let (cancel_tx, mut cancel_rx) = oneshot::channel::<()>();
        app.setup = None;
        app.busy = true;
        app.turn_cancel = Some(cancel_tx);

        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()))
            .await;

        assert!(!app.should_quit, "first Esc should not quit");
        assert!(app.busy, "first Esc should keep the turn running");
        assert!(
            app.esc_pending_cancel,
            "first Esc should arm turn cancellation"
        );
        assert!(
            cancel_rx.try_recv().is_err(),
            "first Esc should not send cancellation yet"
        );
        assert!(
            app.lines
                .iter()
                .any(|line| line.text.contains("Press Esc again to cancel current turn")),
            "first Esc should invite a second press: {:?}",
            app.lines
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn second_esc_while_busy_sends_turn_cancel() {
        let mut fixture = app_with_llmsim().await;
        let app = &mut fixture.app;
        let (cancel_tx, cancel_rx) = oneshot::channel::<()>();
        app.setup = None;
        app.busy = true;
        app.turn_cancel = Some(cancel_tx);

        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()))
            .await;
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()))
            .await;

        assert!(
            !app.esc_pending_cancel,
            "second Esc should disarm cancellation prompt"
        );
        assert!(
            app.turn_cancel.is_none(),
            "second Esc should consume the active cancel sender"
        );
        assert!(
            cancel_rx.await.is_ok(),
            "second Esc should notify the turn worker"
        );
        assert_eq!(app.turn_activity.as_deref(), Some("cancelling"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn second_esc_while_goal_active_pauses_goal_continuation() {
        let mut fixture = app_with_llmsim().await;
        let app = &mut fixture.app;
        let session_id = app.session.session_id();
        let (cancel_tx, cancel_rx) = oneshot::channel::<()>();
        app.setup = None;
        app.goal_store
            .set_active(session_id, "ship the change".into())
            .expect("set active goal");
        assert!(
            app.goal_store.take_pending_turn(session_id),
            "test starts from an in-progress goal turn"
        );
        app.busy = true;
        app.turn_cancel = Some(cancel_tx);

        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()))
            .await;
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()))
            .await;

        assert!(cancel_rx.await.is_ok(), "second Esc should cancel the turn");
        assert!(
            app.goal_store.is_paused(session_id),
            "cancelled goal turn should pause auto-continuation"
        );
        assert!(
            !app.goal_store.take_pending_turn(session_id),
            "paused goal should not keep a pending continuation"
        );
        assert!(
            app.lines
                .iter()
                .any(|line| line.text.contains("goal paused")),
            "cancellation should explain how to resume the goal: {:?}",
            app.lines
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn second_esc_while_busy_clears_stream_preview_immediately() {
        let mut fixture = app_with_llmsim().await;
        let app = &mut fixture.app;
        let (cancel_tx, _cancel_rx) = oneshot::channel::<()>();
        app.setup = None;
        app.busy = true;
        app.turn_cancel = Some(cancel_tx);
        app.stream_preview = Some(StreamPreview {
            kind: StreamKind::Assistant,
            text: "partial answer".into(),
        });

        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()))
            .await;
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()))
            .await;

        assert!(
            app.stream_preview.is_none(),
            "cancellation confirmation should clear stale streaming text immediately"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn esc_after_cancel_requested_does_not_reprompt() {
        let mut fixture = app_with_llmsim().await;
        let app = &mut fixture.app;
        let (cancel_tx, _cancel_rx) = oneshot::channel::<()>();
        app.setup = None;
        app.busy = true;
        app.turn_cancel = Some(cancel_tx);

        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()))
            .await;
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()))
            .await;
        let prompt_count = app
            .lines
            .iter()
            .filter(|line| line.text.contains("Press Esc again to cancel current turn"))
            .count();

        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()))
            .await;

        assert!(
            !app.esc_pending_cancel,
            "Esc after cancellation was requested should not re-arm"
        );
        assert_eq!(
            app.lines
                .iter()
                .filter(|line| line.text.contains("Press Esc again to cancel current turn"))
                .count(),
            prompt_count,
            "Esc after cancellation was requested should not add another prompt"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn non_esc_while_busy_disarms_cancel_prompt() {
        let mut fixture = app_with_llmsim().await;
        let app = &mut fixture.app;
        let (cancel_tx, mut cancel_rx) = oneshot::channel::<()>();
        app.setup = None;
        app.busy = true;
        app.turn_cancel = Some(cancel_tx);

        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()))
            .await;
        app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::empty()))
            .await;

        assert!(
            !app.esc_pending_cancel,
            "non-Esc busy input should disarm cancellation prompt"
        );
        assert!(
            cancel_rx.try_recv().is_err(),
            "non-Esc busy input should not cancel the turn"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cwd_command_prints_workspace_root() {
        let mut fixture = app_with_llmsim().await;
        let app = &mut fixture.app;
        app.lines.clear();

        app.dispatch_command_for_test("cwd").await;

        let root = app.startup.workspace_root.display().to_string();
        assert!(
            app.lines
                .iter()
                .any(|line| line.text.contains("workspace root:") && line.text.contains(&root)),
            "cwd should print the workspace root: {:?}",
            app.lines
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn status_command_switches_between_compact_and_expanded_layouts() {
        let mut fixture = app_with_llmsim().await;
        let app = &mut fixture.app;
        app.lines.clear();

        assert_eq!(app.status_layout, StatusLayout::Compact);
        app.dispatch_command_for_test("status expanded").await;
        assert_eq!(app.status_layout, StatusLayout::Expanded);

        app.dispatch_command_for_test("status compact").await;
        assert_eq!(app.status_layout, StatusLayout::Compact);

        app.dispatch_command_for_test("status nonsense").await;
        assert_eq!(app.status_layout, StatusLayout::Compact);
        assert!(
            app.lines
                .iter()
                .any(|line| line.text.contains("usage: /status")),
            "invalid status layout should render usage: {:?}",
            app.lines
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn clicking_status_rows_toggles_layout() {
        let mut fixture = app_with_llmsim().await;
        let app = &mut fixture.app;
        app.setup = None;

        let area = Rect {
            x: 0,
            y: 10,
            width: 120,
            height: 24,
        };

        assert_eq!(app.status_layout, StatusLayout::Compact);
        assert!(app.handle_mouse(
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 3,
                row: 33,
                modifiers: KeyModifiers::empty(),
            },
            area,
        ));
        assert_eq!(app.status_layout, StatusLayout::Expanded);

        assert!(app.handle_mouse(
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 3,
                row: 31,
                modifiers: KeyModifiers::empty(),
            },
            area,
        ));
        assert_eq!(app.status_layout, StatusLayout::Compact);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tools_command_lists_available_tools() {
        let mut fixture = app_with_llmsim().await;
        let app = &mut fixture.app;
        app.lines.clear();

        app.dispatch_command_for_test("tools").await;

        // The llmsim runtime registers the standard coding toolset, so the
        // listing must be non-empty and name a known tool.
        assert!(
            app.lines
                .iter()
                .any(|line| line.text.starts_with("tools:") && line.text.contains("bash")),
            "tools should list available tools: {:?}",
            app.lines
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn clear_command_wipes_transcript_and_re_emits_banner() {
        let mut fixture = app_with_llmsim().await;
        let app = &mut fixture.app;
        app.push_system("sentinel line that must be cleared".into());
        // Advance the print cursor so the reset assertion below actually
        // guards the behavior rather than passing on the initial value.
        app.printed_lines = app.lines.len();
        assert_ne!(app.printed_lines, 0);

        app.dispatch_command_for_test("clear").await;

        assert!(
            !app.lines
                .iter()
                .any(|line| line.text.contains("sentinel line")),
            "clear should wipe prior transcript lines: {:?}",
            app.lines
        );
        assert_eq!(app.printed_lines, 0, "clear should reset the print cursor");
        // The banner is re-emitted so the cleared screen still shows context.
        assert!(
            app.lines
                .iter()
                .any(|line| line.text.contains("type /help")),
            "clear should re-emit the startup banner: {:?}",
            app.lines
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn quit_command_and_exit_alias_request_shutdown() {
        for command in ["quit", "exit"] {
            let mut fixture = app_with_llmsim().await;
            let app = &mut fixture.app;
            assert!(!app.should_quit);

            app.dispatch_command_for_test(command).await;

            assert!(
                app.should_quit,
                "/{command} should request shutdown via the UI channel"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn app_slash_input_renders_command_suggestions_end_to_end() {
        let mut fixture = app_with_llmsim().await;
        let app = &mut fixture.app;
        app.setup = None;

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
    async fn shift_enter_inserts_newline_without_submitting() {
        let mut fixture = app_with_llmsim().await;
        let app = &mut fixture.app;
        app.setup = None;

        for key in [
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::empty()),
            KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT),
            KeyEvent::new(KeyCode::Char('b'), KeyModifiers::empty()),
            KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT),
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::empty()),
        ] {
            app.handle_key(key).await;
        }

        assert_eq!(app.input_text(), "a\nb\nc");
        assert_eq!(app.input_height(80), 3);
        assert!(
            app.lines
                .iter()
                .all(|line| !matches!(line.author, Author::User)),
            "Shift-Enter should edit the composer, not submit: {:?}",
            app.lines
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn alt_shift_enter_submits_instead_of_inserting_newline() {
        let mut fixture = app_with_llmsim().await;
        let app = &mut fixture.app;
        app.setup = None;

        app.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::empty()))
            .await;
        app.handle_key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::ALT | KeyModifiers::SHIFT,
        ))
        .await;

        assert_eq!(app.input_text(), "");
        assert!(
            app.lines
                .iter()
                .any(|line| matches!(line.author, Author::User) && line.text == "a"),
            "Alt-Shift-Enter should submit the composer: {:?}",
            app.lines
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shifted_printable_chars_insert_literal_character() {
        let mut fixture = app_with_llmsim().await;
        let app = &mut fixture.app;
        app.setup = None;

        for key in [
            KeyEvent::new(KeyCode::Char('/'), KeyModifiers::SHIFT),
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::SHIFT),
            KeyEvent::new(KeyCode::Char('1'), KeyModifiers::SHIFT),
        ] {
            app.handle_key(key).await;
        }

        assert_eq!(app.input_text(), "?A!");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn multiline_input_height_is_bounded() {
        let mut fixture = app_with_llmsim().await;
        let app = &mut fixture.app;

        assert_eq!(app.input_height(80), 1);
        for expected in 2..=MAX_INPUT_HEIGHT {
            app.input.insert_newline();
            assert_eq!(app.input_height(80), expected);
        }
        app.input.insert_newline();
        assert_eq!(app.input_height(80), MAX_INPUT_HEIGHT);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn wrapped_single_line_input_grows_height_with_narrow_width() {
        let mut fixture = app_with_llmsim().await;
        let app = &mut fixture.app;
        app.setup = None;

        for word in ["hello", "world", "again", "here"] {
            for ch in word.chars() {
                app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::empty()))
                    .await;
            }
            app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::empty()))
                .await;
        }

        assert_eq!(
            app.input.lines().len(),
            1,
            "composer stays one logical line"
        );
        let input_width = 10;
        let measured = app.input_height(input_width) as usize;
        assert!(
            measured >= 2,
            "soft-wrapped composer should grow past one row (got {measured})"
        );

        let mut textarea = new_input_area(vec![app.input_text()]);
        let area = Rect {
            x: 0,
            y: 0,
            width: input_width,
            height: MAX_INPUT_HEIGHT,
        };
        let mut buf = Buffer::empty(area);
        Widget::render(&textarea, area, &mut buf);
        textarea.move_cursor(CursorMove::End);
        let expected = textarea.screen_cursor().row as usize + 1;
        assert_eq!(
            measured, expected,
            "composer height should match textarea wrap layout"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn wrapped_input_allocates_multiple_render_rows() {
        let mut fixture = app_with_llmsim().await;
        let app = &mut fixture.app;
        app.setup = None;
        app.set_input_text("alpha beta gamma delta epsilon zeta".into());

        let terminal_width: u16 = 16;
        let input_width = terminal_width.saturating_sub(2);
        let input_height = app.input_height(input_width);
        assert!(
            input_height >= 2,
            "narrow composer should reserve multiple input rows (got {input_height})"
        );

        let rows = render_app_lines(app, terminal_width, COMPOSER_VIEWPORT_HEIGHT);
        let input_rows: Vec<_> = rows
            .iter()
            .filter(|row| row.contains("alpha") || row.contains("beta") || row.contains("gamma"))
            .collect();
        assert!(
            input_rows.len() >= 2,
            "wrapped composer text should appear on multiple screen rows: {rows:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn setup_command_starts_guided_wizard() {
        let mut fixture = app_with_llmsim().await;
        let app = &mut fixture.app;
        app.setup = None;
        app.lines.clear();

        app.handle_command("setup").await;

        let llmsim_index = PROVIDER_OPTIONS
            .iter()
            .position(|option| option.name == "llmsim")
            .expect("llmsim provider option");
        assert!(matches!(
            app.setup,
            Some(SetupStep::Provider { selected }) if selected == llmsim_index
        ));
        assert!(
            app.lines.is_empty(),
            "plain /setup should open the overlay without transcript chatter: {:?}",
            app.lines
        );
        let rendered = setup_overlay_text(app);
        assert!(rendered.iter().any(|line| line.contains("Set Up Yolop")));
        assert!(rendered.iter().any(|line| line.contains("OpenAI")));
        assert!(rendered.iter().any(|line| line.contains("Offline demo")));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn setup_overlay_renders_full_provider_picker() {
        let mut fixture = app_with_llmsim().await;
        let app = &mut fixture.app;
        app.lines.clear();
        app.setup = Some(SetupStep::Provider { selected: 0 });

        let rows = render_app_lines(app, 110, COMPOSER_VIEWPORT_HEIGHT);

        assert!(
            rows.iter().any(|line| line.contains("Set Up Yolop")),
            "setup title should be visible: {rows:?}"
        );
        assert!(
            rows.iter().any(|line| line.contains("OpenAI")),
            "provider choices should be visible: {rows:?}"
        );
        assert!(
            !rows.iter().any(|line| line.contains("recommended")),
            "setup should not recommend a specific provider: {rows:?}"
        );
        assert!(
            rows.iter().any(|line| line.contains("Offline demo mode")),
            "last provider choice should not be clipped: {rows:?}"
        );
        assert!(
            rows.iter().any(|line| line.contains("Esc cancel")),
            "footer should not be clipped: {rows:?}"
        );
    }

    // Holding the env lock across awaits is deliberate: the overlay reads
    // env vars throughout the test, so releasing early would let another
    // env-mutating test change them mid-assertion. The guard owner always
    // makes progress, so this cannot deadlock.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn setup_provider_picker_enters_credential_panel() {
        // Serialize against other env-mutating tests; a present
        // OPENAI_API_KEY would make the provider "connected" and skip the
        // credential panel entirely.
        let _guard = crate::test_env::lock();
        unsafe {
            std::env::remove_var("OPENAI_API_KEY");
        }
        let mut fixture = app_with_llmsim().await;
        let app = &mut fixture.app;
        app.lines.clear();
        app.setup = Some(SetupStep::Provider { selected: 0 });

        app.handle_setup_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()))
            .await;

        assert!(matches!(
            app.setup,
            Some(SetupStep::Credential {
                ref provider,
                selected: 0,
                ..
            }) if provider == "openai"
        ));
        let rendered = setup_overlay_text(app);
        assert!(
            rendered
                .iter()
                .any(|line| line.contains("API Key for OpenAI"))
        );
        assert!(
            rendered
                .iter()
                .any(|line| line.contains("Use OPENAI_API_KEY from environment"))
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn setup_row_keeps_a_gap_when_label_overflows_column() {
        // "Use OPENAI_API_KEY from environment" overflows the 28-col label
        // column; the hint must not butt against it ("environmentnot detected").
        let mut fixture = app_with_llmsim().await;
        let app = &mut fixture.app;
        app.setup = Some(SetupStep::Credential {
            provider: "openai".to_string(),
            selected: 0,
            error: None,
        });

        let rendered = setup_overlay_text(app);
        let env_row = rendered
            .iter()
            .find(|line| line.contains("from environment"))
            .expect("credential panel should render the use-env row");
        assert!(
            env_row.contains("environment  "),
            "hint must stay separated from the overflowing label: {env_row:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn setup_token_input_masks_secret_and_moves_to_model_picker() {
        let mut fixture = app_with_llmsim().await;
        let app = &mut fixture.app;
        app.lines.clear();
        app.setup = Some(SetupStep::TokenInput {
            provider: "openai".to_string(),
            token: String::new(),
            error: None,
        });

        for ch in "test-token".chars() {
            app.handle_setup_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::empty()))
                .await;
        }
        let rendered = setup_overlay_text(app);
        assert!(
            !rendered.iter().any(|line| line.contains("test-token")),
            "raw token should never render: {rendered:?}"
        );
        assert!(
            rendered.iter().any(|line| line.contains("••••••••••")),
            "masked token should render: {rendered:?}"
        );
        app.handle_setup_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()))
            .await;

        assert!(matches!(
            app.setup,
            Some(SetupStep::PickModel {
                ref provider,
                ..
            }) if provider == "openai"
        ));
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
        let llmsim_index = PROVIDER_OPTIONS
            .iter()
            .position(|option| option.name == "llmsim")
            .expect("llmsim provider option");
        app.setup = Some(SetupStep::Provider {
            selected: llmsim_index,
        });

        app.handle_setup_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()))
            .await;

        assert!(app.setup.is_none());
        assert_eq!(app.model.provider_label(), "llmsim/llmsim-yolop");
        assert!(
            app.lines
                .iter()
                .any(|line| line.text == "setup complete: offline demo mode")
        );
        assert!(
            !app.lines
                .iter()
                .any(|line| line.text.starts_with("setup provider changed:")),
            "wizard should hide internal setup command success output"
        );
    }

    // See setup_provider_picker_enters_credential_panel for why the env
    // lock is held across awaits.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn setup_provider_picker_shows_connection_status() {
        let _guard = crate::test_env::lock();
        unsafe {
            std::env::remove_var("OPENAI_API_KEY");
            std::env::remove_var("CUSTOM_BASE_URL");
        }
        let mut fixture = app_with_llmsim().await;
        let app = &mut fixture.app;
        app.setup = Some(SetupStep::Provider { selected: 0 });

        let rendered = setup_overlay_text(app);
        let openai_row = rendered
            .iter()
            .find(|line| line.contains("OpenAI") && !line.contains("compatible"))
            .expect("openai row");
        assert!(
            openai_row.contains("needs API key"),
            "unconnected provider should say so: {openai_row:?}"
        );
        let custom_row = rendered
            .iter()
            .find(|line| line.contains("Custom endpoint"))
            .expect("custom row");
        assert!(
            custom_row.contains("needs base URL"),
            "custom without URL should say so: {custom_row:?}"
        );

        app.settings
            .set_token("openai".to_string(), "sk-test".to_string())
            .expect("save token");
        let rendered = setup_overlay_text(app);
        let openai_row = rendered
            .iter()
            .find(|line| line.contains("OpenAI") && !line.contains("compatible"))
            .expect("openai row");
        assert!(
            openai_row.contains("✓ saved key"),
            "saved key should mark the provider connected: {openai_row:?}"
        );
    }

    fn discovered(model_id: &str, display_name: Option<&str>) -> DiscoveredProviderModel {
        DiscoveredProviderModel {
            model_id: model_id.to_string(),
            display_name: display_name.map(str::to_string),
            description: None,
        }
    }

    #[test]
    fn model_window_centers_selection_in_long_lists() {
        assert_eq!(model_window(0, 5, 8), (0, 5));
        assert_eq!(model_window(0, 300, 8), (0, 8));
        assert_eq!(model_window(150, 300, 8), (146, 154));
        assert_eq!(model_window(299, 300, 8), (292, 300));
    }

    #[test]
    fn discovered_models_convert_to_options_with_custom_escape_hatch() {
        let mut described = discovered("openai/gpt-5.5", Some("OpenAI: GPT-5.5"));
        described.description = Some("frontier model for complex coding".to_string());
        let catalog = model_options_from_discovered(
            "openrouter",
            vec![
                described,
                discovered("nvidia/nemotron-3-super-120b-a12b", None),
            ],
            2,
        );

        assert_eq!(catalog.options.len(), 3);
        assert_eq!(catalog.recommended_count, 2);
        assert_eq!(catalog.options[0].spec.as_deref(), Some("openai/gpt-5.5"));
        assert_eq!(catalog.options[0].label, "openai/gpt-5.5");
        assert_eq!(
            catalog.options[0].hint,
            "OpenAI: GPT-5.5 · frontier model for complex coding"
        );
        assert_eq!(
            catalog.options[1].spec.as_deref(),
            Some("nvidia/nemotron-3-super-120b-a12b")
        );
        assert!(
            catalog.options[2].spec.is_none(),
            "last option must stay Custom..."
        );
    }

    #[test]
    fn discovered_model_hints_are_truncated_for_one_row_display() {
        let mut model = discovered("verbose/model", None);
        model.description = Some("x".repeat(200));

        let catalog = model_options_from_discovered("openrouter", vec![model], 1);

        assert!(
            catalog.options[0].hint.chars().count() <= 72,
            "hint must fit one picker row: {} chars",
            catalog.options[0].hint.chars().count()
        );
        assert!(catalog.options[0].hint.ends_with('…'));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn openrouter_model_picker_shows_recommended_divider() {
        let mut fixture = app_with_llmsim().await;
        let app = &mut fixture.app;
        app.setup = Some(SetupStep::PickModel {
            provider: "openrouter".to_string(),
            selected: 0,
            custom: None,
            error: None,
        });

        let ranked = crate::capabilities::model_ranking::rank_discovered_models(
            "openrouter",
            vec![
                discovered("zai/glm-5", None),
                discovered("openai/gpt-5.5", None),
                discovered("anthropic/claude-opus-4-8", None),
            ],
            None,
        );
        app.apply_model_discovery(ModelDiscovery {
            provider: "openrouter".to_string(),
            result: Ok(Some(model_options_from_discovered(
                "openrouter",
                ranked.models,
                ranked.recommended_count,
            ))),
        });

        let options = app.model_options("openrouter");
        assert_eq!(options[0].spec.as_deref(), Some("openai/gpt-5.5"));
        assert_eq!(
            options[1].spec.as_deref(),
            Some("anthropic/claude-opus-4-8")
        );
        assert_eq!(options[2].spec.as_deref(), Some("zai/glm-5"));
        assert_eq!(app.model_recommended_count("openrouter"), 2);

        let rendered = setup_overlay_text(app);
        assert!(
            rendered.iter().any(|line| line.contains("more models")),
            "recommended section should be separated from the full catalog: {rendered:?}"
        );
    }

    #[test]
    fn openrouter_ranking_applies_curated_order_and_alphabetical_rest() {
        use crate::capabilities::model_ranking::rank_discovered_models;

        let ranked = rank_discovered_models(
            "openrouter",
            vec![
                discovered("zai/glm-5", None),
                discovered("openai/gpt-5.5", None),
                discovered("anthropic/claude-opus-4-8", None),
                discovered("moon/kimi-k3", None),
            ],
            None,
        );

        assert_eq!(ranked.recommended_count, 2);
        let ids: Vec<&str> = ranked
            .models
            .iter()
            .map(|model| model.model_id.as_str())
            .collect();
        assert_eq!(
            ids,
            &[
                "openai/gpt-5.5",
                "anthropic/claude-opus-4-8",
                "moon/kimi-k3",
                "zai/glm-5",
            ]
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn discovered_models_replace_fallback_options_in_open_picker() {
        let mut fixture = app_with_llmsim().await;
        let app = &mut fixture.app;
        app.setup = Some(SetupStep::PickModel {
            provider: "openrouter".to_string(),
            selected: 0,
            custom: None,
            error: None,
        });

        app.apply_model_discovery(ModelDiscovery {
            provider: "openrouter".to_string(),
            result: Ok(Some(model_options_from_discovered(
                "openrouter",
                vec![
                    discovered("zai/glm-5", None),
                    discovered("moon/kimi-k3", None),
                ],
                0,
            ))),
        });

        let options = app.model_options("openrouter");
        assert_eq!(options.len(), 3);
        assert_eq!(options[0].spec.as_deref(), Some("zai/glm-5"));
        let rendered = setup_overlay_text(app);
        assert!(
            rendered.iter().any(|line| line.contains("zai/glm-5")),
            "open picker should render discovered models: {rendered:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unsupported_model_discovery_keeps_fallback_options() {
        let mut fixture = app_with_llmsim().await;
        let app = &mut fixture.app;

        app.apply_model_discovery(ModelDiscovery {
            provider: "ollama".to_string(),
            result: Ok(None),
        });

        let options = app.model_options("ollama");
        assert_eq!(options[0].spec.as_deref(), Some("llama3.2"));

        // The unsupported outcome is cached so reopening the picker doesn't
        // re-query an API that can't answer.
        app.model_discovery_enabled = true;
        app.request_model_discovery("ollama");
        assert!(!app.is_fetching_models("ollama"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn failed_model_discovery_surfaces_error_in_open_picker() {
        let mut fixture = app_with_llmsim().await;
        let app = &mut fixture.app;
        app.setup = Some(SetupStep::PickModel {
            provider: "openai".to_string(),
            selected: 0,
            custom: None,
            error: None,
        });

        app.apply_model_discovery(ModelDiscovery {
            provider: "openai".to_string(),
            result: Err("connection refused".to_string()),
        });

        assert!(matches!(
            app.setup,
            Some(SetupStep::PickModel { ref error, .. })
                if error.as_deref() == Some("model list unavailable: connection refused")
        ));
        // The curated list must remain usable after a failed fetch.
        let options = app.model_options("openai");
        assert_eq!(options[0].spec.as_deref(), Some("gpt-5.5"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn setup_connected_provider_jumps_straight_to_model_picker() {
        let mut fixture = app_with_llmsim().await;
        let app = &mut fixture.app;
        app.lines.clear();
        app.settings
            .set_token("openai".to_string(), "sk-test".to_string())
            .expect("save token");
        app.setup = Some(SetupStep::Provider { selected: 0 });

        app.handle_setup_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()))
            .await;

        assert!(
            matches!(
                app.setup,
                Some(SetupStep::PickModel { ref provider, .. }) if provider == "openai"
            ),
            "connected provider should skip the credential step: {:?}",
            app.setup
        );
        assert_eq!(app.model.provider_label(), "openai/gpt-5.5 medium");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn setup_model_picker_preset_selection_applies_and_persists() {
        let mut fixture = app_with_llmsim().await;
        let app = &mut fixture.app;
        app.lines.clear();
        app.settings
            .set_token("openai".to_string(), "sk-test".to_string())
            .expect("save token");
        app.setup = Some(SetupStep::Provider { selected: 0 });

        // Enter the wizard the way a user does: the connected-provider fast
        // path switches to openai first, so the provider-relative
        // `model <id>` the picker emits resolves against it.
        app.handle_setup_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()))
            .await;
        assert!(
            matches!(app.setup, Some(SetupStep::PickModel { .. })),
            "fast path should open the model picker: {:?}",
            app.setup
        );

        // Navigate to the second preset (gpt-5.4) and confirm it.
        app.handle_setup_key(KeyEvent::new(KeyCode::Down, KeyModifiers::empty()))
            .await;
        app.handle_setup_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()))
            .await;

        assert!(app.setup.is_none(), "wizard should finish: {:?}", app.setup);
        assert_eq!(app.model.provider_label(), "openai/gpt-5.4 none");
        assert!(
            app.lines
                .iter()
                .any(|line| line.text == "setup complete: openai/gpt-5.4 none"),
            "completion line should report the picked model: {:?}",
            app.lines
        );
        // The pick persists, so the next run restores it (see
        // pick_provider_applies_saved_model_for_saved_provider in main.rs).
        let snapshot = app.settings.snapshot();
        assert_eq!(snapshot.default_provider.as_deref(), Some("openai"));
        assert_eq!(snapshot.model_for("openai"), Some("gpt-5.4 none"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn setup_c_key_opens_credential_panel_even_when_connected() {
        let mut fixture = app_with_llmsim().await;
        let app = &mut fixture.app;
        app.settings
            .set_token("openai".to_string(), "sk-test".to_string())
            .expect("save token");
        app.setup = Some(SetupStep::Provider { selected: 0 });

        app.handle_setup_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::empty()))
            .await;

        assert!(
            matches!(
                app.setup,
                Some(SetupStep::Credential { ref provider, .. }) if provider == "openai"
            ),
            "c should open credential config: {:?}",
            app.setup
        );
    }

    // See setup_provider_picker_enters_credential_panel for why the env
    // lock is held across awaits.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn setup_custom_endpoint_flow_collects_url_and_model() {
        let _guard = crate::test_env::lock();
        unsafe {
            std::env::remove_var("CUSTOM_BASE_URL");
            std::env::remove_var("CUSTOM_API_KEY");
        }
        let mut fixture = app_with_llmsim().await;
        let app = &mut fixture.app;
        app.lines.clear();
        let custom_index = PROVIDER_OPTIONS
            .iter()
            .position(|option| option.name == "custom")
            .expect("custom provider option");
        app.setup = Some(SetupStep::Provider {
            selected: custom_index,
        });

        // Not connected yet → Enter opens the base URL input.
        app.handle_setup_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()))
            .await;
        assert!(
            matches!(app.setup, Some(SetupStep::BaseUrlInput { .. })),
            "custom without URL should ask for one: {:?}",
            app.setup
        );

        for ch in "http://localhost:8000/v1".chars() {
            app.handle_setup_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::empty()))
                .await;
        }
        app.handle_setup_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()))
            .await;
        assert!(
            matches!(
                app.setup,
                Some(SetupStep::Credential { ref provider, selected: 0, .. }) if provider == "custom"
            ),
            "saved URL should advance to the credential step: {:?}",
            app.setup
        );

        // "Continue without key" → model picker. With discovery disabled in
        // tests the list holds only the "Custom..." escape hatch; confirming
        // it opens the free-form input.
        app.handle_setup_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()))
            .await;
        assert!(
            matches!(
                app.setup,
                Some(SetupStep::PickModel { ref provider, custom: None, .. })
                    if provider == "custom"
            ),
            "custom credential step should advance to the model picker: {:?}",
            app.setup
        );
        app.handle_setup_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()))
            .await;
        assert!(
            matches!(
                app.setup,
                Some(SetupStep::PickModel { ref provider, custom: Some(_), .. })
                    if provider == "custom"
            ),
            "Custom... should open the free-form model input: {:?}",
            app.setup
        );

        for ch in "qwen3-coder".chars() {
            app.handle_setup_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::empty()))
                .await;
        }
        app.handle_setup_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()))
            .await;

        assert!(app.setup.is_none(), "wizard should finish: {:?}", app.setup);
        assert_eq!(app.model.provider_label(), "custom/qwen3-coder");
        let snapshot = app.settings.snapshot();
        assert_eq!(
            snapshot.base_url_for("custom"),
            Some("http://localhost:8000/v1")
        );
        assert_eq!(snapshot.default_provider.as_deref(), Some("custom"));
        assert_eq!(snapshot.model_for("custom"), Some("qwen3-coder"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn model_command_opens_model_picker_overlay() {
        let mut fixture = app_with_llmsim().await;
        let app = &mut fixture.app;
        app.lines.clear();

        app.dispatch_command_for_test("model").await;

        assert!(matches!(
            app.setup,
            Some(SetupStep::PickModel {
                ref provider,
                ..
            }) if provider == "llmsim"
        ));
        let rendered = setup_overlay_text(app);
        assert!(rendered.iter().any(|line| line.contains("Select Model")));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn model_command_preselects_current_raw_model_id() {
        let mut fixture = app_with_llmsim().await;
        let app = &mut fixture.app;
        app.lines.clear();
        app.setup = None;

        app.handle_command("setup token openai sk-test").await;
        app.run_setup_command(Some("provider openai"))
            .await
            .expect("set openai provider");
        app.run_setup_command(Some("model gpt-5.4"))
            .await
            .expect("set openai model");
        app.lines.clear();

        app.dispatch_command_for_test("model").await;

        assert!(matches!(
            app.setup,
            Some(SetupStep::PickModel {
                ref provider,
                selected,
                ..
            }) if provider == "openai" && selected == 1
        ));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn model_command_with_arg_opens_prefilled_model_modal() {
        let mut fixture = app_with_llmsim().await;
        let app = &mut fixture.app;
        app.lines.clear();
        app.setup = None;

        app.handle_command("setup token openai sk-test").await;
        app.run_setup_command(Some("provider openai"))
            .await
            .expect("set openai provider");
        app.lines.clear();
        app.dispatch_command_for_test("model gpt-5.4 high").await;

        assert_eq!(app.model.provider_label(), "openai/gpt-5.5 medium");
        assert!(matches!(
            app.setup,
            Some(SetupStep::PickModel {
                ref provider,
                ref custom,
                ..
            }) if provider == "openai" && custom.as_deref() == Some("gpt-5.4 high")
        ));

        app.handle_setup_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()))
            .await;

        assert!(app.setup.is_none());
        assert_eq!(app.model.provider_label(), "openai/gpt-5.4 high");
        assert!(
            app.lines
                .iter()
                .any(|line| line.text == "setup complete: openai/gpt-5.4 high"),
            "model modal should report completion: {:?}",
            app.lines
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn effort_command_opens_effort_modal_and_confirms_selection() {
        let mut fixture = app_with_llmsim().await;
        let app = &mut fixture.app;
        app.lines.clear();
        app.setup = None;

        app.handle_command("setup token openai sk-test").await;
        app.run_setup_command(Some("provider openai"))
            .await
            .expect("set openai provider");
        app.run_setup_command(Some("model gpt-5.4"))
            .await
            .expect("set openai model");
        app.lines.clear();
        app.dispatch_command_for_test("effort high").await;

        assert_eq!(app.model.provider_label(), "openai/gpt-5.4 none");
        assert!(matches!(
            app.setup,
            Some(SetupStep::PickEffort { selected: 3, .. })
        ));
        let rendered = setup_overlay_text(app);
        assert!(
            rendered
                .iter()
                .any(|line| line.contains("Select Reasoning Effort"))
        );

        app.handle_setup_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()))
            .await;

        assert!(app.setup.is_none());
        assert_eq!(app.model.provider_label(), "openai/gpt-5.4 high");
        assert!(
            app.lines
                .iter()
                .any(|line| line.text == "setup complete: openai/gpt-5.4 high"),
            "effort modal should report completion: {:?}",
            app.lines
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn effort_modal_does_not_mark_unset_openrouter_effort_current() {
        let mut fixture = app_with_llmsim().await;
        let app = &mut fixture.app;
        app.lines.clear();
        app.setup = None;

        app.handle_command("setup token openrouter sk-test").await;
        app.run_setup_command(Some("provider openrouter"))
            .await
            .expect("set openrouter provider");
        app.run_setup_command(Some("model nvidia/nemotron-3-super-120b-a12b"))
            .await
            .expect("set openrouter model");
        app.lines.clear();
        app.dispatch_command_for_test("effort").await;

        assert_eq!(
            app.model.provider_label(),
            "openrouter/nvidia/nemotron-3-super-120b-a12b"
        );
        let rendered = setup_overlay_text(app);
        assert!(
            !rendered.iter().any(|line| line.contains("· current")),
            "unset OpenRouter effort should not render a current marker: {rendered:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn app_view_state_hides_command_suggestions_when_input_disabled() {
        let mut fixture = app_with_llmsim().await;
        let app = &mut fixture.app;
        app.setup = None;

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
        let rows = render_chrome_lines(&state, 80, 4);
        // Row 0 = message separator. Idle mode shows the keystroke hint.
        assert!(
            rows[0].contains("Enter to send"),
            "idle separator missing Enter hint: rows={rows:?}"
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
        let rows = render_chrome_lines(&state, 80, 4);
        assert!(
            rows[0].contains("reading files"),
            "busy separator should display turn activity: {}",
            rows[0]
        );
        assert!(
            rows[0].contains("input disabled"),
            "busy separator should signal input is disabled: {}",
            rows[0]
        );
    }

    #[test]
    fn chrome_busy_falls_back_to_thinking_when_activity_unset() {
        let state = ViewState {
            busy: true,
            ..view_state_idle()
        };
        let rows = render_chrome_lines(&state, 80, 4);
        assert!(
            rows[0].contains("thinking"),
            "busy separator without activity should show 'thinking': {}",
            rows[0]
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
    fn chrome_session_status_compact_shows_provider_model_effort_approval_and_messages() {
        let state = ViewState {
            provider_name: "openrouter".to_string(),
            lines_count: 42,
            ..view_state_idle()
        };
        let rows = render_chrome_lines(&state, 120, 4);
        let status = &rows[3];
        assert!(
            status.contains("[expand ↓]"),
            "compact status should include expand affordance: {status}"
        );
        assert!(
            status.contains("openrouter"),
            "compact status should include provider: {status}"
        );
        assert!(
            status.contains("gpt-5.5"),
            "compact status should include model id: {status}"
        );
        assert!(
            status.contains("effort medium"),
            "compact status should include effort: {status}"
        );
        assert!(
            status.contains("approval normal"),
            "compact status should include approval: {status}"
        );
        assert!(
            status.contains("42 msgs"),
            "compact status should include message count: {status}"
        );
        assert!(
            !status.contains("session "),
            "compact status should keep session id for expanded layout: {status}"
        );
    }

    #[test]
    fn chrome_session_status_expanded_groups_details_across_four_lines() {
        let state = ViewState {
            model_id: "nvidia/nemotron-3-super-120b-a12b".to_string(),
            provider_name: "openrouter".to_string(),
            reasoning_effort: Some("high".to_string()),
            session_id: SessionId::from_seed(99887766),
            lines_count: 42,
            session_tokens: Some(1234),
            status_layout: StatusLayout::Expanded,
            ..view_state_idle()
        };
        let rows = render_chrome_lines(&state, 180, 7);
        assert!(
            rows[3].contains("[collapse ↑]")
                && rows[3].contains("provider openrouter")
                && rows[3].contains("model nvidia/nemotron-3-super-120b-a12b"),
            "expanded provider/model row should include selected model and provider: {:?}",
            rows[3]
        );
        assert!(
            !rows[3].contains("full "),
            "expanded model row should not duplicate model/provider as a full label: {:?}",
            rows[3]
        );
        assert!(
            rows[4].contains("effort high")
                && rows[4].contains("approval normal")
                && rows[4].contains("hooks none")
                && rows[4].contains("goal"),
            "expanded controls row should include effort, approval, hooks, and goal: {:?}",
            rows[4]
        );
        assert!(
            rows[5].contains("42 msgs") && rows[5].contains("tokens 1234"),
            "expanded counts row should include messages and tokens: {:?}",
            rows[5]
        );
        let session_id_str = state.session_id.to_string();
        assert!(
            rows[6].contains("session ") && rows[6].contains(&session_id_str),
            "expanded session row should include the full session id: {:?}",
            rows[6]
        );
        assert!(
            !rows[6].contains('…'),
            "expanded session row should not shorten the session id: {:?}",
            rows[6]
        );
    }

    #[test]
    fn chrome_session_status_stays_visible_with_multiline_input() {
        let state = ViewState {
            status_layout: StatusLayout::Expanded,
            ..view_state_idle()
        };
        let rows = render_chrome_lines_with_input_height(&state, 120, 8, 3);
        assert!(
            rows[4].contains("[collapse ↑]") && rows[4].contains("provider openai"),
            "expanded model row should remain visible with multiline input: {:?}",
            rows
        );
        assert!(
            rows[5].contains("effort medium") && rows[5].contains("hooks none"),
            "expanded controls row should remain visible with multiline input: {:?}",
            rows
        );
        assert!(
            rows[6].contains("3 msgs") && rows[6].contains("tokens n/a"),
            "expanded counts row should remain visible with multiline input: {:?}",
            rows
        );
        assert!(
            rows[7].contains("session "),
            "expanded session row should remain visible with multiline input: {:?}",
            rows
        );
    }

    #[test]
    fn chrome_idle_does_not_reserve_empty_stream_preview_row() {
        let state = view_state_idle();
        let rows = render_chrome_lines(&state, 80, 4);
        assert!(
            rows[0].contains("Enter to send"),
            "idle chrome should start with the separator instead of an empty preview row: {:?}",
            rows
        );
    }
}

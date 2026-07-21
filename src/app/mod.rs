// TUI app state and event loop.
// Decision: keep the TUI surface tiny. Transcript output is inserted into the
// native terminal scrollback; ratatui owns only a short inline composer at the
// bottom.

use crate::goal::{GOAL_EVALUATE_ARG, GoalStore, parse_evaluation_response};
use crate::host_ui::UiCommand;
use crate::runtime::{BuiltRuntime, ModelState, StartupInfo};
use crate::session::Session;
use crate::user_ask::{
    AskOutcome, USER_ASK_EVALUATE_ARG, UserAskStore, evaluation_status_message,
    parse_evaluation_response as parse_user_ask_evaluation,
};
use crate::worktree::WorktreeManager;
use anyhow::Result;
use crossterm::event::{
    self, Event as CrosstermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton,
    MouseEvent, MouseEventKind,
};
use everruns_core::command::{CommandDescriptor, CommandSource};
use everruns_core::message::ContentPart;
use everruns_core::session_task::{SessionTask, SessionTaskRegistry};
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
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};

mod fullscreen;
mod markdown_table;
mod render;
mod setup;
mod syntax;
mod viewport;

// Re-export the moved free items so the rest of the crate (and the test module)
// can keep referring to them as `crate::app::*`. `setup` exposes only `impl App`
// methods, so it needs no re-export. The rendering module is named `render`
// rather than `draw` so it does not collide with the free `draw` fn it exports.
// The view-model types and runtime-event translation live in `crate::transcript`
// (the single boundary that interprets `everruns_core` events); re-export them
// here so the TUI's own submodules keep referring to them as `crate::app::*`.
pub(crate) use self::{render::*, viewport::*};
pub(crate) use crate::presentation::*;
pub(crate) use crate::transcript::*;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CommandSuggestion {
    completion: String,
    label: String,
}

pub const COMPOSER_VIEWPORT_HEIGHT: u16 = 18;
const SESSION_TASK_REFRESH_INTERVAL: Duration = Duration::from_secs(1);
/// Consecutive failed event-loop iterations tolerated before the terminal is
/// considered gone and the error becomes fatal. The slowest failure mode (an
/// unanswered cursor-position query) blocks ~2s per attempt inside crossterm,
/// so this bounds a permanently dead terminal to ~10s before exit while
/// letting a briefly unresponsive emulator recover.
const MAX_TERMINAL_IO_FAILURES: usize = 5;
/// After the first Ctrl+C arms exit, typing does not disarm the prompt until
/// this grace elapses so a slow second Ctrl+C still exits.
const CTRL_C_EXIT_ARM_GRACE: Duration = Duration::from_secs(5);
const MAX_INPUT_HEIGHT: u16 = 12;
const RECENT_TRANSCRIPT_SOURCE_LINES: usize = 80;
const RECENT_TRANSCRIPT_MAX_TEXT_BYTES: usize = 4_000;
const ACCENT_BLUE: Color = Color::Rgb(45, 91, 158);
const ACCENT_GOLD: Color = Color::Rgb(126, 94, 19);
const TEXT_PRIMARY: Color = Color::Rgb(230, 230, 232);
const TEXT_MUTED: Color = Color::Rgb(140, 140, 145);
const TEXT_DIM: Color = Color::Rgb(72, 72, 78);
const ERROR_RED: Color = Color::Rgb(196, 78, 78);
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
    /// When the first Ctrl+C armed exit; a second press quits.
    ctrl_c_pending_exit_at: Option<Instant>,
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
    /// Codex login runs outside the terminal input handler. The task owns any
    /// network wait; the event loop only polls its channel between frames.
    codex_login: Option<PendingCodexLogin>,
    codex_login_tx: mpsc::UnboundedSender<CodexLoginEvent>,
    codex_login_rx: mpsc::UnboundedReceiver<CodexLoginEvent>,
    next_codex_login_id: u64,
    status_layout: StatusLayout,
    session_tokens: Option<u64>,
    /// Terminal-side commands emitted by `ClientCommandsCapability` (via
    /// `runtime.execute_command`). Drained in the event loop; see
    /// [`App::apply_ui_command`].
    ui_rx: mpsc::UnboundedReceiver<UiCommand>,
    /// Live status-bar text per extension (`ext:<name>` → status), pushed by
    /// extension servers over `status/changed`. Rendered in the status bar via
    /// [`App::presentation_state`].
    extension_status: std::collections::BTreeMap<String, String>,
    /// Incoming extension `ui/ask` requests; each is prompted one at a time via
    /// [`App::pending_ask`].
    ask_rx: mpsc::UnboundedReceiver<crate::host_ui::AskRequest>,
    /// The `ui/ask` prompt currently shown, if any. Owns the keyboard (even
    /// mid-turn) until the user answers or dismisses it.
    pending_ask: Option<PendingAsk>,
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
    /// Wake channel for everruns `spawn_background` completions (fed by the
    /// platform-store wake seam, `crate::background_wake`). Drained while idle to
    /// auto-start a turn so the agent reacts to finished work. See
    /// specs/background.md.
    background_wake: crate::background_wake::WakeReceiver,
    /// Retained for the TUI lifetime so due local schedules keep polling.
    _schedule_runner: everruns_local::LocalScheduleRunnerHandle,
    /// Everruns session-task registry used by `spawn_background`; the TUI reads
    /// it for the background status segment and panel.
    task_registry: Arc<dyn SessionTaskRegistry>,
    session_tasks: Vec<SessionTask>,
    session_tasks_error: Option<String>,
    last_session_tasks_refresh: Option<Instant>,
    /// Open background-tasks panel overlay, holding its scroll offset (in
    /// lines). `None` when closed. Toggled with Ctrl+B; read-only view of the
    /// session task list.
    background_panel: Option<usize>,
    goal_store: Arc<GoalStore>,
    user_ask_store: Arc<UserAskStore>,
    user_ask_enabled: bool,
    worktree: Arc<WorktreeManager>,
    workspace_host: Arc<crate::workspace_host::WorkspaceHost>,
    /// Images from `--image` / `-i` on the CLI, consumed on the first turn.
    pending_images: Vec<ContentPart>,
    /// Large paste placeholders mapped to their full clipboard/terminal payloads.
    pending_pastes: Vec<(String, String)>,
    /// Which renderer this session drives. `Inline` is the default
    /// scrollback-native composer; `Fullscreen` is the experimental
    /// alternate-screen `tuika` renderer (`--fullscreen`).
    render_mode: RenderMode,
    /// Full-screen transcript scroll position (unused in inline mode).
    scroll: tuika::ScrollState,
    /// Last (content_height, viewport_height) the full-screen transcript drew,
    /// so mouse/paging handlers can clamp scrolling without re-laying out.
    scroll_metrics: (u16, u16),
    /// Drives the terminal's native OSC 9;4 progress indicator (Ghostty top
    /// bar / taskbar) while a turn runs. Works in both renderers. Enabled only
    /// for real TUI sessions via [`App::enable_native_progress`] so tests and
    /// non-terminal hosts emit no escape sequences.
    term_progress: tuika::TerminalProgress,
    native_progress: bool,
    /// Shell-style Up/Down recall of previously submitted composer prompts,
    /// persisted across sessions. See [`crate::prompt_history`].
    history: crate::prompt_history::PromptHistory,
    /// Active Ctrl+R reverse-history search, if any. While set it owns the
    /// keyboard and the composer previews the current match.
    history_search: Option<HistorySearch>,
    /// When the active turn began, for the live elapsed timer on the busy
    /// indicator. `None` while idle.
    turn_started_at: Option<Instant>,
    /// Prompt tokens the most recent LLM generation consumed — the current fill
    /// of the model's context window. `None` until the first generation.
    context_used_tokens: Option<u32>,
}

/// In-progress Ctrl+R reverse search over [`App::history`].
#[derive(Clone, Debug)]
pub(crate) struct HistorySearch {
    /// The incremental search query typed so far.
    query: String,
    /// Index of the entry currently matched, or `None` when nothing matches.
    match_index: Option<usize>,
    /// The composer rows captured on entry, restored if the search is cancelled.
    saved_lines: Vec<String>,
}

/// The renderer backing a TUI session.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum RenderMode {
    /// Scrollback-native inline composer (default).
    #[default]
    Inline,
    /// Experimental full-screen alternate-screen renderer (`tuika`).
    Fullscreen,
}

impl RenderMode {
    pub(crate) fn is_fullscreen(self) -> bool {
        matches!(self, RenderMode::Fullscreen)
    }
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
    CodexLogin {
        selected: usize,
        method: CodexLoginMethod,
        device_code: Option<(String, String)>,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CodexLoginMethod {
    Browser,
    Device,
}

struct PendingCodexLogin {
    id: u64,
    task: tokio::task::JoinHandle<()>,
}

/// An in-flight extension `ui/ask`: the prompt shown, the answer being typed,
/// and the oneshot that delivers it back to the extension server.
struct PendingAsk {
    prompt: String,
    placeholder: Option<String>,
    value: String,
    reply: Option<oneshot::Sender<crate::host_ui::AskAnswer>>,
}

enum CodexLoginEvent {
    DeviceCode {
        id: u64,
        verification_uri: String,
        user_code: String,
    },
    Finished {
        id: u64,
        selected: usize,
        result: Result<crate::settings::CodexAuth, String>,
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
    pub presentation: PresentationState,
    pub command_suggestions: Vec<CommandSuggestion>,
    /// Active Ctrl+R reverse-search prompt, if any. Takes over the chrome
    /// preview row and suppresses command suggestions.
    pub history_search: Option<HistorySearchView>,
    pub busy_frame: u64,
}

/// Render-facing view of an active reverse-history search.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HistorySearchView {
    pub query: String,
    pub matched: bool,
}

impl ViewState {
    pub(crate) fn status_row_count(&self) -> u16 {
        self.presentation.status_row_count()
    }
}

impl App {
    pub fn new(runtime: BuiltRuntime, pending_images: Vec<ContentPart>) -> Self {
        let should_setup = runtime.startup.setup_recommended;
        let goal_store = runtime.goal_store.clone();
        let user_ask_store = runtime.user_ask_store.clone();
        let user_ask_enabled = runtime.user_ask_enabled;
        let session_id = runtime.handles.session_id;
        let session = Session::new(runtime.handles, runtime.model.clone());
        let (models_tx, models_rx) = mpsc::unbounded_channel::<ModelDiscovery>();
        let (codex_login_tx, codex_login_rx) = mpsc::unbounded_channel::<CodexLoginEvent>();
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
            ctrl_c_pending_exit_at: None,
            esc_pending_cancel: false,
            busy_frame: 0,
            turn_activity: None,
            stream_preview: None,
            rx: None,
            turn_cancel: None,
            setup: None,
            codex_login: None,
            codex_login_tx,
            codex_login_rx,
            next_codex_login_id: 0,
            status_layout: StatusLayout::Compact,
            session_tokens: None,
            ui_rx: runtime.ui_rx,
            extension_status: std::collections::BTreeMap::new(),
            ask_rx: runtime.ask_rx,
            pending_ask: None,
            settings: runtime.settings,
            model_catalog: HashMap::new(),
            model_fetches_in_flight: HashSet::new(),
            model_discovery_enabled: true,
            models_tx,
            models_rx,
            background_wake: runtime.background_wake,
            _schedule_runner: runtime.schedule_runner,
            task_registry: runtime.task_registry,
            session_tasks: Vec::new(),
            session_tasks_error: None,
            last_session_tasks_refresh: None,
            background_panel: None,
            goal_store,
            user_ask_store,
            user_ask_enabled,
            worktree: runtime.worktree,
            workspace_host: runtime.workspace_host,
            pending_images,
            pending_pastes: Vec::new(),
            render_mode: RenderMode::default(),
            scroll: tuika::ScrollState::new(),
            scroll_metrics: (0, 0),
            term_progress: tuika::TerminalProgress::new(),
            native_progress: false,
            // Tests run in-memory so recall never reads or appends to the real
            // per-user history file.
            history: if cfg!(test) {
                crate::prompt_history::PromptHistory::in_memory()
            } else {
                crate::prompt_history::PromptHistory::load()
            },
            history_search: None,
            turn_started_at: None,
            context_used_tokens: None,
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

    /// Switch this session to the experimental full-screen renderer. Called by
    /// `run_tui` before the loop when `--fullscreen` is set.
    pub(crate) fn set_render_mode(&mut self, mode: RenderMode) {
        self.render_mode = mode;
    }

    /// Enable the terminal's native OSC 9;4 progress indicator for this session.
    /// Called by `run_tui`; left off for tests and non-terminal hosts.
    pub(crate) fn enable_native_progress(&mut self) {
        self.native_progress = true;
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
        let presentation = self.presentation_state();
        let history_search = self.history_search_view();
        ViewState {
            // The search prompt owns the chrome row while active, so suppress
            // command suggestions to avoid two things fighting for it.
            command_suggestions: if history_search.is_none()
                && !presentation.busy
                && self.setup.is_none()
            {
                self.suggestions()
            } else {
                Vec::new()
            },
            history_search,
            busy_frame: self.busy_frame,
            presentation,
        }
    }

    /// The active model's context-window size in tokens, from its static
    /// profile. `None` for models without a profile (e.g. the `llmsim` sim), in
    /// which case no context gauge is shown.
    fn context_window_tokens(&self) -> Option<u32> {
        let driver =
            crate::runtime::Provider::from_name(&self.model.provider_name())?.driver_id()?;
        let profile = everruns_core::get_model_profile(&driver, &self.model.model_id())?;
        u32::try_from(profile.limits?.context).ok()
    }

    pub(crate) fn presentation_state(&self) -> PresentationState {
        let settings = self.settings.snapshot();
        let approval_mode = if settings.sandbox_mode() == crate::settings::SandboxMode::Off {
            format!("{} · UNSAFE HOST", settings.approval_mode().as_str())
        } else {
            settings.approval_mode().as_str().to_string()
        };
        PresentationState {
            stream_preview: self.stream_preview.clone(),
            busy: self.busy,
            turn_activity: self.turn_activity.clone(),
            model_id: self.model.model_id(),
            provider_name: self.model.provider_name(),
            reasoning_effort: self.model.reasoning_effort(),
            session_id: self.session.session_id().to_string(),
            lines_count: self.lines.len(),
            session_tokens: self.session_tokens,
            turn_elapsed_secs: self.turn_started_at.map(|start| start.elapsed().as_secs()),
            context_used_tokens: self.context_used_tokens,
            context_window_tokens: self.context_window_tokens(),
            status_layout: self.status_layout,
            hooks_summary: self.startup.hook_summary(),
            approval_mode,
            background: self.background_counts(),
            goal_indicator: self.goal_indicator(),
            ask_indicator: self.ask_indicator(),
            worktree_compact: self.worktree.status_bar_compact(),
            worktree_expanded: self.worktree.status_bar_expanded(),
            extension_status: self
                .extension_status
                .iter()
                .map(|(ext, status)| {
                    // `ext:<name>` → `<name>` for a compact status-bar label.
                    let label = ext.strip_prefix("ext:").unwrap_or(ext).to_string();
                    (label, status.clone())
                })
                .collect(),
        }
    }

    fn ask_indicator(&self) -> Option<String> {
        if !self.user_ask_enabled {
            return None;
        }
        if !self.user_ask_store.is_active(self.session.session_id()) {
            return None;
        }
        let turns = self
            .user_ask_store
            .status(self.session.session_id())
            .active
            .map(|active| active.evaluated_turns)
            .unwrap_or(0);
        Some(format!("? ask ({turns})"))
    }

    fn background_counts(&self) -> Option<crate::session_tasks_view::BackgroundCounts> {
        crate::session_tasks_view::counts(&self.session_tasks)
    }

    fn background_panel_body(&self) -> String {
        crate::session_tasks_view::render_task_list(
            &self.session_tasks,
            self.session_tasks_error.as_deref(),
        )
    }

    async fn refresh_session_tasks(&mut self) {
        match self
            .task_registry
            .list(self.session.session_id(), None)
            .await
        {
            Ok(tasks) => {
                self.session_tasks = tasks;
                self.session_tasks_error = None;
            }
            Err(err) => {
                self.session_tasks.clear();
                self.session_tasks_error = Some(err.to_string());
            }
        }
        self.last_session_tasks_refresh = Some(Instant::now());
    }

    async fn refresh_session_tasks_if_due(&mut self) {
        if self
            .last_session_tasks_refresh
            .is_some_and(|last| last.elapsed() < SESSION_TASK_REFRESH_INTERVAL)
        {
            return;
        }
        self.refresh_session_tasks().await;
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
        self.push_system(format!("model: {}", self.model.provider_label()));
        let sandbox_mode = self.settings.snapshot().sandbox_mode();
        self.push_system(format!("sandbox: {}", sandbox_mode.as_str()));
        if let Some(warning) = crate::sandbox::danger_warning(sandbox_mode) {
            self.push_system(warning.to_string());
        }
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
                    // Redraw/anchoring can fail before input polling; let a
                    // queued exit key terminate instead of waiting for retries.
                    if let Err(input_err) =
                        self.drain_terminal_input(terminal, Duration::ZERO).await
                    {
                        tracing::warn!(
                            "terminal input drain failed after terminal i/o error: {input_err:#}"
                        );
                    }
                    if self.should_quit {
                        return Ok(());
                    }

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
        // Inline mode streams finalized lines into native scrollback via
        // `insert_before`; full-screen keeps the whole transcript in `lines`
        // and redraws it into the alternate screen each frame, so it skips both
        // the flush and the inline-viewport re-anchoring.
        if !self.render_mode.is_fullscreen() {
            self.flush_transcript(terminal)?;
        }
        self.refresh_session_tasks_if_due().await;
        terminal.autoresize()?;
        if !self.render_mode.is_fullscreen() {
            // Ratatui keeps an inline viewport's cursor offset across resizes
            // and resets its origin on horizontal shrinks. Normalize the
            // viewport to the terminal bottom before every draw.
            maybe_reanchor_inline_viewport(terminal)?;
        }
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
                Ok(TurnEvent::ContextUsed(used)) => {
                    self.context_used_tokens = Some(used);
                    return Ok(());
                }
                Ok(TurnEvent::Done) => {
                    self.finish_busy();
                    if let Some(notice) = self.session.take_checkpoint_notice() {
                        self.refresh_after_checkpoint_restore(notice).await;
                        return Ok(());
                    }
                    self.after_turn_goal_check().await;
                    self.after_turn_user_ask_check().await;
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
            self.apply_ui_command(command).await;
            applied_ui_command = true;
        }
        // Extension `ui/ask` prompts. One at a time; a request arriving while a
        // prompt is open is answered "cancelled" rather than stacking overlays.
        while let Ok(request) = self.ask_rx.try_recv() {
            if self.pending_ask.is_some() {
                let _ = request.reply.send(crate::host_ui::AskAnswer {
                    answer: String::new(),
                    cancelled: true,
                });
                continue;
            }
            self.push_system(format!("extension asks: {}", request.prompt));
            self.pending_ask = Some(PendingAsk {
                prompt: request.prompt,
                placeholder: request.placeholder,
                value: String::new(),
                reply: Some(request.reply),
            });
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

        // 3a) Codex login is intentionally a background operation so a browser
        // close or an abandoned device flow cannot stop terminal input.
        if self.apply_codex_login_events().await {
            return Ok(());
        }

        // 3b) proactive wake: when an everruns `spawn_background` task finishes
        // while the session is idle, auto-start a turn so the agent reacts
        // without a user prompt.
        if self.maybe_wake_from_background_channel() {
            return Ok(());
        }

        // 4) direct terminal input.
        self.drain_terminal_input(terminal, Duration::from_millis(80))
            .await
    }

    async fn drain_terminal_input<B>(
        &mut self,
        terminal: &mut Terminal<B>,
        initial_poll_timeout: Duration,
    ) -> Result<()>
    where
        B: Backend,
        B::Error: std::error::Error + Send + Sync + 'static,
    {
        let mut poll_timeout = initial_poll_timeout;
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
                    if self.render_mode.is_fullscreen() && self.handle_fullscreen_scroll(mouse.kind)
                    {
                        continue;
                    }
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

    async fn refresh_after_checkpoint_restore(&mut self, notice: String) {
        match self.session.active_lines().await {
            Ok(lines) => {
                self.lines = lines;
                self.printed_lines = 0;
            }
            Err(error) => self.push_system(format!("refresh restored transcript: {error}")),
        }
        self.push_system(notice);
        if let Some(prompt) = self.session.take_restored_prompt() {
            self.set_input_text(prompt);
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
        // Reverse-history search owns the keyboard while active (even Ctrl+C,
        // which cancels the search rather than arming exit).
        if self.history_search.is_some() {
            self.handle_search_key(key);
            return;
        }
        // Ctrl+R opens reverse-history search from the idle composer.
        if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('r')) {
            self.history_search_start();
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('c') => {
                    self.handle_ctrl_c();
                    return;
                }
                KeyCode::Char('d') => {
                    self.abort_codex_login();
                    self.should_quit = true;
                    return;
                }
                KeyCode::Char('b') => {
                    // Clear the armed single-Ctrl+C exit ourselves, since this
                    // branch returns before the shared reset below.
                    self.disarm_ctrl_c_pending_exit();
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

        self.disarm_ctrl_c_pending_exit_if_grace_elapsed();

        // The background panel overlay captures navigation keys (even mid-turn,
        // so you can watch tasks while a turn runs).
        if self.background_panel.is_some() {
            self.handle_background_panel_key(key);
            return;
        }

        // An extension `ui/ask` prompt owns the keyboard even mid-turn, since
        // the server typically asks while a tool is running.
        if self.pending_ask.is_some() {
            self.handle_ask_key(key);
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
            KeyCode::Up if self.try_history_prev() => {}
            KeyCode::Down if self.try_history_next() => {}
            _ => {
                let _ = self.input.input(normalize_printable_key(key));
            }
        }
    }

    /// Whether pressing Up should recall an older prompt rather than move the
    /// composer cursor. Recall only kicks in when the composer is empty, or when
    /// an unmodified recalled entry is showing and the cursor sits on the first
    /// line — so editing a multi-line prompt (or a fresh draft) never loses text
    /// to an accidental recall. Mirrors Codex's line-boundary gating.
    fn history_recall_prev_allowed(&self) -> bool {
        let text = self.input_text();
        if text.is_empty() {
            return true;
        }
        match self.history.current_entry() {
            Some(entry) if entry == text => self.input.cursor().0 == 0,
            _ => false,
        }
    }

    /// Handle Up as history recall. Returns `true` when the key was consumed
    /// (recall began, moved, or is parked at the oldest entry), `false` to let
    /// the textarea move the cursor normally.
    fn try_history_prev(&mut self) -> bool {
        if !self.history_recall_prev_allowed() {
            return false;
        }
        let current = self.input_text();
        if let Some(entry) = self.history.navigate_up(&current) {
            self.set_input_text(entry);
            true
        } else {
            // No entries at all → let the textarea handle Up. Already browsing at
            // the oldest entry → swallow the key so it doesn't move the cursor.
            self.history.is_browsing()
        }
    }

    /// Handle Down as history recall. Only active while browsing an unmodified
    /// recalled entry with the cursor on the last line; otherwise the textarea
    /// moves the cursor.
    fn try_history_next(&mut self) -> bool {
        if !self.history.is_browsing() {
            return false;
        }
        let current = self.input_text();
        match self.history.current_entry() {
            Some(entry) if entry == current => {
                let last_row = self.input.lines().len().saturating_sub(1);
                if self.input.cursor().0 != last_row {
                    return false;
                }
            }
            // The user edited the recalled entry; treat Down as normal movement.
            _ => return false,
        }
        if let Some(text) = self.history.navigate_down(&current) {
            self.set_input_text(text);
        }
        true
    }

    /// Open Ctrl+R reverse-history search from the idle composer. No-op while a
    /// turn or overlay owns the keyboard, or when there is no history to search.
    fn history_search_start(&mut self) {
        if self.busy
            || self.setup.is_some()
            || self.background_panel.is_some()
            || self.pending_ask.is_some()
            || self.history.is_empty()
        {
            return;
        }
        self.history.reset_navigation();
        self.history_search = Some(HistorySearch {
            query: String::new(),
            match_index: None,
            saved_lines: self.input.lines().to_vec(),
        });
        // An empty query lands on the newest entry, so Ctrl+R immediately
        // previews the last prompt — like a shell.
        self.history_search_refresh();
    }

    /// Keyboard handling while reverse search owns the composer.
    fn handle_search_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('r') if ctrl => self.history_search_cycle(),
            // Ctrl+C / Ctrl+G / Esc abandon the search and restore the draft.
            KeyCode::Char('c' | 'g') if ctrl => self.history_search_cancel(),
            KeyCode::Esc => self.history_search_cancel(),
            KeyCode::Enter => self.history_search_accept(),
            KeyCode::Backspace => {
                if let Some(search) = self.history_search.as_mut() {
                    search.query.pop();
                }
                self.history_search_refresh();
            }
            KeyCode::Char(c) if !ctrl => {
                if let Some(search) = self.history_search.as_mut() {
                    search.query.push(c);
                }
                self.history_search_refresh();
            }
            // Ignore anything else so stray keys don't leak into the composer.
            _ => {}
        }
    }

    /// Recompute the match from the newest entry for the current query and
    /// preview it in the composer. Called after every query edit.
    fn history_search_refresh(&mut self) {
        let Some(search) = self.history_search.as_ref() else {
            return;
        };
        let Some(newest) = self.history.newest_index() else {
            return;
        };
        let hit = self.history.reverse_search(&search.query, newest);
        self.apply_search_hit(hit);
    }

    /// Advance to the next older match for the current query (repeated Ctrl+R).
    /// Holds on the current match when nothing older matches.
    fn history_search_cycle(&mut self) {
        let Some(search) = self.history_search.as_ref() else {
            return;
        };
        let query = search.query.clone();
        let start = match search.match_index {
            Some(0) => return, // already at the oldest match
            Some(i) => i - 1,
            None => match self.history.newest_index() {
                Some(n) => n,
                None => return,
            },
        };
        if let Some(hit) = self.history.reverse_search(&query, start) {
            self.apply_search_hit(Some(hit));
        }
    }

    /// Store the resolved match and mirror it into the composer (or clear the
    /// composer when nothing matches).
    fn apply_search_hit(&mut self, hit: Option<(usize, String)>) {
        let Some(search) = self.history_search.as_mut() else {
            return;
        };
        match hit {
            Some((index, entry)) => {
                search.match_index = Some(index);
                self.set_input_text(entry);
            }
            None => {
                search.match_index = None;
                self.input = new_input_area(vec![String::new()]);
            }
        }
    }

    /// Accept the current match: keep it in the composer and leave search mode
    /// so the user can edit or submit. With no match, restore the saved draft.
    fn history_search_accept(&mut self) {
        let Some(search) = self.history_search.take() else {
            return;
        };
        match search.match_index.and_then(|i| self.history.entry_at(i)) {
            Some(entry) => {
                let entry = entry.to_string();
                self.set_input_text(entry);
            }
            None => self.restore_saved_input(search.saved_lines),
        }
    }

    /// Abandon the search and restore the composer to its pre-search contents.
    fn history_search_cancel(&mut self) {
        if let Some(search) = self.history_search.take() {
            self.restore_saved_input(search.saved_lines);
        }
    }

    fn restore_saved_input(&mut self, lines: Vec<String>) {
        self.input = new_input_area(if lines.is_empty() {
            vec![String::new()]
        } else {
            lines
        });
        self.input.move_cursor(CursorMove::End);
    }

    /// Snapshot of the active reverse search for rendering, if any.
    pub(crate) fn history_search_view(&self) -> Option<HistorySearchView> {
        self.history_search
            .as_ref()
            .map(|search| HistorySearchView {
                query: search.query.clone(),
                matched: search.match_index.is_some(),
            })
    }

    fn suggestions(&self) -> Vec<CommandSuggestion> {
        // `@`-triggered file-path completion takes over the suggestion row when
        // the word being typed starts with `@`. Restricted to single-line input
        // so completion can safely rebuild the whole line (matching how slash
        // completion works).
        if self.input.lines().len() == 1
            && let Some(file) =
                file_path_suggestions(self.suggestion_input(), &self.startup.workspace_root)
        {
            return file;
        }
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
        // Split on newlines so a recalled multi-line prompt restores as multiple
        // composer rows rather than one row with embedded control characters.
        let lines: Vec<String> = text.split('\n').map(str::to_string).collect();
        self.input = new_input_area(if lines.is_empty() {
            vec![String::new()]
        } else {
            lines
        });
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

    /// Keyboard handling while an extension `ui/ask` prompt is open: edit the
    /// answer, Enter to submit, Esc to cancel.
    fn handle_ask_key(&mut self, key: KeyEvent) {
        let Some(ask) = self.pending_ask.as_mut() else {
            return;
        };
        match key.code {
            KeyCode::Enter => {
                let answer = ask.value.clone();
                self.resolve_ask(crate::host_ui::AskAnswer {
                    answer,
                    cancelled: false,
                });
            }
            KeyCode::Esc => {
                self.resolve_ask(crate::host_ui::AskAnswer {
                    answer: String::new(),
                    cancelled: true,
                });
            }
            KeyCode::Backspace => {
                ask.value.pop();
            }
            KeyCode::Char(c) => {
                ask.value.push(c);
            }
            _ => {}
        }
    }

    /// Deliver an answer to the extension and close the prompt.
    fn resolve_ask(&mut self, answer: crate::host_ui::AskAnswer) {
        if let Some(mut ask) = self.pending_ask.take() {
            let shown = if answer.cancelled {
                "(cancelled)".to_string()
            } else {
                answer.answer.clone()
            };
            if let Some(reply) = ask.reply.take() {
                let _ = reply.send(answer);
            }
            self.push_system(format!("answered: {shown}"));
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
        // Record what the user typed (including slash/bang commands) for
        // shell-style Up/Down recall. Uses the pre-expansion display text so
        // paste placeholders recall as the user saw them.
        self.history.record(&display_text);
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
        if self.user_ask_enabled
            && let Err(err) = self
                .user_ask_store
                .record_user_prompt(self.session.session_id(), &text)
        {
            self.push_system(format!("user ask: {err}"));
        }
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
                    .background_panel_body()
                    .lines()
                    .count()
                    .saturating_sub(1);
                self.background_panel = Some(offset.saturating_add(1).min(max));
            }
            _ => {}
        }
    }

    /// Proactive wake: drain any everruns `spawn_background` completion signal
    /// delivered over [`crate::background_wake`] and, while idle, start a turn so
    /// the agent reacts to the finished work (reads the result, continues, or
    /// reports) without waiting for a user prompt. Only fires when idle, so it
    /// never interrupts an in-flight turn. Only the first pending message wakes a
    /// turn (draining the rest would lose them); the remainder wake on subsequent
    /// idle ticks. Returns true if it started a turn.
    fn maybe_wake_from_background_channel(&mut self) -> bool {
        if self.busy || self.rx.is_some() || self.setup.is_some() {
            return false;
        }
        let Ok(message) = self.background_wake.try_recv() else {
            return false;
        };
        if !self.settings.snapshot().proactive_wake_enabled() {
            self.push_system(
                "✓ background task finished — see /background (proactive wake off)".to_string(),
            );
            return false;
        }
        self.push_system("↻ background task finished — waking agent to review".to_string());
        self.start_turn(crate::background_wake::frame_wake_prompt(&message));
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
    async fn apply_ui_command(&mut self, command: UiCommand) {
        match command {
            UiCommand::ShowHelp => self.show_help(),
            UiCommand::ShowTools => {
                self.push_system(format!("tools: {}", self.startup.tool_names.join(", ")));
            }
            UiCommand::ManageMcp { arg } => self.manage_mcp_command(arg.as_deref()).await,
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
                self.user_ask_store.clear_active(self.session.session_id());
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
            UiCommand::SetExtensionStatus { ext, status } => {
                // Empty status clears the field; a live value replaces it.
                if status.trim().is_empty() {
                    self.extension_status.remove(&ext);
                } else {
                    self.extension_status.insert(ext, status);
                }
            }
            UiCommand::SetExtensionActive {
                capability_id,
                name,
                activate,
            } => {
                // enable_extension/disable_extension already persisted the
                // setting; here we apply it to the running session so it takes
                // effect on the next turn (EVE-795) rather than only next start.
                let result = if activate {
                    self.session.activate_capability(&capability_id).await
                } else {
                    self.session.deactivate_capability(&capability_id).await
                };
                match result {
                    Ok(delta) if delta.changed => self.push_system(format!(
                        "extension `{name}` {} for this session; effective on the next turn.",
                        if activate { "enabled" } else { "disabled" }
                    )),
                    // No overlay change: already in the desired state, or (on
                    // disable) it rides the harness layer from startup and only
                    // settings can drop it — which happens next session.
                    Ok(_) if !activate => self.push_system(format!(
                        "extension `{name}` will be disabled on the next session."
                    )),
                    Ok(_) => {}
                    Err(err) => self.push_system(format!(
                        "extension `{name}` saved, but applying it to this session failed: {err}. \
                         It will load on the next session."
                    )),
                }
            }
        }
    }

    const MCP_USAGE: &'static str =
        "usage: /mcp [reload | login <name> | enable|disable|remove <name> [global|workspace]]";

    async fn manage_mcp_command(&mut self, raw: Option<&str>) {
        let Some(raw) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
            if self.startup.mcp_server_names.is_empty() {
                let global = crate::mcp_config::global_mcp_config_path()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "the yolop config dir".to_string());
                self.push_system(format!(
                    "no MCP servers configured (add them with `yolop mcp add`, .mcp.json in the workspace root, or {global})"
                ));
            } else {
                self.push_system(format!(
                    "active MCP servers: {}",
                    self.startup.mcp_server_names.join(", ")
                ));
            }
            self.push_system(Self::MCP_USAGE.into());
            return;
        };

        let mut parts = raw.split_whitespace();
        let action = parts.next().unwrap_or_default();

        // `reload` re-reads config from disk (picking up `yolop mcp add`,
        // hand edits, or the agent's own config tools) and applies it live.
        if action == "reload" {
            if parts.next().is_some() {
                self.push_system(Self::MCP_USAGE.into());
                return;
            }
            self.reload_mcp_and_report(None).await;
            return;
        }

        // `login <name>` runs the interactive OAuth flow for a remote server.
        if action == "login" {
            let name = parts.next();
            if parts.next().is_some() {
                self.push_system(Self::MCP_USAGE.into());
                return;
            }
            match name {
                Some(name) => self.mcp_login(name).await,
                None => self.push_system(Self::MCP_USAGE.into()),
            }
            return;
        }

        let name = parts.next();
        let scope = match parts.next() {
            None | Some("global") => crate::mcp_config::McpConfigScope::Global,
            Some("workspace") => crate::mcp_config::McpConfigScope::Workspace,
            Some(other) => {
                self.push_system(format!("{} (unknown scope: {other})", Self::MCP_USAGE));
                return;
            }
        };
        if parts.next().is_some() {
            self.push_system(Self::MCP_USAGE.into());
            return;
        }
        let Some(name) = name else {
            self.push_system(Self::MCP_USAGE.into());
            return;
        };

        let store =
            crate::mcp_config::McpConfigStore::default_for_workspace(&self.startup.workspace_root);
        let result = match action {
            "enable" => store
                .set_enabled(scope, name, true)
                .map(|_| format!("enabled MCP server `{name}`")),
            "disable" => store
                .set_enabled(scope, name, false)
                .map(|_| format!("disabled MCP server `{name}`")),
            "remove" => store.remove(scope, name).map(|removed| {
                if removed {
                    format!("removed MCP server `{name}`")
                } else {
                    format!("MCP server `{name}` was not configured")
                }
            }),
            other => {
                self.push_system(format!("{} (unknown action: {other})", Self::MCP_USAGE));
                return;
            }
        };
        match result {
            Ok(message) => self.reload_mcp_and_report(Some(message)).await,
            Err(error) => self.push_system(format!("failed to update MCP config: {error}")),
        }
    }

    /// Run the interactive OAuth login flow for a remote MCP server and persist
    /// the resulting tokens. Because the runtime resolves credentials per turn
    /// through the shared auth provider, the token is used on the next turn with
    /// no restart or reload needed.
    async fn mcp_login(&mut self, name: &str) {
        let servers = crate::mcp_config::load_mcp_servers(&self.startup.workspace_root);
        let Some(server) = servers.get(name) else {
            self.push_system(format!(
                "MCP server `{name}` is not configured or is disabled; add it and run `/mcp reload` first"
            ));
            return;
        };
        if server.transport_type != everruns_core::McpServerTransportType::Http {
            self.push_system(format!(
                "`{name}` is a stdio server; OAuth login only applies to remote HTTP servers"
            ));
            return;
        }
        let url = server.url.clone();
        let provider_key = server
            .oauth_provider_id
            .clone()
            .unwrap_or_else(|| name.to_string());

        self.push_system(format!("opening browser for `{name}` OAuth login…"));
        match crate::mcp_oauth_login::login(&url, None, None).await {
            Ok(tokens) => {
                match crate::mcp_oauth::save_tokens(
                    &self.session.connections(),
                    &provider_key,
                    tokens,
                ) {
                    Ok(()) => self.push_system(format!(
                        "signed in to `{name}` — the token is active on your next message"
                    )),
                    Err(error) => self.push_system(format!(
                        "saved login for `{name}` failed to persist: {error}"
                    )),
                }
            }
            Err(error) => self.push_system(format!("`{name}` OAuth login failed: {error}")),
        }
    }

    /// Apply the on-disk MCP config to the live session and report the active
    /// set. `prelude`, when set, is printed first (e.g. the mutation summary).
    async fn reload_mcp_and_report(&mut self, prelude: Option<String>) {
        if let Some(message) = prelude {
            self.push_system(message);
        }
        match self.session.reload_mcp_servers().await {
            Ok(names) => {
                self.startup.mcp_server_names = names;
                if self.startup.mcp_server_names.is_empty() {
                    self.push_system("active MCP servers: none".into());
                } else {
                    self.push_system(format!(
                        "active MCP servers: {}",
                        self.startup.mcp_server_names.join(", ")
                    ));
                }
            }
            Err(error) => self.push_system(format!("failed to reload MCP servers: {error}")),
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

    /// Route a mouse-wheel event to the full-screen transcript scroll. Returns
    /// true when consumed. Uses the metrics recorded by the last full-screen
    /// draw so it need not re-run layout.
    fn handle_fullscreen_scroll(&mut self, kind: MouseEventKind) -> bool {
        let tuika_kind = match kind {
            MouseEventKind::ScrollUp => tuika::MouseKind::ScrollUp,
            MouseEventKind::ScrollDown => tuika::MouseKind::ScrollDown,
            _ => return false,
        };
        let (content_h, viewport_h) = self.scroll_metrics;
        let event = tuika::Event::Mouse(tuika::Mouse::at(tuika_kind, 0, 0));
        self.scroll.handle(&event, content_h, viewport_h).consumed()
    }

    fn handle_mouse(&mut self, mouse: MouseEvent, terminal_area: Rect) -> bool {
        if !matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
            return false;
        }
        if self.mouse_is_on_status(mouse, terminal_area) {
            self.set_status_layout(None);
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
            state.status_row_count(),
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

    fn ctrl_c_pending_exit(&self) -> bool {
        self.ctrl_c_pending_exit_at.is_some()
    }

    fn disarm_ctrl_c_pending_exit(&mut self) {
        self.ctrl_c_pending_exit_at = None;
    }

    fn disarm_ctrl_c_pending_exit_if_grace_elapsed(&mut self) {
        if let Some(armed_at) = self.ctrl_c_pending_exit_at
            && armed_at.elapsed() >= CTRL_C_EXIT_ARM_GRACE
        {
            self.ctrl_c_pending_exit_at = None;
        }
    }

    fn handle_ctrl_c(&mut self) {
        if !self.busy && !self.input_text().trim().is_empty() {
            self.reset_input();
            self.disarm_ctrl_c_pending_exit();
            return;
        }

        if self.ctrl_c_pending_exit() {
            self.abort_codex_login();
            self.ctrl_c_exit = true;
            self.should_quit = true;
            return;
        }

        self.ctrl_c_pending_exit_at = Some(Instant::now());
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
        self.turn_started_at = None;
        self.stream_preview = None;
        self.rx = None;
        self.turn_cancel = None;
        if self.native_progress {
            self.term_progress.clear();
        }
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

    async fn after_turn_user_ask_check(&mut self) {
        if !self.user_ask_enabled {
            return;
        }
        let session_id = self.session.session_id();
        if !self.user_ask_store.is_active(session_id) {
            return;
        }
        let result = match self
            .session
            .execute_command("ask", Some(USER_ASK_EVALUATE_ARG.to_string()))
            .await
        {
            Ok(result) => result,
            Err(err) => {
                self.push_system(format!("user ask evaluation failed: {err}"));
                return;
            }
        };
        if !result.success {
            self.push_system(format!("user ask evaluation failed: {}", result.message));
            return;
        }
        let evaluation = match parse_user_ask_evaluation(&result.message) {
            Ok(evaluation) => evaluation,
            Err(err) => {
                self.push_system(format!("user ask evaluation failed: {err}"));
                return;
            }
        };
        if evaluation.outcome == AskOutcome::Blocked {
            self.session
                .report_herdr_state(crate::capabilities::herdr::HerdrState::Blocked);
        }
        self.push_system(evaluation_status_message(&evaluation));
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
                        if result.success
                            && matches!(descriptor.name.as_str(), "rewind" | "undo" | "redo")
                            && result.message.starts_with("restored ")
                        {
                            self.refresh_after_checkpoint_restore(result.message).await;
                        } else if !result.message.is_empty() {
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
        self.startup.workspace_root = self.worktree.active_root();
        self.push_user(format!("!shell {command}"));
        let handle = self.session.run_shell(command, self.workspace_host.clone());
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
        self.turn_started_at = Some(Instant::now());
        self.stream_preview = None;
        if self.native_progress {
            // Turn length is unknown, so show the terminal's busy/indeterminate
            // indicator until the turn completes.
            self.term_progress.indeterminate();
        }
    }

    fn start_turn(&mut self, prompt: String) {
        match self.worktree.ensure_before_turn(&prompt) {
            Ok(true) => {
                self.startup.workspace_root = self.worktree.active_root();
                if let Some(notice) = self.worktree.switch_notice() {
                    self.push_system(notice);
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

/// Maximum `@file` completions surfaced at once.
const FILE_SUGGESTION_LIMIT: usize = 12;

/// File-path completions for an `@`-prefixed token in the composer, mirroring
/// the Codex `@file` mention. Returns `None` when the word being typed is not an
/// `@` mention (so command completion can take over). The returned `completion`
/// rebuilds the whole single-line input with the mention replaced, matching how
/// the Tab handler applies suggestions.
fn file_path_suggestions(line: &str, workspace_root: &Path) -> Option<Vec<CommandSuggestion>> {
    let (head, token) = split_trailing_token(line);
    let path_prefix = token.strip_prefix('@')?;
    let matches = list_path_completions(workspace_root, path_prefix, FILE_SUGGESTION_LIMIT);
    if matches.is_empty() {
        return None;
    }
    Some(
        matches
            .into_iter()
            .map(|rel| CommandSuggestion {
                completion: format!("{head}@{rel}"),
                label: format!("@{rel}"),
            })
            .collect(),
    )
}

/// Split a line into `(everything up to and including the last whitespace, last
/// token)`. For `"explain @src/ma"` this yields `("explain ", "@src/ma")`.
fn split_trailing_token(line: &str) -> (&str, &str) {
    match line.rfind(char::is_whitespace) {
        Some(idx) => line.split_at(idx + 1),
        None => ("", line),
    }
}

/// List workspace entries matching `prefix` (a path relative to the workspace
/// root, `/`-separated). Directories get a trailing `/` so the user can keep
/// descending. Hidden entries and `.git` are skipped unless explicitly typed.
fn list_path_completions(workspace_root: &Path, prefix: &str, limit: usize) -> Vec<String> {
    // Split the prefix into its directory part (already-typed dirs) and the
    // final filename fragment we match against.
    let (dir_part, frag) = match prefix.rfind('/') {
        Some(idx) => (&prefix[..idx + 1], &prefix[idx + 1..]),
        None => ("", prefix),
    };
    let scan_dir = workspace_root.join(dir_part);
    let Ok(entries) = std::fs::read_dir(&scan_dir) else {
        return Vec::new();
    };
    let want_hidden = frag.starts_with('.');
    let mut names: Vec<String> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == ".git" {
            continue;
        }
        if name.starts_with('.') && !want_hidden {
            continue;
        }
        if !name.starts_with(frag) {
            continue;
        }
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        let suffix = if is_dir { "/" } else { "" };
        names.push(format!("{dir_part}{name}{suffix}"));
    }
    // Directories first, then files, each alphabetical — a predictable order.
    names.sort_by(|a, b| {
        let a_dir = a.ends_with('/');
        let b_dir = b.ends_with('/');
        b_dir.cmp(&a_dir).then_with(|| a.cmp(b))
    });
    names.truncate(limit);
    names
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
        "  Enter send · Shift-Enter newline · Tab complete (cmds, @files) · ↑/↓ history · Ctrl+R search",
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
    use everruns_core::session_task::{
        CreateSessionTask, SessionTaskState, TASK_KIND_BACKGROUND_TOOL,
    };
    use everruns_core::tool_types::ToolCall;
    use everruns_core::{MessageId, SessionId, TurnId};

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
    fn code_fence_gets_language_aware_highlighting() {
        let mut lines: Vec<Line> = Vec::new();
        append_markdown_lines(
            &mut lines,
            "",
            Style::default(),
            "```rust\nfn demo() {}\n```",
            80,
        );
        let text: String = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect();
        assert!(
            text.contains("fn demo() {}"),
            "code content should render: {text:?}"
        );
        // Tree-sitter highlighting must color at least one token gold (keyword),
        // proving the language-aware path ran rather than the flat fallback.
        let has_keyword_color = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .any(|span| span.style.fg == Some(ACCENT_GOLD));
        assert!(
            has_keyword_color,
            "rust fence should be syntax-highlighted: {lines:?}"
        );
    }

    #[test]
    fn linkify_styles_urls_and_trims_trailing_punctuation() {
        let spans = linkify("see https://example.com/docs, then stop");
        let url = spans
            .iter()
            .find(|s| s.content.as_ref() == "https://example.com/docs")
            .expect("url span present");
        assert_eq!(url.style.fg, Some(ACCENT_BLUE));
        assert!(url.style.add_modifier.contains(Modifier::UNDERLINED));
        // The trailing comma stays as plain text, not part of the link.
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "see https://example.com/docs, then stop");
    }

    #[test]
    fn linkify_leaves_plain_text_untouched() {
        let spans = linkify("no links here");
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].style.fg, None);
        assert_eq!(spans[0].content.as_ref(), "no links here");
    }

    #[test]
    fn transcript_paragraph_links_urls() {
        let mut lines: Vec<Line> = Vec::new();
        append_markdown_lines(
            &mut lines,
            "",
            Style::default(),
            "Visit https://rust-lang.org for docs",
            120,
        );
        let has_link = lines.iter().flat_map(|line| line.spans.iter()).any(|span| {
            span.content.as_ref() == "https://rust-lang.org"
                && span.style.add_modifier.contains(Modifier::UNDERLINED)
        });
        assert!(
            has_link,
            "paragraph URL should be styled as a link: {lines:?}"
        );
    }

    #[test]
    fn history_search_preview_line_shows_query_and_no_match_flag() {
        let matched = HistorySearchView {
            query: "deploy".to_string(),
            matched: true,
        };
        let rendered = line_text(&history_search_preview_line(&matched, 96));
        assert!(rendered.starts_with("(reverse-search) 'deploy'"));
        assert!(!rendered.contains("no match"));

        let missed = HistorySearchView {
            query: "zzz".to_string(),
            matched: false,
        };
        let rendered = line_text(&history_search_preview_line(&missed, 96));
        assert!(rendered.contains("'zzz'"));
        assert!(rendered.contains("no match"));
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
    fn split_trailing_token_isolates_the_last_word() {
        assert_eq!(
            split_trailing_token("explain @src/ma"),
            ("explain ", "@src/ma")
        );
        assert_eq!(split_trailing_token("@src"), ("", "@src"));
        assert_eq!(split_trailing_token(""), ("", ""));
    }

    #[test]
    fn list_path_completions_scopes_by_prefix_and_hides_dotfiles() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::create_dir(root.join("src")).unwrap();
        std::fs::create_dir(root.join(".git")).unwrap();
        std::fs::write(root.join("src/main.rs"), b"").unwrap();
        std::fs::write(root.join("src/lib.rs"), b"").unwrap();
        std::fs::write(root.join("README.md"), b"").unwrap();
        std::fs::write(root.join(".env"), b"").unwrap();

        // Bare prefix: directories first, dotfiles and .git hidden.
        let top = list_path_completions(root, "", 12);
        assert_eq!(top, vec!["src/".to_string(), "README.md".to_string()]);

        // Into a directory.
        let inside = list_path_completions(root, "src/", 12);
        assert_eq!(
            inside,
            vec!["src/lib.rs".to_string(), "src/main.rs".to_string()]
        );

        // Filename fragment filters within a directory.
        let filtered = list_path_completions(root, "src/ma", 12);
        assert_eq!(filtered, vec!["src/main.rs".to_string()]);

        // Explicitly typing a dot reveals dotfiles.
        let dotted = list_path_completions(root, ".e", 12);
        assert_eq!(dotted, vec![".env".to_string()]);
    }

    #[test]
    fn file_path_suggestions_only_fire_on_at_mentions() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::write(root.join("hello.txt"), b"").unwrap();

        assert!(file_path_suggestions("no mention here", root).is_none());

        let suggestions =
            file_path_suggestions("please read @hel", root).expect("mention suggestions");
        assert_eq!(suggestions.len(), 1);
        assert_eq!(suggestions[0].label, "@hello.txt");
        // The completion rebuilds the whole line with the mention replaced.
        assert_eq!(suggestions[0].completion, "please read @hello.txt");
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
        assert_eq!(chrome_height(1, 1, false), 4);
        assert_eq!(chrome_height(1, 1, true), 5);
        assert_eq!(chrome_height(1, 4, false), 7);
        assert_eq!(chrome_height(1, 4, true), 8);
        assert_eq!(chrome_height(3, 1, false), 5);
        assert_eq!(chrome_height(3, 4, false), 8);
        assert_eq!(chrome_height(4, 1, false), 6);
        assert_eq!(chrome_height(4, 4, false), 9);
    }

    #[test]
    fn chrome_dimensions_clamp_input_to_visible_frame() {
        assert_eq!(chrome_dimensions(7, MAX_INPUT_HEIGHT, 4, false), (7, 1));
        assert_eq!(chrome_dimensions(5, MAX_INPUT_HEIGHT, 1, false), (5, 3));
        assert_eq!(chrome_dimensions(0, MAX_INPUT_HEIGHT, 1, false), (0, 0));
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
                        let layout = app_layout_for_frame(
                            frame,
                            desired_input,
                            status_layout.base_row_count(),
                            false,
                        );
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
                arguments: serde_json::json!({ "path": "/repo/Cargo.toml" }),
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
                message_id: MessageId::new(),
                model: None,
                iteration: Some(3),
                phase: None,
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
        let message_id = MessageId::new();

        let delta_event = RuntimeEvent::new(
            SessionId::new(),
            EventContext::empty(),
            OutputMessageDeltaData {
                turn_id,
                message_id,
                delta: "Hel".to_string(),
                accumulated: "Hel".to_string(),
                phase: None,
            },
        );
        handle_live_event(&delta_event, &mut emitted, &mut router, &tx);

        let more = RuntimeEvent::new(
            SessionId::new(),
            EventContext::empty(),
            OutputMessageDeltaData {
                turn_id,
                message_id,
                delta: "lo, world".to_string(),
                accumulated: "Hello, world".to_string(),
                phase: None,
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
                text: "--- /repo/src/app.rs (before)\n+++ /repo/src/app.rs (after)\n@@ -1 +1 @@\n-old\n+new\n unchanged".to_string(),
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
    fn stderr_lines_use_a_red_dot_marker() {
        let mut lines = Vec::new();
        append_chat_lines(
            &mut lines,
            &ChatLine {
                author: Author::Stderr,
                text: "stderr:\nOperation not permitted".to_string(),
            },
            96,
        );

        assert!(line_text(&lines[0]).starts_with("         ● stderr:"));
        assert_eq!(lines[0].spans[0].style.fg, Some(ERROR_RED));
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
                text: "+changed line with a very long path /repo/src/app/render.rs".into(),
            },
            ChatLine {
                author: Author::ToolDetail,
                text: "stdout: output with a long uninterrupted token abcdefghijklmnopqrstuvwxyz"
                    .into(),
            },
            ChatLine {
                author: Author::Stderr,
                text: "stderr: permission denied for a deliberately-long-path".into(),
            },
            ChatLine {
                author: Author::Sandbox,
                text: "native sandbox likely blocked this operation".into(),
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

    #[test]
    fn recent_transcript_mirror_excludes_startup_banner_system_lines() {
        let lines = vec![
            ChatLine {
                author: Author::System,
                text: "workspace: /tmp".into(),
            },
            ChatLine {
                author: Author::User,
                text: "hello".into(),
            },
        ];
        assert!(!include_line_in_recent_transcript_mirror(
            &lines[0], 0, &lines
        ));
        assert!(include_line_in_recent_transcript_mirror(
            &lines[1], 1, &lines
        ));
    }

    #[test]
    fn recent_transcript_mirror_includes_session_system_notices_after_chat() {
        let lines = vec![
            ChatLine {
                author: Author::System,
                text: "workspace: /tmp".into(),
            },
            ChatLine {
                author: Author::User,
                text: "hello".into(),
            },
            ChatLine {
                author: Author::Assistant,
                text: "hi".into(),
            },
            ChatLine {
                author: Author::System,
                text: "attached clipboard image #1 (640x480 PNG)".into(),
            },
        ];
        assert!(!include_line_in_recent_transcript_mirror(
            &lines[0], 0, &lines
        ));
        assert!(include_line_in_recent_transcript_mirror(
            &lines[3], 3, &lines
        ));
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
    use ratatui::TerminalOptions;
    use ratatui::Viewport;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Position;

    struct InlineComposerRender {
        lines: Vec<String>,
        scrollback_height: u16,
        viewport_bottom: u16,
    }

    /// Render the composer through the same inline viewport + bottom anchoring
    /// path the real TUI uses (`Terminal::with_options` + `maybe_reanchor`).
    fn inline_terminal(width: u16, height: u16) -> Terminal<TestBackend> {
        let mut backend = TestBackend::new(width, height);
        backend
            .set_cursor_position(Position { x: 0, y: 1 })
            .unwrap();
        let mut terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Inline(COMPOSER_VIEWPORT_HEIGHT),
            },
        )
        .expect("terminal");
        maybe_reanchor_inline_viewport(&mut terminal).expect("anchor inline viewport");
        terminal
    }

    fn inline_transcript_rows(terminal: &mut Terminal<TestBackend>) -> Vec<String> {
        let viewport_top = terminal.get_frame().area().y;
        let buffer = terminal.backend().buffer();
        let height = buffer.area.height;
        let width = buffer.area.width;
        (viewport_top..height)
            .map(|y| {
                let mut row = String::with_capacity(width as usize);
                for x in 0..width {
                    row.push_str(buffer[(x, y)].symbol());
                }
                row.trim_end().to_string()
            })
            .filter(|row| row.contains('›'))
            .collect()
    }

    fn render_inline_composer(
        app: &mut App,
        width: u16,
        terminal_height: u16,
        cursor_row: u16,
    ) -> InlineComposerRender {
        let mut backend = TestBackend::new(width, terminal_height);
        backend
            .set_cursor_position(Position {
                x: 0,
                y: cursor_row,
            })
            .unwrap();
        let mut terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Inline(COMPOSER_VIEWPORT_HEIGHT),
            },
        )
        .expect("terminal");
        maybe_reanchor_inline_viewport(&mut terminal).expect("anchor inline viewport");
        terminal.draw(|f| draw(f, app)).expect("draw");
        let viewport = terminal.get_frame().area();
        let buffer = terminal.backend().buffer();
        let lines = (0..buffer.area.height)
            .map(|y| {
                let mut row = String::with_capacity(buffer.area.width as usize);
                for x in 0..buffer.area.width {
                    row.push_str(buffer[(x, y)].symbol());
                }
                row.trim_end().to_string()
            })
            .collect();
        InlineComposerRender {
            lines,
            scrollback_height: terminal.backend().scrollback().area.height,
            viewport_bottom: viewport.y.saturating_add(viewport.height),
        }
    }

    fn presentation_state_idle() -> PresentationState {
        PresentationState {
            stream_preview: None,
            busy: false,
            turn_activity: None,
            model_id: "gpt-5.5".to_string(),
            provider_name: "openai".to_string(),
            reasoning_effort: Some("medium".to_string()),
            session_id: SessionId::from_seed(770001).to_string(),
            lines_count: 3,
            session_tokens: None,
            turn_elapsed_secs: None,
            context_used_tokens: None,
            context_window_tokens: None,
            status_layout: StatusLayout::Compact,
            hooks_summary: "none".to_string(),
            approval_mode: "normal".to_string(),
            background: None,
            goal_indicator: None,
            ask_indicator: None,
            worktree_compact: None,
            worktree_expanded: None,
            extension_status: Vec::new(),
        }
    }

    fn view_state_idle() -> ViewState {
        ViewState {
            presentation: presentation_state_idle(),
            command_suggestions: Vec::new(),
            history_search: None,
            busy_frame: 0,
        }
    }

    #[test]
    fn status_bar_shows_background_task_count() {
        let state = ViewState {
            presentation: PresentationState {
                status_layout: StatusLayout::Expanded,
                background: Some(crate::session_tasks_view::BackgroundCounts {
                    running: 1,
                    scheduled: 0,
                    total: 2,
                }),
                ..presentation_state_idle()
            },
            ..view_state_idle()
        };
        let lines = render_chrome_lines(&state, 120, 8).join("\n");
        assert!(
            lines.contains("bg"),
            "status should show bg segment: {lines}"
        );
        assert!(
            lines.contains("1 running · 0 scheduled · 2 total"),
            "status should distinguish running, scheduled, and total: {lines}"
        );
    }

    #[test]
    fn status_bar_hides_background_segment_when_no_tasks() {
        let state = ViewState {
            presentation: PresentationState {
                status_layout: StatusLayout::Expanded,
                background: None,
                ..presentation_state_idle()
            },
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fullscreen_matches_regular_presentation() {
        let mut test = app_with_llmsim().await;
        // Dismiss the first-run setup overlay so the base transcript is visible;
        // overlay compositing is covered by the tuika suite.
        test.app.setup = None;
        test.app.push_user("hello from user".to_string());
        test.app.input.insert_str("draft reply");

        test.app.set_render_mode(RenderMode::Inline);
        let regular = render_app_lines(&mut test.app, 60, 20);
        test.app.set_render_mode(RenderMode::Fullscreen);
        let fullscreen = render_app_lines(&mut test.app, 60, 20);
        let joined = fullscreen.join("\n");

        assert_eq!(fullscreen, regular);
        assert!(joined.contains("hello from user"));
        assert!(joined.contains("draft reply"));
        assert!(joined.contains("llmsim"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fullscreen_scroll_wheel_unsticks_from_bottom() {
        let mut test = app_with_llmsim().await;
        test.app.set_render_mode(RenderMode::Fullscreen);
        for i in 0..80 {
            test.app.push_user(format!("line {i}"));
        }
        // Draw once so scroll metrics reflect the tall transcript.
        let _ = render_app_lines(&mut test.app, 40, 12);
        assert!(test.app.scroll.is_stuck_to_bottom());
        // A wheel-up should be consumed and detach from the bottom.
        assert!(test.app.handle_fullscreen_scroll(MouseEventKind::ScrollUp));
        assert!(!test.app.scroll.is_stuck_to_bottom());
    }

    /// Render the same app state in regular and full-screen mode and return
    /// `(fullscreen_rows, regular_rows)`. Full-screen now routes through the
    /// shared presentation renderer (`render::draw_shared`), so the buffers
    /// must be byte-for-byte identical — this is the helper the
    /// unified-presentation parity tests build on.
    fn render_regular_and_fullscreen(
        app: &mut App,
        width: u16,
        height: u16,
    ) -> (Vec<String>, Vec<String>) {
        app.set_render_mode(RenderMode::Inline);
        let regular = render_app_lines(app, width, height);
        app.set_render_mode(RenderMode::Fullscreen);
        let fullscreen = render_app_lines(app, width, height);
        (fullscreen, regular)
    }

    /// `fullscreen_matches_regular_presentation` pins parity for the base
    /// transcript state; this widens that guard to the states most likely to
    /// tempt a mode-specific rendering fork — a busy turn with a token count, an
    /// extension-pushed status segment, an open setup overlay, an overflowing
    /// transcript, and an oversized paste in the composer. If any of these ever
    /// diverges between the two modes, the unified-presentation invariant has
    /// regressed.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fullscreen_shares_regular_presentation_across_states() {
        type Mutate = fn(&mut App);
        // (name, state mutation, a substring the frame must contain — empty to
        // skip the content check, e.g. for the overlay case whose exact copy
        // lives elsewhere).
        let cases: &[(&str, Mutate, &str)] = &[
            (
                "busy turn with tokens",
                |app| {
                    app.busy = true;
                    app.session_tokens = Some(4096);
                    app.push_user("hello from user".to_string());
                },
                "hello from user",
            ),
            (
                "extension status segment",
                |app| {
                    app.extension_status
                        .insert("ext:lsp".to_string(), "indexing".to_string());
                    app.push_user("hello from user".to_string());
                },
                "hello from user",
            ),
            (
                "open setup overlay",
                |app| {
                    app.setup = Some(SetupStep::Provider { selected: 0 });
                },
                "",
            ),
            (
                "transcript taller than the viewport",
                |app| {
                    for i in 0..80 {
                        app.push_user(format!("line {i}"));
                    }
                },
                "",
            ),
            (
                "oversized paste in the composer",
                |app| {
                    let paste = (0..40)
                        .map(|i| format!("pasteline{i}"))
                        .collect::<Vec<_>>()
                        .join("\n");
                    app.input.insert_str(paste);
                },
                "",
            ),
        ];

        for (name, mutate, needle) in cases {
            let mut test = app_with_llmsim().await;
            // Start from a dismissed first-run overlay; cases that want it re-open
            // it themselves.
            test.app.setup = None;
            mutate(&mut test.app);

            let (fullscreen, regular) = render_regular_and_fullscreen(&mut test.app, 60, 20);

            assert_eq!(
                fullscreen, regular,
                "full-screen must match regular presentation for state: {name}"
            );
            assert!(
                fullscreen.iter().any(|row| !row.is_empty()),
                "state {name} should render a non-blank frame (parity must not be vacuous)"
            );
            if !needle.is_empty() {
                let joined = fullscreen.join("\n");
                assert!(
                    joined.contains(needle),
                    "state {name} should render {needle:?}: {joined}"
                );
            }
        }
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

    /// Seed a synthetic everruns completion signal onto the app's wake channel,
    /// standing in for the platform-store `send_message` a finished
    /// `spawn_background` run makes. Returns the sender so the caller can push
    /// more (or drop it to close the channel).
    fn seed_background_wake(app: &mut App, message: &str) -> mpsc::UnboundedSender<String> {
        let (tx, rx) = mpsc::unbounded_channel::<String>();
        app.background_wake = rx;
        tx.send(message.to_string()).expect("seed wake");
        tx
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn proactive_wake_starts_turn_when_background_task_finishes() {
        let mut test = app_with_llmsim().await;
        let app = &mut test.app;
        // The setup/onboarding overlay opens when no provider credentials are
        // configured (the case in CI, which has no API keys). Wake is correctly
        // suppressed while it's open, so clear it to exercise the idle path.
        app.setup = None;

        // Idle with no signal: nothing to wake for.
        assert!(!app.maybe_wake_from_background_channel());

        // A completion signal arrives on the wake channel ⇒ a turn is started.
        let _tx = seed_background_wake(app, "Background run completed.\n- run_id: bg_1");
        assert!(
            app.maybe_wake_from_background_channel(),
            "a finished background task should wake the agent"
        );
        assert!(app.busy, "proactive wake must start a turn");
        assert!(
            app.lines.iter().any(|l| l.text.contains("waking")),
            "a notice explaining the auto-wake should be shown"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn proactive_wake_suppressed_during_setup_overlay() {
        let mut test = app_with_llmsim().await;
        let app = &mut test.app;
        // With the setup overlay open, a finished task must NOT auto-start a turn
        // and must NOT consume the signal (so it can still wake once closed).
        let _tx = seed_background_wake(app, "Background run completed.\n- run_id: bg_1");
        app.setup = Some(SetupStep::Provider { selected: 0 });
        assert!(
            !app.maybe_wake_from_background_channel(),
            "no wake during setup"
        );
        assert!(!app.busy);
        app.setup = None;
        assert!(
            app.maybe_wake_from_background_channel(),
            "wake should fire once the overlay closes — the signal wasn't consumed"
        );
        assert!(app.busy);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn proactive_wake_disabled_by_setting_does_not_start_turn() {
        let mut test = app_with_llmsim().await;
        let app = &mut test.app;
        app.setup = None;
        app.settings
            .set_proactive_wake(false)
            .expect("disable wake");

        let _tx = seed_background_wake(app, "Background run completed.\n- run_id: bg_1");
        // Setting off: no turn, but the completion is still surfaced once.
        assert!(!app.maybe_wake_from_background_channel());
        assert!(!app.busy, "wake setting off must not start a turn");
        assert!(
            app.lines
                .iter()
                .any(|l| l.text.contains("background task finished")),
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
        assert!(app.ctrl_c_pending_exit());

        app.handle_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL))
            .await;
        assert!(
            !app.ctrl_c_pending_exit(),
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
    async fn background_panel_renders_everruns_session_task_rows() {
        let mut test = app_with_llmsim().await;
        let app = &mut test.app;
        app.setup = None;
        app.task_registry
            .create(CreateSessionTask {
                session_id: app.session.session_id(),
                id: Some("task_panel".to_string()),
                kind: TASK_KIND_BACKGROUND_TOOL.to_string(),
                display_name: "write marker".to_string(),
                spec: serde_json::json!({ "tool": "bash", "command": "echo hi" }),
                state: SessionTaskState::Running,
                links: Default::default(),
                wake_policy: Default::default(),
            })
            .await
            .expect("create session task");
        app.refresh_session_tasks().await;
        app.background_panel = Some(0);

        assert_eq!(
            app.presentation_state().background,
            Some(crate::session_tasks_view::BackgroundCounts {
                running: 1,
                scheduled: 0,
                total: 1,
            })
        );
        let lines = render_app_lines(app, 120, 20).join("\n");
        assert!(lines.contains("Background tasks"), "panel header: {lines}");
        assert!(
            lines.contains("background task(s): 1 running, 0 scheduled"),
            "panel should summarize session tasks: {lines}"
        );
        assert!(
            lines.contains("[task_panel] background_tool running: write marker"),
            "panel should list the session task row: {lines}"
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
            self.pump_ui_commands_for_test().await;
        }

        async fn pump_ui_commands_for_test(&mut self) {
            while let Ok(command) = self.ui_rx.try_recv() {
                self.apply_ui_command(command).await;
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
                        Ok(TurnEvent::ContextUsed(used)) => {
                            self.context_used_tokens = Some(used);
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
    async fn undo_command_refreshes_visible_branch_and_restores_prompt() {
        let mut fixture = app_with_llmsim().await;
        let app = &mut fixture.app;
        app.setup = None;
        app.lines.clear();

        for prompt in ["first turn", "second turn"] {
            app.set_input_text(prompt.to_string());
            app.submit_input().await;
            app.pump_turn_until_idle_for_test().await;
        }

        let preview = app
            .session
            .execute_command("undo", None)
            .await
            .expect("preview undo");
        let token = preview
            .message
            .lines()
            .last()
            .and_then(|line| line.split('`').nth(1))
            .expect("confirmation token");
        let restored = app
            .session
            .execute_command("undo", Some(format!("confirm {token}")))
            .await
            .expect("confirm undo");
        app.refresh_after_checkpoint_restore(restored.message).await;

        assert!(
            app.lines
                .iter()
                .any(|line| { matches!(line.author, Author::User) && line.text == "first turn" })
        );
        assert!(
            !app.lines
                .iter()
                .any(|line| { matches!(line.author, Author::User) && line.text == "second turn" })
        );
        assert_eq!(app.input_text(), "second turn");

        let session_id = fixture.app.session.session_id();
        let workspace_root = fixture._workspace.path().to_path_buf();
        let sessions_root = fixture._sessions.path().to_path_buf();
        drop(fixture.app);
        let settings = std::sync::Arc::new(crate::settings::SettingsStore::open(
            sessions_root.join("settings.toml"),
        ));
        let resumed = crate::runtime::build_with_options(
            workspace_root,
            crate::runtime::ProviderChoice::Sim,
            Some(session_id),
            sessions_root,
            settings,
            crate::runtime::BuildOptions::default(),
        )
        .await
        .expect("resume rewound session");
        let resumed_text = resumed
            .handles
            .runtime
            .messages(session_id)
            .await
            .expect("resumed messages")
            .into_iter()
            .filter_map(|message| message.text().map(str::to_string))
            .collect::<Vec<_>>();
        assert!(resumed_text.iter().any(|text| text == "first turn"));
        assert!(!resumed_text.iter().any(|text| text == "second turn"));
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
        app.pump_ui_commands_for_test().await;

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
        app.pump_ui_commands_for_test().await;

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
    async fn inline_viewport_mirrors_session_system_notices_after_chat() {
        let mut fixture = app_with_llmsim().await;
        let app = &mut fixture.app;
        app.setup = None;
        app.lines.clear();
        app.push_user("first question".into());
        app.lines.push(ChatLine {
            author: Author::Assistant,
            text: "first answer".into(),
        });
        app.push_system("attached clipboard image #1 (640x480 PNG)".into());

        let rows = render_app_lines(app, 96, COMPOSER_VIEWPORT_HEIGHT);

        assert!(
            rows.iter()
                .any(|row| row.contains("attached clipboard image #1")),
            "session system notices should mirror above the composer: {rows:?}"
        );
        assert!(
            !rows.iter().any(|row| row.contains("workspace:")),
            "startup banner should stay out of the inline viewport: {rows:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn incremental_system_notice_stays_adjacent_to_composer_after_flush() {
        let mut fixture = app_with_llmsim().await;
        let app = &mut fixture.app;
        app.setup = None;
        app.lines.clear();
        app.push_user("first question".into());
        app.lines.push(ChatLine {
            author: Author::Assistant,
            text: "first answer".into(),
        });

        let mut terminal = inline_terminal(100, 60);
        app.flush_transcript(&mut terminal)
            .expect("flush prior turn");
        app.push_system("attached clipboard image #1 (640x480 PNG)".into());
        app.flush_transcript(&mut terminal)
            .expect("flush image notice");
        terminal.draw(|f| draw(f, app)).expect("draw after notice");

        let inline_rows = inline_transcript_rows(&mut terminal);
        let notice_row = inline_rows
            .iter()
            .position(|row| row.contains("attached clipboard image #1"))
            .expect("image paste notice in inline viewport");

        assert!(
            notice_row + 1 >= inline_rows.len().saturating_sub(1),
            "incremental system notice should sit near the composer, not above a mirrored tail: inline={inline_rows:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn incremental_turn_failed_notice_stays_adjacent_to_composer_after_flush() {
        let mut fixture = app_with_llmsim().await;
        let app = &mut fixture.app;
        app.setup = None;
        app.lines.clear();
        app.push_user("run it".into());
        app.lines.push(ChatLine {
            author: Author::Assistant,
            text: "done".into(),
        });

        let mut terminal = inline_terminal(100, 60);
        app.flush_transcript(&mut terminal)
            .expect("flush prior turn");
        app.push_system("turn failed: provider timeout".into());
        app.flush_transcript(&mut terminal)
            .expect("flush turn notice");
        terminal.draw(|f| draw(f, app)).expect("draw after notice");

        let inline_rows = inline_transcript_rows(&mut terminal);
        assert!(
            inline_rows
                .iter()
                .any(|row| row.contains("turn failed: provider timeout")),
            "turn-failed notice should mirror above the composer: {inline_rows:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn inline_composer_anchor_leaves_scrollback_empty_in_tall_terminal() {
        let mut fixture = app_with_llmsim().await;
        let app = &mut fixture.app;
        app.setup = None;

        let rendered = render_inline_composer(app, 100, 60, 1);

        assert_eq!(
            rendered.viewport_bottom, 60,
            "inline viewport should sit flush with the terminal bottom"
        );
        assert!(
            rendered.scrollback_height < 20,
            "cursor+resize anchoring should not push a tall blank band into scrollback (height={})",
            rendered.scrollback_height
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn inline_composer_keeps_transcript_adjacent_after_scrollback_flush() {
        let mut fixture = app_with_llmsim().await;
        let app = &mut fixture.app;
        app.setup = None;
        app.lines.clear();
        app.push_user("hello composer gap".into());
        app.lines.push(ChatLine {
            author: Author::Assistant,
            text: "Latest answer tail".into(),
        });
        app.printed_lines = app.lines.len();

        let rendered = render_inline_composer(app, 100, 60, 1);
        let answer_row = rendered
            .lines
            .iter()
            .position(|row| row.contains("Latest answer tail"))
            .expect("recent answer rendered");
        let separator_row = rendered
            .lines
            .iter()
            .position(|row| row.contains("Enter to send"))
            .expect("message separator rendered");

        assert_eq!(
            answer_row + 1,
            separator_row,
            "flushed transcript should stay adjacent to the composer chrome: {:?}",
            rendered.lines
        );
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
    async fn inline_viewport_mirrors_recent_tail_after_scrollback_flush() {
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
            rows.iter().any(|row| row.contains("Do something")),
            "recent user transcript should stay visible above the composer: {rows:?}"
        );
        assert!(
            rows.iter().any(|row| row.contains("Done.")),
            "recent assistant transcript should stay visible above the composer: {rows:?}"
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
        assert!(app.ctrl_c_pending_exit(), "first Ctrl-C should arm exit");
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
            !app.ctrl_c_pending_exit(),
            "clearing input should not arm exit"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn typing_within_ctrl_c_grace_keeps_exit_prompt_armed() {
        let mut fixture = app_with_llmsim().await;
        let app = &mut fixture.app;
        app.setup = None;

        app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
            .await;
        app.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::empty()))
            .await;

        assert!(
            app.ctrl_c_pending_exit(),
            "typing during the grace window should not disarm exit"
        );
        assert!(!app.should_quit);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn typing_after_ctrl_c_grace_disarms_exit_prompt() {
        let mut fixture = app_with_llmsim().await;
        let app = &mut fixture.app;
        app.setup = None;

        app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
            .await;
        tokio::time::sleep(CTRL_C_EXIT_ARM_GRACE + Duration::from_millis(50)).await;
        app.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::empty()))
            .await;

        assert!(
            !app.ctrl_c_pending_exit(),
            "typing after the grace window should disarm the pending exit prompt"
        );
        assert!(!app.should_quit);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn second_ctrl_c_within_grace_exits_after_prompt() {
        let mut fixture = app_with_llmsim().await;
        let app = &mut fixture.app;
        app.setup = None;
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);

        app.handle_key(ctrl_c).await;
        tokio::time::sleep(Duration::from_secs(2)).await;
        app.handle_key(ctrl_c).await;

        assert!(app.should_quit, "second Ctrl-C within grace should quit");
        assert!(app.ctrl_c_exit, "second Ctrl-C should count as Ctrl-C exit");
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
    async fn mcp_reload_applies_config_live_without_restart() {
        let mut fixture = app_with_llmsim().await;
        let app = &mut fixture.app;

        // The workspace declares no servers at startup.
        assert!(!app.startup.mcp_server_names.iter().any(|n| n == "docs"));

        // Write a workspace `.mcp.json` after startup, then `/mcp reload`.
        std::fs::write(
            app.startup.workspace_root.join(".mcp.json"),
            serde_json::to_vec_pretty(&serde_json::json!({
                "mcpServers": { "docs": { "type": "http", "url": "https://example.com/mcp" } }
            }))
            .unwrap(),
        )
        .expect("write .mcp.json");
        app.lines.clear();
        app.dispatch_command_for_test("mcp reload").await;

        // The server is now live on the session and reported as active — and
        // the old "restart required" guidance is gone.
        assert!(app.startup.mcp_server_names.iter().any(|n| n == "docs"));
        assert!(
            app.lines
                .iter()
                .any(|line| line.text.contains("active MCP servers: docs")),
            "reload should report the active set: {:?}",
            app.lines
        );
        assert!(
            !app.lines.iter().any(|line| line.text.contains("restart")),
            "reload must not tell the user to restart: {:?}",
            app.lines
        );

        // Disabling via `/mcp` also applies live: the server drops out.
        app.lines.clear();
        app.dispatch_command_for_test("mcp disable docs workspace")
            .await;
        assert!(
            !app.startup.mcp_server_names.iter().any(|n| n == "docs"),
            "disabling a server removes it from the live set: {:?}",
            app.startup.mcp_server_names
        );
        assert!(
            app.lines
                .iter()
                .any(|line| line.text.contains("active MCP servers")),
            "disable should report the active set: {:?}",
            app.lines
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mcp_login_rejects_unknown_and_stdio_servers() {
        let mut fixture = app_with_llmsim().await;
        let app = &mut fixture.app;

        // Unknown server: guidance to add it first, no browser attempt.
        app.lines.clear();
        app.dispatch_command_for_test("mcp login ghost").await;
        assert!(
            app.lines
                .iter()
                .any(|line| line.text.contains("`ghost` is not configured")),
            "unknown server should be reported: {:?}",
            app.lines
        );

        // A stdio server is not eligible for OAuth login.
        std::fs::write(
            app.startup.workspace_root.join(".mcp.json"),
            serde_json::to_vec_pretty(&serde_json::json!({
                "mcpServers": { "fs": { "type": "stdio", "command": "true" } }
            }))
            .unwrap(),
        )
        .expect("write .mcp.json");
        app.lines.clear();
        app.dispatch_command_for_test("mcp login fs").await;
        assert!(
            app.lines
                .iter()
                .any(|line| line.text.contains("stdio server")
                    && line.text.contains("OAuth login only applies")),
            "stdio server should be rejected before any browser flow: {:?}",
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
    async fn up_down_recall_previous_prompts() {
        let mut fixture = app_with_llmsim().await;
        let app = &mut fixture.app;
        app.setup = None;
        app.history.record("first");
        app.history.record("second");

        let up = || KeyEvent::new(KeyCode::Up, KeyModifiers::empty());
        let down = || KeyEvent::new(KeyCode::Down, KeyModifiers::empty());

        // Up from an empty composer recalls newest-first.
        app.handle_key(up()).await;
        assert_eq!(app.input_text(), "second");
        app.handle_key(up()).await;
        assert_eq!(app.input_text(), "first");
        // Already at the oldest entry: Up holds instead of clearing.
        app.handle_key(up()).await;
        assert_eq!(app.input_text(), "first");

        // Down walks back toward newer, then restores the (empty) draft.
        app.handle_key(down()).await;
        assert_eq!(app.input_text(), "second");
        app.handle_key(down()).await;
        assert_eq!(app.input_text(), "");
        assert!(!app.history.is_browsing());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn up_does_not_recall_over_an_unsent_draft() {
        let mut fixture = app_with_llmsim().await;
        let app = &mut fixture.app;
        app.setup = None;
        app.history.record("old prompt");

        // A non-empty, freshly-typed draft must not be clobbered by recall.
        for ch in ['h', 'i'] {
            app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::empty()))
                .await;
        }
        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::empty()))
            .await;
        assert_eq!(app.input_text(), "hi");
        assert!(!app.history.is_browsing());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn submitting_a_prompt_records_it_for_recall() {
        let mut fixture = app_with_llmsim().await;
        let app = &mut fixture.app;
        app.setup = None;

        // `/cwd` is a client command: it records into history but starts no turn,
        // so the app stays idle and recall stays reachable.
        for ch in ['/', 'c', 'w', 'd'] {
            app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::empty()))
                .await;
        }
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()))
            .await;

        assert_eq!(app.input_text(), "");
        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::empty()))
            .await;
        assert_eq!(app.input_text(), "/cwd");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fullscreen_renders_at_mention_suggestions() {
        let mut fixture = app_with_llmsim().await;
        let app = &mut fixture.app;
        app.setup = None;
        app.set_render_mode(RenderMode::Fullscreen);
        let root = app.startup.workspace_root.clone();
        std::fs::write(root.join("hello.txt"), b"hi").expect("write file");

        for ch in ['@', 'h', 'e', 'l'] {
            app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::empty()))
                .await;
        }
        let rows = render_app_lines(app, 100, 24);
        assert!(
            rows.iter().any(|row| row.contains("@hello.txt")),
            "fullscreen should render the @file hint row: {rows:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fullscreen_renders_reverse_search_prompt() {
        let mut fixture = app_with_llmsim().await;
        let app = &mut fixture.app;
        app.setup = None;
        app.set_render_mode(RenderMode::Fullscreen);
        app.history.record("deploy prod");

        app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL))
            .await;
        let rows = render_app_lines(app, 100, 24);
        assert!(
            rows.iter().any(|row| row.contains("reverse-search")),
            "fullscreen should render the Ctrl+R search prompt: {rows:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn at_mention_completes_workspace_files() {
        let mut fixture = app_with_llmsim().await;
        let app = &mut fixture.app;
        app.setup = None;
        let root = app.startup.workspace_root.clone();
        std::fs::write(root.join("hello.txt"), b"hi").expect("write file");

        // Type "@hel" — the suggestion row should offer the matching file.
        for ch in ['@', 'h', 'e', 'l'] {
            app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::empty()))
                .await;
        }
        let suggestions = app.suggestions();
        assert!(
            suggestions.iter().any(|s| s.label == "@hello.txt"),
            "expected @hello.txt among {suggestions:?}"
        );

        // Tab accepts the first suggestion, completing the mention in place.
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::empty()))
            .await;
        assert_eq!(app.input_text(), "@hello.txt");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ctrl_r_reverse_search_matches_narrows_and_accepts() {
        let mut fixture = app_with_llmsim().await;
        let app = &mut fixture.app;
        app.setup = None;
        for entry in ["deploy staging", "run tests", "deploy prod"] {
            app.history.record(entry);
        }

        // Ctrl+R opens search and previews the newest entry.
        app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL))
            .await;
        assert!(app.history_search.is_some());
        assert_eq!(app.input_text(), "deploy prod");

        // Typing narrows to the newest entry containing the query.
        for ch in ['t', 'e', 's', 't'] {
            app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::empty()))
                .await;
        }
        assert_eq!(app.input_text(), "run tests");
        let view = app.history_search_view().expect("search active");
        assert_eq!(view.query, "test");
        assert!(view.matched);

        // Enter accepts the match into the composer and leaves search mode.
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()))
            .await;
        assert!(app.history_search.is_none());
        assert_eq!(app.input_text(), "run tests");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ctrl_r_cycles_older_matches_then_reports_no_match() {
        let mut fixture = app_with_llmsim().await;
        let app = &mut fixture.app;
        app.setup = None;
        for entry in ["deploy staging", "run tests", "deploy prod"] {
            app.history.record(entry);
        }

        app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL))
            .await;
        for ch in ['d', 'e', 'p', 'l', 'o', 'y'] {
            app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::empty()))
                .await;
        }
        // Newest "deploy" match first.
        assert_eq!(app.input_text(), "deploy prod");
        // Ctrl+R again cycles to the older "deploy" entry.
        app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL))
            .await;
        assert_eq!(app.input_text(), "deploy staging");

        // A query with no match clears the preview and flags it in the view.
        app.handle_key(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::empty()))
            .await;
        assert_eq!(app.input_text(), "");
        assert!(!app.history_search_view().unwrap().matched);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn esc_cancels_reverse_search_and_restores_draft() {
        let mut fixture = app_with_llmsim().await;
        let app = &mut fixture.app;
        app.setup = None;
        app.history.record("some old prompt");

        // Type a draft, then search, then cancel — the draft must come back.
        for ch in ['d', 'r', 'a', 'f', 't'] {
            app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::empty()))
                .await;
        }
        app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL))
            .await;
        assert_eq!(app.input_text(), "some old prompt");
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()))
            .await;
        assert!(app.history_search.is_none());
        assert_eq!(app.input_text(), "draft");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ctrl_r_with_empty_history_is_a_no_op() {
        let mut fixture = app_with_llmsim().await;
        let app = &mut fixture.app;
        app.setup = None;
        app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL))
            .await;
        assert!(app.history_search.is_none());
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
        assert!(
            !rows.iter().any(|line| line.contains("Enter to send")),
            "the composer must not render through the setup sheet: {rows:?}"
        );
        assert!(
            rows.first().is_some_and(String::is_empty) && rows.last().is_some_and(String::is_empty),
            "the centered panel should have clean margins without underlying chrome: {rows:?}"
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
    async fn setup_device_login_wait_can_be_cancelled() {
        let mut fixture = app_with_llmsim().await;
        let app = &mut fixture.app;
        let task = tokio::spawn(std::future::pending());
        app.codex_login = Some(PendingCodexLogin { id: 7, task });
        app.setup = Some(SetupStep::CodexLogin {
            selected: 1,
            method: CodexLoginMethod::Device,
            device_code: Some(("https://example.test/device".into(), "ABCD-EFGH".into())),
        });

        app.handle_setup_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()))
            .await;

        assert!(app.codex_login.is_none());
        assert!(matches!(
            app.setup,
            Some(SetupStep::Credential {
                ref provider,
                selected: 1,
                error: Some(ref error),
            }) if provider == "codex" && error == "Codex sign-in canceled"
        ));
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
        assert_eq!(options[0].spec.as_deref(), Some("gpt-5.6-sol"));
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
        assert_eq!(app.model.provider_label(), "openai/gpt-5.6-sol medium");
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

        // Navigate to the third preset (gpt-5.4) and confirm it.
        app.handle_setup_key(KeyEvent::new(KeyCode::Down, KeyModifiers::empty()))
            .await;
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
            }) if provider == "openai" && selected == 2
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

        assert_eq!(app.model.provider_label(), "openai/gpt-5.6-sol medium");
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
            presentation: PresentationState {
                stream_preview: Some(StreamPreview {
                    kind: StreamKind::Assistant,
                    text: "streaming response".to_string(),
                }),
                ..presentation_state_idle()
            },
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
            presentation: PresentationState {
                busy: true,
                turn_activity: Some("reading files".to_string()),
                ..presentation_state_idle()
            },
            busy_frame: 4,
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
    fn chrome_busy_shows_live_elapsed_timer() {
        let state = ViewState {
            presentation: PresentationState {
                busy: true,
                turn_activity: Some("reading files".to_string()),
                turn_elapsed_secs: Some(75),
                ..presentation_state_idle()
            },
            busy_frame: 4,
            ..view_state_idle()
        };
        let rows = render_chrome_lines(&state, 80, 4);
        assert!(
            rows[0].contains("1m15s"),
            "busy separator should show the elapsed timer: {}",
            rows[0]
        );
    }

    #[test]
    fn format_elapsed_is_compact() {
        assert_eq!(format_elapsed(8), "8s");
        assert_eq!(format_elapsed(75), "1m15s");
        assert_eq!(format_elapsed(3725), "1h02m");
    }

    #[test]
    fn chrome_busy_falls_back_to_thinking_when_activity_unset() {
        let state = ViewState {
            presentation: PresentationState {
                busy: true,
                ..presentation_state_idle()
            },
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
            presentation: PresentationState {
                stream_preview: Some(StreamPreview {
                    kind: StreamKind::Assistant,
                    text: "first line\nsecond line tail".to_string(),
                }),
                ..presentation_state_idle()
            },
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
            presentation: PresentationState {
                provider_name: "openrouter".to_string(),
                lines_count: 42,
                ..presentation_state_idle()
            },
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
    fn chrome_session_status_compact_shows_worktree_indicator() {
        let state = ViewState {
            presentation: PresentationState {
                worktree_compact: Some("bump-outdated-crates-ship".to_string()),
                ..presentation_state_idle()
            },
            ..view_state_idle()
        };
        let rows = render_chrome_lines(&state, 120, 4);
        let status = &rows[3];
        assert!(
            status.contains("wt bump-out"),
            "compact status should include worktree slug: {status}"
        );
    }

    #[test]
    fn chrome_session_status_expanded_shows_worktree_subsection() {
        let state = ViewState {
            presentation: PresentationState {
                status_layout: StatusLayout::Expanded,
                worktree_compact: Some("bump-outdated-crates-ship".to_string()),
                worktree_expanded: Some((
                    "bump-outdated-crates-ship-f6dd3e41".to_string(),
                    "…/session_019ee6c2dfd27223853ea56ff6dd3e41".to_string(),
                )),
                ..presentation_state_idle()
            },
            ..view_state_idle()
        };
        assert_eq!(state.status_row_count(), 5);
        let rows = render_chrome_lines(&state, 180, 8);
        assert!(
            rows[7].contains("worktree bump-outdated-crates-ship-f6dd3e41")
                && rows[7].contains("path …/session_019ee6"),
            "expanded status should include worktree branch and path: {:?}",
            rows[7]
        );
    }

    #[test]
    fn view_state_status_row_count_adds_worktree_row_only_when_expanded() {
        let compact = ViewState {
            presentation: PresentationState {
                worktree_expanded: Some(("branch".into(), "path".into())),
                ..presentation_state_idle()
            },
            ..view_state_idle()
        };
        assert_eq!(compact.status_row_count(), 1);

        let expanded = ViewState {
            presentation: PresentationState {
                status_layout: StatusLayout::Expanded,
                worktree_expanded: Some(("branch".into(), "path".into())),
                ..presentation_state_idle()
            },
            ..view_state_idle()
        };
        assert_eq!(expanded.status_row_count(), 5);
    }

    #[test]
    fn chrome_session_status_expanded_groups_details_across_four_lines() {
        let session_id = SessionId::from_seed(99887766).to_string();
        let state = ViewState {
            presentation: PresentationState {
                model_id: "nvidia/nemotron-3-super-120b-a12b".to_string(),
                provider_name: "openrouter".to_string(),
                reasoning_effort: Some("high".to_string()),
                session_id: session_id.clone(),
                lines_count: 42,
                session_tokens: Some(1234),
                status_layout: StatusLayout::Expanded,
                ..presentation_state_idle()
            },
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
        assert!(
            rows[6].contains("session ") && rows[6].contains(&session_id),
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
            presentation: PresentationState {
                status_layout: StatusLayout::Expanded,
                ..presentation_state_idle()
            },
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

// TUI app state and event loop.
// Decision: keep the TUI surface tiny. Transcript output is published into the
// native terminal scrollback; the renderer owns only a short footer at the
// bottom — tuika's split-footer screen mode (see `tuika::screen`).

use crate::exec::worktree::WorktreeManager;
use crate::runtime::session::Session;
use crate::runtime::{BuiltRuntime, ModelState, StartupInfo};
use crate::session_state::goal::{GOAL_EVALUATE_ARG, GoalStore, parse_evaluation_response};
use crate::session_state::user_ask::{
    AskOutcome, USER_ASK_EVALUATE_ARG, UserAskStore, evaluation_status_message,
    parse_evaluation_response as parse_user_ask_evaluation,
};
use crate::tui::host_ui::{UiCommand, UiRequest};
use anyhow::Result;
use crossterm::event::{
    self, Event as CrosstermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton,
    MouseEvent, MouseEventKind,
};
use everruns_core::command::{CommandDescriptor, CommandSource};
use everruns_core::message::ContentPart;
use everruns_core::session_task::SessionTaskRegistry;
use everruns_core::typed_id::SessionId;
use futures::FutureExt;
use ratatui::Terminal;
use ratatui::backend::Backend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};
use tuika::InputOutcome;
use tuika::components::{ScrollState, TextInputState};
use tuika::keymap::Dispatch;
use tuika::mouse::{SelectionRange, selected_text, word_at};
use tuika::term::hyperlink::BufferLink;
use tuika::term::pointer::{self, PointerShape};
use tuika::term::progress::TerminalProgress;

pub(crate) mod fullscreen;
mod keymap;
mod render;
mod repo_pulse;
mod setup;

pub mod host_ui;
pub mod input;
pub mod presentation;
pub mod prompt_history;
pub mod session_tasks_view;
pub mod transcript;
mod transcript_selection;

// Re-export the moved free items so the rest of the crate (and the test module)
// can keep referring to them as `crate::tui::*`. `setup` exposes only `impl App`
// methods, so it needs no re-export. The rendering module is named `render`
// rather than `draw` so it does not collide with the free `draw` fn it exports.
// The view-model types and runtime-event translation live in `crate::tui::transcript`
// (the single boundary that interprets `everruns_core` events); re-export them
// here so the TUI's own submodules keep referring to them as `crate::tui::*`.
pub(crate) use self::keymap::{GlobalAction, global_keymap};
pub(crate) use self::render::*;
pub(crate) use crate::tui::presentation::*;
pub(crate) use crate::tui::transcript::*;

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

/// Upper bound on retained transcript `ChatLine`s. A session that never stops
/// producing output would otherwise grow `App::lines` — and the full-screen
/// wrap cache built from it — without bound. Past this watermark the oldest
/// lines are dropped from the front (see [`App::trim_transcript`]): in
/// split-footer mode they are already published to native scrollback, and in full-screen
/// mode this bounds how far back the in-app scrollback reaches. The window is
/// generous so only pathologically long sessions ever reach it.
const MAX_RETAINED_TRANSCRIPT_LINES: usize = 50_000;

/// Memoized wrapping for the full-screen transcript.
/// [`render::append_transcript_range`] markdown/syntax-highlights and word-wraps
/// every `ChatLine`; doing that for the whole history on every frame is
/// O(session) work to show O(viewport).
/// Because `App::lines` only grows at the tail (or is reset, which bumps
/// `App::transcript_generation`), the wrapped output is stable except for the
/// newly-appended lines, which is all this cache re-wraps per frame.
#[derive(Default)]
struct TranscriptWrapCache {
    /// Wrap width the cached lines were built at; a change invalidates them.
    width: usize,
    /// Generation the cache was built against; a bump invalidates it.
    generation: u64,
    /// Number of `App::lines` already wrapped into `lines`.
    source_len: usize,
    /// Author of the last emitted chat, so an incremental pass reproduces the
    /// inter-author gap exactly as a full pass would.
    prev_author: Option<Author>,
    /// The wrapped, gap-spaced transcript rows.
    lines: Vec<Line<'static>>,
    /// Hyperlink runs into `lines` (labeled markdown links + bare URLs), used to
    /// embed native OSC 8 targets after painting when the label differs from
    /// the URL.
    links: Vec<BufferLink>,
}

pub struct App {
    session: Session,
    startup: StartupInfo,
    model: ModelState,
    pub lines: Vec<ChatLine>,
    /// Fullscreen repository context for the empty state. Collection runs on a
    /// blocking worker so Git never delays drawing or composer input. Inline
    /// leaves this disabled because adding rows would reflow its pinned footer.
    repo_pulse: Option<RepositoryPulse>,
    repo_pulse_rx: Option<mpsc::UnboundedReceiver<Option<RepositoryPulse>>>,
    printed_lines: usize,
    /// Rendered rows of `lines[printed_lines]` already published, when a flush
    /// cut an entry in half. Publishing is row-granular so the footer's
    /// transcript rows stay exactly full: cutting only on entry boundaries
    /// would leave a hole whenever one tall entry crossed the edge.
    printed_rows: usize,
    /// The single composer model, shared by **both** renderers: a tuika
    /// [`TextInputState`](TextInputState) that owns the draft text and
    /// cursor and applies its own edits (emacs bindings, word movement, wrapping).
    /// Inline and full-screen both read and write it, render it through the same
    /// [`TextInput`](tuika::components::TextInput) view, and place the terminal cursor via
    /// [`cursor_screen`](TextInputState::cursor_screen) — so the two modes
    /// can never drift.
    composer: TextInputState,
    /// User messages submitted while a turn is active. The embedded runtime
    /// cannot accept concurrent inputs, so these start in FIFO order at the
    /// next turn boundary. Cancellation deliberately leaves the queue intact.
    queued_messages: VecDeque<QueuedMessage>,
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
    /// Background MCP OAuth login (browser + loopback), parallel to Codex so
    /// the event loop stays alive in both fullscreen and inline modes.
    mcp_login: Option<PendingMcpLogin>,
    mcp_login_tx: mpsc::UnboundedSender<McpLoginEvent>,
    mcp_login_rx: mpsc::UnboundedReceiver<McpLoginEvent>,
    next_mcp_login_id: u64,
    status_layout: StatusLayout,
    session_tokens: Option<u64>,
    /// Terminal-side commands emitted by `ClientCommandsCapability` (via
    /// `runtime.execute_command`). Drained in the event loop; see
    /// [`App::apply_ui_command`].
    ui_rx: mpsc::UnboundedReceiver<UiRequest>,
    /// Live status-bar text per extension (`ext:<name>` → status), pushed by
    /// extension servers over `status/changed`. Rendered in the status bar via
    /// [`App::presentation_state`].
    extension_status: std::collections::BTreeMap<String, String>,
    /// Turn-scoped status text set by the agent through the host UI.
    agent_status: Option<String>,
    /// Incoming extension `ui/ask` requests; each is prompted one at a time via
    /// [`App::pending_ask`].
    ask_rx: mpsc::UnboundedReceiver<crate::tui::host_ui::AskRequest>,
    /// The `ui/ask` prompt currently shown, if any. Owns the keyboard (even
    /// mid-turn) until the user answers or dismisses it.
    pending_ask: Option<PendingAsk>,
    sandbox_approval_rx: crate::sandbox_approval::ApprovalReceiver,
    pending_sandbox_approval: Option<PendingSandboxApproval>,
    /// Settings store shared with the runtime (same instance
    /// `ModelsCapability` writes). Used to resolve credentials when querying
    /// provider models APIs and to show per-provider connection status in
    /// the setup overlay.
    settings: Arc<crate::config::SettingsStore>,
    /// Per-process override, when one differs from persisted settings.
    sandbox_mode_override: Option<crate::config::SandboxMode>,
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
    /// platform-store wake seam, `crate::runtime::background_wake`). Drained while idle to
    /// auto-start a turn so the agent reacts to finished work. See
    /// knowledge/specs/background.md.
    background_wake: crate::runtime::background_wake::WakeReceiver,
    /// Retained for the TUI lifetime so due local schedules keep polling.
    _schedule_runner: everruns_local::LocalScheduleRunnerHandle,
    /// Everruns session-task registry used by `spawn_background`; the TUI reads
    /// it for the background status segment and panel.
    task_registry: Arc<dyn SessionTaskRegistry>,
    task_schedule_store: Arc<dyn everruns_core::traits::SessionScheduleStore>,
    session_store: Arc<dyn everruns_core::traits::SessionStore>,
    session_tasks: crate::tui::session_tasks_view::TaskTree,
    session_tasks_refresh:
        Option<tokio::task::JoinHandle<crate::tui::session_tasks_view::TaskTree>>,
    last_session_tasks_refresh: Option<Instant>,
    /// Open right-side activity rail. The legacy option shape keeps visibility
    /// separate from focus; scrolling lives in `activity_scroll`.
    background_panel: Option<usize>,
    /// Manual panels capture navigation/cancellation keys; an automatically
    /// opened panel stays passive so it never steals the composer.
    background_panel_focused: bool,
    /// Auto-open at most once per TUI session when the first subagent appears.
    background_panel_auto_opened: bool,
    /// Selected task row while the activity rail is focused.
    background_selected: usize,
    /// Persisted activity-rail viewport. Passive rails stay pinned to new work;
    /// manual navigation detaches until the user returns to the bottom.
    activity_scroll: ScrollState,
    /// Last (content_height, viewport_height) painted by either renderer.
    activity_scroll_metrics: (usize, usize),
    goal_store: Arc<GoalStore>,
    user_ask_store: Arc<UserAskStore>,
    user_ask_enabled: bool,
    completion_budget: crate::session_state::task_completion::CompletionBudget,
    worktree: Arc<WorktreeManager>,
    workspace_host: Arc<crate::exec::workspace_host::WorkspaceHost>,
    /// Images from `--image` / `-i` on the CLI, consumed on the first turn.
    pending_images: Vec<ContentPart>,
    /// Large paste placeholders mapped to their full clipboard/terminal payloads.
    pending_pastes: Vec<(String, String)>,
    /// Which renderer this session drives. `Fullscreen` is the CLI default;
    /// `SplitFooter` is the scrollback-native composer selected by `--inline`.
    render_mode: RenderMode,
    /// Full-screen transcript scroll position (unused in split-footer mode).
    scroll: ScrollState,
    /// Last (content_height, viewport_height) the full-screen transcript drew,
    /// so mouse/paging handlers can clamp scrolling without re-laying out.
    scroll_metrics: (usize, usize),
    /// Memoized full-screen transcript wrapping (see
    /// [`App::full_transcript_lines_cached`]). Bumped whenever `lines` is reset
    /// out from under the cache (clear, checkpoint restore, or a front trim);
    /// tail appends leave it untouched so only the new lines are re-wrapped.
    transcript_generation: u64,
    transcript_cache: TranscriptWrapCache,
    /// Full-screen mouse text selection over the transcript, anchored in
    /// content-row space so it survives scrolling and can span more than one
    /// visible window. `selection_area` is the transcript's inner rect recorded
    /// by the last draw (so the event handler can bound drags to it and map
    /// screen rows to content rows); `pending_copy` defers the OSC 52 copy to
    /// the next draw, where the freshly rendered frame buffer is readable.
    selection: transcript_selection::TranscriptSelection,
    selection_area: Rect,
    /// Link cell runs visible in the last full-screen transcript frame.
    visible_link_regions: Vec<Rect>,
    pointer_shape: PointerShape,
    /// Click targets from the last fullscreen status layout.
    status_hit_regions: Vec<(Rect, StatusAction)>,
    pending_copy: bool,
    /// Drives the terminal's native OSC 9;4 progress indicator (Ghostty top
    /// bar / taskbar) while a turn runs. Works in both renderers. Enabled only
    /// for real TUI sessions via [`App::enable_native_progress`] so tests and
    /// non-terminal hosts emit no escape sequences.
    term_progress: TerminalProgress,
    native_progress: bool,
    /// Shell-style Up/Down recall of previously submitted composer prompts,
    /// persisted across sessions. See [`crate::tui::prompt_history`].
    history: crate::tui::prompt_history::PromptHistory,
    /// Active Ctrl+R reverse-history search, if any. While set it owns the
    /// keyboard and the composer previews the current match.
    history_search: Option<HistorySearch>,
    /// App-global chord shortcuts (Ctrl+R/C/D/B/V), resolved through `tuika`'s
    /// keymap engine rather than a hand-rolled match. See [`crate::tui::keymap`].
    keymap: tuika::keymap::Keymap<GlobalAction>,
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

/// The renderer backing a TUI session — yolop's side of tuika's
/// [`ScreenMode`](tuika::ScreenMode), which decides how much of the terminal
/// the frame owns.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum RenderMode {
    /// Composer pinned to the terminal's last rows, transcript published above
    /// it as ordinary scrollback (tuika's `ScreenMode::SplitFooter`).
    #[default]
    SplitFooter,
    /// Full-screen alternate-screen renderer and CLI default
    /// (tuika's `ScreenMode::Alternate`).
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

struct PendingMcpLogin {
    id: u64,
    name: String,
    task: tokio::task::JoinHandle<()>,
}

/// An in-flight extension `ui/ask`: the prompt shown, the answer being typed,
/// and the oneshot that delivers it back to the extension server.
pub(crate) struct PendingAsk {
    prompt: String,
    placeholder: Option<String>,
    value: String,
    /// Mask the input and never echo the answer (credentials).
    secret: bool,
    /// Selector options; when non-empty the overlay is a picker, not a field.
    options: Vec<String>,
    /// Highlighted option index (selector mode only).
    selected: usize,
    reply: Option<oneshot::Sender<crate::tui::host_ui::AskAnswer>>,
}

struct PendingSandboxApproval {
    reply: oneshot::Sender<crate::sandbox_approval::ApprovalDecision>,
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
        result: Result<crate::config::CodexAuth, String>,
    },
}

enum McpLoginEvent {
    Finished {
        id: u64,
        name: String,
        provider_key: String,
        result: Result<crate::auth::mcp_oauth::McpOAuthTokenSet, String>,
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
        label: "Codex Subscription",
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

struct QueuedMessage {
    prompt: String,
    display: String,
    images: Vec<ContentPart>,
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
/// borrow `App` for the duration of a draw: `draw_input` borrows `App`
/// to paint the composer, and a borrowed `ViewState` would block that
/// within a single `draw()`. The per-frame clone cost is dominated by
/// `String`-sized fields and is negligible compared to the chrome
/// render itself.
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
        let session_store = runtime.handles.session_store.clone();
        let session = Session::new(runtime.handles, runtime.model.clone());
        let (models_tx, models_rx) = mpsc::unbounded_channel::<ModelDiscovery>();
        let (codex_login_tx, codex_login_rx) = mpsc::unbounded_channel::<CodexLoginEvent>();
        let (mcp_login_tx, mcp_login_rx) = mpsc::unbounded_channel::<McpLoginEvent>();
        let mut app = Self {
            session,
            startup: runtime.startup,
            model: runtime.model,
            lines: Vec::new(),
            repo_pulse: None,
            repo_pulse_rx: None,
            printed_lines: 0,
            printed_rows: 0,
            composer: {
                let mut composer = TextInputState::new();
                composer.set_mode(tuika::components::TextInputMode::SubmitOnEnter);
                composer
            },
            queued_messages: VecDeque::new(),
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
            mcp_login: None,
            mcp_login_tx,
            mcp_login_rx,
            next_mcp_login_id: 0,
            status_layout: StatusLayout::Compact,
            session_tokens: None,
            ui_rx: runtime.ui_rx,
            extension_status: std::collections::BTreeMap::new(),
            agent_status: None,
            ask_rx: runtime.ask_rx,
            pending_ask: None,
            sandbox_approval_rx: runtime.sandbox_approval_rx,
            pending_sandbox_approval: None,
            settings: runtime.settings,
            sandbox_mode_override: runtime.sandbox_mode_override,
            model_catalog: HashMap::new(),
            model_fetches_in_flight: HashSet::new(),
            model_discovery_enabled: true,
            models_tx,
            models_rx,
            background_wake: runtime.background_wake,
            _schedule_runner: runtime.schedule_runner,
            task_registry: runtime.task_registry,
            task_schedule_store: runtime.task_schedule_store,
            session_store,
            session_tasks: Default::default(),
            session_tasks_refresh: None,
            last_session_tasks_refresh: None,
            background_panel: None,
            background_panel_focused: false,
            background_panel_auto_opened: false,
            background_selected: 0,
            activity_scroll: ScrollState::new(),
            activity_scroll_metrics: (0, 0),
            goal_store,
            user_ask_store,
            user_ask_enabled,
            completion_budget: Default::default(),
            worktree: runtime.worktree,
            workspace_host: runtime.workspace_host,
            pending_images,
            pending_pastes: Vec::new(),
            render_mode: RenderMode::default(),
            scroll: ScrollState::new(),
            scroll_metrics: (0, 0),
            transcript_generation: 0,
            transcript_cache: TranscriptWrapCache::default(),
            selection: transcript_selection::TranscriptSelection::new(),
            selection_area: Rect::ZERO,
            visible_link_regions: Vec::new(),
            pointer_shape: PointerShape::Default,
            status_hit_regions: Vec::new(),
            pending_copy: false,
            term_progress: TerminalProgress::new(),
            native_progress: false,
            // Tests run in-memory so recall never reads or appends to the real
            // per-user history file.
            history: if cfg!(test) {
                crate::tui::prompt_history::PromptHistory::in_memory()
            } else {
                crate::tui::prompt_history::PromptHistory::load()
            },
            history_search: None,
            keymap: global_keymap(),
            turn_started_at: None,
            context_used_tokens: None,
        };
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

    /// Switch this session to the full-screen renderer. Called by `run_tui`
    /// unless `--inline` is set.
    pub(crate) fn set_render_mode(&mut self, mode: RenderMode) {
        self.render_mode = mode;
        if mode.is_fullscreen() && self.repo_pulse.is_none() && self.repo_pulse_rx.is_none() {
            self.repo_pulse_rx = Some(repo_pulse::spawn(self.startup.workspace_root.clone()));
        }
    }

    /// Resolve a queued double-click into a word selection against the freshly
    /// rendered `buffer`. The pending position is in content space; its row must
    /// still be visible (a double click always lands on a visible cell), so we
    /// read the word bounds off the buffer and re-anchor the result in content
    /// space. Sets `pending_copy` so the word is copied like a drag.
    pub(crate) fn resolve_selection(&mut self, buffer: &ratatui::buffer::Buffer) {
        let Some((column, content_row)) = self.selection.take_pending_word() else {
            return;
        };
        let Some(screen_row) = self.screen_row_for_content(content_row) else {
            return;
        };
        let Some(word) = word_at(buffer, self.selection_area, column, screen_row) else {
            return;
        };
        self.selection
            .select_word(transcript_selection::ContentRange {
                start: (word.start.0, content_row),
                end: (word.end.0, content_row),
            });
        self.pending_copy = true;
    }

    pub(crate) fn set_visible_links(
        &mut self,
        origin: ratatui::layout::Position,
        links: &[BufferLink],
    ) {
        self.visible_link_regions = links
            .iter()
            .filter(|link| link.end_col > link.start_col)
            .map(|link| {
                Rect::new(
                    origin.x.saturating_add(link.start_col),
                    origin.y.saturating_add(link.line),
                    link.end_col.saturating_sub(link.start_col),
                    1,
                )
            })
            .collect();
    }

    fn update_link_pointer(&mut self, mouse: MouseEvent) -> Option<PointerShape> {
        if !matches!(mouse.kind, MouseEventKind::Moved) {
            return None;
        }
        let hovered = self.visible_link_regions.iter().any(|area| {
            mouse.column >= area.x
                && mouse.column < area.right()
                && mouse.row >= area.y
                && mouse.row < area.bottom()
        });
        let next = if hovered {
            PointerShape::Pointer
        } else {
            PointerShape::Default
        };
        if next == self.pointer_shape {
            return None;
        }
        self.pointer_shape = next;
        Some(next)
    }

    /// The selection mapped into the *current* viewport window, for painting.
    /// Returns `None` when the selection is scrolled entirely off-screen even
    /// though it is still active (see [`has_selection`](Self::has_selection)).
    pub(crate) fn selection_range(&self) -> Option<SelectionRange> {
        self.visible_selection_range(self.selection.range()?)
    }

    /// Whether a non-empty transcript selection currently exists, regardless of
    /// whether it is scrolled into view.
    #[cfg(test)]
    pub(crate) fn has_selection(&self) -> bool {
        self.selection.is_active()
    }

    /// Content row for a viewport screen row, clamped to the transcript. Short
    /// content is top-padded to rest at the bottom, so the padding offsets the
    /// mapping; a taller transcript is windowed by the scroll offset instead.
    fn content_row_for_screen(&self, screen_row: u16) -> usize {
        let area = self.selection_area;
        let (content_h, viewport_h) = self.scroll_metrics;
        let top_pad = viewport_h.saturating_sub(content_h);
        let rel = (screen_row.saturating_sub(area.y) as usize)
            .min((area.height.saturating_sub(1)) as usize);
        let content_row = self.scroll.offset() + rel.saturating_sub(top_pad);
        content_row.min(content_h.saturating_sub(1))
    }

    /// The viewport screen row a content row occupies, or `None` when it is
    /// scrolled out of the visible window. Inverse of
    /// [`content_row_for_screen`](Self::content_row_for_screen).
    fn screen_row_for_content(&self, content_row: usize) -> Option<u16> {
        let area = self.selection_area;
        let (content_h, viewport_h) = self.scroll_metrics;
        let offset = self.scroll.offset();
        if content_row < offset {
            return None;
        }
        let top_pad = viewport_h.saturating_sub(content_h);
        let rel = content_row - offset + top_pad;
        (rel < area.height as usize).then(|| area.y + rel as u16)
    }

    /// Clamp a viewport column to the selectable transcript rect.
    fn clamp_selection_col(&self, column: u16) -> u16 {
        let area = self.selection_area;
        column.clamp(area.x, area.right().saturating_sub(1))
    }

    /// Map a content-space selection into the current window as a viewport
    /// [`SelectionRange`]. A start above the window (or end below it)
    /// becomes a full-width edge row so the linear span fills across it.
    fn visible_selection_range(
        &self,
        range: transcript_selection::ContentRange,
    ) -> Option<SelectionRange> {
        let area = self.selection_area;
        let (content_h, viewport_h) = self.scroll_metrics;
        if area.height == 0 || content_h == 0 || viewport_h == 0 {
            return None;
        }
        let offset = self.scroll.offset();
        let top_pad = viewport_h.saturating_sub(content_h);
        let win_top = offset;
        let win_bot = offset + content_h.min(viewport_h) - 1;
        let (mut s_col, mut s_row) = range.start;
        let (mut e_col, mut e_row) = range.end;
        if e_row < win_top || s_row > win_bot {
            return None;
        }
        if s_row < win_top {
            s_row = win_top;
            s_col = area.x;
        }
        if e_row > win_bot {
            e_row = win_bot;
            e_col = area.right().saturating_sub(1);
        }
        let to_screen = |r: usize| area.y + (top_pad + (r - offset)) as u16;
        Some(SelectionRange {
            start: (s_col, to_screen(s_row)),
            end: (e_col, to_screen(e_row)),
        })
    }

    /// The selected text, read from the wrapped transcript cache so a multi-row
    /// selection copies in full even when most of it is scrolled off-screen.
    /// Rows are rendered into a scratch buffer and read back through
    /// [`selected_text`] so wide glyphs and trailing-blank trimming match
    /// what a single-window selection would copy.
    fn selection_copy_text(&self) -> String {
        let Some(range) = self.selection.range() else {
            return String::new();
        };
        let area = self.selection_area;
        let lines = &self.transcript_cache.lines;
        if area.width == 0 || lines.is_empty() {
            return String::new();
        }
        let last = lines.len() - 1;
        let start_row = range.start.1.min(last);
        // A transcript can wrap past u16::MAX rows, so cap the scratch buffer's
        // height (a selection that tall is already far beyond anything readable)
        // to keep the row count inside a u16 without overflow.
        let row_count = (range.end.1.min(last) - start_row + 1).min(u16::MAX as usize);
        let end_row = start_row + row_count - 1;
        let mut buf = ratatui::buffer::Buffer::empty(Rect::new(0, 0, area.width, row_count as u16));
        for (i, row) in (start_row..=end_row).enumerate() {
            buf.set_line(0, i as u16, &lines[row], area.width);
        }
        // Columns are viewport-absolute; the scratch buffer starts at the
        // transcript's left inset, so shift them 0-based.
        let base = area.x;
        let sr = SelectionRange {
            start: (range.start.0.saturating_sub(base), 0),
            end: (range.end.0.saturating_sub(base), row_count as u16 - 1),
        };
        selected_text(&buf, buf.area, sr)
    }

    /// Record the transcript's inner rect (the selectable region) from the draw.
    pub(crate) fn set_selection_area(&mut self, area: Rect) {
        self.selection_area = area;
    }

    pub(crate) fn set_status_hit_regions(&mut self, area: Rect, hits: &[render::StatusHit]) {
        self.status_hit_regions = hits
            .iter()
            .map(|hit| {
                (
                    Rect {
                        x: area.x.saturating_add(hit.start_col),
                        y: area.y.saturating_add(hit.row),
                        width: hit.end_col.saturating_sub(hit.start_col),
                        height: 1,
                    },
                    hit.action,
                )
            })
            .collect();
    }

    /// Take and clear the deferred-copy flag set when a drag was released.
    pub(crate) fn take_pending_copy(&mut self) -> bool {
        std::mem::take(&mut self.pending_copy)
    }

    /// Route a full-screen mouse event to transcript text selection. Returns
    /// `true` when the event was consumed (a redraw follows). A left-drag that
    /// starts inside the transcript selects; releasing it arms a clipboard copy
    /// (performed in the next draw). Shift/Ctrl/Alt gestures are left alone so
    /// the terminal's own Shift-drag selection still works.
    fn handle_fullscreen_selection(&mut self, mouse: MouseEvent) -> bool {
        let Some(tuika::Event::Mouse(m)) = tuika::translate_event(CrosstermEvent::Mouse(mouse))
        else {
            return false;
        };
        if !m.plain() {
            return false;
        }
        match m.kind {
            tuika::MouseKind::Down(tuika::MouseButton::Left) => {
                let a = self.selection_area;
                let inside = a.width > 0
                    && a.height > 0
                    && m.column >= a.x
                    && m.column < a.right()
                    && m.row >= a.y
                    && m.row < a.bottom();
                if inside {
                    let cell = (
                        self.clamp_selection_col(m.column),
                        self.content_row_for_screen(m.row),
                    );
                    self.selection.press(cell);
                    true
                } else {
                    // A press elsewhere dismisses any existing selection but is
                    // otherwise left for the normal mouse handler (e.g. status).
                    let had = self.selection.is_active();
                    self.selection.clear();
                    had
                }
            }
            tuika::MouseKind::Drag(tuika::MouseButton::Left) => {
                if !self.selection.is_pressed() {
                    return false;
                }
                // Auto-scroll when the drag reaches a vertical edge so a single
                // drag can extend past the visible window, the way a terminal
                // does. The content row is then read against the new offset.
                self.autoscroll_for_drag(m.row);
                let cell = (
                    self.clamp_selection_col(m.column),
                    self.content_row_for_screen(m.row),
                );
                self.selection.drag(cell)
            }
            tuika::MouseKind::Up(tuika::MouseButton::Left) => {
                if !self.selection.is_pressed() {
                    return false;
                }
                let cell = (
                    self.clamp_selection_col(m.column),
                    self.content_row_for_screen(m.row),
                );
                let changed = self.selection.release(cell);
                if changed && self.selection.is_active() {
                    self.pending_copy = true;
                }
                changed
            }
            _ => false,
        }
    }

    /// Scroll the transcript one step when a drag reaches the top or bottom edge
    /// of the selectable rect, so a drag alone can extend the selection past the
    /// visible window. A no-op away from the edges.
    fn autoscroll_for_drag(&mut self, screen_row: u16) {
        let a = self.selection_area;
        if a.height == 0 {
            return;
        }
        if screen_row <= a.y {
            self.scroll_transcript(MouseEventKind::ScrollUp);
        } else if screen_row + 1 >= a.bottom() {
            self.scroll_transcript(MouseEventKind::ScrollDown);
        }
    }

    /// Drop any full-screen selection (on key input or a new turn). Scrolling no
    /// longer clears it — the selection is anchored in content space and moves
    /// with the text.
    fn clear_selection(&mut self) {
        self.selection.clear();
    }

    /// Enable the terminal's native OSC 9;4 progress indicator for this session.
    /// Called by `run_tui`; left off for tests and non-terminal hosts.
    pub(crate) fn enable_native_progress(&mut self) {
        self.native_progress = true;
    }

    pub fn should_show_resume_hint(&self) -> bool {
        self.ctrl_c_exit
    }

    /// Final assistant text to leave visible after the full-screen UI closes.
    pub fn last_assistant_message(&self) -> Option<&str> {
        self.lines
            .iter()
            .rev()
            .find(|line| line.author == Author::Assistant)
            .map(|line| line.text.as_str())
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
        let sandbox = self
            .sandbox_mode_override
            .unwrap_or_else(|| settings.sandbox_mode());
        let profile = self.settings.active_profile_name();
        let approval_mode = presentation::safety_status_label(
            profile.as_deref(),
            settings.approval_mode(),
            sandbox,
            settings.approval_policy(),
        );
        PresentationState {
            startup: StartupPresentation {
                workspace: self.startup.workspace_root.display().to_string(),
                repository: if self.render_mode.is_fullscreen() {
                    self.repo_pulse.clone()
                } else {
                    None
                },
                safety_warning: crate::exec::sandbox::danger_warning(sandbox).map(str::to_string),
            },
            stream_preview: self.stream_preview.clone(),
            busy: self.busy,
            queued_messages: self.queued_messages.len(),
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
            compaction_budget_percent: Some(crate::runtime::COMPACTION_BUDGET_PERCENT),
            status_layout: self.status_layout,
            hooks_summary: self.startup.hook_summary(),
            approval_mode,
            background: self.background_counts(),
            goal_indicator: self.goal_indicator(),
            ask_indicator: self.ask_indicator(),
            worktree_compact: self.worktree.status_bar_compact(),
            worktree_expanded: self.worktree.status_bar_expanded(),
            agent_status: self.agent_status.clone(),
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

    fn background_counts(&self) -> Option<crate::tui::session_tasks_view::BackgroundCounts> {
        self.session_tasks.counts()
    }

    #[cfg(test)]
    fn background_panel_body(&self) -> String {
        crate::tui::session_tasks_view::render_activity_rail(
            &self.session_tasks,
            self.background_panel_focused
                .then_some(self.background_selected),
        )
    }

    fn apply_session_tasks(&mut self, tasks: crate::tui::session_tasks_view::TaskTree) {
        self.session_tasks = tasks;
        let selectable = self
            .activity_rail()
            .map(|rail| rail.selectable_task_indices())
            .unwrap_or_default();
        if !selectable.contains(&self.background_selected) {
            self.background_selected = selectable.first().copied().unwrap_or(0);
        }
        if !self.background_panel_auto_opened && self.session_tasks.has_subagents() {
            self.background_panel = Some(0);
            self.background_panel_focused = false;
            self.background_panel_auto_opened = true;
        }
    }

    fn activity_rail(&self) -> Option<crate::tui::session_tasks_view::ActivityRail> {
        crate::tui::session_tasks_view::activity_rail(&self.session_tasks)
    }

    async fn refresh_session_tasks(&mut self) {
        let tasks = crate::tui::session_tasks_view::load_task_tree(
            self.session.session_id(),
            self.task_registry.as_ref(),
            self.session_store.as_ref(),
        )
        .await;
        self.apply_session_tasks(tasks);
        self.last_session_tasks_refresh = Some(Instant::now());
    }

    fn refresh_session_tasks_if_due(&mut self) {
        let refresh_result = self
            .session_tasks_refresh
            .as_mut()
            .and_then(FutureExt::now_or_never);
        if let Some(result) = refresh_result {
            self.session_tasks_refresh = None;
            match result {
                Ok(tasks) => self.apply_session_tasks(tasks),
                Err(error) if error.is_cancelled() => {}
                Err(error) => self
                    .session_tasks
                    .errors
                    .push(format!("task tree refresh failed: {error}")),
            }
        }

        if self.session_tasks_refresh.is_some()
            || self
                .last_session_tasks_refresh
                .is_some_and(|last| last.elapsed() < SESSION_TASK_REFRESH_INTERVAL)
        {
            return;
        }

        let registry = Arc::clone(&self.task_registry);
        let sessions = Arc::clone(&self.session_store);
        let session_id = self.session.session_id();
        self.session_tasks_refresh = Some(tokio::spawn(async move {
            crate::tui::session_tasks_view::load_task_tree(session_id, &*registry, &*sessions).await
        }));
        self.last_session_tasks_refresh = Some(Instant::now());
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

    /// Refresh the memoized full-screen transcript wrapping for `width` and
    /// return the total wrapped-line count.
    ///
    /// Re-wraps only the lines appended since the last call while the width and
    /// [`transcript_generation`](Self::transcript_generation) are unchanged; a
    /// width change or a generation bump (transcript reset) rebuilds from
    /// scratch. The cached rows match a full [`render::append_transcript_range`]
    /// pass over the whole history. Read them back with [`transcript_window`] so
    /// a caller can clone just the slice it paints instead of the whole cache.
    ///
    /// [`transcript_window`]: Self::transcript_window
    fn refresh_transcript_cache(&mut self, width: usize) -> usize {
        // Move the cache out so we can borrow `self.lines` immutably alongside.
        let mut cache = std::mem::take(&mut self.transcript_cache);
        let stale = cache.width != width
            || cache.generation != self.transcript_generation
            || cache.source_len > self.lines.len();
        if stale {
            cache.lines.clear();
            cache.links.clear();
            cache.width = width;
            cache.generation = self.transcript_generation;
            cache.source_len = 0;
            cache.prev_author = None;
        }
        cache.prev_author = render::append_transcript_range(
            &mut cache.lines,
            &mut cache.links,
            &self.lines,
            cache.source_len,
            width,
            cache.prev_author.take(),
        );
        cache.source_len = self.lines.len();
        let len = cache.lines.len();
        self.transcript_cache = cache;
        len
    }

    /// Hyperlink runs for the cached transcript wrapping, used to embed OSC 8
    /// after the full-screen paint.
    fn transcript_links(&self) -> &[BufferLink] {
        &self.transcript_cache.links
    }

    /// Clone the cached wrapped rows in `start..end`. Call
    /// [`refresh_transcript_cache`](Self::refresh_transcript_cache) first; `end`
    /// must not exceed the count it returned. Cloning just the visible window
    /// (rather than the whole cache) is what keeps a full-screen frame
    /// O(viewport) for a long transcript.
    fn transcript_window(&self, start: usize, end: usize) -> Vec<Line<'static>> {
        self.transcript_cache.lines[start..end].to_vec()
    }

    /// The whole cached transcript, cloned. Convenience over
    /// [`refresh_transcript_cache`](Self::refresh_transcript_cache) +
    /// [`transcript_window`](Self::transcript_window) for callers that want every
    /// row; the full-screen renderer takes only the visible window instead.
    #[cfg(test)]
    fn full_transcript_lines_cached(&mut self, width: usize) -> Vec<Line<'static>> {
        let len = self.refresh_transcript_cache(width);
        self.transcript_window(0, len)
    }

    /// Drop transcript history beyond [`MAX_RETAINED_TRANSCRIPT_LINES`] so a very
    /// long session can't grow `lines` (and the wrap cache) without bound.
    ///
    /// Only lines already published to native scrollback are dropped in
    /// split-footer mode — dropping an unpublished line would erase it entirely
    /// — so the cap is a soft target there. Full-screen mode has no native
    /// flush, so the drop bounds how far back its in-app scrollback reaches.
    /// Draining the front shifts the flush cursor and
    /// invalidates the wrap cache.
    fn trim_transcript(&mut self) {
        let len = self.lines.len();
        if len <= MAX_RETAINED_TRANSCRIPT_LINES {
            return;
        }
        let want = len - MAX_RETAINED_TRANSCRIPT_LINES;
        let drop = if self.render_mode.is_fullscreen() {
            want
        } else {
            want.min(self.printed_lines)
        };
        if drop == 0 {
            return;
        }
        self.lines.drain(0..drop);
        self.printed_lines = self.printed_lines.saturating_sub(drop);
        self.transcript_generation = self.transcript_generation.wrapping_add(1);
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
        // mid-session, re-pinning the footer re-anchors the viewport by
        // querying the cursor position (`CSI 6n`), and crossterm abandons that query
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
                        self.deny_pending_sandbox_approval();
                        self.publish_remaining_transcript(terminal);
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
                self.deny_pending_sandbox_approval();
                self.publish_remaining_transcript(terminal);
                return Ok(());
            }
        }
    }

    /// Publish the tail the footer was holding back, on the way out. The
    /// footer's rows are handed back to the terminal right after this (see
    /// `tuika::screen::close_footer`), so an unpublished line would otherwise
    /// be erased instead of left in the scrollback with the rest of the
    /// session. Best-effort: a terminal that fails here is one the session is
    /// leaving anyway, and losing the last lines is not worth a failed exit.
    fn publish_remaining_transcript<B>(&mut self, terminal: &mut Terminal<B>)
    where
        B: Backend,
        B::Error: std::error::Error + Send + Sync + 'static,
    {
        if self.render_mode.is_fullscreen() {
            return;
        }
        if let Err(err) = self.flush_transcript(terminal, 0) {
            tracing::warn!("publishing the final transcript lines failed: {err:#}");
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
        self.refresh_session_tasks_if_due();
        terminal.autoresize()?;
        if !self.render_mode.is_fullscreen() {
            // Ratatui anchors an inline viewport to the cursor row it was
            // created at and resets that origin on horizontal shrinks; tuika's
            // `pin_footer` pushes it back onto the terminal's last rows. Cheap
            // and idempotent once pinned, so it runs before every draw and is
            // also what re-pins the footer after a resize.
            //
            // Both this and the publish below insert rows above the viewport,
            // which scrolls the terminal and clears the footer — hence the
            // repaint that follows, every frame, before input is read again.
            tuika::screen::pin_footer(terminal)?;
            // Split-footer mode publishes finalized lines into native
            // scrollback above the footer, keeping back only what the footer
            // still has room to show. Full-screen keeps the whole transcript in
            // `lines` and redraws it into the alternate screen each frame, so it
            // skips both the flush and the footer pinning.
            //
            // A failed publish is never fatal to the frame: the lines it could
            // not commit stay retained and are still painted in the footer, so
            // the session keeps rendering on a terminal too busy to answer the
            // cursor query `insert_before` ends with — and the same lines are
            // published on a later frame once it recovers.
            let keep_rows = self.footer_transcript_rows(terminal.get_frame().area());
            if let Err(err) = self.flush_transcript(terminal, keep_rows) {
                tracing::warn!("publishing transcript lines to scrollback failed: {err:#}");
            }
        }
        // Bound retained history now that split-footer mode has flushed this
        // frame's lines, so `trim_transcript` can drop the freshly-flushed prefix.
        self.trim_transcript();
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
                Ok(TurnEvent::Done(result)) => {
                    self.finish_busy();
                    if let Some(notice) = self.session.take_checkpoint_notice() {
                        self.refresh_after_checkpoint_restore(notice).await;
                        for display in self
                            .queued_messages
                            .iter()
                            .map(|message| message.display.clone())
                            .collect::<Vec<_>>()
                        {
                            self.push_user(display);
                        }
                        self.start_next_queued_turn();
                        return Ok(());
                    }
                    if self.start_next_queued_turn() {
                        return Ok(());
                    }
                    self.after_turn_goal_check().await;
                    if !self.busy {
                        self.after_turn_user_ask_check(result).await;
                    }
                    return Ok(());
                }
                Ok(TurnEvent::Failed(err)) => {
                    self.finish_busy();
                    self.push_system(format!("turn failed: {err}"));
                    self.record_completion_state(
                        crate::session_state::task_completion::CompletionState::Failed,
                    );
                    self.start_next_queued_turn();
                    return Ok(());
                }
                Err(mpsc::error::TryRecvError::Empty) => {}
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    self.finish_busy();
                    self.start_next_queued_turn();
                }
            }
        }

        // 2) drain terminal-side commands emitted by capabilities. Apply
        // every queued command before re-rendering so a burst (or a future
        // capability that emits more than one) doesn't cost a full
        // flush/draw per command, matching the test dispatch helper.
        let mut applied_ui_command = false;
        while let Ok(request) = self.ui_rx.try_recv() {
            let messages = self.apply_ui_command(request.command).await;
            if let Some(reply) = request.reply {
                let _ = reply.send(messages);
            }
            applied_ui_command = true;
        }
        // Extension `ui/ask` prompts. One at a time; a request arriving while a
        // prompt is open is answered "cancelled" rather than stacking overlays.
        while let Ok(request) = self.ask_rx.try_recv() {
            if self.pending_ask.is_some() {
                let _ = request.reply.send(crate::tui::host_ui::AskAnswer {
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
                secret: request.secret,
                options: request.options,
                selected: 0,
                reply: Some(request.reply),
            });
            applied_ui_command = true;
        }
        if self.pending_ask.is_none()
            && self.pending_sandbox_approval.is_none()
            && let Ok((request, reply)) = self.sandbox_approval_rx.try_recv()
        {
            self.push_system(format!("approval needed: {}", request.reason));
            self.lines.push(ChatLine {
                author: Author::Tool,
                text: request.command,
            });
            let scope = if request.full_access {
                "danger-full-access"
            } else {
                "this command inside the active sandbox"
            };
            self.push_system(format!(
                "press y to approve {scope} once, a to approve {scope} for this session, n or Esc to deny"
            ));
            self.pending_sandbox_approval = Some(PendingSandboxApproval { reply });
            return Ok(());
        }
        if applied_ui_command {
            return Ok(());
        }

        // 3) apply repository context collected off the event-loop thread.
        if let Some(rx) = self.repo_pulse_rx.as_mut() {
            match rx.try_recv() {
                Ok(pulse) => {
                    self.repo_pulse = pulse;
                    self.repo_pulse_rx = None;
                    return Ok(());
                }
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    self.repo_pulse_rx = None;
                }
                Err(mpsc::error::TryRecvError::Empty) => {}
            }
        }

        // 3a) drain finished models API fetches so an open picker refreshes.
        let mut applied_model_discovery = false;
        while let Ok(discovery) = self.models_rx.try_recv() {
            self.apply_model_discovery(discovery);
            applied_model_discovery = true;
        }
        if applied_model_discovery {
            return Ok(());
        }

        // 3b) Codex login is intentionally a background operation so a browser
        // close or an abandoned device flow cannot stop terminal input.
        if self.apply_codex_login_events().await {
            return Ok(());
        }
        if self.apply_mcp_login_events() {
            return Ok(());
        }

        // 3c) proactive wake: when an everruns `spawn_background` task finishes
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
                    if self.render_mode.is_fullscreen() {
                        self.clear_selection();
                    }
                    self.handle_key(key).await;
                }
                CrosstermEvent::Mouse(mouse) => {
                    if self.render_mode.is_fullscreen() {
                        if let Some(shape) = self.update_link_pointer(mouse) {
                            let _ = pointer::write(&mut std::io::stdout(), shape);
                        }
                        if self.handle_fullscreen_scroll(mouse.kind) {
                            continue;
                        }
                        if self.handle_fullscreen_selection(mouse) {
                            continue;
                        }
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
                self.printed_rows = 0;
                self.transcript_generation = self.transcript_generation.wrapping_add(1);
            }
            Err(error) => self.push_system(format!("refresh restored transcript: {error}")),
        }
        self.push_system(notice);
        if let Some(prompt) = self.session.take_restored_prompt() {
            self.set_input_text(prompt);
        }
    }

    /// Rows the footer devotes to not-yet-published transcript this frame —
    /// what is left of `area` once the composer chrome takes its share, and so
    /// exactly what [`draw_recent_transcript`] will paint into.
    fn footer_transcript_rows(&self, area: Rect) -> u16 {
        // Deliberately not special-cased when an overlay owns the footer (see
        // `draw_shared`): the sheet hides the retained tail while it is up, but
        // publishing it instead would empty this region for the rest of the
        // session — nothing would be left to paint here once the sheet closes,
        // and the transcript would resume against a band of blank rows.
        let input_width = area.width.saturating_sub(2);
        let state = self.view_state();
        app_layout_for_frame(
            area,
            self.input_height(input_width),
            state.status_row_count(),
            chrome_preview_visible(&state),
        )
        .transcript
        .height
    }

    /// Publish finalized transcript lines into the terminal scrollback above
    /// the footer, holding back the tail that fills its `keep_rows` rows.
    ///
    /// A line is shown in exactly one place: the footer paints what is not yet
    /// published (see [`recent_transcript_lines`]), the terminal owns the rest.
    /// Publishing the whole transcript eagerly and *also* mirroring its tail in
    /// the footer would print every line twice on a screen tall enough to show
    /// both — the footer reserves the same rows either way, so the retained
    /// window costs nothing and reads as one continuous transcript.
    ///
    /// `keep_rows` is the footer's transcript region for this frame; pass 0 to
    /// publish everything, which is what [`App::run`] does on the way out so no
    /// line is lost when the footer's rows are handed back.
    fn flush_transcript<B>(&mut self, terminal: &mut Terminal<B>, keep_rows: u16) -> Result<()>
    where
        B: Backend,
        B::Error: std::error::Error + Send + Sync + 'static,
    {
        if self.printed_lines >= self.lines.len() {
            return Ok(());
        }

        let width = terminal.size()?.width.saturating_sub(2).max(20) as usize;
        let (rendered, boundaries) = self.unpublished_rows(width);
        let publish = rendered.len().saturating_sub(keep_rows as usize);
        if publish == 0 {
            return Ok(());
        }

        // Publishing goes through tuika's split-footer seam rather than a raw
        // `insert_before`: `publish_block` commits one view above the footer,
        // sized by what it measures, and paints without a background fill so a
        // block reads as part of the surrounding terminal session. It borrows,
        // so the block needs no `'static`/`Send` round-trip through
        // `Scrollback` — this loop already owns the frame.
        let theme = fullscreen::yolop_theme();
        let ctx = tuika::RenderCtx::new(&theme);
        for chunk in rendered[..publish].chunks(tuika::Scrollback::MAX_BLOCK_ROWS as usize) {
            let block = tuika::components::Text::new(chunk.to_vec());
            tuika::screen::publish_block(terminal, &block, &ctx)?;
        }
        self.advance_publish_cursor(publish, &boundaries);
        Ok(())
    }

    /// The transcript rows the terminal does not own yet, oldest first, plus
    /// how many rows each entry contributed. Rows already published out of the
    /// entry the last flush cut in half are dropped from the front, so the
    /// result is exactly what is still owed to the screen.
    fn unpublished_rows(&self, width: usize) -> (Vec<Line<'static>>, Vec<(usize, usize)>) {
        let mut rendered: Vec<Line<'static>> = Vec::new();
        let mut boundaries: Vec<(usize, usize)> = Vec::new();
        for index in self.printed_lines..self.lines.len() {
            let before = rendered.len();
            append_chat_lines(&mut rendered, &self.lines[index], width);
            if should_insert_chat_gap(
                &self.lines[index].author,
                self.lines.get(index + 1).map(|line| &line.author),
            ) {
                rendered.push(Line::from(""));
            }
            boundaries.push((index, rendered.len() - before));
        }
        let skip = self.printed_rows.min(rendered.len());
        (rendered.split_off(skip), boundaries)
    }

    /// Move the publish cursor forward by `rows` rendered rows, which may stop
    /// part-way through an entry.
    fn advance_publish_cursor(&mut self, rows: usize, boundaries: &[(usize, usize)]) {
        let mut remaining = self.printed_rows + rows;
        for (index, entry_rows) in boundaries {
            if remaining >= *entry_rows {
                remaining -= entry_rows;
                self.printed_lines = index + 1;
                self.printed_rows = 0;
            } else {
                self.printed_lines = *index;
                self.printed_rows = remaining;
                return;
            }
        }
    }

    async fn handle_key(&mut self, key: KeyEvent) {
        // Reverse-history search owns the keyboard while active (even Ctrl+C,
        // which cancels the search rather than arming exit).
        if self.history_search.is_some() {
            self.handle_search_key(key);
            return;
        }
        if self.busy && key.code != KeyCode::Esc {
            self.esc_pending_cancel = false;
        }
        // App-global chord shortcuts resolve through the keymap (see
        // `crate::tui::keymap`), so yolop's bindings live in one declarative
        // place instead of a hand-rolled match. They fire regardless of mode
        // (mid-turn, during setup, or with an overlay open) — the same ordering
        // they had before, since this sits ahead of every modal guard below.
        if let Some(action) = self.dispatch_global_key(key) {
            match action {
                GlobalAction::ReverseSearch => self.history_search_start(),
                GlobalAction::Interrupt => self.handle_ctrl_c(),
                GlobalAction::Quit => {
                    self.abort_codex_login();
                    self.abort_mcp_login();
                    self.should_quit = true;
                }
                GlobalAction::ToggleBackground => {
                    // This branch returns before the shared grace reset below, so
                    // clear the armed single-Ctrl+C exit ourselves.
                    self.disarm_ctrl_c_pending_exit();
                    self.toggle_background_panel();
                }
                GlobalAction::PasteImage => self.try_paste_clipboard(),
            }
            return;
        }

        self.disarm_ctrl_c_pending_exit_if_grace_elapsed();

        if self.pending_sandbox_approval.is_some() {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    if let Some(pending) = self.pending_sandbox_approval.take() {
                        let _ = pending
                            .reply
                            .send(crate::sandbox_approval::ApprovalDecision::ApproveOnce);
                        self.push_system("approved".into());
                    }
                }
                KeyCode::Char('a') | KeyCode::Char('A') => {
                    if let Some(pending) = self.pending_sandbox_approval.take() {
                        let _ = pending
                            .reply
                            .send(crate::sandbox_approval::ApprovalDecision::ApproveForSession);
                        self.push_system("approved for this session".into());
                    }
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                    self.deny_pending_sandbox_approval();
                }
                _ => {}
            }
            return;
        }

        // A manually opened/focused sidebar captures task navigation keys. An
        // automatically opened sidebar is passive and leaves the composer live.
        if self.background_panel_focused {
            self.handle_background_panel_key(key).await;
            return;
        }

        // An extension `ui/ask` prompt owns the keyboard even mid-turn, since
        // the server typically asks while a tool is running.
        if self.pending_ask.is_some() {
            self.handle_ask_key(key);
            return;
        }

        if self.busy && key.code == KeyCode::Esc {
            self.handle_busy_key(key);
            return;
        }
        if self.setup.is_some() {
            self.handle_setup_key(key).await;
            return;
        }
        match key.code {
            KeyCode::Enter => match self
                .composer
                .handle_enter(key.modifiers == KeyModifiers::SHIFT)
            {
                InputOutcome::Submitted => self.submit_input().await,
                InputOutcome::Ignored
                | InputOutcome::Consumed
                | InputOutcome::Changed
                | InputOutcome::Cancelled => {}
            },
            KeyCode::Tab => {
                if !self.busy
                    && let Some(suggestion) = self.suggestions().first()
                {
                    self.set_input_text(suggestion.completion.clone());
                } else {
                    self.composer_edit_key(key);
                }
            }
            KeyCode::Up if self.try_history_prev() => {}
            KeyCode::Down if self.try_history_next() => {}
            _ => {
                self.composer_edit_key(normalize_printable_key(key));
            }
        }
    }

    /// Resolve one crossterm key against the app-global [`keymap`](crate::tui::keymap),
    /// returning the [`GlobalAction`] to run when a global chord matches.
    ///
    /// Every global binding is a single stroke, so the keymap never returns
    /// `Pending` here; a non-match (`Unmatched`) — or a key that does not
    /// translate to a `tuika` event, such as a release — yields `None` so the
    /// key falls through to the composer and modal handlers below.
    fn dispatch_global_key(&mut self, key: KeyEvent) -> Option<GlobalAction> {
        let tuika::Event::Key(translated) = tuika::translate_event(CrosstermEvent::Key(key))?
        else {
            return None;
        };
        match self.keymap.dispatch(translated) {
            Dispatch::Command(action) => Some(action),
            Dispatch::Pending | Dispatch::Unmatched => None,
        }
    }

    /// Feed one editing key to the composer. The tuika `TextInputState` handles
    /// the event itself (component-driven input) after translation from crossterm.
    fn composer_edit_key(&mut self, key: KeyEvent) {
        if let Some(event) = tuika::translate_event(CrosstermEvent::Key(key)) {
            let _ = self.composer.handle(&event);
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
            Some(entry) if entry == text => self.composer_cursor_row() == 0,
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
                let last_row = self.composer_line_count().saturating_sub(1);
                if self.composer_cursor_row() != last_row {
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
            || self.background_panel_focused
            || self.pending_ask.is_some()
            || self.history.is_empty()
        {
            return;
        }
        self.history.reset_navigation();
        self.history_search = Some(HistorySearch {
            query: String::new(),
            match_index: None,
            saved_lines: self.composer_lines(),
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
                self.clear_composer_text();
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
        self.set_input_text(lines.join("\n"));
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
        if self.composer_line_count() == 1
            && let Some(file) =
                file_path_suggestions(&self.suggestion_input(), &self.startup.workspace_root)
        {
            return file;
        }
        command_suggestions(&self.suggestion_input(), &self.startup.capability_commands)
    }

    fn composer_line_count(&self) -> usize {
        self.composer.line_count()
    }

    /// The composer's first line — the anchor for `@`/slash completion.
    fn suggestion_input(&self) -> String {
        self.composer_lines().into_iter().next().unwrap_or_default()
    }

    // ---- composer read/write surface (over the shared `TextInputState`) --------

    fn input_text(&self) -> String {
        self.composer.text()
    }

    /// The composer's logical lines.
    fn composer_lines(&self) -> Vec<String> {
        self.composer
            .text()
            .split('\n')
            .map(str::to_string)
            .collect()
    }

    /// The composer cursor's logical row (line index).
    fn composer_cursor_row(&self) -> usize {
        self.composer.cursor().0
    }

    fn set_input_text(&mut self, text: String) {
        // `set_text` splits on newlines (restoring a recalled multi-line prompt as
        // multiple rows) and parks the cursor at the end.
        self.composer.set_text(&text);
    }

    /// Clear the composer text (only) to a single empty line, leaving pending
    /// pastes intact — used by reverse-search's "no match" state.
    fn clear_composer_text(&mut self) {
        self.composer.clear();
    }

    fn reset_input(&mut self) {
        self.clear_composer_text();
        self.pending_pastes.clear();
    }

    fn input_height(&self, input_width: u16) -> u16 {
        self.composer
            .visual_height(input_width)
            .clamp(1, MAX_INPUT_HEIGHT)
    }

    fn handle_paste(&mut self, pasted: String) {
        if self.setup.is_some() || self.background_panel_focused {
            return;
        }
        self.esc_pending_cancel = false;

        let pasted = crate::tui::input::paste_attachment::normalize_pasted_text(&pasted);
        if pasted.is_empty() {
            return;
        }

        if pasted.len() > crate::tui::input::paste_attachment::MAX_PASTE_ATTACHMENT_BYTES {
            self.push_system(format!(
                "paste too large (max {} KiB)",
                crate::tui::input::paste_attachment::MAX_PASTE_ATTACHMENT_BYTES / 1024
            ));
            return;
        }

        if crate::tui::input::paste_attachment::is_large_paste(&pasted) {
            let char_count = pasted.chars().count();
            let placeholder = crate::tui::input::paste_attachment::next_large_paste_placeholder(
                char_count,
                &self.pending_pastes,
            );
            self.composer_insert_str(&placeholder);
            self.pending_pastes.push((placeholder, pasted));
        } else {
            self.composer_insert_str(&pasted);
        }
    }

    /// Insert a string at the composer cursor (honoring embedded newlines).
    fn composer_insert_str(&mut self, s: &str) {
        self.composer.insert_str(s);
    }

    fn try_paste_clipboard(&mut self) {
        if self.setup.is_some() || self.background_panel_focused {
            return;
        }
        match crate::tui::input::clipboard_paste::paste_image_content_part() {
            Ok((part, info)) => {
                self.pending_images.push(part);
                let index = self.pending_images.len();
                self.push_system(format!(
                    "attached clipboard image #{index} ({}x{} PNG)",
                    info.width, info.height
                ));
            }
            Err(crate::tui::input::clipboard_paste::PasteImageError::NoImage(_)) => {
                if let Ok(text) = crate::tui::input::clipboard_paste::paste_clipboard_text() {
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
        // Selector mode: arrow keys move, Enter picks the highlighted option.
        if !ask.options.is_empty() {
            match key.code {
                KeyCode::Up => {
                    ask.selected = ask.selected.saturating_sub(1);
                }
                KeyCode::Down => {
                    ask.selected = (ask.selected + 1).min(ask.options.len() - 1);
                }
                KeyCode::Enter => {
                    let answer = ask.options.get(ask.selected).cloned().unwrap_or_default();
                    self.resolve_ask(crate::tui::host_ui::AskAnswer {
                        answer,
                        cancelled: false,
                    });
                }
                KeyCode::Esc => {
                    self.resolve_ask(crate::tui::host_ui::AskAnswer {
                        answer: String::new(),
                        cancelled: true,
                    });
                }
                _ => {}
            }
            return;
        }
        match key.code {
            KeyCode::Enter => {
                let answer = ask.value.clone();
                self.resolve_ask(crate::tui::host_ui::AskAnswer {
                    answer,
                    cancelled: false,
                });
            }
            KeyCode::Esc => {
                self.resolve_ask(crate::tui::host_ui::AskAnswer {
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
    fn resolve_ask(&mut self, answer: crate::tui::host_ui::AskAnswer) {
        if let Some(mut ask) = self.pending_ask.take() {
            let shown = if answer.cancelled {
                "(cancelled)".to_string()
            } else if ask.secret {
                // Never echo a secret answer into the transcript.
                "(saved)".to_string()
            } else {
                answer.answer.clone()
            };
            if let Some(reply) = ask.reply.take() {
                let _ = reply.send(answer);
            }
            self.push_system(format!("answered: {shown}"));
        }
    }

    fn deny_pending_sandbox_approval(&mut self) {
        if let Some(pending) = self.pending_sandbox_approval.take() {
            let _ = pending
                .reply
                .send(crate::sandbox_approval::ApprovalDecision::Deny);
            self.push_system("denied".into());
        }
    }

    async fn submit_input(&mut self) {
        let raw = self.input_text();
        crate::tui::input::paste_attachment::prune_pending_pastes(&raw, &mut self.pending_pastes);
        let pending_pastes = std::mem::take(&mut self.pending_pastes);
        let expanded =
            crate::tui::input::paste_attachment::expand_pending_pastes(&raw, &pending_pastes);
        self.reset_input();
        let text = expanded.trim().to_string();
        let display_text = raw.trim().to_string();
        // Record what the user typed (including slash/bang commands) for
        // shell-style Up/Down recall. Uses the pre-expansion display text so
        // paste placeholders recall as the user saw them.
        self.history.record(&display_text);
        if !self.busy {
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
        }
        if text.is_empty() && self.pending_images.is_empty() {
            return;
        }
        let image_count = self.pending_images.len();
        let display = crate::tui::input::image_input::user_display_text(&display_text, image_count);
        self.push_user(display.clone());
        let images = std::mem::take(&mut self.pending_images);
        if self.busy {
            self.queued_messages.push_back(QueuedMessage {
                prompt: text,
                display,
                images,
            });
        } else {
            self.begin_user_request(&text);
            self.start_turn_with_images(text, images);
        }
    }

    /// Focus a passive activity rail, close a focused one, or open it focused.
    /// Suppressed while the setup overlay is up so two modals never stack.
    fn toggle_background_panel(&mut self) {
        if self.background_panel.is_some() && self.background_panel_focused {
            self.background_panel = None;
            self.background_panel_focused = false;
        } else if self.background_panel.is_some() {
            self.background_panel_focused = true;
            self.select_first_visible_activity();
        } else if self.setup.is_none() {
            self.background_panel = Some(0);
            self.background_panel_focused = true;
            self.select_first_visible_activity();
        }
    }

    fn select_first_visible_activity(&mut self) {
        let offset = self.activity_scroll.offset();
        if let Some(first) = self.activity_rail().and_then(|rail| {
            rail.rows
                .iter()
                .skip(offset)
                .find_map(|row| match row {
                    crate::tui::session_tasks_view::ActivityRailRow::Task(task) => {
                        Some(task.task_index)
                    }
                    _ => None,
                })
                .or_else(|| rail.selectable_task_indices().last().copied())
        }) {
            self.background_selected = first;
            self.ensure_activity_selection_visible();
        }
    }

    fn ensure_activity_selection_visible(&mut self) {
        let Some(row) = self
            .activity_rail()
            .and_then(|rail| rail.body_row_for_task(self.background_selected))
        else {
            return;
        };
        let (content, viewport) = self.activity_scroll_metrics;
        if viewport == 0 {
            return;
        }
        let offset = self.activity_scroll.offset();
        if row < offset {
            self.activity_scroll.set_offset(row);
        } else if row >= offset.saturating_add(viewport) {
            self.activity_scroll
                .set_offset(row.saturating_add(1).saturating_sub(viewport));
        }
        self.activity_scroll.clamp(content, viewport);
    }

    /// Navigation and cooperative cancellation for the focused activity rail.
    async fn handle_background_panel_key(&mut self, key: KeyEvent) {
        if self.background_panel.is_none() {
            return;
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.background_panel = None;
                self.background_panel_focused = false;
            }
            KeyCode::Up | KeyCode::Char('k') => {
                let selectable = self
                    .activity_rail()
                    .map(|rail| rail.selectable_task_indices())
                    .unwrap_or_default();
                if let Some(position) = selectable
                    .iter()
                    .position(|index| *index == self.background_selected)
                    && position > 0
                {
                    self.background_selected = selectable[position - 1];
                }
                self.ensure_activity_selection_visible();
            }
            KeyCode::Down | KeyCode::Char('j') => {
                let selectable = self
                    .activity_rail()
                    .map(|rail| rail.selectable_task_indices())
                    .unwrap_or_default();
                if let Some(position) = selectable
                    .iter()
                    .position(|index| *index == self.background_selected)
                    && let Some(next) = selectable.get(position + 1)
                {
                    self.background_selected = *next;
                }
                self.ensure_activity_selection_visible();
            }
            KeyCode::PageUp => {
                let (_, viewport) = self.activity_scroll_metrics;
                self.activity_scroll.set_offset(
                    self.activity_scroll
                        .offset()
                        .saturating_sub(viewport.saturating_sub(1).max(1)),
                );
            }
            KeyCode::PageDown => {
                let (content, viewport) = self.activity_scroll_metrics;
                let next = self
                    .activity_scroll
                    .offset()
                    .saturating_add(viewport.saturating_sub(1).max(1));
                if next >= ScrollState::max_offset(content, viewport) {
                    self.activity_scroll.jump_to_bottom(content, viewport);
                } else {
                    self.activity_scroll.set_offset(next);
                }
            }
            KeyCode::Char('x') | KeyCode::Delete => {
                self.cancel_selected_task().await;
            }
            _ => {}
        }
    }

    async fn cancel_selected_task(&mut self) {
        let Some(task) = self
            .session_tasks
            .selected(self.background_selected)
            .cloned()
        else {
            return;
        };
        if task.state.is_terminal() {
            self.push_system(format!("task {} is already {}", task.id, task.state));
            return;
        }
        if task.kind == everruns_core::session_task::TASK_KIND_MONITOR {
            match crate::capabilities::session_tasks_override::cancel_monitor_task(
                &task,
                self.task_registry.as_ref(),
                self.task_schedule_store.as_ref(),
            )
            .await
            {
                Ok(_) => self.push_system(format!("monitor {} disarmed", task.id)),
                Err(error) => self.push_system(format!("could not cancel {}: {error}", task.id)),
            }
        } else {
            match self
                .task_registry
                .request_cancel(task.session_id, &task.id)
                .await
            {
                Ok(Some(_)) => self.push_system(format!("cancellation requested for {}", task.id)),
                Ok(None) => self.push_system(format!("task {} no longer exists", task.id)),
                Err(error) => self.push_system(format!("could not cancel {}: {error}", task.id)),
            }
        }
        self.refresh_session_tasks().await;
    }

    /// Proactive wake: drain any everruns `spawn_background` completion signal
    /// delivered over [`crate::runtime::background_wake`] and, while idle, start a turn so
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
        let message = crate::runtime::background_wake::coalesce_pending_wakes(
            message,
            &mut self.background_wake,
        )
        .with_active_goal(self.goal_store.active_condition(self.session.session_id()))
        .with_active_ask(self.user_ask_store.active_text(self.session.session_id()));
        if !self.settings.snapshot().proactive_wake_enabled() {
            self.push_system(
                "✓ background task finished — see /background (proactive wake off)".to_string(),
            );
            return false;
        }
        self.push_system("↻ background task finished — waking agent to review".to_string());
        let prompt = crate::runtime::background_wake::frame_wake_prompt(&message);
        let input = crate::runtime::background_wake::input_for_wake(&message);
        self.start_turn_input(prompt, input);
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
    ///
    /// Returns the system transcript lines produced while applying the command
    /// so agent-facing `HostUi::request` callers (e.g. `run_command`) can
    /// surface `/mcp` / `/tools` output conversationally.
    async fn apply_ui_command(&mut self, command: UiCommand) -> Vec<String> {
        let clearing = matches!(&command, UiCommand::ClearTranscript);
        let start = self.lines.len();
        match command {
            UiCommand::ShowHelp => self.show_help(),
            UiCommand::ShowTools => self.show_tools().await,
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
                self.printed_rows = 0;
                self.transcript_generation = self.transcript_generation.wrapping_add(1);
                self.goal_store.clear_active(self.session.session_id());
                self.user_ask_store.clear_active(self.session.session_id());
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
            UiCommand::SetAgentStatus { status } => {
                let status = status.trim();
                self.agent_status = (!status.is_empty()).then(|| status.to_string());
            }
            UiCommand::SetExtensionStatus { ext, status } => {
                let status = status.trim();
                if status.is_empty() {
                    self.extension_status.remove(&ext);
                } else {
                    self.extension_status.insert(ext, status.to_string());
                }
            }
            UiCommand::SetExtensionActive {
                capability_id,
                name,
                activate,
            } => {
                self.set_extension_active(&capability_id, &name, activate)
                    .await;
            }
        }
        let from = if clearing {
            0
        } else {
            start.min(self.lines.len())
        };
        self.lines[from..]
            .iter()
            .filter(|line| line.author == Author::System)
            .map(|line| line.text.clone())
            .collect()
    }

    /// List capability tools plus live MCP tools discovered from the session's
    /// scoped servers (so `/tools` matches what the next turn can call).
    async fn show_tools(&mut self) {
        let mut names = self.startup.tool_names.clone();
        let mcp_names = self.session.list_mcp_tool_names().await;
        for name in mcp_names {
            if !names.iter().any(|existing| existing == &name) {
                names.push(name);
            }
        }
        names.sort();
        self.push_system(format!("tools: {}", names.join(", ")));
    }

    async fn set_extension_active(&mut self, capability_id: &str, name: &str, activate: bool) {
        // enable_extension/disable_extension already persisted the setting;
        // here we apply it to the running session so it takes effect on the
        // next turn (EVE-795) rather than only next start.
        let result = if activate {
            self.session.activate_capability(capability_id).await
        } else {
            self.session.deactivate_capability(capability_id).await
        };
        match result {
            Ok(delta) if delta.changed => self.push_system(format!(
                "extension `{name}` {} for this session; effective on the next turn.",
                if activate { "enabled" } else { "disabled" }
            )),
            // No overlay change: already in the desired state, or (on disable)
            // it rides the harness layer from startup and only settings can
            // drop it — which happens next session.
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

    const MCP_USAGE: &'static str =
        "usage: /mcp [reload | login <name> | enable|disable|remove <name> [global|workspace]]";

    async fn manage_mcp_command(&mut self, raw: Option<&str>) {
        let Some(raw) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
            if self.startup.mcp_server_names.is_empty() {
                let global = crate::config::mcp::global_mcp_config_path()
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
            None | Some("global") => crate::config::mcp::McpConfigScope::Global,
            Some("workspace") => crate::config::mcp::McpConfigScope::Workspace,
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
            crate::config::mcp::McpConfigStore::default_for_workspace(&self.startup.workspace_root);
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

    /// Start the interactive OAuth login flow for a remote MCP server without
    /// blocking the host event loop. Prints the authorize URL to the transcript
    /// (so fullscreen and inline both stay conversational if the browser is
    /// invisible), best-effort opens the browser, and completes in the
    /// background — same pattern as Codex login.
    async fn mcp_login(&mut self, name: &str) {
        let servers = crate::config::mcp::load_mcp_servers(&self.startup.workspace_root);
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
        if self.mcp_login.is_some() {
            self.push_system(
                "an MCP OAuth login is already in progress; finish or wait for it before starting another"
                    .into(),
            );
            return;
        }

        let url = server.url.clone();
        let provider_key = server
            .oauth_provider_id
            .clone()
            .unwrap_or_else(|| name.to_string());
        let server_name = name.to_string();

        let prepared = match crate::auth::mcp_oauth_login::prepare_login(&url, None, None).await {
            Ok(prepared) => prepared,
            Err(error) => {
                self.push_system(format!("`{server_name}` OAuth login failed: {error}"));
                return;
            }
        };

        self.push_system(format!(
            "MCP OAuth for `{server_name}` — open this URL to sign in:\n{}",
            prepared.authorize_url
        ));
        if let Err(error) = crate::auth::oauth_flow::open_browser(prepared.authorize_url.as_str()) {
            self.push_system(format!(
                "could not open a browser automatically ({error}); open the URL above manually"
            ));
        } else {
            self.push_system(format!(
                "opened a browser for `{server_name}` OAuth login (waiting for redirect)…"
            ));
        }

        self.next_mcp_login_id = self.next_mcp_login_id.wrapping_add(1);
        let id = self.next_mcp_login_id;
        let tx = self.mcp_login_tx.clone();
        let name_for_task = server_name.clone();
        let provider_for_task = provider_key.clone();
        let task = tokio::spawn(async move {
            let result = crate::auth::mcp_oauth_login::complete_login(prepared)
                .await
                .map_err(|error| error.to_string());
            let _ = tx.send(McpLoginEvent::Finished {
                id,
                name: name_for_task,
                provider_key: provider_for_task,
                result,
            });
        });
        self.mcp_login = Some(PendingMcpLogin {
            id,
            name: server_name,
            task,
        });
    }

    fn apply_mcp_login_events(&mut self) -> bool {
        let mut applied = false;
        while let Ok(event) = self.mcp_login_rx.try_recv() {
            let McpLoginEvent::Finished {
                id,
                name,
                provider_key,
                result,
            } = event;
            let current_id = self.mcp_login.as_ref().map(|login| login.id);
            if current_id != Some(id) {
                continue;
            }
            self.mcp_login = None;
            applied = true;
            match result {
                Ok(tokens) => {
                    match crate::auth::mcp_oauth::save_tokens(
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
                Err(error) => {
                    self.push_system(format!("`{name}` OAuth login failed: {error}"));
                }
            }
        }
        applied
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
        // The selection is content-anchored, so scrolling keeps it: it simply
        // moves with the text (and lets a drag extend across the boundary).
        self.scroll_transcript(kind)
    }

    /// Apply one wheel step to the transcript scroll. Returns true when consumed.
    fn scroll_transcript(&mut self, kind: MouseEventKind) -> bool {
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
        if self.render_mode.is_fullscreen() {
            let action = self.status_hit_regions.iter().find_map(|(area, action)| {
                (mouse.column >= area.x
                    && mouse.column < area.right()
                    && mouse.row >= area.y
                    && mouse.row < area.bottom())
                .then_some(*action)
            });
            let Some(action) = action else {
                return false;
            };
            match action {
                StatusAction::ToggleLayout => self.set_status_layout(None),
                StatusAction::OpenModel => self.start_model_setup(),
                StatusAction::OpenEffort => self.start_effort_setup(""),
                StatusAction::OpenBackground => {
                    self.background_panel = Some(0);
                    self.background_panel_focused = true;
                    self.select_first_visible_activity();
                }
            }
            return true;
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
        if !self.input_text().trim().is_empty() {
            self.reset_input();
            self.disarm_ctrl_c_pending_exit();
            return;
        }

        if self.ctrl_c_pending_exit() {
            self.abort_codex_login();
            self.abort_mcp_login();
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
        self.agent_status = None;
        self.turn_started_at = None;
        self.stream_preview = None;
        self.rx = None;
        self.turn_cancel = None;
        if self.native_progress {
            self.term_progress.clear();
        }
        self.esc_pending_cancel = false;
    }

    /// Start the oldest steering message after the active turn has fully
    /// settled. One runtime turn runs at a time; later messages remain queued.
    fn start_next_queued_turn(&mut self) -> bool {
        let Some(message) = self.queued_messages.pop_front() else {
            return false;
        };
        self.begin_user_request(&message.prompt);
        self.start_turn_with_images(message.prompt, message.images);
        true
    }

    fn begin_user_request(&mut self, prompt: &str) {
        self.completion_budget.reset();
        if self.user_ask_enabled
            && let Err(err) = self
                .user_ask_store
                .record_user_prompt(self.session.session_id(), prompt)
        {
            self.push_system(format!("user ask: {err}"));
        }
    }

    fn record_completion_state(
        &mut self,
        state: crate::session_state::task_completion::CompletionState,
    ) {
        let session_id = self.session.session_id();
        if !self.user_ask_enabled || !self.user_ask_store.is_active(session_id) {
            return;
        }
        let evaluation = crate::session_state::task_completion::evaluation_for_state(state);
        if let Err(err) = self
            .user_ask_store
            .record_evaluation(session_id, &evaluation)
        {
            self.push_system(format!("user ask: {err}"));
            return;
        }
        self.push_system(evaluation_status_message(&evaluation));
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

    async fn after_turn_user_ask_check(&mut self, result: Option<everruns_runtime::TurnResult>) {
        if !self.user_ask_enabled {
            return;
        }
        let session_id = self.session.session_id();
        if !self.user_ask_store.is_active(session_id) {
            return;
        }
        let Some(result) = result else { return };
        let tokens = self.session.turn_tokens(result.turn_id).await;
        if !self.completion_budget.observe_turn(tokens) {
            self.push_system("user ask budget exhausted; send a message to resume".into());
            return;
        }
        let has_background = self
            .task_registry
            .list(session_id, None)
            .await
            .unwrap_or_default()
            .iter()
            .any(|task| !task.state.is_terminal());
        if let crate::session_state::task_completion::GateDecision::Conclusive(state) =
            crate::session_state::task_completion::gate_turn(&result, has_background)
        {
            let evaluation = crate::session_state::task_completion::evaluation_for_state(state);
            let outcome = evaluation.outcome;
            let reason = evaluation.reason.clone();
            if let Err(err) = self
                .user_ask_store
                .record_evaluation(session_id, &evaluation)
            {
                self.push_system(format!("user ask: {err}"));
                return;
            }
            self.push_system(evaluation_status_message(&evaluation));
            match outcome {
                AskOutcome::InProgress => {
                    let prompt =
                        crate::session_state::task_completion::continuation_prompt(&reason);
                    self.start_continuation_turn(prompt);
                }
                AskOutcome::Blocked => self
                    .session
                    .report_herdr_state(crate::capabilities::herdr::HerdrState::Blocked),
                AskOutcome::Achieved | AskOutcome::Failed | AskOutcome::WaitingOnBackground => {}
            }
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
        if evaluation.outcome == AskOutcome::InProgress {
            let prompt =
                crate::session_state::task_completion::continuation_prompt(&evaluation.reason);
            self.start_continuation_turn(prompt);
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

    /// Wire a freshly started [`crate::runtime::session::TurnHandle`] into the event loop:
    /// store its receiver and cancel trigger and flip into the busy state.
    fn begin_turn(
        &mut self,
        handle: crate::runtime::session::TurnHandle,
        activity: Option<String>,
    ) {
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
        let images = std::mem::take(&mut self.pending_images);
        self.start_turn_with_images(prompt, images);
    }

    fn start_continuation_turn(&mut self, prompt: String) {
        let input = crate::session_state::task_completion::tag_continuation(
            self.model.input_message(prompt.clone()),
        );
        self.start_turn_input(prompt, input);
    }

    fn start_turn_with_images(&mut self, prompt: String, images: Vec<ContentPart>) {
        self.prepare_turn(&prompt);
        let handle = self.session.run_turn(prompt, images);
        self.begin_turn(handle, None);
    }

    fn start_turn_input(
        &mut self,
        prompt: String,
        input: everruns_core::message_retriever::InputMessage,
    ) {
        self.prepare_turn(&prompt);
        let handle = self.session.run_turn_input(prompt, input);
        self.begin_turn(handle, None);
    }

    fn prepare_turn(&mut self, prompt: &str) {
        match self.worktree.ensure_before_turn(prompt) {
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
        "  Ctrl+V paste image/text · Ctrl+B activity · !<cmd> shell alias",
        "  exit: Ctrl-C twice / Ctrl-D",
        "  steer while busy: type and Enter to queue · cancel turn: Esc twice",
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
    use tuika::term::hyperlink;

    use super::*;
    use crate::capabilities::model_discovery::DiscoveredProviderModel;
    use everruns_core::events::{
        Event as RuntimeEvent, EventContext, InputMessageData, OutputMessageCompletedData,
        OutputMessageStartedData, ReasonCompletedData, ToolCompletedData,
    };
    use everruns_core::message::Message;
    use everruns_core::session_task::{
        CreateSessionTask, SessionTaskState, TASK_KIND_BACKGROUND_TOOL, TASK_KIND_MONITOR,
        TASK_KIND_SUBAGENT,
    };
    use everruns_core::tool_types::ToolCall;
    use everruns_core::{MessageId, SessionId, TurnId};

    use everruns_core::command::{CommandArg, CommandDescriptor, CommandSource};

    #[tokio::test]
    async fn last_assistant_message_ignores_later_non_assistant_lines() {
        let mut test = app_with_llmsim().await;
        test.app.lines.extend([
            ChatLine {
                author: Author::Assistant,
                text: "first".into(),
            },
            ChatLine {
                author: Author::Tool,
                text: "tool output".into(),
            },
            ChatLine {
                author: Author::Assistant,
                text: "last answer".into(),
            },
            ChatLine {
                author: Author::System,
                text: "done".into(),
            },
        ]);

        assert_eq!(test.app.last_assistant_message(), Some("last answer"));
    }

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
        impl crate::tui::host_ui::HostUi for NoopUi {
            fn send(&self, _: crate::tui::host_ui::UiCommand) {}
            fn request(
                &self,
                _: crate::tui::host_ui::UiCommand,
            ) -> tokio::sync::oneshot::Receiver<Vec<String>> {
                let (tx, rx) = tokio::sync::oneshot::channel();
                let _ = tx.send(Vec::new());
                rx
            }
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
    fn mermaid_fence_renders_as_terminal_diagram() {
        let mut lines: Vec<Line> = Vec::new();
        append_markdown_lines(
            &mut lines,
            "",
            Style::default(),
            "```mermaid\nflowchart LR\nA --> B\n```",
            80,
        );
        let text: String = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect();

        assert!(
            text.contains('A') && text.contains('B'),
            "diagram labels: {text:?}"
        );
        assert!(
            !text.contains("flowchart LR"),
            "source should become a diagram: {text:?}"
        );
    }

    #[test]
    fn safe_html_block_renders_as_styled_text() {
        let mut lines: Vec<Line> = Vec::new();
        append_markdown_lines(
            &mut lines,
            "",
            Style::default(),
            "<p>Hello <strong>terminal</strong></p><script>alert(1)</script>",
            80,
        );
        let spans: Vec<&Span> = lines.iter().flat_map(|line| line.spans.iter()).collect();
        let terminal = spans
            .iter()
            .find(|span| span.content.contains("terminal"))
            .expect("HTML text should render");

        assert!(
            terminal.style.add_modifier.contains(Modifier::BOLD),
            "strong HTML should render bold: {lines:?}"
        );
        assert!(
            spans.iter().all(|span| !span.content.contains("<strong>")),
            "safe HTML markup should not render literally: {lines:?}"
        );
        assert!(
            spans.iter().all(|span| !span.content.contains("alert(1)")),
            "unsafe HTML content should be ignored: {lines:?}"
        );
    }

    #[test]
    fn transcript_paragraph_links_urls() {
        let mut lines: Vec<Line> = Vec::new();
        let links = append_markdown_lines(
            &mut lines,
            "",
            Style::default(),
            "Visit https://rust-lang.org for docs",
            120,
        );
        let has_link = lines.iter().flat_map(|line| line.spans.iter()).any(|span| {
            span.content.contains("https://rust-lang.org")
                && span.style.fg == Some(fullscreen::yolop_theme().code.link)
                && !span.style.add_modifier.contains(Modifier::UNDERLINED)
        });
        assert!(
            has_link,
            "paragraph URL should be link-colored without masking native hover: {lines:?}"
        );
        assert!(
            links.iter().any(|l| l.url.contains("rust-lang.org")),
            "bare URL must produce a BufferLink: {links:?}"
        );
    }

    #[test]
    fn transcript_labeled_markdown_link_has_native_target() {
        // Labeled `[text](url)` must stay clickable after the transcript paints —
        // the third time this regressed, style was kept but the destination was
        // dropped so Ghostty had nothing to open.
        use ratatui::layout::Position;
        let mut lines: Vec<Line> = Vec::new();
        let links = append_markdown_lines(
            &mut lines,
            "agent › ",
            Style::default(),
            "PR: [#2875](https://github.com/everruns/everruns/pull/2875) merged.",
            100,
        );
        let link = links
            .iter()
            .find(|l| l.url.contains("pull/2875"))
            .expect("labeled markdown link must yield a BufferLink");
        let mut buffer = ratatui::buffer::Buffer::empty(Rect::new(0, 0, 120, 4));
        for (row, line) in lines.iter().enumerate() {
            let mut x = 0u16;
            for span in &line.spans {
                x = buffer.set_span(x, row as u16, span, 120).0;
            }
        }
        hyperlink::apply_buffer_links(
            &mut buffer,
            Position { x: 0, y: 0 },
            &links,
            hyperlink::LinkPolicy::WEB,
        );
        let mut event = tuika::Mouse::at(
            tuika::MouseKind::Up(tuika::MouseButton::Left),
            link.start_col + 1,
            link.line,
        );
        event.ctrl = true;
        assert_eq!(
            hyperlink::ctrl_click_url(&event, &buffer, Rect::new(0, 0, 120, 4)).as_deref(),
            Some(link.url.as_str()),
            "the PR label must preserve the markdown destination"
        );
    }

    #[test]
    fn transcript_markdown_renders_commonmark_emphasis() {
        // Assistant transcript now flows through tuika's CommonMark renderer, so
        // inline **bold** / *italic* are resolved — the previous line-oriented
        // formatter could not. Drive the real transcript entry point
        // (`append_chat_lines` for an assistant line) and assert on the
        // presentation model (styled spans), not the terminal buffer.
        let mut lines: Vec<Line> = Vec::new();
        append_chat_lines(
            &mut lines,
            &ChatLine {
                author: Author::Assistant,
                text: "a **bold** and *italic* word".to_string(),
            },
            80,
        );
        let spans: Vec<&Span> = lines.iter().flat_map(|line| line.spans.iter()).collect();
        let bold = spans
            .iter()
            .find(|s| s.content.contains("bold"))
            .expect("a span carrying the bold word");
        assert!(
            bold.style.add_modifier.contains(Modifier::BOLD),
            "**bold** should render bold: {lines:?}"
        );
        let italic = spans
            .iter()
            .find(|s| s.content.contains("italic"))
            .expect("a span carrying the italic word");
        assert!(
            italic.style.add_modifier.contains(Modifier::ITALIC),
            "*italic* should render italic: {lines:?}"
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn composer_supports_multiline_and_cursor_editing() {
        let mut fixture = app_with_llmsim().await;
        let app = &mut fixture.app;
        app.setup = None;

        // Type "ac", move left, insert "b" → "abc", move right, newline, "d".
        for key in [
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::empty()),
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::empty()),
            KeyEvent::new(KeyCode::Left, KeyModifiers::empty()),
            KeyEvent::new(KeyCode::Char('b'), KeyModifiers::empty()),
            KeyEvent::new(KeyCode::Right, KeyModifiers::empty()),
            KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT),
            KeyEvent::new(KeyCode::Char('d'), KeyModifiers::empty()),
        ] {
            app.handle_key(key).await;
        }

        assert_eq!(app.composer.text(), "abc\nd");
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
        assert_eq!(chrome_height(3, 1, false), 6);
        assert_eq!(chrome_height(3, 4, false), 9);
        assert_eq!(chrome_height(4, 1, false), 7);
        assert_eq!(chrome_height(4, 4, false), 10);
    }

    #[test]
    fn chrome_dimensions_clamp_input_to_visible_frame() {
        assert_eq!(chrome_dimensions(7, MAX_INPUT_HEIGHT, 4, false), (7, 1));
        assert_eq!(chrome_dimensions(5, MAX_INPUT_HEIGHT, 1, false), (5, 2));
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

    /// A `mermaid` fence in an assistant message is painted as a diagram, not
    /// echoed as source.
    #[test]
    fn markdown_mermaid_fence_renders_as_a_diagram() {
        let mut lines = Vec::new();
        append_chat_lines(
            &mut lines,
            &ChatLine {
                author: Author::Assistant,
                text: "```mermaid\nflowchart LR\n  A[Parse] --> B[Paint]\n```".to_string(),
            },
            80,
        );

        let body = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(
            body.contains("Parse") && body.contains("Paint"),
            "node labels should survive rendering: {body}"
        );
        assert!(
            !body.contains("flowchart LR"),
            "the Mermaid source should be replaced by the diagram: {body}"
        );
        assert!(
            body.contains('─') || body.contains('│'),
            "the diagram should be drawn with box-drawing cells: {body}"
        );
        assert!(
            lines.iter().all(|line| line_text(line).starts_with("agent")
                || line_text(line).starts_with("      ")),
            "diagram rows should keep the author gutter: {lines:?}"
        );
    }

    /// Mermaid mmdflux cannot parse falls back to the ordinary code block, so
    /// the source a user can still read stays on screen.
    #[test]
    fn markdown_unrenderable_mermaid_falls_back_to_source() {
        let mut lines = Vec::new();
        append_chat_lines(
            &mut lines,
            &ChatLine {
                author: Author::Assistant,
                text: "```mermaid\nthis is not a diagram\n```".to_string(),
            },
            80,
        );

        let body = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(
            body.contains("this is not a diagram"),
            "unrenderable Mermaid should keep its source visible: {body}"
        );
    }

    /// mmdflux lays diagrams out at their natural size; when that overflows the
    /// transcript we show the source instead of a clipped half-diagram.
    #[test]
    fn markdown_mermaid_too_wide_for_the_transcript_falls_back_to_source() {
        let text = "```mermaid\nflowchart LR\n  A[Parse] --> B[Layout] --> C[Paint]\n```";
        let mut narrow = Vec::new();
        append_chat_lines(
            &mut narrow,
            &ChatLine {
                author: Author::Assistant,
                text: text.to_string(),
            },
            30,
        );

        let body = narrow.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(
            body.contains("flowchart LR"),
            "a diagram wider than the transcript should fall back to source: {body}"
        );

        // The same diagram fits — and renders — once the transcript is wide
        // enough, so the fallback is about width, not about this input.
        let mut wide = Vec::new();
        append_chat_lines(
            &mut wide,
            &ChatLine {
                author: Author::Assistant,
                text: text.to_string(),
            },
            80,
        );
        assert!(
            !wide
                .iter()
                .map(line_text)
                .collect::<Vec<_>>()
                .join("\n")
                .contains("flowchart LR"),
            "the same diagram should render at width 80: {wide:?}"
        );
    }

    /// Other fence languages keep the syntax-highlighted code block.
    #[test]
    fn markdown_non_mermaid_fence_is_untouched_by_the_diagram_renderer() {
        let mut lines = Vec::new();
        append_chat_lines(
            &mut lines,
            &ChatLine {
                author: Author::Assistant,
                text: "```rust\nfn main() { println!(\"hi\"); }\n```".to_string(),
            },
            60,
        );

        let body = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(
            body.contains("fn main()"),
            "rust fences should render as code: {body}"
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
    fn should_not_insert_chat_gap_inside_tool_or_diff_blocks() {
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
        assert!(!should_insert_chat_gap(&Author::Diff, Some(&Author::Diff)));
        assert!(should_insert_chat_gap(
            &Author::Diff,
            Some(&Author::Assistant)
        ));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn recent_transcript_mirror_includes_image_notice_before_first_chat() {
        let mut fixture = app_with_llmsim().await;
        let app = &mut fixture.app;
        app.lines = vec![ChatLine {
            author: Author::System,
            text: "workspace: /tmp".into(),
        }];
        app.push_system("attached clipboard image #1 (640x480 PNG)".into());

        let visible = recent_transcript_lines(app, 80, 10);
        let visible = visible.iter().map(line_text).collect::<Vec<_>>();
        assert!(visible.iter().any(|line| line.contains("workspace: /tmp")));
        assert!(
            visible
                .iter()
                .any(|line| line.contains("attached clipboard image #1 (640x480 PNG)"))
        );
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
    use ratatui::backend::TestBackend;
    use ratatui::layout::Position;

    struct FooterComposerRender {
        lines: Vec<String>,
        viewport_bottom: u16,
    }

    /// The split-footer viewport the real TUI runs in: tuika's `ScreenMode`
    /// picks the viewport and `pin_footer` puts it on the terminal's last rows.
    fn split_footer_terminal(width: u16, height: u16) -> Terminal<TestBackend> {
        let mut backend = TestBackend::new(width, height);
        backend
            .set_cursor_position(Position { x: 0, y: 1 })
            .unwrap();
        let mut terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: tuika::ScreenMode::split_footer(COMPOSER_VIEWPORT_HEIGHT).viewport(),
            },
        )
        .expect("terminal");
        tuika::screen::pin_footer(&mut terminal).expect("pin footer");
        terminal
    }

    /// Publish through the same path the event loop uses: the footer keeps the
    /// tail that fits in its transcript rows, the terminal takes the rest.
    fn flush_for_frame(app: &mut App, terminal: &mut Terminal<TestBackend>) -> Result<()> {
        let keep_rows = app.footer_transcript_rows(terminal.get_frame().area());
        app.flush_transcript(terminal, keep_rows)
    }

    /// Every row of the footer viewport, top to bottom.
    fn viewport_rows(terminal: &mut Terminal<TestBackend>) -> Vec<String> {
        let viewport = terminal.get_frame().area();
        let buffer = terminal.backend().buffer();
        (viewport.y..viewport.bottom())
            .map(|y| {
                let mut row = String::with_capacity(viewport.width as usize);
                for x in 0..buffer.area.width {
                    row.push_str(buffer[(x, y)].symbol());
                }
                row.trim_end().to_string()
            })
            .collect()
    }

    fn footer_rows_text(terminal: &mut Terminal<TestBackend>) -> Vec<String> {
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

    fn render_footer_composer(
        app: &mut App,
        width: u16,
        terminal_height: u16,
        cursor_row: u16,
    ) -> FooterComposerRender {
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
                viewport: tuika::ScreenMode::split_footer(COMPOSER_VIEWPORT_HEIGHT).viewport(),
            },
        )
        .expect("terminal");
        tuika::screen::pin_footer(&mut terminal).expect("pin footer");
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
        FooterComposerRender {
            lines,
            viewport_bottom: viewport.y.saturating_add(viewport.height),
        }
    }

    fn presentation_state_idle() -> PresentationState {
        PresentationState {
            startup: StartupPresentation {
                workspace: "/work/yolop".to_string(),
                repository: None,
                safety_warning: None,
            },
            stream_preview: None,
            busy: false,
            queued_messages: 0,
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
            compaction_budget_percent: None,
            status_layout: StatusLayout::Compact,
            hooks_summary: "none".to_string(),
            approval_mode: "normal".to_string(),
            background: None,
            goal_indicator: None,
            ask_indicator: None,
            worktree_compact: None,
            worktree_expanded: None,
            agent_status: None,
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
                background: Some(crate::tui::session_tasks_view::BackgroundCounts {
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
        // Both renderers share one composer model, so the same draft appears in
        // either mode.
        test.app.composer.set_text("draft reply");

        test.app.set_render_mode(RenderMode::SplitFooter);
        let regular = render_app_lines(&mut test.app, 60, 20).join("\n");
        test.app.set_render_mode(RenderMode::Fullscreen);
        let fullscreen = render_app_lines(&mut test.app, 60, 20).join("\n");

        // Full-screen is composed with tuika and is *visually equivalent* to the
        // inline renderer (not necessarily byte-identical): the same transcript,
        // separators, composer prompt, and status content appear in both.
        for needle in ["hello from user", "draft reply", "llmsim", "───", "> "] {
            assert!(
                fullscreen.contains(needle),
                "full-screen should render {needle:?}: {fullscreen}"
            );
            assert!(
                regular.contains(needle),
                "inline should render {needle:?}: {regular}"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fullscreen_renders_blue_and_gold_separators() {
        use ratatui::backend::TestBackend;
        let mut test = app_with_llmsim().await;
        test.app.setup = None;
        test.app.set_render_mode(RenderMode::Fullscreen);
        test.app.push_user("hello".to_string());

        let backend = TestBackend::new(60, 20);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal.draw(|f| draw(f, &mut test.app)).expect("draw");
        let buffer = terminal.backend().buffer();

        // tuika paints the message separator in blue and the status separator in
        // gold — the design's "blue/gold line". Find a rule cell of each color.
        let mut blue_rule = false;
        let mut gold_rule = false;
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                let cell = &buffer[(x, y)];
                if cell.symbol() == "─" {
                    blue_rule |= cell.fg == ACCENT_BLUE;
                    gold_rule |= cell.fg == ACCENT_GOLD;
                }
            }
        }
        assert!(
            blue_rule,
            "message separator should render in blue via tuika"
        );
        assert!(
            gold_rule,
            "status separator should render in gold via tuika"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fullscreen_keeps_gold_separator_after_two_newlines() {
        use ratatui::backend::TestBackend;
        let mut test = app_with_llmsim().await;
        test.app.setup = None;
        test.app.set_render_mode(RenderMode::Fullscreen);
        for _ in 0..2 {
            test.app
                .handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT))
                .await;
        }
        assert_eq!(test.app.input_height(58), 3);

        let backend = TestBackend::new(60, 20);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal.draw(|f| draw(f, &mut test.app)).expect("draw");
        let buffer = terminal.backend().buffer();
        let gold_rule = (0..buffer.area.height).any(|y| {
            (0..buffer.area.width).any(|x| {
                let cell = &buffer[(x, y)];
                cell.symbol() == "─" && cell.fg == ACCENT_GOLD
            })
        });

        assert!(
            gold_rule,
            "status separator should remain visible after two composer newlines"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fullscreen_composer_renders_multiline_input_via_tuika() {
        use ratatui::backend::TestBackend;
        let mut test = app_with_llmsim().await;
        test.app.setup = None;
        test.app.set_render_mode(RenderMode::Fullscreen);
        // Two composer rows in full-screen's own model of record (the tuika
        // TextInputState); the TextInput must render both, after the blue "> ".
        test.app.composer.set_text("first line\nsecond line");

        let backend = TestBackend::new(60, 20);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal.draw(|f| draw(f, &mut test.app)).expect("draw");
        let buffer = terminal.backend().buffer();

        let text: String = (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        for needle in ["first line", "second line", "> "] {
            assert!(
                text.contains(needle),
                "tuika composer should render {needle:?}:\n{text}"
            );
        }
        // The blue prompt is bold accent-blue at the composer's left edge.
        let has_prompt = (0..buffer.area.height).any(|y| {
            let cell = &buffer[(0, y)];
            cell.symbol() == ">" && cell.fg == ACCENT_BLUE
        });
        assert!(has_prompt, "the blue '>' prompt should render");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fullscreen_keys_route_into_the_composer() {
        let mut test = app_with_llmsim().await;
        test.app.setup = None;
        test.app.set_render_mode(RenderMode::Fullscreen);
        // Editing keys drive the shared TextInputState (component-driven input).
        for c in "hi".chars() {
            test.app
                .handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::empty()))
                .await;
        }
        test.app
            .handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT))
            .await;
        test.app
            .handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::empty()))
            .await;
        assert_eq!(test.app.composer.text(), "hi\nx");
    }

    #[test]
    fn fullscreen_theme_uses_yolop_palette_not_tuika_default() {
        // Full-screen builds its own tuika Theme from yolop's palette (item 6),
        // rather than inheriting tuika's neutral toolkit default.
        let theme = fullscreen::yolop_theme();
        assert_eq!(theme.accent, ACCENT_BLUE);
        assert_eq!(theme.accent_alt, ACCENT_GOLD);
        assert_eq!(theme.surface, PANEL_BG);
        // tuika's default is a different (red) identity.
        assert_ne!(theme.accent, tuika::Theme::default().accent);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fullscreen_model_picker_is_a_windowed_selectlist() {
        let mut test = app_with_llmsim().await;
        // The model list renders as a windowed tuika SelectList (item 4 / PR J).
        test.app.setup = Some(SetupStep::PickModel {
            provider: "openai".to_string(),
            selected: 0,
            custom: None,
            error: None,
        });
        let picker = render::setup_picker(&test.app).expect("model list is a picker");
        assert_eq!(picker.viewport, Some(render::MAX_VISIBLE_MODEL_ROWS as u16));
        assert!(!picker.options.is_empty(), "model list has options");
        // The custom-id sub-mode is a text input, not a list, so it falls back to
        // the shared text panel path (not a picker).
        test.app.setup = Some(SetupStep::PickModel {
            provider: "openai".to_string(),
            selected: 0,
            custom: Some("my-model".to_string()),
            error: None,
        });
        assert!(
            render::setup_picker(&test.app).is_none(),
            "custom-id sub-mode is not a SelectList picker"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fullscreen_setup_overlay_renders_via_tuika() {
        use ratatui::backend::TestBackend;
        let mut test = app_with_llmsim().await;
        test.app.set_render_mode(RenderMode::Fullscreen);
        // First-run setup overlay is present; it should composite as a tuika
        // overlay — a bordered panel with the setup content.
        assert!(test.app.setup.is_some(), "first run should offer setup");

        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal.draw(|f| draw(f, &mut test.app)).expect("draw");
        let buffer = terminal.backend().buffer();

        let text: String = (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            text.contains("Set Up Yolop"),
            "setup overlay title should render:\n{text}"
        );
        // tuika's Boxed draws a rounded border — find one of its corners.
        let has_border = (0..buffer.area.height).any(|y| {
            (0..buffer.area.width).any(|x| matches!(buffer[(x, y)].symbol(), "╭" | "╮" | "╰" | "╯"))
        });
        assert!(has_border, "the tuika overlay panel should draw a border");
        // The provider list is a tuika SelectList (item 4): the selected row is
        // caret-marked and highlighted with the theme's selection background
        // (yolop's accent, via yolop_theme).
        let has_caret = (0..buffer.area.height)
            .any(|y| (0..buffer.area.width).any(|x| buffer[(x, y)].symbol() == "›"));
        assert!(
            has_caret,
            "the SelectList should mark the selected row:\n{text}"
        );
        let has_selection_bg = (0..buffer.area.height)
            .any(|y| (0..buffer.area.width).any(|x| buffer[(x, y)].bg == ACCENT_BLUE));
        assert!(
            has_selection_bg,
            "the selected row should use the theme selection background"
        );
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fullscreen_scroll_reveals_full_history() {
        let mut test = app_with_llmsim().await;
        test.app.setup = None;
        test.app.set_render_mode(RenderMode::Fullscreen);
        for i in 0..80 {
            test.app.push_user(format!("line {i}"));
        }
        // Bottom-stuck by default: the newest line is visible, the oldest is
        // scrolled off the top.
        let bottom = render_app_lines(&mut test.app, 40, 12).join("\n");
        assert!(
            bottom.contains("line 79"),
            "newest line should be visible at the bottom:\n{bottom}"
        );
        assert!(
            !bottom.contains("line 0"),
            "oldest line should be scrolled off:\n{bottom}"
        );
        // Scroll to the very top: the *full* history is reachable — the old
        // recent-tail mirror could never render "line 0". Startup lines now
        // precede it in the same transcript, so scan until chat begins.
        test.app.scroll.jump_to_top();
        let mut history = render_app_lines(&mut test.app, 40, 12).join("\n");
        for _ in 0..100 {
            if history.contains("line 0") {
                break;
            }
            test.app
                .handle_fullscreen_scroll(MouseEventKind::ScrollDown);
            history.push_str(&render_app_lines(&mut test.app, 40, 12).join("\n"));
        }
        assert!(
            history.contains("line 0"),
            "oldest chat line should be reachable after the startup transcript:\n{history}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fullscreen_wrap_cache_matches_full_rebuild() {
        let mut test = app_with_llmsim().await;
        test.app.setup = None;
        test.app.set_render_mode(RenderMode::Fullscreen);
        let width = 40;

        let text_of = |lines: &[Line<'static>]| -> Vec<String> {
            lines
                .iter()
                .map(|l| l.spans.iter().map(|s| s.content.to_string()).collect())
                .collect()
        };
        // A from-scratch reference: one full pass over the whole history.
        let full_rebuild = |app: &App, w: usize| -> Vec<Line<'static>> {
            let mut lines = Vec::new();
            append_transcript_range(&mut lines, &mut Vec::new(), &app.lines, 0, w, None);
            lines
        };

        // Prime the cache on an initial batch, then append more of a different
        // author (which exercises the inter-author gap across the resume
        // boundary) and read the cache again — the incrementally-built result
        // must match a full from-scratch rebuild exactly.
        for i in 0..30 {
            test.app.push_user(format!(
                "user message {i} long enough to wrap at width forty here"
            ));
        }
        let _ = test.app.full_transcript_lines_cached(width);
        for i in 0..30 {
            test.app.push_system(format!("system note {i}"));
        }
        let incremental = test.app.full_transcript_lines_cached(width);
        assert_eq!(
            text_of(&incremental),
            text_of(&full_rebuild(&test.app, width)),
            "incremental wrap cache drifted from a full rebuild"
        );

        // A width change must invalidate and rebuild rather than reuse the old
        // wrapping.
        let narrower = test.app.full_transcript_lines_cached(24);
        assert_eq!(
            text_of(&narrower),
            text_of(&full_rebuild(&test.app, 24)),
            "width change did not rebuild the wrap cache"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn trim_transcript_caps_fullscreen_history() {
        let mut test = app_with_llmsim().await;
        test.app.setup = None;
        test.app.set_render_mode(RenderMode::Fullscreen);
        for i in 0..(MAX_RETAINED_TRANSCRIPT_LINES + 100) {
            test.app.push_user(format!("l{i}"));
        }
        let gen_before = test.app.transcript_generation;
        test.app.trim_transcript();
        assert_eq!(
            test.app.lines.len(),
            MAX_RETAINED_TRANSCRIPT_LINES,
            "full-screen history should be capped at the retention window"
        );
        assert!(
            test.app.transcript_generation > gen_before,
            "trimming must invalidate the wrap cache"
        );
        // Newest lines survive; the oldest were dropped from the front.
        assert_eq!(test.app.lines.last().unwrap().text, "l50099");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn trim_transcript_keeps_unflushed_inline_lines() {
        let mut test = app_with_llmsim().await;
        test.app.setup = None;
        // Inline mode (the default): lines past `printed_lines` have not been
        // flushed into native scrollback yet and must not be dropped.
        for i in 0..(MAX_RETAINED_TRANSCRIPT_LINES + 100) {
            test.app.push_user(format!("l{i}"));
        }
        let before = test.app.lines.len();
        test.app.trim_transcript();
        assert_eq!(
            test.app.lines.len(),
            before,
            "inline mode must not drop lines still awaiting flush"
        );
        // Once flushed, the flushed prefix can be trimmed down to the cap.
        test.app.printed_lines = test.app.lines.len();
        test.app.trim_transcript();
        assert_eq!(test.app.lines.len(), MAX_RETAINED_TRANSCRIPT_LINES);
        assert_eq!(test.app.printed_lines, MAX_RETAINED_TRANSCRIPT_LINES);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fullscreen_ctrl_click_is_left_to_the_terminal() {
        let mut test = app_with_llmsim().await;
        test.app.setup = None;
        test.app.set_render_mode(RenderMode::Fullscreen);
        let area = Rect::new(2, 3, 40, 1);
        test.app.set_selection_area(area);
        let event = MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: area.x + 10,
            row: area.y,
            modifiers: KeyModifiers::CONTROL,
        };
        assert!(!test.app.handle_fullscreen_selection(event));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fullscreen_link_hover_changes_pointer_shape() {
        let mut test = app_with_llmsim().await;
        test.app.setup = None;
        test.app.set_render_mode(RenderMode::Fullscreen);
        test.app.set_visible_links(
            ratatui::layout::Position { x: 4, y: 6 },
            &[BufferLink {
                line: 1,
                start_col: 3,
                end_col: 9,
                url: "https://example.com".to_string(),
            }],
        );
        let moved = |column, row| MouseEvent {
            kind: MouseEventKind::Moved,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        };

        assert_eq!(
            test.app.update_link_pointer(moved(8, 7)),
            Some(PointerShape::Pointer)
        );
        assert_eq!(test.app.update_link_pointer(moved(9, 7)), None);
        assert_eq!(
            test.app.update_link_pointer(moved(2, 2)),
            Some(PointerShape::Default)
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fullscreen_mouse_drag_selects_and_copies_transcript() {
        let mut test = app_with_llmsim().await;
        test.app.setup = None;
        test.app.set_render_mode(RenderMode::Fullscreen);
        test.app.push_user("hello from user".to_string());

        // First draw records the transcript's selectable inner rect.
        let _ = render_app_lines(&mut test.app, 60, 12);
        let area = test.app.selection_area;
        assert!(
            area.width > 0 && area.height > 0,
            "the draw should record the transcript rect"
        );

        // Drag across the bottom transcript row (where the pushed line renders).
        let row = area.bottom() - 1;
        let ev = |kind, column| MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::empty(),
        };
        assert!(
            test.app
                .handle_fullscreen_selection(ev(MouseEventKind::Down(MouseButton::Left), area.x))
        );
        assert!(
            test.app.handle_fullscreen_selection(ev(
                MouseEventKind::Drag(MouseButton::Left),
                area.x + 5
            ))
        );
        assert!(test.app.handle_fullscreen_selection(ev(
            MouseEventKind::Up(MouseButton::Left),
            area.right() - 1
        )));

        let range = test
            .app
            .selection_range()
            .expect("a left-drag builds a selection");
        assert!(test.app.pending_copy, "releasing the drag arms the copy");

        // Second draw: highlights the selection and performs the deferred copy.
        let backend = TestBackend::new(60, 12);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal.draw(|f| draw(f, &mut test.app)).expect("draw");
        assert!(!test.app.pending_copy, "the draw consumes the pending copy");

        let buffer = terminal.backend().buffer();
        let highlighted = (range.start.0..=range.end.0).any(|x| {
            buffer[(x, row)]
                .modifier
                .contains(ratatui::style::Modifier::REVERSED)
        });
        assert!(highlighted, "the selected row should be highlighted");
        let text = selected_text(buffer, test.app.selection_area, range);
        assert!(
            text.contains("hello"),
            "the selection should copy the transcript text, got {text:?}"
        );

        // Scrolling no longer discards the selection: it is anchored in content
        // space, so it survives the scroll and stays available to copy.
        test.app.handle_fullscreen_scroll(MouseEventKind::ScrollUp);
        assert!(test.app.has_selection());
    }

    /// Repro for "impossible to select more than one window of text": a drag
    /// that keeps going past the top edge must auto-scroll and keep extending
    /// the selection, so the copied text spans lines that were never on screen
    /// at the same time. Before the fix, edge/wheel scrolling cleared the
    /// selection outright, capping it at the single visible window.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fullscreen_selection_spans_more_than_one_window() {
        let mut test = app_with_llmsim().await;
        test.app.setup = None;
        test.app.set_render_mode(RenderMode::Fullscreen);
        for i in 0..80 {
            test.app.push_user(format!("line {i:02}"));
        }

        // First draw records metrics + the selectable rect; bottom-stuck, so the
        // newest lines are visible and the oldest are scrolled off the top.
        let _ = render_app_lines(&mut test.app, 40, 12);
        let area = test.app.selection_area;
        assert!(area.height >= 3, "need a few transcript rows to drag over");

        let ev = |kind, column, row| MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::empty(),
        };

        // Anchor at the right end of the newest (bottom) visible row so the
        // selection's bottom endpoint covers that whole line, then drag up-left.
        let bottom_row = area.bottom() - 1;
        test.app.handle_fullscreen_selection(ev(
            MouseEventKind::Down(MouseButton::Left),
            area.right() - 1,
            bottom_row,
        ));
        test.app.handle_fullscreen_selection(ev(
            MouseEventKind::Drag(MouseButton::Left),
            area.x,
            bottom_row,
        ));
        assert!(
            test.app.selection_range().is_some(),
            "the drag should have started a selection"
        );

        // Keep dragging at the top edge: each drag auto-scrolls to reveal earlier
        // lines and grows the selection, so it climbs far past one window.
        for _ in 0..120 {
            test.app.handle_fullscreen_selection(ev(
                MouseEventKind::Drag(MouseButton::Left),
                area.x,
                area.y,
            ));
        }
        test.app.handle_fullscreen_selection(ev(
            MouseEventKind::Up(MouseButton::Left),
            area.x,
            area.y,
        ));
        assert!(test.app.pending_copy, "releasing the drag arms the copy");

        // Redraw so the deferred copy runs against the current window.
        let backend = TestBackend::new(40, 12);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal.draw(|f| draw(f, &mut test.app)).expect("draw");

        let text = test.app.selection_copy_text();
        // An 8-row window can show only a handful of "line NN" rows at once; a
        // selection naming lines dozens apart could only come from spanning many.
        assert!(
            text.contains("line 79"),
            "selection should still include the newest line, got:\n{text}"
        );
        assert!(
            text.contains("line 15"),
            "selection should reach lines from far earlier windows, got:\n{text}"
        );
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
        app.set_render_mode(RenderMode::SplitFooter);
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
                    app.composer.insert_str(&paste);
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

            // Visual equivalence (tuika full-screen vs inline): both render a
            // non-blank frame and, where checked, the same key content.
            assert!(
                fullscreen.iter().any(|row| !row.is_empty()),
                "state {name} should render a non-blank frame"
            );
            if !needle.is_empty() {
                let fs = fullscreen.join("\n");
                let reg = regular.join("\n");
                assert!(
                    fs.contains(needle),
                    "full-screen state {name} should render {needle:?}: {fs}"
                );
                assert!(
                    reg.contains(needle),
                    "inline state {name} should render {needle:?}: {reg}"
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

    #[tokio::test]
    async fn pending_session_task_refresh_does_not_block_ui_loop() {
        let mut test = app_with_llmsim().await;
        test.app.last_session_tasks_refresh = None;
        test.app.session_tasks_refresh = Some(tokio::spawn(std::future::pending()));

        let started = Instant::now();
        test.app.refresh_session_tasks_if_due();
        let elapsed = started.elapsed();

        assert!(
            elapsed < Duration::from_millis(50),
            "a pending refresh blocked the UI loop for {elapsed:?}"
        );
        assert!(test.app.session_tasks_refresh.is_some());
        test.app
            .session_tasks_refresh
            .take()
            .expect("pending refresh")
            .abort();
    }

    #[tokio::test]
    async fn blocked_completion_state_is_in_presentation_transcript() {
        use everruns_core::turn::TurnStopReason;
        use everruns_core::typed_id::TurnId;

        let mut test = app_with_llmsim().await;
        let session_id = test.app.session.session_id();
        test.app
            .user_ask_store
            .record_user_prompt(session_id, "edit the file")
            .expect("record ask");
        test.app
            .after_turn_user_ask_check(Some(everruns_runtime::TurnResult {
                response: "I need the path. Which file should I edit?".to_string(),
                iterations: 1,
                tool_calls_count: 0,
                success: true,
                error: None,
                stop_reason: TurnStopReason::EndTurn,
                turn_id: TurnId::new(),
            }))
            .await;

        assert!(test.app.lines.iter().any(|line| {
            line.author == Author::System && line.text.contains("user ask blocked")
        }));
        assert!(!test.app.busy, "blocked state must not auto-continue");
    }

    #[tokio::test]
    async fn completed_session_task_refresh_is_applied_without_panicking() {
        let mut test = app_with_llmsim().await;
        let mut expected = crate::tui::session_tasks_view::TaskTree::default();
        expected.errors.push("refreshed".into());
        test.app.session_tasks_refresh = Some(tokio::spawn(async move { expected }));
        test.app.last_session_tasks_refresh = Some(Instant::now());
        while !test
            .app
            .session_tasks_refresh
            .as_ref()
            .expect("refresh task")
            .is_finished()
        {
            tokio::task::yield_now().await;
        }

        test.app.refresh_session_tasks_if_due();

        assert!(test.app.session_tasks_refresh.is_none());
        assert_eq!(test.app.session_tasks.errors, ["refreshed"]);
    }

    async fn app_with_llmsim() -> TestApp {
        let workspace = tempfile::tempdir().expect("workspace tempdir");
        let sessions = tempfile::tempdir().expect("sessions tempdir");
        let settings = std::sync::Arc::new(crate::config::SettingsStore::open(
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

    #[tokio::test]
    async fn sandbox_approval_can_be_granted_for_the_session() {
        let mut test = app_with_llmsim().await;
        let (reply, answer) = oneshot::channel();
        test.app.pending_sandbox_approval = Some(PendingSandboxApproval { reply });

        test.app
            .handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::empty()))
            .await;

        assert_eq!(
            answer.await.unwrap(),
            crate::sandbox_approval::ApprovalDecision::ApproveForSession
        );
        assert!(test.app.pending_sandbox_approval.is_none());
        assert!(test.app.lines.iter().any(|line| {
            line.author == Author::System && line.text == "approved for this session"
        }));
    }

    /// Seed a synthetic everruns completion signal onto the app's wake channel,
    /// standing in for the platform-store `send_message` a finished
    /// `spawn_background` run makes. Returns the sender so the caller can push
    /// more (or drop it to close the channel).
    fn seed_background_wake(
        app: &mut App,
        message: &str,
    ) -> crate::runtime::background_wake::WakeSender {
        let (tx, rx) = mpsc::unbounded_channel();
        app.background_wake = crate::runtime::background_wake::WakeReceiver::unrouted(rx);
        tx.send(crate::runtime::background_wake::WakeMessage::unstructured(
            message,
        ))
        .expect("seed wake");
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
    async fn proactive_wake_coalesces_queued_completions_into_one_turn() {
        let mut test = app_with_llmsim().await;
        let app = &mut test.app;
        app.setup = None;
        let tx = seed_background_wake(app, "task_1 result_path=/results/1");
        tx.send(crate::runtime::background_wake::WakeMessage::unstructured(
            "task_2 result_path=/results/2",
        ))
        .unwrap();
        tx.send(crate::runtime::background_wake::WakeMessage::unstructured(
            "task_3 result_path=/results/3",
        ))
        .unwrap();

        assert!(app.maybe_wake_from_background_channel());
        assert!(app.busy);
        assert!(
            app.lines
                .iter()
                .filter(|line| line.text.contains("waking"))
                .count()
                == 1,
            "the host should present one wake notice"
        );
        assert!(
            app.background_wake.try_recv().is_err(),
            "queued completions must not create duplicate idle wakes"
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
    async fn setup_picker_navigation_clamps_at_ends() {
        let mut test = app_with_llmsim().await;
        let app = &mut test.app;
        app.setup = Some(SetupStep::Provider { selected: 0 });

        fn provider_selected(app: &App) -> usize {
            match app.setup {
                Some(SetupStep::Provider { selected }) => selected,
                _ => panic!("expected the provider step"),
            }
        }

        // Up at the top holds at 0 — navigation clamps, it does not wrap.
        app.handle_setup_key(KeyEvent::new(KeyCode::Up, KeyModifiers::empty()))
            .await;
        assert_eq!(provider_selected(app), 0);

        // Down walks forward one row at a time and clamps at the last option
        // even when pressed past the end.
        let last = PROVIDER_OPTIONS.len() - 1;
        for _ in 0..last + 3 {
            app.handle_setup_key(KeyEvent::new(KeyCode::Down, KeyModifiers::empty()))
                .await;
        }
        assert_eq!(provider_selected(app), last);
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
        app.handle_background_panel_key(KeyEvent::new(KeyCode::Down, KeyModifiers::empty()))
            .await;
        assert_eq!(app.background_panel, Some(0));

        app.handle_background_panel_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()))
            .await;
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
    async fn ctrl_d_quits_through_the_keymap() {
        // Exercises the keymap dispatch seam end to end: the Ctrl+D chord must
        // resolve to `GlobalAction::Quit` and set `should_quit`.
        let mut test = app_with_llmsim().await;
        let app = &mut test.app;
        app.setup = None;
        assert!(!app.should_quit);

        app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL))
            .await;
        assert!(app.should_quit, "Ctrl+D must quit the session");
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
    async fn activity_rail_auto_opens_passively_and_ctrl_b_focuses_it() {
        let mut test = app_with_llmsim().await;
        let app = &mut test.app;
        app.setup = None;
        app.task_registry
            .create(CreateSessionTask {
                session_id: app.session.session_id(),
                id: Some("task_panel".to_string()),
                kind: TASK_KIND_SUBAGENT.to_string(),
                display_name: "Flight Lead".to_string(),
                spec: serde_json::json!({ "instructions": "coordinate flight fixes" }),
                state: SessionTaskState::Running,
                links: Default::default(),
                wake_policy: Default::default(),
            })
            .await
            .expect("create session task");
        for (id, kind, name) in [
            ("task_tests", TASK_KIND_BACKGROUND_TOOL, "cargo test"),
            ("task_ci_monitor", TASK_KIND_MONITOR, "CI monitor"),
        ] {
            app.task_registry
                .create(CreateSessionTask {
                    session_id: app.session.session_id(),
                    id: Some(id.to_string()),
                    kind: kind.to_string(),
                    display_name: name.to_string(),
                    spec: serde_json::json!({}),
                    state: SessionTaskState::Running,
                    links: Default::default(),
                    wake_policy: Default::default(),
                })
                .await
                .expect("create background activity");
        }
        app.refresh_session_tasks().await;
        assert_eq!(app.background_panel, Some(0));
        assert!(
            !app.background_panel_focused,
            "automatic panel must stay passive"
        );

        assert_eq!(
            app.presentation_state().background,
            Some(crate::tui::session_tasks_view::BackgroundCounts {
                running: 2,
                scheduled: 1,
                total: 3,
            })
        );
        let (fullscreen, regular) = render_regular_and_fullscreen(app, 120, 20);
        for (mode, lines) in [
            ("default", regular.join("\n")),
            ("fullscreen", fullscreen.join("\n")),
        ] {
            assert!(lines.contains("ACTIVITY"), "{mode} rail title: {lines}");
            assert!(lines.contains("AGENTS 1"), "{mode} agent section: {lines}");
            assert!(
                lines.contains("Flight Lead") && lines.contains("running"),
                "{mode} rail should list the agent status: {lines}"
            );
            assert!(
                lines.contains("BACKGROUND 2"),
                "{mode} background section: {lines}"
            );
            assert!(
                lines.contains("cargo test"),
                "{mode} background command: {lines}"
            );
            assert!(
                lines.contains("CI monitor") && lines.contains("waiting"),
                "{mode} waiting monitor: {lines}"
            );
            assert!(
                lines.contains("Ctrl+B focus"),
                "{mode} passive hint: {lines}"
            );
        }

        app.handle_key(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::empty()))
            .await;
        assert_eq!(app.input_text(), "z", "passive panel must not steal typing");

        app.handle_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL))
            .await;
        assert!(
            app.background_panel.is_some(),
            "Ctrl+B focuses a passive rail"
        );
        assert!(app.background_panel_focused);

        app.handle_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL))
            .await;
        assert!(
            app.background_panel.is_none(),
            "Ctrl+B closes a focused rail"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn activity_rail_overflow_stays_pinned_to_newest_agents() {
        let mut test = app_with_llmsim().await;
        let app = &mut test.app;
        app.setup = None;
        for index in 0..20 {
            app.task_registry
                .create(CreateSessionTask {
                    session_id: app.session.session_id(),
                    id: Some(format!("task_agent_{index:02}")),
                    kind: TASK_KIND_SUBAGENT.to_string(),
                    display_name: format!("Agent {index:02}"),
                    spec: serde_json::json!({ "instructions": "demo" }),
                    state: SessionTaskState::Running,
                    links: Default::default(),
                    wake_policy: Default::default(),
                })
                .await
                .expect("create session task");
        }
        app.refresh_session_tasks().await;

        let (fullscreen, regular) = render_regular_and_fullscreen(app, 120, 12);
        for (mode, lines) in [
            ("default", regular.join("\n")),
            ("fullscreen", fullscreen.join("\n")),
        ] {
            assert!(
                lines.contains("Agent 19"),
                "{mode} follows newest work: {lines}"
            );
            assert!(
                !lines.contains("Agent 00"),
                "{mode} clips rows outside the viewport: {lines}"
            );
            assert!(
                lines.contains('█'),
                "{mode} exposes overflow scrollbar: {lines}"
            );
        }
        assert!(app.activity_scroll.offset() > 0);
        assert!(app.activity_scroll.is_stuck_to_bottom());
    }

    #[test]
    fn activity_rail_visual_hierarchy_is_flat_and_distinguishes_leads() {
        use crate::tui::session_tasks_view::{
            ActivityCounts, ActivityRail, ActivityRailRow, ActivitySection, ActivityStatus,
            ActivityTaskKind, ActivityTaskRow,
        };

        let task = |task_index: usize, depth: usize, name: &str| {
            ActivityRailRow::Task(ActivityTaskRow {
                task_index,
                depth,
                name: name.to_string(),
                kind: ActivityTaskKind::Agent,
                status: ActivityStatus::Succeeded,
                usage: None,
                canceling: false,
            })
        };
        let rail = ActivityRail {
            agents: ActivityCounts {
                total: 2,
                succeeded: 2,
                ..ActivityCounts::default()
            },
            background: ActivityCounts::default(),
            rows: vec![
                ActivityRailRow::Section {
                    kind: ActivitySection::Agents,
                    counts: ActivityCounts {
                        total: 2,
                        succeeded: 2,
                        ..ActivityCounts::default()
                    },
                },
                task(0, 0, "Flight Lead"),
                task(1, 1, "F1 Orbit Clock"),
            ],
        };

        let lines = render::activity_rail_lines(&rail, None, 38);
        let lead = &lines[1];
        let worker = &lines[2];
        assert_eq!(lead.spans[3].style.fg, Some(ACCENT_GOLD));
        assert!(
            lead.spans[3].style.add_modifier.contains(Modifier::BOLD),
            "lead names should anchor each agent group"
        );
        assert_eq!(worker.spans[3].style.fg, Some(TEXT_PRIMARY));
        assert_eq!(worker.spans[1].content, "↳ ");
        assert!(
            lines
                .iter()
                .flat_map(|line| &line.spans)
                .all(|span| span.style.bg != Some(PANEL_BG)),
            "passive rows should inherit the terminal background"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn background_panel_cancels_selected_task() {
        let mut test = app_with_llmsim().await;
        let app = &mut test.app;
        let session_id = app.session.session_id();
        app.task_registry
            .create(CreateSessionTask {
                session_id,
                id: Some("task_cancel_me".to_string()),
                kind: TASK_KIND_BACKGROUND_TOOL.to_string(),
                display_name: "long command".to_string(),
                spec: serde_json::json!({ "tool": "bash" }),
                state: SessionTaskState::Running,
                links: Default::default(),
                wake_policy: Default::default(),
            })
            .await
            .unwrap();
        app.refresh_session_tasks().await;
        app.background_panel = Some(0);

        app.handle_background_panel_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::empty()))
            .await;

        let task = app
            .task_registry
            .get(session_id, "task_cancel_me")
            .await
            .unwrap()
            .unwrap();
        assert!(task.cancel_requested_at.is_some());
        assert!(app.background_panel_body().contains("canceling"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn background_panel_disarms_selected_monitor() {
        let mut test = app_with_llmsim().await;
        let app = &mut test.app;
        let session_id = app.session.session_id();
        let schedule = app
            .task_schedule_store
            .create_schedule(
                session_id,
                "scheduled check".into(),
                None,
                Some(chrono::Utc::now() + chrono::Duration::minutes(10)),
                "UTC".into(),
            )
            .await
            .expect("create schedule");
        app.task_registry
            .create(CreateSessionTask {
                session_id,
                id: Some("task_monitor_panel".to_string()),
                kind: TASK_KIND_MONITOR.to_string(),
                display_name: "scheduled check".to_string(),
                spec: serde_json::json!({ "schedule_id": schedule.id.to_string() }),
                state: SessionTaskState::Running,
                links: Default::default(),
                wake_policy: Default::default(),
            })
            .await
            .expect("create monitor task");
        app.refresh_session_tasks().await;
        app.background_panel = Some(0);

        app.handle_background_panel_key(KeyEvent::new(KeyCode::Delete, KeyModifiers::empty()))
            .await;

        let task = app
            .task_registry
            .get(session_id, "task_monitor_panel")
            .await
            .unwrap()
            .unwrap();
        let schedules = app
            .task_schedule_store
            .list_schedules(session_id)
            .await
            .unwrap();
        assert_eq!(task.state, SessionTaskState::Canceled);
        assert!(!schedules[0].enabled);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn presentation_state_exposes_sandbox_and_hard_approval_matrix() {
        let test = app_with_llmsim().await;
        test.app
            .settings
            .set_sandbox_mode(crate::config::SandboxMode::ReadOnly)
            .unwrap();
        test.app
            .settings
            .set_approval_policy(crate::config::ApprovalPolicy::Never)
            .unwrap();

        assert_eq!(
            test.app.presentation_state().approval_mode,
            "normal · read-only · never"
        );
    }

    #[test]
    fn activity_rail_docks_hides_or_draws_without_trapping_focus() {
        let wide = Rect::new(0, 0, 120, 24);
        let docked = render::activity_rail_layout(wide, true, false);
        assert_eq!(docked.placement, render::ActivityRailPlacement::Docked);
        assert!(docked.main.width < wide.width);
        assert!(docked.rail.is_some());

        let narrow = Rect::new(0, 0, 70, 24);
        let passive = render::activity_rail_layout(narrow, true, false);
        assert_eq!(passive.placement, render::ActivityRailPlacement::Hidden);
        assert_eq!(passive.main, narrow);

        let focused = render::activity_rail_layout(narrow, true, true);
        assert_eq!(focused.placement, render::ActivityRailPlacement::Drawer);
        assert_eq!(focused.main, narrow, "drawer must overlay, not crush chat");
        assert!(focused.rail.is_some(), "focused activity is always visible");

        let tiny = render::activity_rail_layout(Rect::new(0, 0, 18, 8), true, true);
        assert_eq!(tiny.placement, render::ActivityRailPlacement::Drawer);
        assert_eq!(tiny.rail.expect("tiny drawer").width, 18);
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
            while let Ok(request) = self.ui_rx.try_recv() {
                let messages = self.apply_ui_command(request.command).await;
                if let Some(reply) = request.reply {
                    let _ = reply.send(messages);
                }
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
                        Ok(TurnEvent::Done(_)) => {
                            self.finish_busy();
                            self.start_next_queued_turn();
                        }
                        Ok(TurnEvent::Failed(err)) => {
                            self.finish_busy();
                            self.push_system(format!("turn failed: {err}"));
                            self.start_next_queued_turn();
                        }
                        Err(mpsc::error::TryRecvError::Empty) => {}
                        Err(mpsc::error::TryRecvError::Disconnected) => {
                            self.finish_busy();
                            self.start_next_queued_turn();
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
    ) -> std::sync::Arc<crate::config::SettingsStore> {
        let settings_path = sessions.path().join("settings.toml");
        std::fs::write(settings_path, "provider = \"llmsim\"\n").expect("write settings");
        std::sync::Arc::new(crate::config::SettingsStore::open(
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
        let settings = std::sync::Arc::new(crate::config::SettingsStore::open(
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
    async fn startup_screen_is_ready_without_adding_transcript_messages() {
        let fixture = app_with_llmsim().await;
        let startup = fixture.app.presentation_state().startup_lines();

        assert!(
            fixture.app.lines.is_empty(),
            "startup context must not become transcript history"
        );
        assert!(
            startup
                .iter()
                .any(|line| line.text.contains("Ctrl+V to paste an image")),
            "startup screen should mention image paste: {startup:?}"
        );
        assert!(
            startup
                .iter()
                .any(|line| line.text.contains("Ctrl-C twice (or Ctrl-D) to exit")),
            "startup screen should name Ctrl-C/Ctrl-D exits: {startup:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn repository_pulse_only_occupies_the_empty_state() {
        let mut fixture = app_with_llmsim().await;
        let app = &mut fixture.app;
        app.setup = None;
        app.set_render_mode(RenderMode::Fullscreen);
        app.repo_pulse = Some(RepositoryPulse {
            name: "everruns/yolop".to_string(),
            branch: "main".to_string(),
            changed_paths: 0,
            last_commit: Some("feat(tui): add repo pulse".to_string()),
        });

        let startup = recent_transcript_lines(app, 100, 12)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>();
        assert!(
            startup
                .iter()
                .any(|line| line.contains("everruns/yolop on main"))
        );
        assert!(startup.iter().any(|line| line.contains("● clean")));

        app.push_user("start immediately".to_string());
        let transcript = recent_transcript_lines(app, 100, 12)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>();
        assert!(
            transcript
                .iter()
                .any(|line| line.contains("start immediately"))
        );
        assert!(
            transcript
                .iter()
                .all(|line| !line.contains("everruns/yolop on main")),
            "repository pulse must disappear once the transcript starts: {transcript:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn inline_startup_stays_stable_when_repository_context_exists() {
        let mut fixture = app_with_llmsim().await;
        let app = &mut fixture.app;
        app.setup = None;
        app.repo_pulse = Some(RepositoryPulse {
            name: "everruns/yolop".to_string(),
            branch: "main".to_string(),
            changed_paths: 0,
            last_commit: Some("feat(tui): add repo pulse".to_string()),
        });

        let rendered = recent_transcript_lines(app, 100, 12)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>();

        assert!(rendered.iter().any(|line| line.contains("Ready in")));
        assert!(
            rendered.iter().all(|line| !line.contains("everruns/yolop")),
            "inline startup must not reflow when repository context arrives: {rendered:?}"
        );
        assert!(
            app.repo_pulse_rx.is_none(),
            "inline mode must not start repository inspection"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn large_paste_attaches_placeholder_and_expands_on_submit() {
        use crate::tui::input::paste_attachment::LARGE_PASTE_CHAR_THRESHOLD;

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

        let mut terminal = split_footer_terminal(100, 60);
        flush_for_frame(app, &mut terminal).expect("flush prior turn");
        app.push_system("attached clipboard image #1 (640x480 PNG)".into());
        flush_for_frame(app, &mut terminal).expect("flush image notice");
        terminal.draw(|f| draw(f, app)).expect("draw after notice");

        let footer_rows = footer_rows_text(&mut terminal);
        let notice_row = footer_rows
            .iter()
            .position(|row| row.contains("attached clipboard image #1"))
            .expect("image paste notice in inline viewport");

        assert!(
            notice_row + 1 >= footer_rows.len().saturating_sub(1),
            "incremental system notice should sit near the composer, not above a mirrored tail: inline={footer_rows:?}"
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

        let mut terminal = split_footer_terminal(100, 60);
        flush_for_frame(app, &mut terminal).expect("flush prior turn");
        app.push_system("turn failed: provider timeout".into());
        flush_for_frame(app, &mut terminal).expect("flush turn notice");
        terminal.draw(|f| draw(f, app)).expect("draw after notice");

        let footer_rows = footer_rows_text(&mut terminal);
        assert!(
            footer_rows
                .iter()
                .any(|row| row.contains("turn failed: provider timeout")),
            "turn-failed notice should mirror above the composer: {footer_rows:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn split_footer_composer_sits_on_the_last_rows_of_a_tall_terminal() {
        let mut fixture = app_with_llmsim().await;
        let app = &mut fixture.app;
        app.setup = None;

        let rendered = render_footer_composer(app, 100, 60, 1);

        assert_eq!(
            rendered.viewport_bottom, 60,
            "the footer should sit flush with the terminal bottom"
        );
        // What makes the composer a footer rather than an inline panel: the
        // rows it reserved are the terminal's last ones, and everything the
        // session publishes lands above them.
        assert!(
            rendered.lines[(60 - COMPOSER_VIEWPORT_HEIGHT) as usize..]
                .iter()
                .any(|row| row.trim_start().starts_with('>')),
            "the composer prompt should be painted inside the footer rows: {:?}",
            rendered.lines
        );
    }

    /// Pinning runs before every frame, so it has to be idempotent: a second
    /// call on an already-pinned footer must not reserve another band of rows.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn repinning_an_anchored_footer_reserves_no_further_rows() {
        let mut terminal = split_footer_terminal(100, 60);
        let scrollback_after_pin = terminal.backend().scrollback().area.height;

        tuika::screen::pin_footer(&mut terminal).expect("re-pin footer");

        assert_eq!(
            terminal.backend().scrollback().area.height,
            scrollback_after_pin,
            "re-pinning an already-pinned footer should be a no-op"
        );
        let viewport = terminal.get_frame().area();
        assert_eq!(viewport.y.saturating_add(viewport.height), 60);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn split_footer_keeps_the_unpublished_tail_next_to_the_composer() {
        let mut fixture = app_with_llmsim().await;
        let app = &mut fixture.app;
        app.setup = None;
        app.lines.clear();
        app.push_user("hello composer gap".into());
        app.lines.push(ChatLine {
            author: Author::Assistant,
            text: "Latest answer tail".into(),
        });

        let mut terminal = split_footer_terminal(100, 60);
        flush_for_frame(app, &mut terminal).expect("publish what the footer cannot hold");
        terminal.draw(|f| draw(f, app)).expect("draw");

        let rows = viewport_rows(&mut terminal);
        let answer_row = rows
            .iter()
            .position(|row| row.contains("Latest answer tail"))
            .expect("retained answer rendered");
        let separator_row = rows
            .iter()
            .position(|row| row.contains("Enter to send"))
            .expect("message separator rendered");

        assert_eq!(
            answer_row + 1,
            separator_row,
            "the retained tail should sit against the composer chrome: {rows:?}"
        );
    }

    /// The point of the retained window: a line the terminal already owns is
    /// never repainted in the footer, so a tall screen shows it once, not twice.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn split_footer_does_not_repaint_published_lines() {
        let mut fixture = app_with_llmsim().await;
        let app = &mut fixture.app;
        app.setup = None;
        app.lines.clear();
        for index in 0..40 {
            app.push_user(format!("question number {index}"));
            app.lines.push(ChatLine {
                author: Author::Assistant,
                text: format!("answer number {index}"),
            });
        }

        let mut terminal = split_footer_terminal(100, 60);
        flush_for_frame(app, &mut terminal).expect("publish the overflow");
        terminal.draw(|f| draw(f, app)).expect("draw");

        assert!(
            app.printed_lines > 0,
            "a transcript this long must not fit the footer whole"
        );
        let rows = viewport_rows(&mut terminal);
        assert!(
            !rows.iter().any(|row| row.contains("question number 0")),
            "a published line must not be repainted in the footer: {rows:?}"
        );
        assert!(
            rows.iter().any(|row| row.contains("answer number 39")),
            "the unpublished tail should still be shown: {rows:?}"
        );
    }

    /// The retained tail has to *fill* the footer's transcript rows: publishing
    /// on entry boundaries alone would leave a hole between the scrollback and
    /// the composer whenever one tall entry straddled the edge.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn split_footer_retains_exactly_the_rows_it_can_show() {
        let mut fixture = app_with_llmsim().await;
        let app = &mut fixture.app;
        app.setup = None;
        app.lines.clear();
        // One entry far taller than the footer, so the cut can only land inside
        // it, plus a short one after it.
        app.push_system(
            (0..60)
                .map(|n| format!("line {n}"))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        app.push_user("after the wall of text".into());

        let mut terminal = split_footer_terminal(100, 60);
        let keep_rows = app.footer_transcript_rows(terminal.get_frame().area());
        app.flush_transcript(&mut terminal, keep_rows)
            .expect("publish the overflow");

        let width = terminal
            .size()
            .expect("size")
            .width
            .saturating_sub(2)
            .max(20) as usize;
        let (retained, _) = app.unpublished_rows(width);
        assert_eq!(
            retained.len(),
            keep_rows as usize,
            "the retained tail should fill the footer's transcript rows exactly"
        );
        assert!(
            app.printed_rows > 0,
            "the cut should land inside the tall entry, not on its boundary"
        );
    }

    /// The footer's rows are handed back at exit, so whatever it was holding
    /// has to reach the scrollback first or it is simply erased.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn split_footer_publishes_the_retained_tail_on_exit() {
        let mut fixture = app_with_llmsim().await;
        let app = &mut fixture.app;
        app.setup = None;
        app.lines.clear();
        app.push_user("last question".into());

        let mut terminal = split_footer_terminal(100, 60);
        flush_for_frame(app, &mut terminal).expect("flush");
        assert_eq!(app.printed_lines, 0, "the tail should still be retained");

        app.publish_remaining_transcript(&mut terminal);

        assert_eq!(
            app.printed_lines,
            app.lines.len(),
            "every line should be published before the footer's rows go back"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn inline_viewport_shows_startup_banner() {
        let mut fixture = app_with_llmsim().await;
        fixture.app.setup = None;
        let rows = render_app_lines(&mut fixture.app, 96, COMPOSER_VIEWPORT_HEIGHT);

        assert!(
            rows.iter().any(|row| row.contains("type /help")),
            "inline rendering should show the initial help message: {rows:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fullscreen_viewport_shows_startup_banner() {
        let mut fixture = app_with_llmsim().await;
        fixture.app.setup = None;
        fixture.app.set_render_mode(RenderMode::Fullscreen);

        let rows = render_app_lines(&mut fixture.app, 96, COMPOSER_VIEWPORT_HEIGHT);

        assert!(
            rows.iter().any(|row| row.contains("type /help")),
            "fullscreen should render the same initial help message: {rows:?}"
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
    async fn inline_viewport_shows_the_tail_the_terminal_does_not_own_yet() {
        let mut fixture = app_with_llmsim().await;
        let app = &mut fixture.app;
        app.setup = None;
        app.lines.clear();
        app.push_user("Do something".into());
        app.lines.push(ChatLine {
            author: Author::Assistant,
            text: "Done.".into(),
        });

        let rows = render_app_lines(app, 96, COMPOSER_VIEWPORT_HEIGHT);

        assert!(
            rows.iter().any(|row| row.contains("Do something")),
            "unpublished user transcript should be visible above the composer: {rows:?}"
        );
        assert!(
            rows.iter().any(|row| row.contains("Done.")),
            "unpublished assistant transcript should be visible above the composer: {rows:?}"
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
        let session_dir =
            crate::runtime::session_log::session_dir_path(sessions.path(), session_id);
        std::fs::create_dir_all(&session_dir).expect("session dir");
        let log_path = crate::runtime::session_log::session_log_path(&session_dir);
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

        let settings = std::sync::Arc::new(crate::config::SettingsStore::open(
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
    async fn busy_composer_queues_messages_for_fifo_delivery() {
        let mut fixture = app_with_llmsim().await;
        let app = &mut fixture.app;
        app.setup = None;
        app.lines.clear();

        app.set_input_text("first turn".into());
        app.submit_input().await;
        assert!(app.busy);

        for ch in "steer this way".chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::empty()))
                .await;
        }
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()))
            .await;
        app.set_input_text("then verify it".into());
        app.submit_input().await;

        assert_eq!(app.queued_messages.len(), 2);
        assert_eq!(
            app.user_ask_store
                .active_text(app.session.session_id())
                .as_deref(),
            Some("first turn"),
            "queued pivots must not replace the ask before their turn starts"
        );
        assert!(app.input_text().is_empty());
        assert!(app.busy, "queueing must not interrupt the active turn");

        app.pump_turn_until_idle_for_test().await;

        assert!(app.queued_messages.is_empty());
        let user_lines = app
            .lines
            .iter()
            .filter(|line| line.author == Author::User)
            .map(|line| line.text.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            user_lines,
            ["first turn", "steer this way", "then verify it"]
        );
        let assistant_count = app
            .lines
            .iter()
            .filter(|line| line.author == Author::Assistant)
            .count();
        assert_eq!(assistant_count, 3, "each queued message must be delivered");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelling_active_turn_retains_and_delivers_queued_message() {
        let mut fixture = app_with_llmsim().await;
        let app = &mut fixture.app;
        app.setup = None;
        app.lines.clear();

        app.set_input_text("cancel this turn".into());
        app.submit_input().await;
        app.set_input_text("keep this steering".into());
        app.submit_input().await;

        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()))
            .await;
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()))
            .await;

        assert_eq!(app.queued_messages.len(), 1);
        app.pump_turn_until_idle_for_test().await;

        assert!(app.queued_messages.is_empty());
        assert!(
            app.lines
                .iter()
                .any(|line| { line.author == Author::User && line.text == "keep this steering" })
        );
        assert!(app.lines.iter().any(|line| {
            line.author == Author::Assistant && line.text.contains("offline mode")
        }));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ctrl_c_clears_busy_composer_without_interrupting_turn() {
        let mut fixture = app_with_llmsim().await;
        let app = &mut fixture.app;
        let (cancel_tx, mut cancel_rx) = oneshot::channel::<()>();
        app.setup = None;
        app.busy = true;
        app.turn_cancel = Some(cancel_tx);
        app.set_input_text("unsent steering draft".into());

        app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
            .await;

        assert!(app.input_text().is_empty());
        assert!(app.busy);
        assert!(cancel_rx.try_recv().is_err());
        assert!(!app.should_quit);
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

    // --- MCP activation / OAuth reproductions (see issue report) -------------
    // Ignored tests encode desired behavior. Re-run with:
    //   cargo test --all-features repro_ -- --ignored --nocapture

    /// Desired: `/mcp` stays conversational (transcript). Overlay is optional
    /// secondary UI — not required for activation. Locks the no-window baseline
    /// so a future manager sheet cannot become the only path.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn repro_mcp_command_does_not_open_setup_overlay_in_fullscreen() {
        let mut fixture = app_with_llmsim().await;
        let app = &mut fixture.app;
        app.setup = None;
        app.set_render_mode(RenderMode::Fullscreen);
        app.lines.clear();

        app.dispatch_command_for_test("mcp").await;

        assert!(
            app.setup.is_none(),
            "/mcp must not open SetupStep (no MCP manager window). setup={:?}",
            app.setup
        );
        assert!(
            app.lines.iter().any(|line| {
                line.text.contains("MCP")
                    || line.text.contains("mcp")
                    || line.text.contains("usage")
            }),
            "/mcp should print guidance in the transcript: {:?}",
            app.lines
        );
    }

    /// Desired: `/tools` lists MCP tools the session can call.
    ///
    /// Current bug: `/tools` prints frozen `startup.tool_names` and never
    /// includes discovered `mcp_*` tools — matching the Linear report where
    /// login succeeded but `/tools` showed no Linear operations.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn repro_tools_command_lists_live_mcp_tools() {
        let Some(python) =
            crate::testing::mcp_e2e::require_python3("repro_tools_command_lists_live_mcp_tools")
        else {
            return;
        };
        let marker = tempfile::tempdir().expect("marker").keep();
        let tool = crate::testing::mcp_e2e::mcp_tool("echo", "echo");
        let fixture_path = crate::testing::mcp_e2e::fixture_server();

        let mut fixture = app_with_llmsim().await;
        let app = &mut fixture.app;
        std::fs::write(
            app.startup.workspace_root.join(".mcp.json"),
            serde_json::to_vec_pretty(&serde_json::json!({
                "mcpServers": {
                    "echo": {
                        "type": "stdio",
                        "command": python.to_str().unwrap(),
                        "args": [
                            fixture_path.to_str().unwrap(),
                            marker.to_str().unwrap(),
                        ]
                    }
                }
            }))
            .unwrap(),
        )
        .expect("write .mcp.json");
        app.dispatch_command_for_test("mcp reload").await;
        assert!(
            app.startup.mcp_server_names.iter().any(|n| n == "echo"),
            "precondition: echo server must be live: {:?}",
            app.startup.mcp_server_names
        );

        // Control: the same workspace MCP config is executable via a fresh
        // runtime (proves the tool name is real, not hypothetical).
        let scripted = crate::runtime::build_with_options(
            app.startup.workspace_root.clone(),
            crate::runtime::ProviderChoice::Sim,
            None,
            tempfile::tempdir().expect("sessions").keep(),
            std::sync::Arc::new(crate::config::SettingsStore::open(
                tempfile::tempdir()
                    .expect("settings dir")
                    .keep()
                    .join("settings.toml"),
            )),
            crate::runtime::BuildOptions {
                llmsim_override: Some(
                    crate::testing::mcp_e2e::script(&tool, "repro-visible")
                        .with_model("llmsim-yolop"),
                ),
                client_commands: true,
                ..crate::runtime::BuildOptions::default()
            },
        )
        .await
        .expect("build scripted runtime");
        let session_id = scripted.handles.session_id;
        let input = scripted.model.input_message("use echo");
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(20),
            scripted.handles.runtime.run_turn(session_id, input),
        )
        .await
        .expect("timeout")
        .expect("run_turn");
        assert!(
            result.success,
            "precondition: MCP tool must run: {result:?}"
        );
        assert!(
            marker.join("echo.called").exists(),
            "precondition: echo MCP tool must have executed"
        );

        app.lines.clear();
        app.dispatch_command_for_test("tools").await;
        let tools_line = app
            .lines
            .iter()
            .find(|line| line.text.starts_with("tools:"))
            .map(|line| line.text.clone())
            .expect("/tools should print a tools: line");

        assert!(
            tools_line.contains(&tool),
            "/tools must list live MCP tool `{tool}` when the server is active and callable.\n\
             got: {tools_line}\n\
             (Screenshot failure mode: OAuth login succeeded, /tools still showed no MCP tools.)"
        );
    }

    /// MCP OAuth surfaces the authorize URL in the transcript before waiting,
    /// so fullscreen / invisible-browser cases stay conversational in both
    /// render modes.
    #[test]
    fn repro_mcp_oauth_login_exposes_authorize_url_in_host() {
        let tui_src = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/tui/mod.rs"));
        let login_src = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/auth/mcp_oauth_login.rs"
        ));

        assert!(
            tui_src.contains("open this URL to sign in"),
            "mcp_login must print the authorize URL in the transcript before waiting"
        );
        assert!(
            login_src.contains("prepare_login") && login_src.contains("complete_login"),
            "OAuth login must split prepare/complete so the host can show the URL"
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
    async fn clear_command_wipes_transcript_and_reveals_startup_screen() {
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
        assert!(
            app.lines.is_empty(),
            "clear should leave no synthetic history"
        );
        let rendered = recent_transcript_lines(app, 100, 12)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>();
        assert!(
            rendered.iter().any(|line| line.contains("type /help")),
            "clear should reveal the startup screen: {rendered:?}"
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
    async fn shift_enter_inserts_newline_instead_of_submitting() {
        let mut fixture = app_with_llmsim().await;
        let app = &mut fixture.app;
        app.setup = None;

        app.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::empty()))
            .await;
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT))
            .await;
        app.handle_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::empty()))
            .await;

        assert_eq!(app.input_text(), "a\nb");
        assert!(
            !app.lines
                .iter()
                .any(|line| matches!(line.author, Author::User))
        );
    }

    /// Terminals without the kitty keyboard protocol often encode Shift+Enter as
    /// a bare LF, which crossterm surfaces in raw mode as Ctrl+J. That must insert
    /// a newline in the tuika composer — the previous bug was that Ctrl+J was an
    /// unbound no-op, so Shift+Enter appeared to do nothing.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ctrl_j_inserts_newline_without_submitting() {
        let mut fixture = app_with_llmsim().await;
        let app = &mut fixture.app;
        app.setup = None;

        app.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::empty()))
            .await;
        app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL))
            .await;
        app.handle_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::empty()))
            .await;

        assert_eq!(app.input_text(), "a\nb");
        assert!(
            app.lines
                .iter()
                .all(|line| !matches!(line.author, Author::User)),
            "Ctrl+J (raw-mode LF / terminal Shift+Enter) must not submit: {:?}",
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
            app.composer.newline();
            assert_eq!(app.input_height(80), expected);
        }
        app.composer.newline();
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
            app.composer.line_count(),
            1,
            "composer stays one logical line"
        );
        let input_width = 10;
        let measured = app.input_height(input_width);
        assert!(
            measured >= 2,
            "soft-wrapped composer should grow past one row (got {measured})"
        );

        // `input_height` is the composer's own visual height, clamped to the
        // bound — the App delegates straight to the shared TextInputState.
        let expected = TextInputState::from_text(&app.input_text())
            .visual_height(input_width)
            .clamp(1, MAX_INPUT_HEIGHT);
        assert_eq!(
            measured, expected,
            "composer height should match TextInput wrap layout"
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
        assert!(
            rendered
                .iter()
                .any(|line| line.contains("Codex Subscription")),
            "provider picker should use the title-cased Codex Subscription label: {rendered:?}"
        );
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
        let _guard = crate::testing::test_env::lock();
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
        let _guard = crate::testing::test_env::lock();
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
    async fn setup_model_picker_preset_selection_applies_without_persisting() {
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
        // Persistence waits until the selected model completes a turn.
        let snapshot = app.settings.snapshot();
        assert_eq!(snapshot.default_provider.as_deref(), Some("openai"));
        assert_eq!(snapshot.model_for("openai"), None);
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
        let _guard = crate::testing::test_env::lock();
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
    async fn app_view_state_hides_command_suggestions_while_busy() {
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
                queued_messages: 2,
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
            rows[0].contains("2 queued") && rows[0].contains("Enter to queue"),
            "busy separator should expose steering and its queue: {}",
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
    fn fullscreen_expanded_status_uses_responsive_columns() {
        let state = ViewState {
            presentation: PresentationState {
                status_layout: StatusLayout::Expanded,
                agent_status: Some("running tests 3/8".to_string()),
                worktree_expanded: Some(("codex/status-drawer".into(), "…/bb69/yolop".into())),
                ..presentation_state_idle()
            },
            ..view_state_idle()
        };

        let wide = fullscreen_status_layout(&state, 120);
        let plain = |line: &Line<'_>| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        };
        let wide_text = wide.lines.iter().map(&plain).collect::<Vec<_>>().join("\n");
        assert!(
            wide_text.lines().next().is_some_and(|line| {
                line.contains("Runtime") && line.contains("Session") && line.contains("Workspace")
            }),
            "wide drawer should place all sections in columns: {wide_text}"
        );
        assert!(wide_text.contains("agent running tests 3/8"));
        assert!(
            wide.lines
                .iter()
                .all(|line| tuika::components::text::line_width(line) <= 120)
        );

        let narrow = fullscreen_status_layout(&state, 80);
        let narrow_text = narrow
            .lines
            .iter()
            .map(plain)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            narrow.lines.len() > wide.lines.len(),
            "two-column drawer should reflow to more rows"
        );
        assert!(narrow_text.lines().next().is_some_and(|line| {
            line.contains("Runtime") && line.contains("Session") && !line.contains("Workspace")
        }));
        assert!(narrow_text.contains("Workspace"));
        assert!(
            narrow
                .lines
                .iter()
                .all(|line| tuika::components::text::line_width(line) <= 80)
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fullscreen_status_model_and_effort_fields_are_clickable() {
        let mut fixture = app_with_llmsim().await;
        let app = &mut fixture.app;
        app.setup = None;
        app.status_layout = StatusLayout::Expanded;
        app.set_render_mode(RenderMode::Fullscreen);
        let _ = render_app_lines(app, 120, 36);

        for action in [StatusAction::OpenModel, StatusAction::OpenEffort] {
            let area = app
                .status_hit_regions
                .iter()
                .find_map(|(area, candidate)| (*candidate == action).then_some(*area))
                .expect("status action hit region");
            assert!(app.handle_mouse(
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: area.x,
                    row: area.y,
                    modifiers: KeyModifiers::empty(),
                },
                Rect::new(0, 0, 120, 36),
            ));
            match action {
                StatusAction::OpenModel => {
                    assert!(matches!(app.setup, Some(SetupStep::PickModel { .. })));
                }
                StatusAction::OpenEffort => {
                    assert!(matches!(app.setup, Some(SetupStep::PickEffort { .. })));
                }
                _ => unreachable!(),
            }
            app.setup = None;
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn agent_status_is_visible_for_the_turn_and_clears_on_finish() {
        let mut fixture = app_with_llmsim().await;
        let app = &mut fixture.app;
        app.apply_ui_command(UiCommand::SetAgentStatus {
            status: "running tests 3/8".to_string(),
        })
        .await;

        assert_eq!(
            app.presentation_state().agent_status.as_deref(),
            Some("running tests 3/8")
        );

        app.finish_busy();

        assert!(app.presentation_state().agent_status.is_none());
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

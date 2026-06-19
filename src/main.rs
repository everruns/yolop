// Entrypoint for the Yolop coding agent example.
// Decision: support both interactive TUI and a `--print` one-shot mode so the
// example is testable in CI and easy to demo against a real codebase.

mod acp;
mod app;
mod capabilities;
mod capability_settings;
mod clipboard_paste;
mod codex_auth;
mod codex_driver;
mod config_schema;
mod config_service;
mod connectors;
mod goal;
mod hooks_config;
mod host_ui;
mod image_input;
mod into;
mod mcp_config;
mod runtime;
mod session;
mod session_log;
mod settings;
#[cfg(test)]
mod test_env;
mod tools;
mod transcript;
mod version;
mod worktree;

#[cfg(test)]
mod streaming_tests;

#[cfg(test)]
mod mcp_e2e_tests;

#[cfg(test)]
mod agent_scenarios;

use anyhow::{Context, Result};

// Force-link integration crates whose inventory registrations must survive
// LTO/dead-code elimination when we register capabilities explicitly.
extern crate everruns_integrations_daytona;
use app::{App, COMPOSER_VIEWPORT_HEIGHT, maybe_reanchor_inline_viewport};
use clap::{Args, Parser, Subcommand};
use crossterm::event::{
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use crossterm::{execute, queue};
use everruns_core::command::ExecuteCommandRequest;
use everruns_core::message::{ContentPart, MessageRole};
use everruns_core::typed_id::SessionId;
use ratatui::backend::CrosstermBackend;
use ratatui::{Terminal, TerminalOptions, Viewport};
use runtime::{BuiltRuntime, ProviderChoice, ResolvedProviderChoice, resolve_for_settings};
use settings::SettingsStore;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Parser, Debug)]
#[command(
    name = "yolop",
    version = version::VERSION_DETAILS,
    about = "Yolop coding agent — embedded terminal agent built on everruns-runtime"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Workspace root the agent operates inside (default: current dir)
    #[arg(short = 'C', long = "cwd")]
    cwd: Option<PathBuf>,

    /// Force a provider (auto-detected from env vars otherwise)
    #[arg(long, value_enum)]
    provider: Option<ProviderArg>,

    /// Override the model id
    #[arg(short, long)]
    model: Option<String>,

    /// Reasoning effort for model calls (validated against the model's
    /// supported values, e.g. minimal/low/medium/high). Applies to any
    /// provider whose selected model exposes a reasoning-effort setting.
    #[arg(long)]
    reasoning_effort: Option<String>,

    /// Run a single prompt non-interactively and print the result. Useful for CI smoke tests.
    #[arg(short = 'p', long)]
    print: Option<String>,

    /// Attach one or more image files to the prompt. Separate multiple paths
    /// with commas or repeat the flag. Supported formats: png, jpeg, gif, webp.
    #[arg(
        long = "image",
        short = 'i',
        value_name = "FILE",
        value_delimiter = ',',
        num_args = 1..
    )]
    images: Vec<PathBuf>,

    /// Speak the Agent Client Protocol (ACP) over stdio instead of launching
    /// the TUI. Editors such as Zed spawn `yolop --acp` and drive it as an
    /// external agent. Builds one runtime per ACP session (cwd comes from the
    /// client); the `-C/--cwd`, `--print`, and `--session` flags are
    /// ignored in this mode. See `specs/acp.md`.
    #[arg(long, conflicts_with = "print")]
    acp: bool,

    /// Resume an existing session. Reads the JSONL log for this id and
    /// seeds the message history; the new run continues appending to the
    /// same file. If no log exists, a new session starts with this id.
    /// Without `--session`, a fresh id is generated each run.
    #[arg(long)]
    session: Option<String>,

    /// Directory where per-session folders are stored. Default: the
    /// platform-native user data directory (`$XDG_DATA_HOME/yolop/sessions/`
    /// on Linux, `~/Library/Application Support/yolop/sessions/` on macOS,
    /// `%APPDATA%\yolop\sessions\` on Windows).
    #[arg(long)]
    session_dir: Option<PathBuf>,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Print version information.
    Version,
    /// Add yolop into supported editors.
    Into(IntoCommand),
    /// Git worktree maintenance.
    Worktree(WorktreeArgs),
}

#[derive(Args, Debug)]
struct WorktreeArgs {
    #[command(subcommand)]
    command: WorktreeCommand,
}

#[derive(Subcommand, Debug)]
enum WorktreeCommand {
    /// List session worktree directories on disk.
    List,
    /// Remove worktrees not referenced by any saved session.
    Prune {
        /// Print what would be removed without deleting anything.
        #[arg(long)]
        dry_run: bool,
        /// Session storage parent directory (default: platform data dir).
        #[arg(long)]
        session_dir: Option<PathBuf>,
    },
}

#[derive(Args, Debug)]
struct IntoCommand {
    #[command(subcommand)]
    target: IntoTarget,
}

#[derive(Subcommand, Debug)]
enum IntoTarget {
    /// Configure Zed to launch yolop as a custom ACP agent.
    Zed(ZedIntoArgs),
}

#[derive(Args, Debug)]
struct ZedIntoArgs {
    /// Zed settings file to update (default: ~/.config/zed/settings.json).
    #[arg(long = "settings")]
    settings_path: Option<PathBuf>,

    /// Agent server name to write under `agent_servers`.
    #[arg(long, default_value = "yolop")]
    name: String,

    /// Command path Zed should spawn (default: this yolop executable).
    #[arg(long)]
    command: Option<PathBuf>,

    /// Replace an existing `agent_servers.<name>` entry instead of preserving its env/extra fields.
    #[arg(long)]
    force: bool,
}

#[derive(clap::ValueEnum, Debug, Clone, Copy)]
enum ProviderArg {
    Anthropic,
    Codex,
    Openai,
    Google,
    Openrouter,
    Ollama,
    /// Generic OpenAI-compatible endpoint (CUSTOM_BASE_URL / saved base URL).
    Custom,
    #[value(name = "llmsim", alias = "sim")]
    Sim,
}

fn provider_name_for_arg(arg: ProviderArg) -> &'static str {
    match arg {
        ProviderArg::Anthropic => "anthropic",
        ProviderArg::Codex => "codex",
        ProviderArg::Openai => "openai",
        ProviderArg::Google => "google",
        ProviderArg::Openrouter => "openrouter",
        ProviderArg::Ollama => "ollama",
        ProviderArg::Custom => "custom",
        ProviderArg::Sim => "llmsim",
    }
}

/// Resolution order: explicit `--provider` flag > persisted settings >
/// env-var auto-detection. Model and reasoning-effort flags layer on top
/// of whichever base was chosen.
fn pick_provider(cli: &Cli, settings: &SettingsStore) -> (ProviderChoice, Vec<String>) {
    let snapshot = settings.snapshot();
    let cli_reasoning_effort = runtime::normalize_reasoning_effort(cli.reasoning_effort.clone());
    let mut notes = Vec::new();

    let resolved = if let Some(arg) = cli.provider {
        resolve_for_settings(provider_name_for_arg(arg), &snapshot)
            .expect("ProviderArg names are always valid")
    } else if let Some(saved) = snapshot.default_provider.as_deref() {
        match resolve_for_settings(saved, &snapshot) {
            Ok(resolved) => resolved,
            Err(err) => {
                eprintln!("yolop: ignoring saved provider `{saved}`: {err}");
                let auto = ProviderChoice::from_env_or_settings(&snapshot);
                resolve_for_settings(auto.provider_name(), &snapshot).unwrap_or(
                    ResolvedProviderChoice {
                        choice: auto,
                        source: runtime::ModelResolutionSource::ProviderDefault,
                        notes: vec![],
                    },
                )
            }
        }
    } else {
        let auto = ProviderChoice::from_env_or_settings(&snapshot);
        resolve_for_settings(auto.provider_name(), &snapshot).unwrap_or(ResolvedProviderChoice {
            choice: auto,
            source: runtime::ModelResolutionSource::ProviderDefault,
            notes: vec![],
        })
    };

    notes.extend(resolved.notes);
    let base = resolved.choice;
    let selected = if let Some(model) = cli.model.clone() {
        let spec = match cli_reasoning_effort.clone() {
            Some(effort) => format!("{model} {effort}"),
            None => model,
        };
        match base.resolve_model_spec(&spec) {
            Ok(selected) => selected,
            Err(err) => {
                notes.push(format!("model override ignored: {err}"));
                base
            }
        }
    } else {
        base
    };
    let selected = match (selected, cli_reasoning_effort) {
        (
            ProviderChoice::Anthropic {
                model,
                reasoning_effort,
            },
            effort,
        ) => ProviderChoice::Anthropic {
            model,
            reasoning_effort: effort.or(reasoning_effort),
        },
        (
            ProviderChoice::OpenAi {
                model,
                reasoning_effort,
            },
            effort,
        ) => ProviderChoice::OpenAi {
            model,
            reasoning_effort: effort.or(reasoning_effort),
        },
        (
            ProviderChoice::Codex {
                model,
                reasoning_effort,
            },
            effort,
        ) => ProviderChoice::Codex {
            model,
            reasoning_effort: effort.or(reasoning_effort),
        },
        (
            ProviderChoice::Google {
                model,
                base_url,
                reasoning_effort,
            },
            effort,
        ) => ProviderChoice::Google {
            model,
            base_url,
            reasoning_effort: effort.or(reasoning_effort),
        },
        (
            ProviderChoice::OpenRouter {
                model,
                base_url,
                reasoning_effort,
            },
            effort,
        ) => ProviderChoice::OpenRouter {
            model,
            base_url,
            reasoning_effort: effort.or(reasoning_effort),
        },
        (
            ProviderChoice::Ollama {
                model,
                base_url,
                reasoning_effort,
            },
            effort,
        ) => ProviderChoice::Ollama {
            model,
            base_url,
            reasoning_effort: effort.or(reasoning_effort),
        },
        (
            ProviderChoice::Custom {
                model,
                reasoning_effort,
            },
            effort,
        ) => ProviderChoice::Custom {
            model,
            reasoning_effort: effort.or(reasoning_effort),
        },
        (other, _) => other,
    };
    (selected, notes)
}

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("error")),
        )
        .with_writer(io::stderr)
        .init();

    let cli = Cli::parse();
    if let Some(command) = cli.command {
        return run_command(command);
    }

    // Fall back to an unwritable scratch path when no platform config dir
    // is resolvable (minimal containers, CI without HOME). `SettingsStore`
    // loads to defaults when the file does not exist, and writes will
    // error visibly via `/setup` rather than killing
    // startup — keeps `--print` usable in stripped-down environments.
    let settings_path = settings::default_settings_path().unwrap_or_else(|| {
        eprintln!(
            "yolop: no platform config dir resolvable — settings will not persist across runs"
        );
        std::path::PathBuf::from("/dev/null/yolop/settings.toml")
    });
    let settings = Arc::new(SettingsStore::open(settings_path));
    let (mut provider, mut notes) = pick_provider(&cli, &settings);
    let snapshot = settings.snapshot();
    let (reconciled, catalog_notes) =
        capabilities::model_discovery::reconcile_provider_with_catalog(provider, &snapshot).await;
    provider = reconciled;
    notes.extend(catalog_notes);
    for note in notes {
        eprintln!("yolop: {note}");
    }

    let resume_session_id = match cli.session.as_deref() {
        Some(raw) => Some(
            raw.parse()
                .map_err(|e| anyhow::anyhow!("invalid --session id `{raw}`: {e}"))?,
        ),
        None => None,
    };
    let sessions_dir = match cli.session_dir.clone() {
        Some(p) => p,
        None => session_log::default_sessions_dir()?,
    };
    let cwd = resolve_workspace_root(cli.cwd.clone(), resume_session_id, &sessions_dir)?;

    // ACP mode builds runtimes per session (cwd arrives via `session/new`), so
    // it bypasses the up-front runtime build and the TUI.
    if cli.acp {
        return acp::run_stdio(provider, settings, sessions_dir).await;
    }

    // Only the interactive TUI can apply terminal-side commands (overlays,
    // transcript clear, quit), so only it enables `ClientCommandsCapability`.
    // `--print` is one-shot and never dispatches them.
    let interactive = cli.print.is_none();
    let runtime = runtime::build_with_options(
        cwd,
        provider,
        resume_session_id,
        sessions_dir,
        settings,
        runtime::BuildOptions {
            client_commands: interactive,
            ..Default::default()
        },
    )
    .await?;

    if let Some(prompt) = cli.print {
        let image_parts = image_input::load_image_parts(&cli.images)?;
        return run_print_mode(runtime, prompt, image_parts).await;
    }
    let pending_images = image_input::load_image_parts(&cli.images)?;
    run_tui(runtime, pending_images).await
}

fn resolve_workspace_root(
    cli_cwd: Option<PathBuf>,
    resume_session_id: Option<SessionId>,
    sessions_dir: &Path,
) -> Result<PathBuf> {
    if let Some(cwd) = cli_cwd {
        return Ok(cwd);
    }
    if let Some(session_id) = resume_session_id {
        let session_dir = session_log::session_dir_path(sessions_dir, session_id);
        if let Some(saved) = session_log::read_session_workspace(&session_dir)? {
            return Ok(saved);
        }
    }
    std::env::current_dir().context("resolve current workspace directory")
}

fn run_worktree_command(command: WorktreeCommand) -> Result<()> {
    match command {
        WorktreeCommand::List => {
            let paths = worktree::list_worktree_paths_on_disk()?;
            if paths.is_empty() {
                println!("no yolop worktrees found on disk");
            } else {
                for path in paths {
                    println!("{}", path.display());
                }
            }
            Ok(())
        }
        WorktreeCommand::Prune {
            dry_run,
            session_dir,
        } => {
            let sessions_dir = match session_dir {
                Some(path) => path,
                None => session_log::default_sessions_dir()?,
            };
            let report = worktree::prune_orphan_worktrees(&sessions_dir, dry_run)?;
            let action = if dry_run { "would remove" } else { "removed" };
            for path in &report.removed {
                println!("{action}: {}", path.display());
            }
            println!(
                "kept {} referenced worktree(s); {} orphan(s) {action}",
                report.kept,
                report.removed.len()
            );
            for err in &report.errors {
                eprintln!("error: {err}");
            }
            if report.errors.is_empty() {
                Ok(())
            } else {
                std::process::exit(1);
            }
        }
    }
}

fn run_command(command: Commands) -> Result<()> {
    match command {
        Commands::Version => {
            println!("{}", version::VERSION_LINE);
            Ok(())
        }
        Commands::Worktree(args) => run_worktree_command(args.command),
        Commands::Into(into) => match into.target {
            IntoTarget::Zed(args) => {
                let command = match args.command {
                    Some(path) => path,
                    None => std::env::current_exe().unwrap_or_else(|_| PathBuf::from("yolop")),
                };
                let result = into::into_zed(into::ZedIntoOptions {
                    settings_path: args.settings_path,
                    agent_name: args.name,
                    command,
                    force: args.force,
                })?;
                match result.status {
                    into::IntoStatus::Unchanged => {
                        println!(
                            "yolop: Zed already has `{}` configured at {}",
                            result.agent_name,
                            result.settings_path.display()
                        );
                    }
                    into::IntoStatus::Created => {
                        println!(
                            "yolop: added `{}` ACP agent to {}",
                            result.agent_name,
                            result.settings_path.display()
                        );
                    }
                    into::IntoStatus::Updated => {
                        println!(
                            "yolop: updated `{}` ACP agent in {}",
                            result.agent_name,
                            result.settings_path.display()
                        );
                    }
                }
                println!("yolop: Zed command: {} --acp", result.command);
                Ok(())
            }
        },
    }
}

async fn run_tui(runtime: BuiltRuntime, pending_images: Vec<ContentPart>) -> Result<()> {
    let mut raw_mode = RawModeGuard::new()?;
    let mut keyboard_enhancements = KeyboardEnhancementGuard::new();
    let stdout = io::stdout();
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Inline(COMPOSER_VIEWPORT_HEIGHT),
        },
    )?;
    // Anchoring is cosmetic. Since ratatui 0.30.1 `insert_before` snapshots
    // the cursor (via `Terminal::clear`) with a blocking `CSI 6n` query that
    // slow emulators (ttyd / xterm.js) may not answer before crossterm's ~2s
    // timeout. Start unanchored rather than dying.
    if let Err(err) = maybe_reanchor_inline_viewport(&mut terminal) {
        tracing::warn!("inline viewport anchoring failed, starting unanchored: {err:#}");
    }

    let mut app = App::new(runtime, pending_images);
    let result = app.run(&mut terminal).await;
    let show_resume_hint = app.should_show_resume_hint();
    let session_id = app.session_id();

    // Cosmetic cleanup must not turn a successful session into an error
    // exit: since ratatui 0.30.1 `Terminal::clear` issues the same blocking
    // cursor query as anchoring above. The two steps are independent —
    // restoring the cursor is a plain escape write that should still happen
    // when the clear's query times out. Raw-mode restore below still fails
    // hard — leaving the terminal unusable is worth a nonzero exit.
    if let Err(err) = terminal.clear() {
        tracing::warn!("terminal clear failed: {err:#}");
    }
    if let Err(err) = terminal.show_cursor() {
        tracing::warn!("cursor restore failed: {err:#}");
    }
    drop(terminal);
    keyboard_enhancements.disable();
    raw_mode.disable()?;

    if show_resume_hint {
        println!();
        print_resume_divider();
        println!("Resume with yolop --session {session_id}");
        println!();
        print_centered_ukraine_banner();
    }
    result
}

struct RawModeGuard {
    active: bool,
}

impl RawModeGuard {
    fn new() -> Result<Self> {
        enable_raw_mode()?;
        Ok(Self { active: true })
    }

    fn disable(&mut self) -> Result<()> {
        if self.active {
            disable_raw_mode()?;
            self.active = false;
        }
        Ok(())
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        if self.active {
            let _ = disable_raw_mode();
            self.active = false;
        }
    }
}

struct KeyboardEnhancementGuard {
    active: bool,
}

impl KeyboardEnhancementGuard {
    fn new() -> Self {
        let flags = KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
            | KeyboardEnhancementFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES;
        let mut stdout = io::stdout();
        let active = execute!(stdout, PushKeyboardEnhancementFlags(flags)).is_ok();
        Self { active }
    }

    fn disable(&mut self) {
        if self.active {
            let mut stdout = io::stdout();
            let _ = queue!(stdout, PopKeyboardEnhancementFlags);
            let _ = stdout.flush();
            self.active = false;
        }
    }
}

impl Drop for KeyboardEnhancementGuard {
    fn drop(&mut self) {
        self.disable();
    }
}

fn print_resume_divider() {
    let width = crossterm::terminal::size()
        .map(|(width, _)| width as usize)
        .unwrap_or(80)
        .max(1);
    println!("\x1b[38;2;45;91;158m{}\x1b[0m", "─".repeat(width));
}

fn print_centered_ukraine_banner() {
    let text = ">> Зроблено в Україні <<";
    let width = crossterm::terminal::size()
        .map(|(width, _)| width as usize)
        .unwrap_or(0);
    let pad = width.saturating_sub(text.chars().count()) / 2;
    println!(
        "{}\x1b[38;2;45;91;158m>> Зроблено в \x1b[38;2;126;94;19mУкраїні <<\x1b[0m",
        " ".repeat(pad)
    );
}

async fn run_print_mode(
    runtime: BuiltRuntime,
    prompt: String,
    images: Vec<ContentPart>,
) -> Result<()> {
    let BuiltRuntime {
        handles,
        mut startup,
        model,
        goal_store,
        worktree,
        ..
    } = runtime;
    let color = io::stdout().is_terminal();
    let display_prompt = image_input::user_display_text(&prompt, images.len());
    println!("{}", paint(color, "90", &format!("› {display_prompt}")));
    println!();
    if let Err(err) = worktree.ensure_before_turn(prompt.trim()) {
        eprintln!("worktree: {err}");
    }
    startup.workspace_root = worktree.active_root();
    println!(
        "{} {}",
        paint(color, "90", "workspace"),
        startup.workspace_root.display()
    );
    println!(
        "{}  {}",
        paint(color, "90", "provider"),
        paint(color, "96", &model.provider_label())
    );
    println!(
        "{}     {}",
        paint(color, "90", "tools"),
        startup.tool_names.join(", ")
    );
    println!(
        "{}   {} (folder: {}; log: {}; {} prior event(s))",
        paint(color, "90", "session"),
        handles.session_id,
        startup.session_dir.display(),
        startup.session_log_path.display(),
        startup.replayed_events,
    );
    if !startup.capability_commands.is_empty() {
        let names: Vec<String> = startup
            .capability_commands
            .iter()
            .map(|c| format!("/{}", c.name))
            .collect();
        println!("{} {}", paint(color, "90", "commands"), names.join(", "));
    }
    if startup.hook_configured {
        println!(
            "{}     {}",
            paint(color, "90", "hooks"),
            startup.hook_summary()
        );
    }
    println!();

    let trimmed = prompt.trim();
    if let Some(goal_args) = trimmed.strip_prefix("/goal") {
        return run_print_goal(
            &handles,
            &worktree,
            &model,
            &goal_store,
            goal_args.trim(),
            color,
        )
        .await;
    }

    let result = run_print_turn(&handles, &worktree, &model, trimmed, images, color).await?;
    if !result.success {
        std::process::exit(1);
    }
    Ok(())
}

async fn run_print_goal(
    handles: &runtime::RuntimeHandles,
    worktree: &crate::worktree::WorktreeManager,
    model: &runtime::ModelState,
    goal_store: &goal::GoalStore,
    arguments: &str,
    color: bool,
) -> Result<()> {
    let session_id = handles.session_id;
    let request = ExecuteCommandRequest {
        name: "goal".to_string(),
        arguments: if arguments.is_empty() {
            None
        } else {
            Some(arguments.to_string())
        },
        controls: None,
    };
    let result = handles.runtime.execute_command(session_id, request).await?;
    if !result.message.is_empty() {
        println!("{}", paint(color, "90", &result.message));
    }
    if !result.success {
        eprintln!("goal command failed: {}", result.message);
        std::process::exit(1);
    }

    if !goal_store.take_pending_turn(session_id) {
        return Ok(());
    }

    let Some(mut turn_prompt) = goal_store.active_condition(session_id) else {
        return Ok(());
    };

    loop {
        println!();
        println!("{}", paint(color, "90", &format!("› {turn_prompt}")));
        let turn = run_print_turn(handles, worktree, model, &turn_prompt, vec![], color).await?;
        if !turn.success {
            std::process::exit(1);
        }
        if !goal_store.is_active(session_id) {
            return Ok(());
        }

        let evaluation = handles
            .runtime
            .execute_command(
                session_id,
                ExecuteCommandRequest {
                    name: "goal".to_string(),
                    arguments: Some(goal::GOAL_EVALUATE_ARG.to_string()),
                    controls: None,
                },
            )
            .await?;
        if !evaluation.success {
            eprintln!("goal evaluation failed: {}", evaluation.message);
            std::process::exit(1);
        }
        let parsed = goal::parse_evaluation_response(&evaluation.message)?;
        if parsed.met {
            println!(
                "{}",
                paint(color, "92", &format!("goal achieved: {}", parsed.reason))
            );
            return Ok(());
        }
        println!(
            "{}",
            paint(color, "94", &format!("goal: {}", parsed.reason))
        );
        turn_prompt = goal_store
            .continuation_prompt(session_id)
            .unwrap_or_else(|| turn_prompt.clone());
    }
}

async fn run_print_turn(
    handles: &runtime::RuntimeHandles,
    worktree: &crate::worktree::WorktreeManager,
    model: &runtime::ModelState,
    prompt: &str,
    images: Vec<ContentPart>,
    color: bool,
) -> Result<everruns_runtime::TurnResult> {
    if let Err(err) = worktree.ensure_before_turn(prompt) {
        eprintln!("worktree: {err}");
    }
    let before_events = handles.runtime.events().await.map(|e| e.len()).unwrap_or(0);
    let before_msgs = handles
        .runtime
        .messages(handles.session_id)
        .await
        .map(|m| m.len())
        .unwrap_or(0);

    let input = model.input_message_with_images(prompt, images);
    let result = handles.runtime.run_turn(handles.session_id, input).await?;
    let events = handles.runtime.events().await.unwrap_or_default();
    let messages = handles
        .runtime
        .messages(handles.session_id)
        .await
        .unwrap_or_default();

    for event in events.iter().skip(before_events) {
        if let Some(status) = transcript::status_for_event(event) {
            print_status_line(&status.text, color);
        }
        for line in transcript::lines_for_event(event) {
            print_transcript_line(&line, color);
        }
    }
    for msg in messages.iter().skip(before_msgs) {
        if msg.role == MessageRole::Agent
            && !msg.has_tool_calls()
            && let Some(text) = msg.text()
        {
            let t = text.trim();
            if !t.is_empty() {
                println!();
                print_transcript_line(
                    &transcript::ChatLine {
                        author: transcript::Author::Assistant,
                        text: t.to_string(),
                    },
                    color,
                );
            }
        }
    }
    println!(
        "\n{} success={} iterations={} tool_calls={}",
        paint(color, if result.success { "92" } else { "91" }, "done"),
        result.success,
        result.iterations,
        result.tool_calls_count
    );
    if !result.success
        && let Some(err) = &result.error
    {
        eprintln!("turn error: {err}");
    }
    Ok(result)
}

fn print_transcript_line(line: &transcript::ChatLine, color: bool) {
    match line.author {
        transcript::Author::Assistant => {
            println!("{} {}", paint(color, "90", "•"), line.text);
        }
        transcript::Author::Narration => {
            println!(
                "{} {} {}",
                paint(color, "90", "•"),
                paint(color, "90", line.author.label()),
                paint(color, "90", &line.text)
            );
        }
        transcript::Author::Tool => {
            println!(
                "{} {} {}",
                paint(color, "92", "•"),
                paint(color, "93", line.author.label()),
                line.text
            );
        }
        transcript::Author::ToolDetail => {
            println!("           {}", line.text);
        }
        transcript::Author::Diff => {
            println!("  {}", paint(color, "95", &line.text));
        }
        transcript::Author::System | transcript::Author::User => {
            println!(
                "{} {} {}",
                paint(color, "90", "•"),
                paint(color, "90", line.author.label()),
                line.text
            );
        }
    }
}

fn print_status_line(text: &str, color: bool) {
    println!("{} {}", paint(color, "94", "•"), text);
}

fn paint(enabled: bool, code: &str, text: &str) -> String {
    if enabled {
        format!("\x1b[{code}m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cli_with_reasoning_effort(reasoning_effort: Option<&str>) -> Cli {
        Cli {
            command: None,
            cwd: None,
            provider: Some(ProviderArg::Openrouter),
            model: Some("nvidia/nemotron-3-super-120b-a12b".to_string()),
            reasoning_effort: reasoning_effort.map(str::to_string),
            print: None,
            images: vec![],
            acp: false,
            session: None,
            session_dir: None,
        }
    }

    #[test]
    fn pick_provider_normalizes_cli_reasoning_effort() {
        let tmp = tempfile::tempdir().expect("settings tempdir");
        let settings = SettingsStore::open(tmp.path().join("settings.toml"));

        let (provider, _notes) =
            pick_provider(&cli_with_reasoning_effort(Some(" HIGH ")), &settings);

        assert_eq!(
            provider.label(),
            "openrouter/nvidia/nemotron-3-super-120b-a12b high"
        );
    }

    #[test]
    fn pick_provider_ignores_blank_cli_reasoning_effort() {
        let tmp = tempfile::tempdir().expect("settings tempdir");
        let settings = SettingsStore::open(tmp.path().join("settings.toml"));

        let (provider, _notes) = pick_provider(&cli_with_reasoning_effort(Some("  ")), &settings);

        assert_eq!(
            provider.label(),
            "openrouter/nvidia/nemotron-3-super-120b-a12b"
        );
    }

    #[test]
    fn pick_provider_applies_saved_model_for_saved_provider() {
        let _guard = crate::test_env::lock();
        unsafe {
            std::env::remove_var("EVERRUNS_CLI_MODEL");
        }
        let tmp = tempfile::tempdir().expect("settings tempdir");
        let path = tmp.path().join("settings.toml");
        std::fs::write(
            &path,
            "provider = \"openai\"\n\n[models]\nopenai = \"gpt-5.4 high\"\n",
        )
        .expect("write settings");
        let settings = SettingsStore::open(path);
        let cli = Cli {
            command: None,
            cwd: None,
            provider: None,
            model: None,
            reasoning_effort: None,
            print: None,
            images: vec![],
            acp: false,
            session: None,
            session_dir: None,
        };

        let (provider, _notes) = pick_provider(&cli, &settings);

        assert_eq!(provider.label(), "openai/gpt-5.4 high");
    }

    #[test]
    fn resolve_workspace_root_uses_saved_session_workspace() {
        let sessions = tempfile::tempdir().expect("sessions tempdir");
        let workspace = tempfile::tempdir().expect("workspace tempdir");
        let session_id = SessionId::from_seed(42);
        let session_dir = session_log::session_dir_path(sessions.path(), session_id);
        session_log::write_session_workspace(
            &session_dir,
            &session_log::SessionWorkspaceMetadata {
                active_root: workspace.path().to_path_buf(),
                repo_root: None,
                worktree: None,
            },
        )
        .expect("write workspace metadata");

        let resolved =
            resolve_workspace_root(None, Some(session_id), sessions.path()).expect("resolve");

        assert_eq!(resolved, workspace.path());
    }

    #[test]
    fn resolve_workspace_root_prefers_explicit_cwd() {
        let sessions = tempfile::tempdir().expect("sessions tempdir");
        let saved = tempfile::tempdir().expect("saved workspace tempdir");
        let explicit = tempfile::tempdir().expect("explicit workspace tempdir");
        let session_id = SessionId::from_seed(43);
        let session_dir = session_log::session_dir_path(sessions.path(), session_id);
        session_log::write_session_workspace(
            &session_dir,
            &session_log::SessionWorkspaceMetadata {
                active_root: saved.path().to_path_buf(),
                repo_root: None,
                worktree: None,
            },
        )
        .expect("write workspace metadata");

        let resolved = resolve_workspace_root(
            Some(explicit.path().to_path_buf()),
            Some(session_id),
            sessions.path(),
        )
        .expect("resolve");

        assert_eq!(resolved, explicit.path());
    }

    #[test]
    fn inline_viewport_anchor_fills_space_above_bottom_target() {
        assert_eq!(app::blank_lines_after_inline_viewport(4, 18, 60), 38);
    }

    #[test]
    fn inline_viewport_anchor_does_not_scroll_when_already_low_enough() {
        assert_eq!(app::blank_lines_after_inline_viewport(42, 18, 60), 0);
        assert_eq!(app::blank_lines_after_inline_viewport(50, 18, 60), 0);
    }

    #[test]
    fn inline_viewport_anchor_handles_small_terminals() {
        assert_eq!(app::blank_lines_after_inline_viewport(0, 18, 10), 0);
    }
}

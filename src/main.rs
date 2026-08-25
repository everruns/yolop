// Entrypoint for the Yolop coding agent example.
// Decision: support both interactive TUI and a `--print` one-shot mode so the
// example is testable in CI and easy to demo against a real codebase.

mod auth;
mod capabilities;
mod config;
mod connectors;
mod control;
mod crash_report;
mod drivers;
mod editor;
mod exec;
mod extensions;
mod models;
mod runtime;
mod sandbox_approval;
mod session_state;
mod tui;
mod version;

#[cfg(test)]
mod testing;

use crate::capabilities::ClientUiContext;
use anyhow::{Context, Result};

// Force-link integration crates whose inventory registrations must survive
// LTO/dead-code elimination when we register capabilities explicitly.
extern crate everruns_integrations_daytona;
extern crate everruns_integrations_parallel;
use clap::{Args, CommandFactory, FromArgMatches, Parser, Subcommand};
use config::SettingsStore;
use config::mcp::{McpConfigScope, McpConfigStore};
use crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use crossterm::{execute, queue};
use everruns_core::command::ExecuteCommandRequest;
use everruns_core::{ContentPart, MessageRole};
use everruns_provider::typed_id::SessionId;
use ratatui::{Terminal, TerminalOptions};
use runtime::{
    BuiltRuntime, DEFAULT_LOCAL_MODEL, ProviderChoice, ResolvedProviderChoice, resolve_for_settings,
};
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tracing_subscriber::fmt::writer::BoxMakeWriter;
use tui::{App, COMPOSER_VIEWPORT_HEIGHT};
use tuika::term::capabilities::Capabilities;
use tuika::term::hyperlink::{HyperlinkBackend, LinkPolicy};

const MAX_INTERACTIVE_TRACE_LOGS: usize = 5;
const MAX_INTERACTIVE_TRACE_BYTES: u64 = 4 * 1024 * 1024;

struct BoundedTraceWriter {
    file: std::fs::File,
    remaining: u64,
}

impl BoundedTraceWriter {
    fn new(file: std::fs::File, max_bytes: u64) -> Self {
        Self {
            file,
            remaining: max_bytes,
        }
    }
}

impl Write for BoundedTraceWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let writable = bytes.len().min(self.remaining as usize);
        self.file.write_all(&bytes[..writable])?;
        self.remaining -= writable as u64;
        // Once capped, consume later trace events without growing the file or
        // surfacing write failures into the application being diagnosed.
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

#[derive(Parser, Debug)]
#[command(
    name = "yolop",
    version = version::VERSION_DETAILS,
    about = "Yolop coding agent — embedded terminal agent built on everruns-host"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Internal one-shot control-plane child. The parent supplies anonymous
    /// pipes; this flag is never a public session-selection mechanism.
    #[arg(long = "__attached-control-child", hide = true)]
    attached_control_child: bool,

    /// Workspace root the agent operates inside (default: current dir)
    #[arg(short = 'C', long = "cwd")]
    cwd: Option<PathBuf>,

    /// Force a provider (auto-detected from env vars otherwise)
    #[arg(long, value_enum)]
    provider: Option<ProviderArg>,

    /// Load a named profile from `profiles/<name>.toml` under the config
    /// directory (see `--config-dir`): an
    /// overlay of provider, model, approval, sandbox, and worktree settings plus
    /// the profile's own capabilities, extensions, MCP servers, skills, and
    /// system-prompt instructions.
    #[arg(long, value_name = "NAME")]
    profile: Option<String>,

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

    /// Directory holding yolop's global configuration (settings, profiles,
    /// hooks, installed extensions). Defaults to `yolop` inside the platform
    /// config directory; `YOLOP_CONFIG_DIR` sets it for a whole shell. Point
    /// both this and `--data-dir` somewhere private to run an isolated yolop
    /// identity without moving `HOME`.
    #[arg(long = "config-dir", value_name = "PATH", global = true)]
    config_dir: Option<PathBuf>,

    /// Directory holding yolop's global data (sessions, logs, models, prompt
    /// history). Defaults to `yolop` inside the platform data directory;
    /// `YOLOP_DATA_DIR` sets it for a whole shell.
    #[arg(long = "data-dir", value_name = "PATH", global = true)]
    data_dir: Option<PathBuf>,

    /// Speak the Agent Client Protocol (ACP) over stdio instead of launching
    /// the TUI. Editors such as Zed spawn `yolop --acp` and drive it as an
    /// external agent. Builds one runtime per ACP session (cwd comes from the
    /// client); the `-C/--cwd`, `--print`, and `--session` flags are
    /// ignored in this mode. See `knowledge/specs/acp.md`.
    #[arg(long, conflicts_with = "print")]
    acp: bool,

    /// Offer the loopback provider setup page to ACP clients for this run: an
    /// extra authentication method, plus a link posted when a session opens
    /// with no provider connected. ACP has no secure way to ask for an API key,
    /// so the page is where one can be typed. Off unless this flag or the
    /// `acp_setup_page` setting is on. See `knowledge/specs/acp.md`.
    #[arg(long = "acp-setup-page", requires = "acp")]
    acp_setup_page: bool,

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

    /// Write the full session as an ATIF v1.7 trajectory JSON file to this
    /// path at end of run. Works interactively and with `-p/--print`; see
    /// `knowledge/specs/trajectory.md`.
    #[arg(long, value_name = "PATH")]
    trajectory_out: Option<PathBuf>,

    /// Render the interactive TUI as a split footer — a composer pinned to the
    /// bottom rows with the transcript above it — instead of using the default
    /// fullscreen alternate-screen renderer. Native terminal scrollback is
    /// available in this mode.
    #[arg(long)]
    inline: bool,

    /// Collapse each active turn's narration and tool output into one updating
    /// transcript row. Press Ctrl+O to expand or collapse the latest turn's
    /// retained work details. Fullscreen only because inline scrollback cannot
    /// repaint a previously published accordion.
    #[arg(long, conflicts_with_all = ["inline", "print", "acp"])]
    compact_work: bool,

    /// Color theme for the interactive TUI. `yolop` (default) is yolop's own
    /// palette; other values select a bundled `tuika` preset (e.g.
    /// `solarized-dark`, `gruvbox-dark`, `dracula`, `light`). Persisted default
    /// comes from settings when unset.
    #[arg(long, value_name = "NAME")]
    theme: Option<String>,

    /// Enable shell sandboxing for this run. Commands may write only in the
    /// workspace and temporary directories, and network access is blocked.
    #[arg(long)]
    sandbox: bool,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Print version information.
    Version,
    /// Add yolop into supported editors.
    Into(IntoCommand),
    /// Git worktree maintenance.
    Worktree(WorktreeArgs),
    /// Manage MCP servers in global settings or workspace `.mcp.json`.
    Mcp(McpArgs),
    /// Manage downloaded weights for the `local` provider.
    ///
    /// Named `weights` rather than `models` because `yolop models` is the
    /// user's model list (see `capabilities::model_list`); this command is
    /// about files on disk.
    Weights(WeightsArgs),
    /// Live demo of the experimental `tuika` TUI toolkit (spinners, progress
    /// bars, loader). Press `q` or `Esc` to quit. Hidden dev helper.
    #[command(hide = true)]
    TuikaGallery,
    /// Internal Linux Landlock/seccomp worker. Not a user-facing command.
    #[cfg(target_os = "linux")]
    #[command(name = "__sandbox-exec", hide = true)]
    SandboxExec {
        #[arg(long)]
        cwd: PathBuf,
        #[arg(long)]
        temp: PathBuf,
        #[arg(long)]
        mode: config::SandboxMode,
        #[arg(long, allow_hyphen_values = true)]
        script: String,
    },
}

#[derive(Args, Debug)]
struct McpArgs {
    #[command(subcommand)]
    command: McpCommand,
}

#[derive(Args, Debug)]
struct WeightsArgs {
    #[command(subcommand)]
    command: WeightsCommand,
}

#[derive(Subcommand, Debug)]
enum WeightsCommand {
    /// List downloaded models and the disk they use.
    List,
    /// Download a model into the store.
    ///
    /// Specs are Hugging Face repos (`Qwen/Qwen3-8B`) or a GGUF file inside one
    /// (`unsloth/Qwen3-8B-GGUF::Qwen3-8B-Q4_K_M.gguf`).
    Pull {
        /// Model spec to download.
        spec: String,
    },
    /// Delete a downloaded model and reclaim its disk.
    Rm {
        /// Hugging Face repo to remove.
        repo: String,
    },
}

#[derive(Subcommand, Debug)]
enum McpCommand {
    /// List configured MCP servers.
    List {
        /// Scope to inspect (`global`, `workspace`, or `effective`).
        #[arg(long, default_value = "effective")]
        scope: McpScopeArg,
        /// Workspace root for workspace/effective config.
        #[arg(short = 'C', long = "cwd")]
        cwd: Option<PathBuf>,
    },
    /// Add or replace an MCP server.
    Add {
        /// Scope to write (`global` or `workspace`).
        #[arg(long, default_value = "global")]
        scope: McpScopeArg,
        /// Server name.
        name: String,
        /// Transport type (`stdio`, `http`, or `sse`).
        #[arg(long = "type")]
        transport_type: String,
        /// Command for stdio servers.
        #[arg(long)]
        command: Option<String>,
        /// Command arguments for stdio servers. Repeat or comma-separate.
        #[arg(long, value_delimiter = ',', num_args = 0..)]
        args: Vec<String>,
        /// URL for http/sse servers.
        #[arg(long)]
        url: Option<String>,
        /// Header as KEY=VALUE. Repeat for multiple headers.
        #[arg(long = "header", value_parser = parse_key_value)]
        headers: Vec<(String, String)>,
        /// Auth mode (`bearer`, `oauth`, or `none`).
        #[arg(long)]
        auth_mode: Option<String>,
        /// OAuth provider id for auth-mode=oauth.
        #[arg(long)]
        oauth_provider_id: Option<String>,
        /// Disable the server immediately after adding it.
        #[arg(long, default_value_t = false)]
        disabled: bool,
        /// Workspace root when writing workspace config.
        #[arg(short = 'C', long = "cwd")]
        cwd: Option<PathBuf>,
    },
    /// Remove an MCP server.
    Remove {
        /// Scope to write (`global` or `workspace`).
        #[arg(long, default_value = "global")]
        scope: McpScopeArg,
        /// Server name.
        name: String,
        /// Workspace root when writing workspace config.
        #[arg(short = 'C', long = "cwd")]
        cwd: Option<PathBuf>,
    },
    /// Enable or disable an MCP server without deleting it.
    Enable {
        /// Scope to write (`global` or `workspace`).
        #[arg(long, default_value = "global")]
        scope: McpScopeArg,
        /// Server name.
        name: String,
        /// Disable instead of enable.
        #[arg(long, default_value_t = false)]
        disable: bool,
        /// Workspace root when writing workspace config.
        #[arg(short = 'C', long = "cwd")]
        cwd: Option<PathBuf>,
    },
}

#[derive(clap::ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
enum McpScopeArg {
    Global,
    Workspace,
    Effective,
}

fn parse_key_value(value: &str) -> Result<(String, String), String> {
    let (key, val) = value
        .split_once('=')
        .ok_or_else(|| "expected KEY=VALUE".to_string())?;
    if key.trim().is_empty() {
        return Err("header key cannot be empty".to_string());
    }
    Ok((key.trim().to_string(), val.to_string()))
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
    /// Configure Buzz Desktop to launch yolop as a custom ACP harness.
    Buzz(BuzzIntoArgs),
    /// Configure Paseo to launch yolop as a custom ACP provider.
    Paseo(PaseoIntoArgs),
    /// Configure Zed to launch yolop as a custom ACP agent.
    Zed(ZedIntoArgs),
}

#[derive(Args, Debug)]
struct BuzzIntoArgs {
    /// Replace the custom harness instead of preserving its env/extra fields.
    #[arg(long)]
    force: bool,
}

#[derive(Args, Debug)]
struct PaseoIntoArgs {
    /// Replace an existing `agents.providers.yolop` entry instead of preserving its env/extra fields.
    #[arg(long)]
    force: bool,
}

#[derive(Args, Debug)]
struct ZedIntoArgs {
    /// Replace an existing `agent_servers.yolop` entry instead of preserving its env/extra fields.
    #[arg(long)]
    force: bool,
}

#[derive(clap::ValueEnum, Debug, Clone, Copy)]
enum ProviderArg {
    Anthropic,
    Meta,
    Codex,
    Openai,
    Google,
    Openrouter,
    Ollama,
    /// In-process inference, no external server. Requires a build with the
    /// `local-inference` feature: the accelerated release binaries
    /// (`yolop-<target>-metal` / `-cuda`), not the default one or Homebrew.
    Local,
    /// Generic OpenAI-compatible endpoint (CUSTOM_BASE_URL / saved base URL).
    Custom,
    #[value(name = "llmsim", alias = "sim")]
    Sim,
}

fn provider_name_for_arg(arg: ProviderArg) -> &'static str {
    match arg {
        ProviderArg::Anthropic => "anthropic",
        ProviderArg::Meta => "meta",
        ProviderArg::Codex => "codex",
        ProviderArg::Openai => "openai",
        ProviderArg::Google => "google",
        ProviderArg::Openrouter => "openrouter",
        ProviderArg::Ollama => "ollama",
        ProviderArg::Local => "local",
        ProviderArg::Custom => "custom",
        ProviderArg::Sim => "llmsim",
    }
}

/// Resolution order: explicit `--provider` flag > persisted settings >
/// env-var auto-detection. Model and reasoning-effort flags layer on top
/// of whichever base was chosen.
/// Reject a provider this build cannot serve, before the runtime starts.
///
/// `local` resolves in every build so the picker and settings stay
/// feature-independent, but only a `local-inference` build registers a driver
/// for it. Without this the failure surfaces mid-turn as the driver registry's
/// "No driver registered for provider type" — an internal message that tells
/// the user nothing they can act on.
fn ensure_provider_is_built_in(provider: &ProviderChoice) -> Result<()> {
    if matches!(provider, ProviderChoice::Local { .. }) && !cfg!(feature = "local-inference") {
        anyhow::bail!(
            "this build has no local inference engine, so `--provider local` cannot run. \
             Download an accelerated build (`yolop-<target>-metal` or `-cuda`) from \
             https://github.com/everruns/yolop/releases/latest, or build with \
             `--features metal` / `--features cuda`."
        );
    }
    Ok(())
}

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
            ProviderChoice::Meta {
                model,
                reasoning_effort,
            },
            effort,
        ) => ProviderChoice::Meta {
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

fn main() -> Result<()> {
    // First: the crash reporter below, and the CLI registry that builds the
    // command line, both resolve global paths, so the directory overrides have
    // to be known before either runs.
    config::paths::init_from_args(std::env::args_os());

    // Windows caps the main-thread stack at 1 MiB. The entry future (settings
    // load → runtime build → TUI/print loop) is large enough to overflow that,
    // though it fits comfortably in the 8 MiB default Linux and macOS give the
    // main thread. Run the whole program on a thread with an explicit large
    // stack so every platform behaves like the generous default, and give the
    // tokio worker threads a matching stack for deep async work spawned onto
    // them.
    let crash_reporter = crash_report::CrashReporter::install();
    let worker_crash_reporter = crash_reporter.clone();
    let worker = std::thread::Builder::new()
        .name("yolop-main".to_string())
        .stack_size(16 * 1024 * 1024)
        .spawn(move || {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(4)
                .thread_stack_size(8 * 1024 * 1024)
                .enable_all()
                .build()
                .expect("build tokio runtime")
                .block_on(async_main(&worker_crash_reporter))
        })
        .expect("spawn yolop main thread");
    join_worker(worker, &crash_reporter)
}

fn join_worker<T>(
    worker: std::thread::JoinHandle<T>,
    crash_reporter: &crash_report::CrashReporter,
) -> T {
    match worker.join() {
        Ok(result) => result,
        Err(payload) => {
            if let Some(session_id) = crash_reporter.session_id() {
                // Session IDs are opaque local locators, not credentials; emitting
                // one here is intentional so users can recover a crashed session.
                eprintln!("yolop: crashed session id: {session_id}");
            }
            if let Some(path) = crash_reporter.report_path() {
                eprintln!("yolop: crash report written to {}", path.display());
            }
            std::panic::resume_unwind(payload)
        }
    }
}

/// Render a clap parse failure, listing the valid subcommands when the user
/// named one that does not exist.
///
/// Clap's own message is `unrecognized subcommand '<x>'` followed by a usage
/// line whose `[COMMAND]` is a placeholder, so it never says what the real
/// commands are. A caller that guesses wrong learns nothing and has to fetch
/// `--help` separately, which is a wasted round trip for a human and several
/// for an agent. Every other error keeps clap's own rendering and exit code.
fn exit_with_cli_error(err: clap::Error, root: &clap::Command) -> ! {
    if err.kind() == clap::error::ErrorKind::InvalidSubcommand
        && let Some((path, names)) = unknown_subcommand_context(root)
        && !names.is_empty()
    {
        eprint!("{err}");
        eprintln!("Available `{path}` commands:");
        for name in names {
            eprintln!("  {name}");
        }
        std::process::exit(2);
    }
    err.exit()
}

/// Walk the real argv down the command tree to the group that owns the
/// unrecognized name, and return that group's path and its subcommand names.
/// Flags and their values are skipped: only positionals can name a subcommand.
fn unknown_subcommand_context(root: &clap::Command) -> Option<(String, Vec<String>)> {
    let mut command = root;
    let mut path = vec![root.get_name().to_string()];
    for arg in std::env::args().skip(1) {
        if arg.starts_with('-') {
            continue;
        }
        match command
            .get_subcommands()
            .find(|sub| sub.get_name() == arg || sub.get_all_aliases().any(|alias| alias == arg))
        {
            // A known subcommand: descend and keep looking.
            Some(sub) => {
                path.push(sub.get_name().to_string());
                command = sub;
            }
            // The first name that does not match is the failure; `command` owns it.
            None => {
                let names = command
                    .get_subcommands()
                    .filter(|sub| !sub.is_hide_set())
                    .map(|sub| sub.get_name().to_string())
                    .collect();
                return Some((path.join(" "), names));
            }
        }
    }
    None
}

async fn async_main(crash_reporter: &crash_report::CrashReporter) -> Result<()> {
    let cli_registry = detached_cli_registry()?;
    let root = cli_registry.augment(Cli::command())?;
    let mut matches = match root.clone().try_get_matches() {
        Ok(matches) => matches,
        Err(err) => exit_with_cli_error(err, &root),
    };
    if let Some(invocation) = cli_registry.invocation(&matches)? {
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("error")),
            )
            .with_ansi(true)
            .with_writer(trace_writer(false))
            .init();
        if matches.get_flag("attached_control_child") {
            return control::run_control_child(invocation.request).await;
        }
        return invocation.execute().await;
    }
    let cli = Cli::from_arg_matches_mut(&mut matches)?;
    let interactive = uses_interactive_renderer(&cli);
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("error")),
        )
        .with_ansi(!interactive)
        .with_writer(trace_writer(interactive))
        .init();

    if cli.attached_control_child {
        anyhow::bail!("attached control accepts only a contributed CLI capability command");
    }

    if let Some(command) = cli.command {
        return run_command(command).await;
    }

    // Fall back to an unwritable scratch path when no platform config dir
    // is resolvable (minimal containers, CI without HOME). `SettingsStore`
    // loads to defaults when the file does not exist, and writes will
    // error visibly via `/setup` rather than killing
    // startup — keeps `--print` usable in stripped-down environments.
    let settings_path = config::default_settings_path().unwrap_or_else(|| {
        eprintln!(
            "yolop: no platform config dir resolvable — settings will not persist across runs"
        );
        std::path::PathBuf::from("/dev/null/yolop/settings.toml")
    });
    let settings = Arc::new(match cli.profile.as_deref() {
        Some(profile) => SettingsStore::open_with_profile(settings_path, profile)?,
        None => SettingsStore::open(settings_path),
    });
    for warning in settings.profile_warnings() {
        eprintln!("yolop: {warning}");
    }
    if let (Some(name), Some(path)) = (
        settings.active_profile_name(),
        settings.active_profile_path(),
    ) {
        eprintln!("yolop: profile `{name}` loaded from {}", path.display());
    }
    let sandbox_mode_override = cli.sandbox.then_some(config::SandboxMode::WorkspaceWrite);
    let effective_sandbox_mode =
        sandbox_mode_override.unwrap_or_else(|| settings.snapshot().sandbox_mode());
    let (mut provider, mut notes) = pick_provider(&cli, &settings);
    ensure_provider_is_built_in(&provider)?;
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
        None => runtime::session_log::default_sessions_dir()?,
    };
    let cwd = resolve_workspace_root(cli.cwd.clone(), resume_session_id, &sessions_dir)?;

    // ACP mode builds runtimes per session (cwd arrives via `session/new`), so
    // it bypasses the up-front runtime build and the TUI.
    if cli.acp {
        if let Some(warning) = exec::sandbox::danger_warning(effective_sandbox_mode) {
            eprintln!("yolop: {warning}");
        }
        if cli.trajectory_out.is_some() {
            eprintln!("yolop: --trajectory-out is ignored in --acp mode");
        }
        // The flag turns the page on for one run; the setting turns it on for
        // every editor launch, which is what matters when the editor owns the
        // spawn arguments.
        let setup_page = cli.acp_setup_page || settings.snapshot().acp_setup_page_enabled();
        return editor::acp::run_stdio(
            provider,
            settings,
            sessions_dir,
            sandbox_mode_override,
            setup_page,
        )
        .await;
    }

    // Only the interactive TUI can apply terminal-side commands (overlays,
    // transcript clear, quit), so only it enables `ClientCommandsCapability`.
    // `--print` is one-shot and never dispatches them.
    let interactive = cli.print.is_none();
    if let Some(warning) = exec::sandbox::danger_warning(effective_sandbox_mode) {
        eprintln!("yolop: {warning}");
    }
    // Captured before `settings` is moved into the runtime build below; used to
    // resolve the TUI theme once the runtime is ready.
    let saved_theme = settings.snapshot().theme().map(str::to_string);
    let runtime = runtime::build_with_options(
        cwd,
        provider,
        resume_session_id,
        sessions_dir,
        settings,
        runtime::BuildOptions {
            client_commands: interactive,
            client_ui: if interactive {
                ClientUiContext::Tui
            } else {
                ClientUiContext::Print
            },
            session_kind: if interactive {
                runtime::session_log::SessionKind::Interactive
            } else {
                runtime::session_log::SessionKind::Print
            },
            initial_prompt: cli.print.clone(),
            sandbox_mode_override,
            ..Default::default()
        },
    )
    .await?;
    crash_reporter.set_session_id(runtime.handles.session_id.to_string());

    // Resolve the interactive TUI theme: the `--theme` flag wins, else the
    // persisted `theme` setting. Done before the print-mode branch so a bad
    // `--theme` is rejected in every mode; the override only affects the TUI
    // (print/ACP never read it), so setting it here is otherwise harmless.
    if let Some(name) = cli.theme.as_deref() {
        // An explicit flag is a hard error when unknown.
        let theme = tui::fullscreen::resolve_theme(name).ok_or_else(|| {
            anyhow::anyhow!(
                "unknown --theme `{name}`; expected one of: {}",
                tui::fullscreen::theme_names().join(", ")
            )
        })?;
        tui::fullscreen::set_theme_override(theme);
    } else if let Some(name) = saved_theme.as_deref() {
        // A persisted value is best-effort: warn and fall back rather than
        // refusing to start (e.g. a theme from a newer version).
        match tui::fullscreen::resolve_theme(name) {
            Some(theme) => tui::fullscreen::set_theme_override(theme),
            None => eprintln!("yolop: ignoring unknown saved theme `{name}`"),
        }
    }

    if let Some(prompt) = cli.print {
        let image_parts = tui::input::image_input::load_image_parts(&cli.images)?;
        return run_print_mode(runtime, prompt, image_parts, cli.trajectory_out).await;
    }
    let pending_images = tui::input::image_input::load_image_parts(&cli.images)?;
    run_tui(
        runtime,
        pending_images,
        cli.trajectory_out,
        !cli.inline,
        cli.compact_work,
    )
    .await
}

fn uses_interactive_renderer(cli: &Cli) -> bool {
    (cli.command.is_none() && cli.print.is_none() && !cli.acp)
        || matches!(cli.command.as_ref(), Some(Commands::TuikaGallery))
}

fn trace_writer(interactive: bool) -> BoxMakeWriter {
    if !interactive {
        return BoxMakeWriter::new(io::stderr);
    }
    let Some(data_dir) = config::paths::data_dir() else {
        eprintln!("yolop: no platform data dir resolvable — interactive tracing is disabled");
        return BoxMakeWriter::new(io::sink);
    };
    let trace_dir = data_dir.join("logs");
    match open_interactive_trace_log(&trace_dir) {
        Ok((file, _path)) => BoxMakeWriter::new(Mutex::new(BoundedTraceWriter::new(
            file,
            MAX_INTERACTIVE_TRACE_BYTES,
        ))),
        Err(err) => {
            eprintln!(
                "yolop: cannot open interactive trace log in {}: {err}",
                trace_dir.display()
            );
            BoxMakeWriter::new(io::sink)
        }
    }
}

fn open_interactive_trace_log(dir: &Path) -> io::Result<(std::fs::File, PathBuf)> {
    let timestamp = chrono::Utc::now().format("%Y-%m-%dT%H-%M-%S%.6fZ");
    open_interactive_trace_log_at(dir, &timestamp.to_string(), std::process::id())
}

fn open_interactive_trace_log_at(
    dir: &Path,
    timestamp: &str,
    process_id: u32,
) -> io::Result<(std::fs::File, PathBuf)> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }

    let path = dir.join(format!("{timestamp}-{process_id}-trace.log"));
    let mut options = std::fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(&path)?;
    prune_interactive_trace_logs(dir, MAX_INTERACTIVE_TRACE_LOGS);
    Ok((file, path))
}

fn prune_interactive_trace_logs(dir: &Path, keep: usize) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut logs = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_file()
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.ends_with("-trace.log"))
        })
        .collect::<Vec<_>>();
    logs.sort();
    let remove_count = logs.len().saturating_sub(keep);
    for path in logs.into_iter().take(remove_count) {
        let _ = std::fs::remove_file(path);
    }
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
        let session_dir = runtime::session_log::session_dir_path(sessions_dir, session_id);
        if let Some(saved) = runtime::session_log::read_session_workspace(&session_dir)? {
            return Ok(saved);
        }
    }
    std::env::current_dir().context("resolve current workspace directory")
}

/// What to tell someone whose weights store is empty.
///
/// `list` and `rm` compile into every build, so one that dropped the engine can
/// still clean up after one that had it. Pulling does not, and the default
/// release binary is now the engine-less one, so the obvious hint would
/// dead-end most of the people who read it.
fn empty_weights_store_hint() -> String {
    if cfg!(feature = "local-inference") {
        format!("Pull one with `yolop weights pull {DEFAULT_LOCAL_MODEL}`.")
    } else {
        "This build has no local inference engine, so there is nothing here to run weights. \
         Download an accelerated build (`yolop-<target>-metal` or `-cuda`) from \
         https://github.com/everruns/yolop/releases/latest to pull them."
            .to_string()
    }
}

async fn run_weights_command(command: WeightsCommand) -> Result<()> {
    match command {
        WeightsCommand::List => {
            let models = models::installed()?;
            if models.is_empty() {
                let location = models::store_root()
                    .map(|root| root.display().to_string())
                    .unwrap_or_else(|| "<no data directory>".to_string());
                println!("No models downloaded. Store: {location}");
                println!("{}", empty_weights_store_hint());
                return Ok(());
            }
            let total: u64 = models.iter().map(|model| model.bytes).sum();
            for model in &models {
                println!("{:<44} {}", model.repo, models::human_bytes(model.bytes));
            }
            println!("{:<44} {}", "total", models::human_bytes(total));
            Ok(())
        }
        WeightsCommand::Rm { repo } => {
            let reclaimed = models::remove(&repo)?;
            println!(
                "Removed {repo} ({} reclaimed)",
                models::human_bytes(reclaimed)
            );
            Ok(())
        }
        WeightsCommand::Pull { spec } => run_models_pull(&spec).await,
    }
}

/// Download `spec`, awaiting on the caller's runtime.
///
/// This must not build a runtime of its own. `run_command` is reached from
/// `#[tokio::main]`, so a nested `Runtime::new().block_on(..)` panics with
/// "Cannot start a runtime from within a runtime" before a single byte is
/// fetched — which is exactly what shipped in 0.16.0.
#[cfg(feature = "local-inference")]
async fn run_models_pull(spec: &str) -> Result<()> {
    use std::sync::{Arc, Mutex};

    if models::is_installed(spec) {
        println!("{spec} is already downloaded.");
        return Ok(());
    }

    println!("Pulling {spec}…");
    let sink: Arc<Mutex<dyn models::download::ProgressSink>> =
        Arc::new(Mutex::new(models::download::StderrProgress::new()));
    let summary = models::download::pull(spec, sink).await?;

    if summary.files.is_empty() {
        println!("Nothing to fetch — {spec} was already complete.");
    } else {
        println!(
            "Downloaded {} file(s), {}",
            summary.files.len(),
            models::human_bytes(summary.bytes)
        );
    }
    Ok(())
}

/// Without the engine there is nothing to run the weights, so pulling gigabytes
/// would only waste the disk. List and remove still work, so a build that drops
/// the feature can still clean up what an earlier build downloaded.
#[cfg(not(feature = "local-inference"))]
async fn run_models_pull(_spec: &str) -> Result<()> {
    Err(anyhow::anyhow!(
        "this build has no local inference engine, so downloaded weights could not be run. \
         Download an accelerated build (`yolop-<target>-metal` or `-cuda`) from \
         https://github.com/everruns/yolop/releases/latest, or build with \
         `--features metal` / `--features cuda`."
    ))
}

fn run_mcp_command(command: McpCommand) -> Result<()> {
    use crate::config::mcp::{McpServerEntry, McpServerSummary};
    use everruns_core::{McpServerTransportType, ScopedMcpServer};
    use std::collections::HashMap;

    fn store(cwd: Option<PathBuf>) -> Result<McpConfigStore> {
        let workspace_root = match cwd {
            Some(path) => path,
            None => std::env::current_dir().context("resolve current workspace directory")?,
        };
        Ok(McpConfigStore::default_for_workspace(&workspace_root))
    }

    fn write_scope(scope: McpScopeArg) -> Result<McpConfigScope> {
        match scope {
            McpScopeArg::Global => Ok(McpConfigScope::Global),
            McpScopeArg::Workspace => Ok(McpConfigScope::Workspace),
            McpScopeArg::Effective => anyhow::bail!(
                "effective scope is read-only; use --scope global or --scope workspace"
            ),
        }
    }

    match command {
        McpCommand::List { scope, cwd } => {
            let store = store(cwd)?;
            let servers: Vec<McpServerSummary> = match scope {
                McpScopeArg::Global => store
                    .effective()
                    .map_err(anyhow::Error::msg)?
                    .servers
                    .into_iter()
                    .filter(|server| server.scope == McpConfigScope::Global)
                    .collect(),
                McpScopeArg::Workspace => store
                    .effective()
                    .map_err(anyhow::Error::msg)?
                    .servers
                    .into_iter()
                    .filter(|server| server.scope == McpConfigScope::Workspace)
                    .collect(),
                McpScopeArg::Effective => store
                    .effective()
                    .map_err(anyhow::Error::msg)?
                    .servers
                    .into_iter()
                    .filter(|server| server.effective)
                    .collect(),
            };
            if servers.is_empty() {
                println!("no MCP servers configured");
            } else {
                for server in servers {
                    let enabled = if server.enabled {
                        "enabled"
                    } else {
                        "disabled"
                    };
                    println!(
                        "{}	{}	{}",
                        server.name,
                        mcp_scope_label(server.scope),
                        enabled
                    );
                }
            }
            Ok(())
        }
        McpCommand::Add {
            scope,
            name,
            transport_type,
            command,
            args,
            url,
            headers,
            auth_mode,
            oauth_provider_id,
            disabled,
            cwd,
        } => {
            let transport = transport_type.to_ascii_lowercase();
            let server = match transport.as_str() {
                "stdio" => ScopedMcpServer {
                    transport_type: McpServerTransportType::Stdio,
                    command: Some(command.context("--command is required for stdio MCP servers")?),
                    args,
                    env: HashMap::new(),
                    ..ScopedMcpServer::default()
                },
                "http" | "sse" => ScopedMcpServer {
                    transport_type: McpServerTransportType::Http,
                    url: url.context("--url is required for remote MCP servers")?,
                    headers: headers.into_iter().collect(),
                    auth_mode: auth_mode
                        .as_deref()
                        .map(parse_mcp_auth_mode)
                        .transpose()?
                        .unwrap_or_default(),
                    oauth_provider_id,
                    ..ScopedMcpServer::default()
                },
                other => anyhow::bail!(
                    "unsupported MCP transport `{other}`; expected stdio, http, or sse"
                ),
            };
            let store = store(cwd)?;
            let _summary = store
                .upsert(
                    write_scope(scope)?,
                    &name,
                    McpServerEntry {
                        enabled: !disabled,
                        server,
                    },
                )
                .map_err(anyhow::Error::msg)?;
            let action = "saved";
            println!(
                "{action} MCP server `{name}` in {} scope",
                mcp_scope_label(write_scope(scope)?)
            );
            println!(
                "restart or start a new yolop session for MCP connection changes to take effect"
            );
            Ok(())
        }
        McpCommand::Remove { scope, name, cwd } => {
            let store = store(cwd)?;
            let removed = store
                .remove(write_scope(scope)?, &name)
                .map_err(anyhow::Error::msg)?;
            if removed {
                println!(
                    "removed MCP server `{name}` from {} scope",
                    mcp_scope_label(write_scope(scope)?)
                );
                println!(
                    "restart or start a new yolop session for MCP connection changes to take effect"
                );
            } else {
                println!(
                    "MCP server `{name}` was not configured in {} scope",
                    mcp_scope_label(write_scope(scope)?)
                );
            }
            Ok(())
        }
        McpCommand::Enable {
            scope,
            name,
            disable,
            cwd,
        } => {
            let store = store(cwd)?;
            store
                .set_enabled(write_scope(scope)?, &name, !disable)
                .map_err(anyhow::Error::msg)?;
            println!(
                "{} MCP server `{name}` in {} scope",
                if disable { "disabled" } else { "enabled" },
                mcp_scope_label(write_scope(scope)?)
            );
            println!(
                "restart or start a new yolop session for MCP connection changes to take effect"
            );
            Ok(())
        }
    }
}

fn mcp_scope_label(scope: McpConfigScope) -> &'static str {
    match scope {
        McpConfigScope::Global => "global",
        McpConfigScope::Workspace => "workspace",
    }
}

fn parse_mcp_auth_mode(value: &str) -> Result<everruns_core::McpServerAuthMode> {
    match value.to_ascii_lowercase().as_str() {
        "none" => Ok(everruns_core::McpServerAuthMode::None),
        "bearer" | "api_key" | "api-key" => Ok(everruns_core::McpServerAuthMode::ApiKey),
        "oauth" | "o_auth" => Ok(everruns_core::McpServerAuthMode::OAuth),
        other => anyhow::bail!(
            "unsupported auth mode `{other}`; expected none, bearer/api_key, or oauth"
        ),
    }
}

fn run_worktree_command(command: WorktreeCommand) -> Result<()> {
    match command {
        WorktreeCommand::List => {
            let paths = exec::worktree::list_worktree_paths_on_disk()?;
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
                None => runtime::session_log::default_sessions_dir()?,
            };
            let report = exec::worktree::prune_orphan_worktrees(&sessions_dir, dry_run)?;
            let action = if dry_run { "would remove" } else { "removed" };
            for path in &report.removed {
                println!("{action}: {}", path.display());
            }
            let ref_action = if dry_run { "would detach" } else { "detached" };
            for reference in &report.checkpoint_refs {
                println!("{ref_action} checkpoint ref: {reference}");
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

async fn run_command(command: Commands) -> Result<()> {
    match command {
        Commands::Version => {
            println!("{}", version::VERSION_LINE);
            Ok(())
        }
        Commands::Worktree(args) => run_worktree_command(args.command),
        Commands::Mcp(args) => run_mcp_command(args.command),
        Commands::Weights(args) => run_weights_command(args.command).await,
        Commands::TuikaGallery => run_tuika_gallery(),
        #[cfg(target_os = "linux")]
        Commands::SandboxExec {
            cwd,
            temp,
            mode,
            script,
        } => exec::sandbox::run_linux_worker(&cwd, &temp, mode, &script),
        Commands::Into(into) => match into.target {
            IntoTarget::Buzz(args) => {
                let command = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("yolop"));
                let result = editor::into::into_buzz(editor::into::BuzzIntoOptions {
                    harness_path: None,
                    agent_name: "yolop".to_string(),
                    command,
                    force: args.force,
                })?;
                match result.status {
                    editor::into::IntoStatus::Unchanged => println!(
                        "yolop: Buzz already has `{}` configured at {}",
                        result.agent_name,
                        result.harness_path.display()
                    ),
                    editor::into::IntoStatus::Created => println!(
                        "yolop: added `{}` custom harness to {}",
                        result.agent_name,
                        result.harness_path.display()
                    ),
                    editor::into::IntoStatus::Updated => println!(
                        "yolop: updated `{}` custom harness in {}",
                        result.agent_name,
                        result.harness_path.display()
                    ),
                }
                println!("yolop: Buzz command: {} --acp", result.command);
                println!("yolop: restart Buzz Desktop to reload this configuration");
                Ok(())
            }
            IntoTarget::Paseo(args) => {
                let command = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("yolop"));
                let result = editor::into::into_paseo(editor::into::PaseoIntoOptions {
                    settings_path: None,
                    agent_name: "yolop".to_string(),
                    command,
                    force: args.force,
                })?;
                match result.status {
                    editor::into::IntoStatus::Unchanged => {
                        println!(
                            "yolop: Paseo already has `{}` ACP provider configured at {}",
                            result.agent_name,
                            result.settings_path.display()
                        );
                    }
                    editor::into::IntoStatus::Created => {
                        println!(
                            "yolop: added `{}` ACP provider to {}",
                            result.agent_name,
                            result.settings_path.display()
                        );
                    }
                    editor::into::IntoStatus::Updated => {
                        println!(
                            "yolop: updated `{}` ACP provider in {}",
                            result.agent_name,
                            result.settings_path.display()
                        );
                    }
                }
                println!("yolop: Paseo command: {} --acp", result.command);
                println!("yolop: restart the Paseo daemon to reload this configuration");
                Ok(())
            }
            IntoTarget::Zed(args) => {
                let command = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("yolop"));
                let result = editor::into::into_zed(editor::into::ZedIntoOptions {
                    settings_path: None,
                    agent_name: "yolop".to_string(),
                    command,
                    force: args.force,
                })?;
                match result.status {
                    editor::into::IntoStatus::Unchanged => {
                        println!(
                            "yolop: Zed already has `{}` configured at {}",
                            result.agent_name,
                            result.settings_path.display()
                        );
                    }
                    editor::into::IntoStatus::Created => {
                        println!(
                            "yolop: added `{}` ACP agent to {}",
                            result.agent_name,
                            result.settings_path.display()
                        );
                    }
                    editor::into::IntoStatus::Updated => {
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

fn detached_cli_registry() -> Result<control::CliRegistry> {
    // CLI metadata must remain available even in stripped-down environments
    // with no platform config directory. Operations against these fallbacks
    // still fail visibly if they need to persist state.
    let fallback = PathBuf::from("/dev/null/yolop");
    let settings_path =
        config::default_settings_path().unwrap_or_else(|| fallback.join("settings.toml"));
    let extensions_dir =
        extensions::extensions_dir().unwrap_or_else(|| fallback.join("extensions"));
    let connections_path =
        connectors::default_connections_path().unwrap_or_else(|| fallback.join("connections.toml"));
    let settings = Arc::new(SettingsStore::open(settings_path));
    let model_list_settings = settings.clone();
    let secrets = extensions::ExtensionSecrets::new(Arc::new(connectors::ConnectionStore::open(
        connections_path,
    )));
    let workspace = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let capability = Arc::new(
        extensions::ExtensionsCapability::new(
            extensions_dir,
            workspace,
            settings,
            extensions::LiveProcessRegistry::default(),
            None,
        )
        .with_secrets(secrets)
        .with_ask_sink(Some(terminal_extension_ask_sink())),
    );
    let mut registry = control::CliRegistry::default();
    registry.register(capability)?;
    // `yolop models` edits the model list. Detached it writes global settings
    // like any other CLI invocation; `use` needs a session and says so.
    registry.register(Arc::new(capabilities::ModelListCapability::new(
        model_list_settings,
        None,
    )))?;
    let coordination_store = match runtime::session_log::default_sessions_dir()
        .ok()
        .and_then(|dir| capabilities::CoordinationStore::open(&dir).ok())
    {
        Some(store) => store,
        None => capabilities::CoordinationStore::new(everruns::local::SqliteDb::open_in_memory()?)?,
    };
    registry.register(Arc::new(
        capabilities::SessionCoordinationCapability::detached(coordination_store),
    ))?;
    Ok(registry)
}

fn terminal_extension_ask_sink() -> extensions::AskSink {
    Arc::new(|params| {
        Box::pin(async move {
            tokio::task::spawn_blocking(move || terminal_extension_ask(params))
                .await
                .unwrap_or_else(|_| extensions::protocol::UiAskResult {
                    answer: String::new(),
                    cancelled: true,
                })
        })
    })
}

fn terminal_extension_ask(
    params: extensions::protocol::UiAskParams,
) -> extensions::protocol::UiAskResult {
    use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers, read};

    if !io::stdin().is_terminal() || !io::stderr().is_terminal() {
        eprintln!("yolop: interactive extension setup requires a terminal");
        return extensions::protocol::UiAskResult {
            answer: String::new(),
            cancelled: true,
        };
    }
    eprint!("{}", params.prompt);
    if !params.options.is_empty() {
        eprint!(" [{}]", params.options.join("/"));
    }
    eprint!(": ");
    let _ = io::stderr().flush();

    if !params.secret {
        let mut answer = String::new();
        let cancelled = io::stdin().read_line(&mut answer).is_err();
        return extensions::protocol::UiAskResult {
            answer: answer.trim().to_string(),
            cancelled,
        };
    }

    struct RawModeGuard;
    impl Drop for RawModeGuard {
        fn drop(&mut self) {
            let _ = disable_raw_mode();
        }
    }
    if enable_raw_mode().is_err() {
        eprintln!("could not enable masked terminal input");
        return extensions::protocol::UiAskResult {
            answer: String::new(),
            cancelled: true,
        };
    }
    let _guard = RawModeGuard;
    let mut answer = String::new();
    loop {
        let Ok(Event::Key(key)) = read() else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match key.code {
            KeyCode::Enter => {
                eprintln!();
                return extensions::protocol::UiAskResult {
                    answer,
                    cancelled: false,
                };
            }
            KeyCode::Esc => {
                eprintln!();
                return extensions::protocol::UiAskResult {
                    answer: String::new(),
                    cancelled: true,
                };
            }
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                eprintln!();
                return extensions::protocol::UiAskResult {
                    answer: String::new(),
                    cancelled: true,
                };
            }
            KeyCode::Backspace => {
                if answer.pop().is_some() {
                    eprint!("\u{8} \u{8}");
                    let _ = io::stderr().flush();
                }
            }
            KeyCode::Char(ch) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                answer.push(ch);
                eprint!("*");
                let _ = io::stderr().flush();
            }
            _ => {}
        }
    }
}

/// The OSC 8 hyperlink policy from the environment.
///
/// - `YOLOP_HYPERLINKS=0` / `false` / `off` — force off
/// - `YOLOP_HYPERLINKS=1` / `true` / `on` — force on (`http(s)`)
/// - unset — **auto**: on when [`Capabilities`] reports hyperlink support
///   (Ghostty, Kitty, WezTerm, iTerm2, …), off otherwise
///
/// When on, `YOLOP_HYPERLINK_MAILTO=1` also opts `mailto:` links in.
pub(crate) fn hyperlink_policy() -> LinkPolicy {
    let raw = std::env::var("YOLOP_HYPERLINKS").ok();
    let enabled = match raw.as_deref().map(str::trim) {
        Some("0") | Some("false") | Some("off") => false,
        Some("1") | Some("true") | Some("on") => true,
        // Auto: trust capability detection so Ghostty/etc. get clickable links
        // without a manual env knob — the third-time regression was leaving this
        // off by default while the UI still *looked* linked.
        None | Some("") | Some("auto") => Capabilities::from_env().hyperlinks,
        Some(_) => Capabilities::from_env().hyperlinks,
    };
    if !enabled {
        return LinkPolicy::NONE;
    }
    let mut policy = LinkPolicy::WEB;
    let mailto = std::env::var("YOLOP_HYPERLINK_MAILTO")
        .map(|v| matches!(v.as_str(), "1" | "true" | "on"))
        .unwrap_or(false);
    if mailto {
        policy = policy.with_mailto();
    }
    policy
}

/// Live demo of the `tuika` motion components. Renders spinners, progress bars,
/// and a loader on the alternate screen, and drives the terminal's native
/// OSC 9;4 progress indicator while running. Quits on `q`/`Esc`/`Ctrl-C`.
fn run_tuika_gallery() -> Result<()> {
    use std::time::Duration;

    // Route through the same hyperlink-aware backend as the main TUI so the
    // demo's URL becomes a clickable OSC 8 link when YOLOP_HYPERLINKS is set.
    let backend = HyperlinkBackend::with_policy(io::stdout(), hyperlink_policy());
    let theme = tuika::Theme::default();
    let mut progress = tuika::term::progress::TerminalProgress::new();
    progress.indeterminate();
    let runner = tuika::Runner::new(tuika::RunnerConfig {
        tick_rate: Duration::from_millis(80),
        // The gallery owns the whole terminal; `ScreenMode::Alternate` is the
        // default, and split-footer mode is for hosts that publish scrollback.
        ..tuika::RunnerConfig::default()
    });
    // The gallery has no state of its own, so the closure seam (`from_fn`) is
    // cheaper than naming an `Application` type for a unit state.
    let mut state = ();
    runner.run_with_backend(
        &theme,
        backend,
        tuika::runner::from_fn(
            &mut state,
            |_state, frame| build_gallery(frame, &theme),
            |_state, signal| match signal {
                tuika::Signal::Event(tuika::Event::Key(key))
                    if matches!(key.code, tuika::KeyCode::Esc | tuika::KeyCode::Char('q'))
                        || (key.ctrl && matches!(key.code, tuika::KeyCode::Char('c'))) =>
                {
                    tuika::UpdateResult::Exit
                }
                _ => tuika::UpdateResult::Dirty,
            },
        ),
    )?;
    progress.clear();
    Ok(())
}

/// Build the gallery view tree for `frame`, using [`ratatui::text`] helpers via
/// `tuika` components.
fn build_gallery(frame: u64, theme: &tuika::Theme) -> tuika::Element {
    use ratatui::style::Modifier;
    use ratatui::text::{Line, Span};
    use tuika::components::{Loader, MarkdownState, ProgressBar, Spinner, SpinnerStyle, Text};
    use tuika::highlight::CodeHighlighter;

    // The whole demo is expressed with the declarative `view!` DSL. Leaf and
    // third-party components (Spinner, ProgressBar, Loader, Text) enter through
    // `node(expr)`; layout is the `col`/`row`/`boxed`/`fixed`/`grow` sugar.
    let labeled_spinner = |style: SpinnerStyle, label: &str| -> tuika::Element {
        tuika::view! {
            row(gap = 1) {
                fixed(1) { node(Spinner::new(frame).style(style)) }
                text(label.to_string())
            }
        }
    };

    let animated = tuika::anim::ping_pong(frame, 120);

    // Markdown + syntax-highlighted code — the same renderer that formats
    // assistant replies. (Rendered whole here; `MarkdownState` also streams
    // deltas incrementally — see the `markdown` example in the tuika crate.)
    let md_doc = "Highlighted `code` in **markdown**:\n\n```rust\nfn fib(n: u64) -> u64 { n }\n```";
    let highlighter = tuika_codeformatters::TreeSitterHighlighter::new();
    let sheet = tuika::StyleSheet::from_theme(theme);
    let mut markdown = MarkdownState::new();
    markdown.set(md_doc);
    let markdown_lines = markdown
        .lines(46, theme, &sheet, CodeHighlighter::With(&highlighter))
        .to_vec();

    tuika::view! {
        col(
            background = ratatui::style::Style::default().bg(theme.background),
            padding = tuika::Padding::all(1),
            gap = 1
        ) {
            fixed(5) {
                boxed(title = Line::from(Span::styled(" spinners ", theme.accent_style()))) {
                    col {
                        fixed(1) { node(labeled_spinner(SpinnerStyle::Braille, "Braille")) }
                        fixed(1) { node(labeled_spinner(SpinnerStyle::Line, "Line")) }
                        fixed(1) { node(labeled_spinner(SpinnerStyle::Dots, "Dots")) }
                    }
                }
            }
            fixed(6) {
                boxed(title = Line::from(Span::styled(" progress ", theme.accent_style()))) {
                    col {
                        fixed(1) { node(ProgressBar::determinate(0.25).percent(true)) }
                        fixed(1) { node(ProgressBar::determinate(0.60).percent(true)) }
                        fixed(1) { node(ProgressBar::determinate(animated).percent(true)) }
                        fixed(1) { node(ProgressBar::indeterminate(frame)) }
                    }
                }
            }
            fixed(3) {
                boxed(title = Line::from(Span::styled(" loader ", theme.accent_style()))) {
                    node(Loader::new(frame, "working…").hint("esc to quit"))
                }
            }
            // Takes the leftover space (replacing the old spacer) so the footer
            // below always renders; on a short viewport it simply shows fewer
            // lines rather than pushing the footer off-screen.
            grow(1) {
                boxed(title = Line::from(Span::styled(" markdown + code ", theme.accent_style()))) {
                    node(Text::new(markdown_lines))
                }
            }
            fixed(1) {
                node(Text::new(vec![Line::from(vec![
                    Span::styled("docs ", theme.muted_style()),
                    Span::styled(
                        "https://github.com/everruns/yolop",
                        theme.accent_style().add_modifier(Modifier::UNDERLINED),
                    ),
                    Span::styled(
                        "  ·  native progress is live · press q to quit",
                        theme.muted_style(),
                    ),
                ])]))
            }
        }
    }
}

async fn run_tui(
    runtime: BuiltRuntime,
    pending_images: Vec<ContentPart>,
    trajectory_out: Option<PathBuf>,
    fullscreen: bool,
    compact_work: bool,
) -> Result<()> {
    // Cheap Arc clones taken before `App` consumes the runtime, so the
    // trajectory can be exported after the TUI loop ends.
    let trajectory_handles = runtime.handles.clone();
    let trajectory_model = runtime.model.clone();
    let mut raw_mode = RawModeGuard::new()?;
    // Which part of the terminal the renderer owns, in tuika's terms: the whole
    // alternate screen, or a footer pinned to the bottom of the main screen with
    // the transcript published above it as ordinary scrollback. `ScreenMode`
    // then decides the viewport, the pinning, and the teardown below, so the
    // two renderers differ in one value rather than in scattered `if fullscreen`
    // branches.
    let screen_mode = if fullscreen {
        tuika::ScreenMode::Alternate
    } else {
        // Mouse capture stays off (the `ScreenMode::split_footer` default): on
        // the main screen it would take the wheel and drag-selection away from
        // the terminal, which is the scrollback interaction this mode exists to
        // preserve.
        tuika::ScreenMode::split_footer(COMPOSER_VIEWPORT_HEIGHT)
    };
    // In full-screen mode `tuika` owns the alternate screen + mouse capture via
    // an RAII guard that restores them on drop.
    let mut alt_screen = if screen_mode.is_alternate() {
        Some(tuika::host::AltScreen::enter_with_mouse_capture()?)
    } else {
        None
    };
    // Kitty keyboard modes are screen-local, so enable them only after entering
    // the alternate screen and restore them before leaving it.
    let mut keyboard_enhancements = KeyboardEnhancementGuard::new();
    let mut bracketed_paste = BracketedPasteGuard::new();
    let stdout = io::stdout();
    // Opt-in OSC 8 hyperlinks: wrap the crossterm backend so http(s) (and, with
    // YOLOP_HYPERLINK_MAILTO, mailto) URLs in rendered output become clickable.
    // OSC 8 hyperlinks via HyperlinkBackend — see `hyperlink_policy` (auto-on
    // for Ghostty and other known OSC 8 terminals; override with YOLOP_HYPERLINKS).
    let backend = HyperlinkBackend::with_policy(stdout, hyperlink_policy());
    let mut terminal = Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: screen_mode.viewport(),
        },
    )?;
    // Pinning is cosmetic and split-footer-only. Since ratatui 0.30.1
    // `insert_before` snapshots the cursor (via `Terminal::clear`) with a
    // blocking `CSI 6n` query that slow emulators (ttyd / xterm.js) may not
    // answer before crossterm's ~2s timeout. Start unpinned rather than dying.
    if !screen_mode.is_alternate()
        && let Err(err) = tuika::screen::pin_footer(&mut terminal)
    {
        tracing::warn!("footer pinning failed, starting unpinned: {err:#}");
    }

    let mut app = if compact_work {
        App::new_with_work_display(runtime, pending_images, tui::WorkDisplayMode::Compact)
    } else {
        App::new(runtime, pending_images)
    };
    app.enable_native_progress();
    if fullscreen {
        app.set_render_mode(tui::RenderMode::Fullscreen);
    }
    let result = app.run(&mut terminal).await;
    let show_resume_hint = app.should_show_resume_hint();
    let session_id = app.session_id().to_string();
    let last_assistant_message = app.last_assistant_message().map(str::to_owned);

    // Hand the footer's rows back to the terminal so the shell prompt resumes
    // directly below the published transcript instead of after — or on top of —
    // the last frame. Against the full-screen viewport this degrades to a plain
    // clear, which is what that mode wants anyway.
    //
    // Cosmetic cleanup must not turn a successful session into an error exit:
    // since ratatui 0.30.1 `Terminal::clear` (which `close_footer` performs)
    // issues the same blocking cursor query as pinning above. The two steps are
    // independent — restoring the cursor is a plain escape write that should
    // still happen when the clear's query times out. Raw-mode restore below
    // still fails hard — leaving the terminal unusable is worth a nonzero exit.
    if let Err(err) = tuika::screen::close_footer(&mut terminal) {
        tracing::warn!("footer teardown failed: {err:#}");
    }
    if let Err(err) = terminal.show_cursor() {
        tracing::warn!("cursor restore failed: {err:#}");
    }
    drop(terminal);
    bracketed_paste.disable();
    keyboard_enhancements.disable();
    // Leave the alternate screen before any post-loop stdout (resume hint) so
    // it lands on the user's normal screen, not the one about to be torn down.
    if let Some(alt) = alt_screen.as_mut() {
        alt.leave();
    }
    raw_mode.disable()?;

    write_trajectory_if_requested(
        &trajectory_handles,
        &trajectory_model,
        trajectory_out.as_deref(),
    )
    .await;
    // Same teardown contract as `--print`: let the trace extensions export the
    // session's final events before their servers are killed.
    trajectory_handles.flush_trace_exporters().await;

    if show_resume_hint {
        println!();
        if fullscreen && let Some(message) = last_assistant_message {
            println!("{message}\n");
        }
        print_resume_divider();
        println!(
            "Continue with {}",
            continuation_command(std::env::args_os(), &session_id)
        );
        println!();
        print_centered_ukraine_banner();
    }
    result
}

fn continuation_command(
    args: impl IntoIterator<Item = std::ffi::OsString>,
    session_id: &str,
) -> String {
    let mut args = args.into_iter();
    let _executable = args.next();
    let mut preserved = Vec::new();
    let mut skip_value = false;

    for arg in args {
        if skip_value {
            skip_value = false;
            continue;
        }
        let value = arg.to_string_lossy();
        if matches!(
            value.as_ref(),
            "-p" | "--print" | "-i" | "--image" | "--session" | "--trajectory-out"
        ) {
            skip_value = true;
            continue;
        }
        if value.starts_with("--print=")
            || value.starts_with("--image=")
            || value.starts_with("--session=")
            || value.starts_with("--trajectory-out=")
        {
            continue;
        }
        preserved.push(shell_quote(&value));
    }

    preserved.push("--session".to_string());
    preserved.push(shell_quote(session_id));
    format!("yolop {}", preserved.join(" "))
}

fn shell_quote(value: &str) -> String {
    if !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_@%+=:,./-".contains(&byte))
    {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', "'\"'\"'"))
    }
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

struct BracketedPasteGuard {
    active: bool,
}

impl BracketedPasteGuard {
    fn new() -> Self {
        let mut stdout = io::stdout();
        let active = execute!(stdout, EnableBracketedPaste).is_ok();
        Self { active }
    }

    fn disable(&mut self) {
        if self.active {
            let mut stdout = io::stdout();
            let _ = execute!(stdout, DisableBracketedPaste);
            self.active = false;
        }
    }
}

impl Drop for BracketedPasteGuard {
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

/// Export the session as an ATIF trajectory when `--trajectory-out` was
/// given. Best-effort: export problems are reported on stderr and never turn
/// a finished run into a failure.
async fn write_trajectory_if_requested(
    handles: &runtime::RuntimeHandles,
    model: &runtime::ModelState,
    path: Option<&Path>,
) {
    let Some(path) = path else { return };
    let events = match handles.runtime.events().await {
        Ok(events) => events,
        Err(err) => {
            eprintln!("yolop: trajectory export failed to read session events: {err}");
            return;
        }
    };
    let trajectory = session_state::atif::trajectory_from_events(
        session_state::atif::AgentInfo {
            name: "yolop".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            model_name: Some(model.model_id()),
        },
        handles.session_id,
        &events,
    );
    if let Err(err) = session_state::atif::write_trajectory_file(path, &trajectory) {
        eprintln!(
            "yolop: failed to write trajectory to {}: {err}",
            path.display()
        );
    }
}

async fn run_print_mode(
    runtime: BuiltRuntime,
    prompt: String,
    images: Vec<ContentPart>,
    trajectory_out: Option<PathBuf>,
) -> Result<()> {
    let BuiltRuntime {
        handles,
        startup,
        model,
        goal_store,
        user_ask_store,
        user_ask_enabled,
        task_registry,
        worktree,
        ..
    } = runtime;
    let color = io::stdout().is_terminal();
    if let Err(err) = worktree.ensure_before_turn(prompt.trim()) {
        eprintln!("worktree: {err}");
    }
    let _ = (&startup.session_dir, &startup.session_log_path);

    let trimmed = prompt.trim();
    if let Some(goal_args) = trimmed.strip_prefix("/goal") {
        // `run_print_goal` reports failure as a flag (not `process::exit`)
        // so the trajectory export still runs on failed goal runs.
        let success = run_print_goal(
            &handles,
            &worktree,
            &model,
            &goal_store,
            goal_args.trim(),
            color,
        )
        .await?;
        write_trajectory_if_requested(&handles, &model, trajectory_out.as_deref()).await;
        if !success {
            std::process::exit(1);
        }
        return Ok(());
    }

    if user_ask_enabled
        && let Err(err) = user_ask_store.record_user_prompt(handles.session_id, trimmed)
    {
        eprintln!("user ask: {err}");
    }

    let mut prompt = trimmed.to_string();
    let mut turn_images = images;
    let mut automatic = false;
    let mut budget = session_state::task_completion::CompletionBudget::default();
    loop {
        let turn =
            match collect_print_turn(&handles, &worktree, &model, &prompt, turn_images, automatic)
                .await
            {
                Ok(turn) => turn,
                Err(err) => {
                    if user_ask_enabled && user_ask_store.is_active(handles.session_id) {
                        let evaluation = session_state::task_completion::evaluation_for_state(
                            session_state::task_completion::CompletionState::Failed,
                        );
                        user_ask_store.record_evaluation(handles.session_id, &evaluation)?;
                    }
                    return Err(err);
                }
            };
        print_final_output(&turn.output);
        if !turn.result.success {
            if user_ask_enabled && user_ask_store.is_active(handles.session_id) {
                let evaluation = session_state::task_completion::evaluation_for_state(
                    session_state::task_completion::CompletionState::Failed,
                );
                user_ask_store.record_evaluation(handles.session_id, &evaluation)?;
                if let Some(message) =
                    session_state::user_ask::evaluation_status_message(&evaluation)
                {
                    eprintln!("{message}");
                }
            }
            write_trajectory_if_requested(&handles, &model, trajectory_out.as_deref()).await;
            std::process::exit(1);
        }
        if !user_ask_enabled || !user_ask_store.is_active(handles.session_id) {
            break;
        }

        let tokens = handles.turn_tokens(turn.result.turn_id).await;
        if !budget.observe_turn(tokens) {
            eprintln!("user ask budget exhausted; use --session to resume");
            break;
        }
        let has_background = task_registry
            .list(handles.session_id, None)
            .await
            .unwrap_or_default()
            .iter()
            .any(|task| !task.state.is_terminal());
        let (outcome, reason) =
            match session_state::task_completion::gate_turn(&turn.result, has_background) {
                session_state::task_completion::GateDecision::Conclusive(state) => {
                    let evaluation = session_state::task_completion::evaluation_for_state(state);
                    user_ask_store.record_evaluation(handles.session_id, &evaluation)?;
                    (evaluation.outcome, evaluation.reason)
                }
                session_state::task_completion::GateDecision::Evaluate => {
                    let evaluation = handles
                        .runtime
                        .execute_command(
                            handles.session_id,
                            ExecuteCommandRequest {
                                name: "ask".to_string(),
                                arguments: Some(
                                    session_state::user_ask::USER_ASK_EVALUATE_ARG.to_string(),
                                ),
                                controls: None,
                            },
                        )
                        .await?;
                    if !evaluation.success {
                        eprintln!("user ask evaluation failed: {}", evaluation.message);
                        break;
                    }
                    let parsed =
                        session_state::user_ask::parse_evaluation_response(&evaluation.message)?;
                    (parsed.outcome, parsed.reason)
                }
            };

        match outcome {
            session_state::user_ask::AskOutcome::InProgress => {
                prompt = session_state::task_completion::continuation_prompt(&reason);
                turn_images = Vec::new();
                automatic = true;
            }
            session_state::user_ask::AskOutcome::Blocked => {
                handles.report_herdr_state(capabilities::herdr::HerdrState::Blocked);
                break;
            }
            session_state::user_ask::AskOutcome::Achieved
            | session_state::user_ask::AskOutcome::Failed
            | session_state::user_ask::AskOutcome::WaitingOnBackground => break,
        }
    }
    write_trajectory_if_requested(&handles, &model, trajectory_out.as_deref()).await;
    // Let the trace extensions export the turn's final events before the
    // process exits and their servers are killed.
    handles.flush_trace_exporters().await;
    Ok(())
}

/// Returns `Ok(false)` on goal/turn failure instead of exiting so the caller
/// can finish end-of-run work (trajectory export) before setting the exit code.
async fn run_print_goal(
    handles: &runtime::RuntimeHandles,
    worktree: &crate::exec::worktree::WorktreeManager,
    model: &runtime::ModelState,
    goal_store: &session_state::goal::GoalStore,
    arguments: &str,
    color: bool,
) -> Result<bool> {
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
    if !result.success {
        eprintln!("goal command failed: {}", result.message);
        return Ok(false);
    }

    if !goal_store.take_pending_turn(session_id) {
        if !result.message.is_empty() {
            println!("{}", paint(color, "90", &result.message));
        }
        return Ok(true);
    }

    let Some(mut turn_prompt) = goal_store.active_condition(session_id) else {
        return Ok(true);
    };

    loop {
        let turn =
            collect_print_turn(handles, worktree, model, &turn_prompt, vec![], false).await?;
        if !turn.result.success {
            print_final_output(&turn.output);
            return Ok(false);
        }
        if !goal_store.is_active(session_id) {
            print_final_output(&turn.output);
            return Ok(true);
        }

        let evaluation = handles
            .runtime
            .execute_command(
                session_id,
                ExecuteCommandRequest {
                    name: "goal".to_string(),
                    arguments: Some(session_state::goal::GOAL_EVALUATE_ARG.to_string()),
                    controls: None,
                },
            )
            .await?;
        if !evaluation.success {
            eprintln!("goal evaluation failed: {}", evaluation.message);
            return Ok(false);
        }
        let parsed = session_state::goal::parse_evaluation_response(&evaluation.message)?;
        if parsed.met {
            print_final_output(&turn.output);
            return Ok(true);
        }
        turn_prompt = goal_store
            .continuation_prompt(session_id)
            .unwrap_or_else(|| turn_prompt.clone());
    }
}

struct PrintTurn {
    result: everruns_host::TurnResult,
    output: Vec<String>,
}

async fn collect_print_turn(
    handles: &runtime::RuntimeHandles,
    worktree: &crate::exec::worktree::WorktreeManager,
    model: &runtime::ModelState,
    prompt: &str,
    images: Vec<ContentPart>,
    automatic: bool,
) -> Result<PrintTurn> {
    if let Err(err) = worktree.ensure_before_turn(prompt) {
        eprintln!("worktree: {err}");
    }
    let before_msgs = handles
        .runtime
        .messages(handles.session_id)
        .await
        .map(|m| m.len())
        .unwrap_or(0);

    let mut input = model.input_message_with_images(prompt, images);
    if automatic {
        input = session_state::task_completion::tag_continuation(input);
    }
    let result = handles.run_checkpointed_turn(prompt, input).await?;
    if let Some(notice) = handles.checkpoints.take_notice() {
        eprintln!("{notice}");
    }
    let messages = handles
        .runtime
        .messages(handles.session_id)
        .await
        .unwrap_or_default();

    let mut output = Vec::new();
    for msg in messages.iter().skip(before_msgs) {
        if msg.role == MessageRole::Agent
            && !msg.has_tool_calls()
            && let Some(text) = msg.text()
        {
            let t = text.trim();
            if !t.is_empty() {
                output.push(t.to_string());
            }
        }
    }
    if !result.success
        && let Some(err) = &result.error
    {
        eprintln!("turn error: {err}");
    }
    Ok(PrintTurn { result, output })
}

fn print_final_output(output: &[String]) {
    for (index, text) in output.iter().enumerate() {
        if index > 0 {
            println!();
        }
        println!("{text}");
    }
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
    use std::ffi::OsString;
    use std::process::Command;

    #[test]
    fn trace_routing_follows_terminal_ownership() {
        let tui = Cli::try_parse_from(["yolop", "--provider", "llmsim"]).expect("parse TUI");
        let gallery = Cli::try_parse_from(["yolop", "tuika-gallery"]).expect("parse TUI gallery");
        let print = Cli::try_parse_from(["yolop", "--provider", "llmsim", "-p", "hi"])
            .expect("parse print mode");
        let command = Cli::try_parse_from(["yolop", "version"]).expect("parse command");

        assert!(uses_interactive_renderer(&tui));
        assert!(uses_interactive_renderer(&gallery));
        assert!(!uses_interactive_renderer(&print));
        assert!(!uses_interactive_renderer(&command));
    }

    #[test]
    fn compact_work_is_fullscreen_only() {
        let compact =
            Cli::try_parse_from(["yolop", "--compact-work"]).expect("compact fullscreen TUI");
        assert!(compact.compact_work);
        assert!(Cli::try_parse_from(["yolop", "--compact-work", "--inline"]).is_err());
        assert!(Cli::try_parse_from(["yolop", "--compact-work", "-p", "hi"]).is_err());
    }

    #[test]
    fn interactive_trace_logs_are_private_and_bounded() {
        let dir = tempfile::tempdir().expect("trace tempdir");
        for index in 0..MAX_INTERACTIVE_TRACE_LOGS {
            std::fs::write(
                dir.path().join(format!("2026-08-14-{index:02}-trace.log")),
                b"old",
            )
            .expect("write old trace log");
        }

        let (file, path) =
            open_interactive_trace_log_at(dir.path(), "2026-08-15T07-00-49.403905Z", 42)
                .expect("open interactive trace log");
        drop(file);

        let trace_logs = std::fs::read_dir(dir.path())
            .expect("read trace directory")
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.ends_with("-trace.log"))
            })
            .collect::<Vec<_>>();
        assert_eq!(trace_logs.len(), MAX_INTERACTIVE_TRACE_LOGS);
        assert!(path.exists());
        assert!(!dir.path().join("2026-08-14-00-trace.log").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(dir.path())
                    .expect("trace directory metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
            assert_eq!(
                std::fs::metadata(path)
                    .expect("trace metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn interactive_trace_log_stops_growing_at_its_byte_limit() {
        let dir = tempfile::tempdir().expect("trace tempdir");
        let (file, path) =
            open_interactive_trace_log_at(dir.path(), "2026-08-15T07-00-49.403905Z", 43)
                .expect("open interactive trace log");
        let mut writer = BoundedTraceWriter::new(file, 4);

        writer
            .write_all(b"warning one")
            .expect("write first warning");
        writer
            .write_all(b"warning two")
            .expect("discard warning past cap");
        writer.flush().expect("flush trace log");

        assert_eq!(std::fs::read(path).expect("read trace log"), b"warn");
    }

    #[test]
    fn worker_join_preserves_original_panic_payload() {
        let reporter = crash_report::CrashReporter::disabled();
        let worker = std::thread::spawn(|| panic!("original worker panic"));

        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            join_worker(worker, &reporter)
        }))
        .expect_err("worker panic should propagate");
        assert_eq!(
            panic.downcast_ref::<&str>().copied(),
            Some("original worker panic")
        );
    }

    #[test]
    fn worker_join_prints_session_id() {
        let output = Command::new(std::env::current_exe().expect("test executable"))
            .args([
                "--exact",
                "tests::worker_join_prints_session_id_child",
                "--nocapture",
            ])
            .env("YOLOP_TEST_WORKER_CRASH", "1")
            .output()
            .expect("run worker crash child");

        assert!(!output.status.success(), "worker crash child should fail");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("yolop: crashed session id: session_worker_crash_test"),
            "missing session id diagnostic: {stderr}"
        );
    }

    #[test]
    fn worker_join_prints_session_id_child() {
        if std::env::var_os("YOLOP_TEST_WORKER_CRASH").is_none() {
            return;
        }
        let reporter = crash_report::CrashReporter::disabled();
        reporter.set_session_id("session_worker_crash_test");
        let worker = std::thread::spawn(|| panic!("worker-crash-test"));
        join_worker(worker, &reporter);
    }

    #[test]
    fn continuation_preserves_reusable_arguments_and_replaces_turn_arguments() {
        let args = [
            "yolop",
            "--inline",
            "--sandbox",
            "--model",
            "gpt test",
            "--print",
            "do not repeat",
            "--image=initial.png",
            "--trajectory-out",
            "old.json",
            "--session=old-session",
        ]
        .into_iter()
        .map(OsString::from);

        assert_eq!(
            continuation_command(args, "new-session"),
            "yolop --inline --sandbox --model 'gpt test' --session new-session"
        );
    }

    #[test]
    fn continuation_preserves_compact_work_mode() {
        let args = ["yolop", "--compact-work", "--session=old-session"]
            .into_iter()
            .map(OsString::from);

        assert_eq!(
            continuation_command(args, "new-session"),
            "yolop --compact-work --session new-session"
        );
    }

    fn cli_with_reasoning_effort(reasoning_effort: Option<&str>) -> Cli {
        Cli {
            command: None,
            attached_control_child: false,
            cwd: None,
            provider: Some(ProviderArg::Openrouter),
            profile: None,
            model: Some("nvidia/nemotron-3-super-120b-a12b".to_string()),
            reasoning_effort: reasoning_effort.map(str::to_string),
            print: None,
            images: vec![],
            acp: false,
            config_dir: None,
            data_dir: None,
            acp_setup_page: false,
            session: None,
            session_dir: None,
            trajectory_out: None,
            inline: false,
            compact_work: false,
            theme: None,
            sandbox: false,
        }
    }

    #[test]
    fn a_build_without_the_engine_rejects_the_local_provider_up_front() {
        let local = ProviderChoice::Local {
            model: "Qwen/Qwen3-8B".to_string(),
        };
        let result = ensure_provider_is_built_in(&local);

        if cfg!(feature = "local-inference") {
            assert!(result.is_ok(), "an engine build must accept `local`");
        } else {
            // The point is the message: the registry's own failure names an
            // internal driver id and offers the user no way forward.
            let err = result.expect_err("a build without the engine must reject `local`");
            let message = err.to_string();
            // The default release binary and Homebrew both ship without the
            // engine, so pointing there would send the user in a circle. The
            // accelerated download is the only prebuilt answer.
            assert!(message.contains("releases/latest"), "{message}");
            assert!(message.contains("--features metal"), "{message}");
            assert!(!message.contains("brew install"), "{message}");
        }
    }

    #[test]
    fn an_empty_weights_store_only_suggests_pulling_when_something_could_run_it() {
        let hint = empty_weights_store_hint();

        if cfg!(feature = "local-inference") {
            assert!(hint.contains("yolop weights pull"), "{hint}");
        } else {
            // `weights pull` is compiled out here, so suggesting it would send
            // the reader to a command that only errors.
            assert!(!hint.contains("yolop weights pull"), "{hint}");
            assert!(hint.contains("releases/latest"), "{hint}");
        }
    }

    #[test]
    fn providers_other_than_local_are_never_gated_on_the_engine_feature() {
        for provider in [
            ProviderChoice::Sim,
            ProviderChoice::default_for_provider_name("openai").expect("openai"),
        ] {
            assert!(ensure_provider_is_built_in(&provider).is_ok());
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
        let _guard = crate::testing::test_env::lock();
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
            attached_control_child: false,
            cwd: None,
            provider: None,
            profile: None,
            model: None,
            reasoning_effort: None,
            print: None,
            images: vec![],
            acp: false,
            config_dir: None,
            data_dir: None,
            acp_setup_page: false,
            session: None,
            session_dir: None,
            trajectory_out: None,
            inline: false,
            compact_work: false,
            theme: None,
            sandbox: false,
        };

        let (provider, _notes) = pick_provider(&cli, &settings);

        assert_eq!(provider.label(), "openai/gpt-5.4 high");
    }

    #[test]
    fn resolve_workspace_root_uses_saved_session_workspace() {
        let sessions = tempfile::tempdir().expect("sessions tempdir");
        let workspace = tempfile::tempdir().expect("workspace tempdir");
        let session_id = SessionId::from_seed(42);
        let session_dir = runtime::session_log::session_dir_path(sessions.path(), session_id);
        runtime::session_log::write_session_workspace(
            &session_dir,
            &runtime::session_log::SessionWorkspaceMetadata::new(
                workspace.path().to_path_buf(),
                None,
            ),
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
        let session_dir = runtime::session_log::session_dir_path(sessions.path(), session_id);
        runtime::session_log::write_session_workspace(
            &session_dir,
            &runtime::session_log::SessionWorkspaceMetadata::new(saved.path().to_path_buf(), None),
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
}

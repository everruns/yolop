// Entrypoint for the Yolop coding agent example.
// Decision: support both interactive TUI and a `--print` one-shot mode so the
// example is testable in CI and easy to demo against a real codebase.

mod auth;
mod capabilities;
mod config;
mod connectors;
mod drivers;
mod editor;
mod exec;
mod extensions;
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
use clap::{Args, Parser, Subcommand};
use config::SettingsStore;
use config::mcp::{McpConfigScope, McpConfigStore};
use crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use crossterm::{execute, queue};
use everruns_core::command::ExecuteCommandRequest;
use everruns_core::message::{ContentPart, MessageRole};
use everruns_core::typed_id::SessionId;
use ratatui::{Terminal, TerminalOptions, Viewport};
use runtime::{BuiltRuntime, ProviderChoice, ResolvedProviderChoice, resolve_for_settings};
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tui::{App, COMPOSER_VIEWPORT_HEIGHT, maybe_reanchor_inline_viewport};

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

    /// Write the full session as an ATIF v1.7 trajectory JSON file to this
    /// path at end of run. Works interactively and with `-p/--print`; see
    /// `specs/trajectory.md`.
    #[arg(long, value_name = "PATH")]
    trajectory_out: Option<PathBuf>,

    /// EXPERIMENTAL: render the interactive TUI full-screen on the alternate
    /// screen (via the `tuika` toolkit) instead of the default inline
    /// scrollback composer. Overlays and scrolling are owned in-app; native
    /// terminal scrollback is unavailable in this mode.
    #[arg(long)]
    fullscreen: bool,

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
    /// Configure Paseo to launch yolop as a custom ACP provider.
    Paseo(PaseoIntoArgs),
    /// Configure Zed to launch yolop as a custom ACP agent.
    Zed(ZedIntoArgs),
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

fn main() -> Result<()> {
    // Windows caps the main-thread stack at 1 MiB. The entry future (settings
    // load → runtime build → TUI/print loop) is large enough to overflow that,
    // though it fits comfortably in the 8 MiB default Linux and macOS give the
    // main thread. Run the whole program on a thread with an explicit large
    // stack so every platform behaves like the generous default, and give the
    // tokio worker threads a matching stack for deep async work spawned onto
    // them.
    let worker = std::thread::Builder::new()
        .name("yolop-main".to_string())
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(4)
                .thread_stack_size(8 * 1024 * 1024)
                .enable_all()
                .build()
                .expect("build tokio runtime")
                .block_on(async_main())
        })
        .expect("spawn yolop main thread");
    worker.join().expect("yolop main thread panicked")
}

async fn async_main() -> Result<()> {
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
    let settings_path = config::default_settings_path().unwrap_or_else(|| {
        eprintln!(
            "yolop: no platform config dir resolvable — settings will not persist across runs"
        );
        std::path::PathBuf::from("/dev/null/yolop/settings.toml")
    });
    let settings = Arc::new(SettingsStore::open(settings_path));
    let sandbox_mode_override = cli.sandbox.then_some(config::SandboxMode::WorkspaceWrite);
    let effective_sandbox_mode =
        sandbox_mode_override.unwrap_or_else(|| settings.snapshot().sandbox_mode());
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
        return editor::acp::run_stdio(provider, settings, sessions_dir, sandbox_mode_override)
            .await;
    }

    // Only the interactive TUI can apply terminal-side commands (overlays,
    // transcript clear, quit), so only it enables `ClientCommandsCapability`.
    // `--print` is one-shot and never dispatches them.
    let interactive = cli.print.is_none();
    if let Some(warning) = exec::sandbox::danger_warning(effective_sandbox_mode) {
        eprintln!("yolop: {warning}");
    }
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

    if let Some(prompt) = cli.print {
        let image_parts = tui::input::image_input::load_image_parts(&cli.images)?;
        return run_print_mode(runtime, prompt, image_parts, cli.trajectory_out).await;
    }
    let pending_images = tui::input::image_input::load_image_parts(&cli.images)?;
    run_tui(runtime, pending_images, cli.trajectory_out, cli.fullscreen).await
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

fn run_command(command: Commands) -> Result<()> {
    match command {
        Commands::Version => {
            println!("{}", version::VERSION_LINE);
            Ok(())
        }
        Commands::Worktree(args) => run_worktree_command(args.command),
        Commands::Mcp(args) => run_mcp_command(args.command),
        Commands::TuikaGallery => run_tuika_gallery(),
        #[cfg(target_os = "linux")]
        Commands::SandboxExec {
            cwd,
            temp,
            mode,
            script,
        } => exec::sandbox::run_linux_worker(&cwd, &temp, mode, &script),
        Commands::Into(into) => match into.target {
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

/// Whether opt-in OSC 8 hyperlinks are enabled via `YOLOP_HYPERLINKS`
/// (`1`/`true`/`on`). Default off until the rendering is verified across
/// terminals; see `tuika::HyperlinkBackend` and the tuika README matrix.
fn hyperlinks_enabled() -> bool {
    std::env::var("YOLOP_HYPERLINKS")
        .map(|v| matches!(v.as_str(), "1" | "true" | "on"))
        .unwrap_or(false)
}

/// Live demo of the `tuika` motion components. Renders spinners, progress bars,
/// and a loader on the alternate screen, and drives the terminal's native
/// OSC 9;4 progress indicator while running. Quits on `q`/`Esc`/`Ctrl-C`.
fn run_tuika_gallery() -> Result<()> {
    use std::ops::ControlFlow;
    use std::time::Duration;

    // Route through the same hyperlink-aware backend as the main TUI so the
    // demo's URL becomes a clickable OSC 8 link when YOLOP_HYPERLINKS is set.
    let backend = tuika::HyperlinkBackend::new(io::stdout(), hyperlinks_enabled());
    let theme = tuika::Theme::default();
    let mut progress = tuika::TerminalProgress::new();
    progress.indeterminate();
    let runner = tuika::Runner::new(tuika::RunnerConfig {
        tick_rate: Duration::from_millis(80),
    });
    runner.run_with_backend(
        &theme,
        backend,
        |frame| build_gallery(frame, &theme),
        |event| match event {
            tuika::Event::Key(key)
                if matches!(key.code, tuika::KeyCode::Esc | tuika::KeyCode::Char('q'))
                    || (key.ctrl && matches!(key.code, tuika::KeyCode::Char('c'))) =>
            {
                ControlFlow::Break(())
            }
            _ => ControlFlow::Continue(()),
        },
    )?;
    progress.clear();
    Ok(())
}

/// Build the gallery view tree for `frame`, using [`ratatui::text`] helpers via
/// `tuika` components.
fn build_gallery(frame: u64, theme: &tuika::Theme) -> tuika::Element {
    use ratatui::style::Modifier;
    use ratatui::text::{Line, Span};
    use tuika::{CodeHighlighter, Loader, MarkdownState, ProgressBar, Spinner, SpinnerStyle, Text};

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
    let mut markdown = MarkdownState::new();
    markdown.set(md_doc);
    let markdown_lines = markdown
        .lines(46, theme, CodeHighlighter::With(&highlighter))
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
) -> Result<()> {
    // Cheap Arc clones taken before `App` consumes the runtime, so the
    // trajectory can be exported after the TUI loop ends.
    let trajectory_handles = runtime.handles.clone();
    let trajectory_model = runtime.model.clone();
    let mut raw_mode = RawModeGuard::new()?;
    let mut keyboard_enhancements = KeyboardEnhancementGuard::new();
    let mut bracketed_paste = BracketedPasteGuard::new();
    // In full-screen mode `tuika` owns the alternate screen + mouse capture via
    // an RAII guard that restores them on drop. The viewport becomes the whole
    // terminal instead of a short inline strip.
    let alt_screen = if fullscreen {
        Some(tuika::AltScreen::enter()?)
    } else {
        None
    };
    let stdout = io::stdout();
    // Opt-in OSC 8 hyperlinks: wrap the crossterm backend so http(s) URLs in
    // rendered output become clickable. Default off (pure pass-through) until the
    // rendering is verified across terminals; enable with YOLOP_HYPERLINKS=1.
    let backend = tuika::HyperlinkBackend::new(stdout, hyperlinks_enabled());
    let viewport = if fullscreen {
        Viewport::Fullscreen
    } else {
        Viewport::Inline(COMPOSER_VIEWPORT_HEIGHT)
    };
    let mut terminal = Terminal::with_options(backend, TerminalOptions { viewport })?;
    // Anchoring is cosmetic and inline-only. Since ratatui 0.30.1
    // `insert_before` snapshots the cursor (via `Terminal::clear`) with a
    // blocking `CSI 6n` query that slow emulators (ttyd / xterm.js) may not
    // answer before crossterm's ~2s timeout. Start unanchored rather than
    // dying.
    if !fullscreen && let Err(err) = maybe_reanchor_inline_viewport(&mut terminal) {
        tracing::warn!("inline viewport anchoring failed, starting unanchored: {err:#}");
    }

    let mut app = App::new(runtime, pending_images);
    app.enable_native_progress();
    if fullscreen {
        app.set_render_mode(tui::RenderMode::Fullscreen);
    }
    let result = app.run(&mut terminal).await;
    let show_resume_hint = app.should_show_resume_hint();
    let session_id = app.session_id().to_string();
    let last_assistant_message = app.last_assistant_message().map(str::to_owned);

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
    // Leave the alternate screen before any post-loop stdout (resume hint) so
    // it lands on the user's normal screen, not the one about to be torn down.
    if let Some(mut alt) = alt_screen {
        alt.leave();
    }
    bracketed_paste.disable();
    keyboard_enhancements.disable();
    raw_mode.disable()?;

    write_trajectory_if_requested(
        &trajectory_handles,
        &trajectory_model,
        trajectory_out.as_deref(),
    )
    .await;

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

    let turn = collect_print_turn(&handles, &worktree, &model, trimmed, images).await?;
    print_final_output(&turn.output);
    if !turn.result.success {
        write_trajectory_if_requested(&handles, &model, trajectory_out.as_deref()).await;
        std::process::exit(1);
    }
    if user_ask_enabled && user_ask_store.is_active(handles.session_id) {
        let evaluation = handles
            .runtime
            .execute_command(
                handles.session_id,
                ExecuteCommandRequest {
                    name: "ask".to_string(),
                    arguments: Some(session_state::user_ask::USER_ASK_EVALUATE_ARG.to_string()),
                    controls: None,
                },
            )
            .await?;
        if evaluation.success {
            if let Ok(parsed) =
                session_state::user_ask::parse_evaluation_response(&evaluation.message)
                && parsed.outcome == session_state::user_ask::AskOutcome::Blocked
            {
                handles.report_herdr_state(capabilities::herdr::HerdrState::Blocked);
            }
        } else {
            eprintln!("user ask evaluation failed: {}", evaluation.message);
        }
    }
    write_trajectory_if_requested(&handles, &model, trajectory_out.as_deref()).await;
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
        let turn = collect_print_turn(handles, worktree, model, &turn_prompt, vec![]).await?;
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
    result: everruns_runtime::TurnResult,
    output: Vec<String>,
}

async fn collect_print_turn(
    handles: &runtime::RuntimeHandles,
    worktree: &crate::exec::worktree::WorktreeManager,
    model: &runtime::ModelState,
    prompt: &str,
    images: Vec<ContentPart>,
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

    let input = model.input_message_with_images(prompt, images);
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

    #[test]
    fn continuation_preserves_reusable_arguments_and_replaces_turn_arguments() {
        let args = [
            "yolop",
            "--fullscreen",
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
            "yolop --fullscreen --sandbox --model 'gpt test' --session new-session"
        );
    }

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
            trajectory_out: None,
            fullscreen: false,
            sandbox: false,
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
            cwd: None,
            provider: None,
            model: None,
            reasoning_effort: None,
            print: None,
            images: vec![],
            acp: false,
            session: None,
            session_dir: None,
            trajectory_out: None,
            fullscreen: false,
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

    #[test]
    fn inline_viewport_anchor_fills_space_above_bottom_target() {
        assert_eq!(tui::rows_below_inline_viewport(4, 18, 60), 38);
    }

    #[test]
    fn inline_viewport_anchor_does_not_scroll_when_already_low_enough() {
        assert_eq!(tui::rows_below_inline_viewport(42, 18, 60), 0);
        assert_eq!(tui::rows_below_inline_viewport(50, 18, 60), 0);
    }

    #[test]
    fn inline_viewport_anchor_handles_small_terminals() {
        assert_eq!(tui::rows_below_inline_viewport(0, 18, 10), 0);
    }
}

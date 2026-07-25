//! Experimental second execution backend: yolop's turn story built on the
//! [`agentyk`](https://github.com/everruns/agentyk) library instead of
//! `everruns-runtime`.
//!
//! This exists to answer one question — *what does agentyk still lack before a
//! real coding agent can be built on it?* — so it is deliberately isolated:
//! nothing outside this module knows it exists except the `--engine agentyk`
//! branch in `main`, and it shares only yolop's containment (`exec::sandbox`)
//! and its provider/model resolution. Compiled out entirely unless the
//! `agentyk-backend` feature is on.
//!
//! What it covers today: provider resolution → `ModelSpec`, the real sandboxed
//! `bash` tool, filesystem tools (agentyk's `FileSystemCapability` plus the
//! `edit_file` / `grep_files` yolop needs and agentyk does not ship),
//! AGENTS.md/CLAUDE.md instructions, hint-driven approval as middleware,
//! streaming render to stdout, cancellation on ctrl-c, and a JSONL event log
//! per session. Findings are recorded in
//! `knowledge/specs/agentyk-backend.md`.

mod approval;
mod instructions;
mod model;
mod render;
mod tools;

use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::sync::Arc;

use agentyk::{
    Agent, AnthropicDriver, CancellationToken, EventLog, InMemoryEventLog, JsonlEventLog,
    OpenAiDriver, SimDriver, SimTurn,
};
use anyhow::{Context, Result};

use crate::config::{SandboxMode, Settings};
use crate::runtime::ProviderChoice;

pub use approval::ApprovalMode;

/// Everything the backend needs from `main`, resolved before it starts.
pub struct RunConfig {
    /// Workspace root every tool is rooted at.
    pub workspace: PathBuf,
    /// Provider/model as yolop resolved it (CLI flags, env, settings).
    pub provider: ProviderChoice,
    /// Persisted settings, read for provider credentials.
    pub settings: Settings,
    /// Containment for the `bash` tool.
    pub sandbox: SandboxMode,
    /// One-shot prompt (`-p`), or `None` for the line-based REPL.
    pub prompt: Option<String>,
    /// Where the JSONL event log goes; `None` keeps the session in memory.
    pub session_log: Option<PathBuf>,
}

const SYSTEM_PROMPT: &str = "\
You are yolop, a terse terminal coding agent working inside one workspace \
directory.

Work by acting, not by narrating: read what you need, make the change, verify \
it. Prefer `read_file`, `list_directory`, and `grep_files` over shell \
equivalents — they are cheaper and never need approval. Use `edit_file` for \
targeted changes and `write_file` only for new or fully rewritten files. Use \
`bash` for everything else (git, builds, tests).

Keep replies to a few lines. Report what you found or changed, not how you \
looked. When a command fails, show the part of the output that explains why.";

/// Build the agent and drive it: one-shot for `-p`, otherwise a line REPL.
pub async fn run(config: RunConfig) -> Result<()> {
    let workspace = config
        .workspace
        .canonicalize()
        .unwrap_or_else(|_| config.workspace.clone());
    let model = model::resolve(&config.provider, &config.settings)?;

    let interactive = config.prompt.is_none();
    // A one-shot run has nobody to answer an approval prompt, so it runs
    // unattended — the same bargain `--print` makes on the everruns backend.
    let approval = match interactive && std::io::stdin().is_terminal() {
        true => ApprovalMode::Prompt,
        false => ApprovalMode::Auto,
    };

    let log: Arc<dyn EventLog> = match &config.session_log {
        Some(path) => {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).with_context(|| {
                    format!("create session log directory {}", parent.display())
                })?;
            }
            Arc::new(JsonlEventLog::new(path).map_err(anyhow::Error::msg)?)
        }
        None => Arc::new(InMemoryEventLog::new()),
    };

    let color = std::io::stdout().is_terminal();
    let agent = build_agent(&workspace, model, config.sandbox, approval, color)?;
    let mut session = agent.session_with_log(log);

    if let Some(prompt) = config.prompt {
        let turn = session
            .run(prompt)
            .await
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        render::final_answer(&turn.response, color);
        return Ok(());
    }

    println!("yolop (agentyk backend) — {}", workspace.display());
    println!("type a prompt, or ctrl-d to exit");
    let stdin = std::io::stdin();
    loop {
        print!("\n› ");
        std::io::stdout().flush().ok();
        let mut line = String::new();
        if stdin.read_line(&mut line).context("read prompt")? == 0 {
            return Ok(());
        }
        let prompt = line.trim();
        if prompt.is_empty() {
            continue;
        }
        // Ctrl-c cancels the turn rather than the process, matching the TUI.
        let token = CancellationToken::new();
        let interrupt = token.clone();
        let handle = tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                interrupt.cancel();
            }
        });
        let outcome = session.run_cancellable(prompt, token).await;
        handle.abort();
        match outcome {
            Ok(turn) => render::final_answer(&turn.response, color),
            Err(agentyk::Error::Cancelled) => println!("(cancelled)"),
            Err(error) => eprintln!("yolop: {error}"),
        }
    }
}

fn build_agent(
    workspace: &std::path::Path,
    model: agentyk::ModelSpec,
    sandbox: SandboxMode,
    approval: ApprovalMode,
    color: bool,
) -> Result<Agent> {
    let files = agentyk::WriteBlocklistFileSystem::wrap(Arc::new(
        agentyk::RealDiskFileSystem::new(workspace).map_err(anyhow::Error::msg)?,
    ));

    let mut builder = Agent::builder()
        .name("yolop")
        .system_prompt(SYSTEM_PROMPT)
        .model(model.clone())
        .capability(agentyk::FileSystemCapability::new(files))
        .capability(instructions::WorkspaceInstructions::new(workspace))
        .capability(tools::WorkspaceToolsCapability::new(
            workspace.to_path_buf(),
            sandbox,
        ))
        .middleware(approval::ApprovalMiddleware::new(approval))
        .listener(render::StdoutRenderer::new(color))
        .max_iterations(48);

    builder = match model.driver.as_str() {
        "anthropic" => builder.driver(AnthropicDriver::new().max_tokens(16_000)),
        // The offline script drives one real tool call before answering, so
        // `--provider llmsim --engine agentyk` proves the whole path — model
        // step, middleware, sandboxed execution, event render — without a key.
        "llmsim" => builder.driver(SimDriver::new([
            SimTurn::tool_call("bash", serde_json::json!({"command": "echo agentyk-ok"})),
            SimTurn::text("llmsim: the agentyk backend is wired up."),
        ])),
        _ => builder.driver(OpenAiDriver::new()),
    };

    builder.build().map_err(|error| anyhow::anyhow!("{error}"))
}

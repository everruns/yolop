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
mod catalog;
mod instructions;
mod mcp;
mod model;
mod render;
mod tools;

use std::future::Future;
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
    /// Images attached with `-i/--image`, already loaded by the caller. They
    /// ride on the first message of the run — the one they were attached to.
    pub images: Vec<everruns_core::message::ContentPart>,
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
    // Configured MCP servers come from the same store `yolop mcp add` writes,
    // so both backends see one configuration.
    let (servers, warnings) = mcp::capabilities(&workspace);
    for warning in warnings {
        eprintln!("yolop: {warning}");
    }
    let agent = build_agent(&workspace, model, config.sandbox, approval, color, servers)?;
    let mut session = agent.session_with_log(log);

    if let Some(prompt) = config.prompt {
        let turn = session
            .run(opening_message(&prompt, &config.images))
            .await
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        render::final_answer(&turn.response, color);
        return Ok(());
    }

    println!("yolop (agentyk backend) — {}", workspace.display());
    println!("type a prompt, or ctrl-d to exit; type again while it works to steer it");
    repl(session, color, config.images).await
}

/// The message that opens a turn: the typed text, plus any attached images.
///
/// One message rather than a text turn followed by images, because the
/// question and what it is about belong together — a model asked "what is
/// wrong here?" needs the screenshot in the same breath.
fn opening_message(
    prompt: &str,
    images: &[everruns_core::message::ContentPart],
) -> agentyk::Message {
    if images.is_empty() {
        return agentyk::Message::user(prompt);
    }
    let mut content = vec![agentyk::ContentPart::text(prompt)];
    content.extend(images.iter().filter_map(to_agentyk_part));
    agentyk::Message::user_multimodal(content)
}

/// everruns' content part → agentyk's. Same fields, different crate: yolop
/// loads and validates images once, for whichever backend is running.
fn to_agentyk_part(part: &everruns_core::message::ContentPart) -> Option<agentyk::ContentPart> {
    match part {
        everruns_core::message::ContentPart::Image(image) => {
            let mapped = match (&image.url, &image.base64) {
                (Some(url), _) => agentyk::ImageContentPart::from_url(url.clone()),
                (None, Some(base64)) => agentyk::ImageContentPart::from_base64(
                    base64.clone(),
                    image
                        .media_type
                        .clone()
                        .unwrap_or_else(|| "image/png".to_string()),
                ),
                // Neither a URL nor bytes is not an image; dropping it beats
                // sending the provider something it will reject.
                (None, None) => return None,
            };
            Some(agentyk::ContentPart::Image(mapped))
        }
        everruns_core::message::ContentPart::Text(text) => {
            Some(agentyk::ContentPart::text(text.text.clone()))
        }
        _ => None,
    }
}

/// The interactive loop.
///
/// Reading stdin on a dedicated task rather than between turns is the whole
/// point: a line typed *while* the agent works becomes steering
/// (`Session::input()`), and one typed while it is idle starts the next turn.
/// The operator does not have to know which state the agent is in — the same
/// keystrokes mean the right thing either way.
async fn repl(
    mut session: agentyk::Session,
    color: bool,
    images: Vec<everruns_core::message::ContentPart>,
) -> Result<()> {
    let (lines, mut inbox) = tokio::sync::mpsc::unbounded_channel::<String>();
    // Console reads block a thread, so they get their own; dropping the
    // sender on EOF is what ends the loop.
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        loop {
            let mut line = String::new();
            match stdin.read_line(&mut line) {
                Ok(0) | Err(_) => return,
                Ok(_) => {
                    if lines.send(line).is_err() {
                        return;
                    }
                }
            }
        }
    });

    let steering = session.input();
    // Images attached on the command line belong to the first thing typed,
    // the way they would if it had been one invocation.
    let mut pending_images = images;
    loop {
        print!("\n› ");
        std::io::stdout().flush().ok();
        let Some(prompt) = next_prompt(&mut inbox).await else {
            return Ok(());
        };

        // Ctrl-c cancels the turn rather than the process, matching the TUI.
        let token = CancellationToken::new();
        let interrupt = token.clone();
        let signals = tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                interrupt.cancel();
            }
        });

        let opening = opening_message(&prompt, &pending_images);
        pending_images.clear();
        let turn = session.run_cancellable(opening, token);
        let outcome = drive_turn(turn, &mut inbox, &steering, color).await;
        signals.abort();

        match outcome {
            Ok(turn) => render::final_answer(&turn.response, color),
            Err(agentyk::Error::Cancelled) => println!("(cancelled)"),
            Err(error) => eprintln!("yolop: {error}"),
        }
    }
}

/// Run a turn to completion, treating anything typed meanwhile as steering.
///
/// Split out from the loop so the decision — *is this line a prompt or a
/// course correction?* — is testable without a terminal. It is only a
/// decision because the turn is already running: the queue delivers what it
/// collects at the turn's next reasoning step.
async fn drive_turn<F: Future<Output = O>, O>(
    turn: F,
    inbox: &mut tokio::sync::mpsc::UnboundedReceiver<String>,
    steering: &agentyk::InputQueue,
    color: bool,
) -> O {
    let mut turn = std::pin::pin!(turn);
    loop {
        tokio::select! {
            outcome = &mut turn => return outcome,
            line = inbox.recv() => match line {
                Some(line) if !line.trim().is_empty() => {
                    steering.push(agentyk::Message::user(line.trim()));
                    println!("{}", render::steered(line.trim(), color));
                }
                // An empty line is not a course correction, and stdin closing
                // is not a reason to abandon work already in flight.
                Some(_) => {}
                None => return (&mut turn).await,
            },
        }
    }
}

/// The next non-empty line, or `None` when stdin closes.
async fn next_prompt(inbox: &mut tokio::sync::mpsc::UnboundedReceiver<String>) -> Option<String> {
    loop {
        let line = inbox.recv().await?;
        let trimmed = line.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
}

fn build_agent(
    workspace: &std::path::Path,
    model: agentyk::ModelSpec,
    sandbox: SandboxMode,
    approval: ApprovalMode,
    color: bool,
    servers: Vec<Box<dyn agentyk::Capability>>,
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
        .model_catalog(catalog::YolopModelCatalog)
        .middleware(approval::ApprovalMiddleware::new(approval))
        .listener(render::StdoutRenderer::new(color))
        .max_iterations(48);

    for server in servers {
        builder = builder.capability_arc(Arc::from(server));
    }

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

#[cfg(test)]
mod tests {
    use super::*;
    use agentyk::InputQueue;
    use tokio::sync::mpsc;

    /// A turn that stays pending until told otherwise, so a test controls
    /// exactly when it ends.
    fn pending_turn(
        done: Arc<std::sync::atomic::AtomicBool>,
    ) -> impl Future<Output = &'static str> {
        std::future::poll_fn(
            move |context| match done.load(std::sync::atomic::Ordering::SeqCst) {
                true => std::task::Poll::Ready("finished"),
                false => {
                    context.waker().wake_by_ref();
                    std::task::Poll::Pending
                }
            },
        )
    }

    #[tokio::test]
    async fn a_line_typed_during_a_turn_becomes_steering() {
        let (lines, mut inbox) = mpsc::unbounded_channel();
        let steering = InputQueue::new();
        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));

        lines
            .send("actually, skip the tests\n".to_string())
            .unwrap();
        // Blank lines are keystrokes, not corrections.
        lines.send("   \n".to_string()).unwrap();
        let finish = done.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            finish.store(true, std::sync::atomic::Ordering::SeqCst);
        });

        let outcome = drive_turn(pending_turn(done), &mut inbox, &steering, false).await;
        assert_eq!(outcome, "finished");

        let queued = steering.drain();
        assert_eq!(queued.len(), 1, "only the real correction was queued");
        assert_eq!(queued[0].text(), "actually, skip the tests");
    }

    #[tokio::test]
    async fn closing_stdin_lets_the_turn_finish() {
        let (lines, mut inbox) = mpsc::unbounded_channel::<String>();
        let steering = InputQueue::new();
        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));

        // The operator hit ctrl-d while the agent was working: abandoning the
        // turn would throw away work already done.
        drop(lines);
        let finish = done.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            finish.store(true, std::sync::atomic::Ordering::SeqCst);
        });

        assert_eq!(
            drive_turn(pending_turn(done), &mut inbox, &steering, false).await,
            "finished"
        );
        assert!(steering.drain().is_empty());
    }

    #[test]
    fn an_attached_image_rides_on_the_message_that_asks_about_it() {
        use everruns_core::message::{ContentPart as ErContentPart, ImageContentPart as ErImage};

        let images = vec![ErContentPart::Image(ErImage::from_base64(
            "iVBORw0KGgo=",
            "image/png",
        ))];
        let message = opening_message("what is wrong here?", &images);

        assert_eq!(message.text(), "what is wrong here?");
        assert!(
            matches!(message.content.last(), Some(agentyk::ContentPart::Image(_))),
            "the question and the screenshot travel together: {:?}",
            message.content
        );
    }

    #[test]
    fn a_prompt_with_no_images_is_a_plain_message() {
        let message = opening_message("just text", &[]);
        assert_eq!(message.text(), "just text");
        assert_eq!(message.content.len(), 1);
    }

    #[test]
    fn an_image_with_neither_bytes_nor_url_is_dropped() {
        use everruns_core::message::{ContentPart as ErContentPart, ImageContentPart as ErImage};

        // Sending the provider an empty image would be a 400; saying nothing
        // is the better failure.
        let empty = ErContentPart::Image(ErImage {
            url: None,
            base64: None,
            media_type: None,
        });
        assert!(to_agentyk_part(&empty).is_none());
    }

    #[tokio::test]
    async fn blank_lines_never_start_a_turn() {
        let (lines, mut inbox) = mpsc::unbounded_channel();
        lines.send("\n".to_string()).unwrap();
        lines.send("  \n".to_string()).unwrap();
        lines.send("go\n".to_string()).unwrap();
        assert_eq!(next_prompt(&mut inbox).await.as_deref(), Some("go"));

        drop(lines);
        assert_eq!(next_prompt(&mut inbox).await, None);
    }
}

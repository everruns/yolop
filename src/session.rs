//! The session facade: the single owner of [`RuntimeHandles`] and the only
//! place that drives `everruns_core` runtime APIs (`run_turn`, `messages`,
//! `events`, `execute_command`, the live event broadcast).
//!
//! The TUI (`crate::app`) talks to a [`Session`] and consumes the
//! [`crate::transcript::TurnEvent`] stream it produces; it never subscribes to
//! the raw runtime event bus or reaches into runtime internals. This keeps the
//! `everruns_core` event/message model from leaking across the UI surface and
//! makes the turn lifecycle independently testable.

use std::collections::HashSet;
use std::sync::Arc;

use anyhow::Result;
use everruns_core::command::ExecuteCommandRequest;
use everruns_core::message::ContentPart;
use everruns_core::tools::Tool;
use everruns_core::typed_id::SessionId;
use tokio::sync::{broadcast, mpsc, oneshot};

use crate::runtime::{ModelState, RuntimeHandles};
use crate::tools::{BashTool, Workspace};
use crate::transcript::{
    Author, ChatLine, DeltaRouter, TurnEvent, assistant_lines_since, handle_live_event,
    lines_for_event_with_router, lines_for_replayed_event, remember_write_todos_args,
    shell_result_lines, status_for_event, tokens_for_event,
};

/// Outcome of a capability-provided slash command, reduced to what the host UI
/// needs so the `everruns_core` command result type does not leak past the
/// facade.
pub(crate) struct CommandOutcome {
    pub success: bool,
    pub message: String,
}

/// A running turn (or `!shell` command): the event stream the host drains and a
/// one-shot cancel trigger.
pub(crate) struct TurnHandle {
    pub events: mpsc::UnboundedReceiver<TurnEvent>,
    pub cancel: oneshot::Sender<()>,
}

/// Facade over the runtime for a single session. Cheap to construct; holds
/// shared handles (`Arc`-backed) plus a [`ModelState`] clone (also `Arc`-shared,
/// so model changes made elsewhere are observed here).
#[derive(Clone)]
pub(crate) struct Session {
    handles: RuntimeHandles,
    model: ModelState,
}

impl Session {
    pub fn new(handles: RuntimeHandles, model: ModelState) -> Self {
        Self { handles, model }
    }

    pub fn session_id(&self) -> SessionId {
        self.handles.session_id
    }

    pub(crate) fn report_herdr_state(&self, state: crate::capabilities::herdr::HerdrState) {
        self.handles.report_herdr_state(state);
    }

    /// Re-read the merged MCP server config and swap it into the live session
    /// so add / remove / enable / disable apply on the next turn without a
    /// restart. Returns the sorted names now active. See
    /// [`RuntimeHandles::reload_mcp_servers`](crate::runtime::RuntimeHandles::reload_mcp_servers).
    pub async fn reload_mcp_servers(&self) -> Result<Vec<String>> {
        self.handles.reload_mcp_servers().await
    }

    /// The shared connection store backing MCP OAuth tokens. `/mcp login` saves
    /// through this so the runtime's auth provider (which holds the same handle)
    /// sees the new token immediately.
    pub fn connections(&self) -> Arc<crate::connectors::ConnectionStore> {
        self.handles.connections.clone()
    }

    /// Translate the prefix of persisted events that were replayed from disk
    /// into transcript lines (used to seed the transcript on resume).
    pub async fn replayed_lines(&self, count: usize) -> Result<Vec<ChatLine>> {
        let events = self.handles.runtime.events().await?;
        Ok(events
            .iter()
            .take(count)
            .flat_map(lines_for_replayed_event)
            .collect())
    }

    /// Execute a capability-provided command through the runtime, returning a
    /// host-facing [`CommandOutcome`].
    pub async fn execute_command(
        &self,
        name: &str,
        arguments: Option<String>,
    ) -> Result<CommandOutcome> {
        let request = ExecuteCommandRequest {
            name: name.to_string(),
            arguments,
            controls: None,
        };
        let result = self
            .handles
            .runtime
            .execute_command(self.handles.session_id, request)
            .await?;
        Ok(CommandOutcome {
            success: result.success,
            message: result.message,
        })
    }

    /// Run an agent turn for `prompt` (with any pending `images`). Spawns the
    /// turn task and returns a [`TurnHandle`]; the task emits `TurnEvent`s and
    /// finishes with `Done` (or `Failed`).
    pub fn run_turn(&self, prompt: String, images: Vec<ContentPart>) -> TurnHandle {
        let handles = self.handles.clone();
        let model = self.model.clone();
        let (tx, rx) = mpsc::unbounded_channel::<TurnEvent>();
        let (cancel_tx, mut cancel_rx) = oneshot::channel::<()>();

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

            let input = model.input_message_with_images(prompt, images);
            let runtime = handles.runtime.clone();
            let mut turn = tokio::spawn(async move { runtime.run_turn(session_id, input).await });

            let mut emitted_events = HashSet::new();
            let mut delta_router = DeltaRouter::default();
            // High-water mark into the persisted event vec. Each catch-up
            // advances it to `events.len()` so a later catch-up only scans the
            // newly persisted suffix — repeated `Lagged` recoveries on a long
            // session stay O(total new events), not O(n²) re-scans.
            let mut events_cursor = events_before;
            let mut cancelled = false;
            // Set once the turn task joins from within the loop, so we don't
            // await the `JoinHandle` twice.
            let mut joined = None;
            loop {
                tokio::select! {
                    biased;
                    _ = &mut cancel_rx => {
                        cancelled = true;
                        turn.abort();
                        break;
                    }
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
                                &mut events_cursor,
                                &mut emitted_events,
                                &mut delta_router,
                                &tx,
                            )
                            .await;
                        }
                        Err(broadcast::error::RecvError::Closed) => break,
                    },
                    // Turn finished: no poll/latency. Live events buffered
                    // before this point are preferred (biased order); the tail
                    // is drained by the catch-up below.
                    res = &mut turn => {
                        joined = Some(res);
                        break;
                    }
                }
            }

            if cancelled {
                handles.report_herdr_state(crate::capabilities::herdr::HerdrState::Idle);
                let _ = tx.send(TurnEvent::Stream(None));
                let _ = tx.send(TurnEvent::Lines(vec![ChatLine {
                    author: Author::System,
                    text: "turn cancelled".into(),
                }]));
                let _ = tx.send(TurnEvent::Done);
                return;
            }

            // Drain any tail events emitted between the last broadcast
            // poll and the turn's actual completion.
            catch_up_events(
                &handles,
                &mut events_cursor,
                &mut emitted_events,
                &mut delta_router,
                &tx,
            )
            .await;
            // Clear any in-flight streaming preview before we finalize.
            let _ = tx.send(TurnEvent::Stream(None));

            // `joined` is set unless the loop broke on a closed broadcast
            // before the turn finished; await the handle in that rare case.
            let result = match joined {
                Some(res) => res,
                None => turn.await,
            };
            let result = match result {
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

            // Assistant text from the turn.
            let mut out = assistant_lines_since(&messages, before);
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

        TurnHandle {
            events: rx,
            cancel: cancel_tx,
        }
    }

    /// Run a host-local `!shell` command (not part of a tool-call lifecycle).
    /// Output is rendered inline; nothing is persisted to the session.
    pub fn run_shell(
        &self,
        command: String,
        workspace: Arc<crate::workspace_host::WorkspaceHost>,
    ) -> TurnHandle {
        let (tx, rx) = mpsc::unbounded_channel::<TurnEvent>();
        let (cancel_tx, mut cancel_rx) = oneshot::channel::<()>();

        tokio::spawn(async move {
            let tool = BashTool::new(Workspace::new(workspace));
            let run = tool.execute(serde_json::json!({
                "command": command,
                // Direct shell output is not persisted through a tool-call
                // lifecycle, so render a useful bounded window inline.
                "output": "normal",
            }));
            tokio::select! {
                result = run => {
                    let _ = tx.send(TurnEvent::Lines(shell_result_lines(result)));
                }
                _ = &mut cancel_rx => {
                    let _ = tx.send(TurnEvent::Lines(vec![ChatLine {
                        author: Author::System,
                        text: "turn cancelled".into(),
                    }]));
                }
            }
            let _ = tx.send(TurnEvent::Done);
        });

        TurnHandle {
            events: rx,
            cancel: cancel_tx,
        }
    }
}

/// Drain any persisted events (from `runtime.events()`) that the broadcast
/// receiver may have missed — used after a `Lagged` recv error and once more at
/// end-of-turn so the transcript is never missing tool/reason completion lines.
async fn catch_up_events(
    handles: &RuntimeHandles,
    cursor: &mut usize,
    emitted_events: &mut HashSet<String>,
    router: &mut DeltaRouter,
    tx: &mpsc::UnboundedSender<TurnEvent>,
) {
    let events = handles.runtime.events().await.unwrap_or_default();
    let mut lines = Vec::new();
    // Only scan the suffix persisted since the last catch-up; `emitted_events`
    // still de-dupes overlap with events already delivered live.
    for event in events.iter().skip(*cursor) {
        let event_id = event.id.to_string();
        if !emitted_events.insert(event_id) {
            continue;
        }
        if let Some(tokens) = tokens_for_event(event) {
            let _ = tx.send(TurnEvent::Tokens(tokens));
        }
        remember_write_todos_args(event, router);
        if let Some(activity) = status_for_event(event) {
            let _ = tx.send(TurnEvent::Activity(activity));
        }
        lines.extend(lines_for_event_with_router(event, router));
    }
    *cursor = events.len();
    if !lines.is_empty() {
        let _ = tx.send(TurnEvent::Lines(lines));
    }
}

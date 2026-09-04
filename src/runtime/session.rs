//! The session facade: the single owner of [`RuntimeHandles`] and the only
//! place that drives `everruns_core` runtime APIs (`run_turn`, `messages`,
//! `events`, `execute_command`, the live event broadcast).
//!
//! The TUI (`crate::tui`) talks to a [`Session`] and consumes the
//! [`crate::tui::transcript::TurnEvent`] stream it produces; it never subscribes to
//! the raw runtime event bus or reaches into runtime internals. This keeps the
//! `everruns_core` event/message model from leaking across the UI surface and
//! makes the turn lifecycle independently testable.

use std::collections::HashSet;
use std::sync::Arc;

use anyhow::Result;
use everruns_core::ContentPart;
use everruns_core::Event;
use everruns_core::InputMessage;
use everruns_core::Tool;
use everruns_core::command::ExecuteCommandRequest;
use everruns_provider::typed_id::SessionId;
use tokio::sync::{broadcast, mpsc, oneshot};

use crate::exec::tools::{BashTool, Workspace};
use crate::runtime::background_wake::WakeMessage;
use crate::runtime::{ModelState, RuntimeHandles};
use crate::tui::transcript::{
    Author, ChatLine, DeltaRouter, TurnEvent, assistant_lines_since, handle_live_event,
    lines_for_event_with_router, lines_for_replayed_event, remember_write_todos_args,
    shell_result_lines, shell_result_succeeded, status_for_event, tokens_for_event,
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

    pub(crate) async fn turn_tokens(&self, turn_id: everruns_provider::typed_id::TurnId) -> u64 {
        self.handles.turn_tokens(turn_id).await
    }

    /// Re-read the merged MCP server config and swap it into the live session
    /// so add / remove / enable / disable apply on the next turn without a
    /// restart. Returns the sorted names now active. See
    /// [`RuntimeHandles::reload_mcp_servers`](crate::runtime::RuntimeHandles::reload_mcp_servers).
    pub async fn reload_mcp_servers(&self) -> Result<Vec<String>> {
        self.handles.reload_mcp_servers().await
    }

    /// Live MCP tool names (`mcp_<server>__<tool>`) for `/tools`.
    pub async fn list_mcp_tool_names(&self) -> Vec<String> {
        self.handles.list_mcp_tool_names().await
    }

    /// The shared connection store backing MCP OAuth tokens. `/mcp login` saves
    /// through this so the runtime's auth provider (which holds the same handle)
    /// sees the new token immediately.
    pub fn connections(&self) -> Arc<crate::connectors::ConnectionStore> {
        self.handles.connections.clone()
    }

    /// Activate a registered `ext:<name>` capability on the live session so its
    /// tools/prompt/hooks/commands/MCP appear on the next turn (hot-enable). See
    /// [`RuntimeHandles::activate_capability`](crate::runtime::RuntimeHandles::activate_capability).
    /// Make an extension installed after startup resolvable on this runtime.
    ///
    /// Returns whether a registration happened; `Ok(false)` means the id was
    /// already registered (the ordinary case, for packages present at startup).
    /// Registration is not activation: the caller still activates the id.
    pub fn register_installed_extension(&self, name: &str) -> Result<bool> {
        let capability_id = crate::extensions::extension_capability_id(name);
        if self
            .handles
            .runtime
            .is_capability_registered(&capability_id)
        {
            return Ok(false);
        }
        let factory = self
            .handles
            .extension_factory
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("this session has no extensions directory"))?;
        let capability = factory(name).map_err(|error| anyhow::anyhow!(error))?;
        self.handles.runtime.register_capability(capability)?;
        Ok(true)
    }

    pub async fn activate_capability(
        &self,
        capability_id: &str,
    ) -> Result<everruns_host::CapabilityDelta> {
        self.handles.activate_capability(capability_id).await
    }

    /// Deactivate a session-activated capability. See
    /// [`RuntimeHandles::deactivate_capability`](crate::runtime::RuntimeHandles::deactivate_capability).
    pub async fn deactivate_capability(
        &self,
        capability_id: &str,
    ) -> Result<everruns_host::CapabilityDelta> {
        self.handles.deactivate_capability(capability_id).await
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

    pub async fn active_lines(&self) -> Result<Vec<ChatLine>> {
        Ok(self
            .handles
            .runtime
            .events()
            .await?
            .iter()
            .flat_map(lines_for_replayed_event)
            .collect())
    }

    pub(crate) async fn completion_already_observed(&self, message: &WakeMessage) -> bool {
        self.handles.runtime.events().await.is_ok_and(|events| {
            crate::runtime::background_wake::completion_already_observed(message, &events)
        })
    }

    pub fn take_checkpoint_notice(&self) -> Option<String> {
        self.handles.checkpoints.take_notice()
    }

    pub fn take_restored_prompt(&self) -> Option<String> {
        self.handles.checkpoints.take_restored_prompt()
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
        let input = self.model.input_message_with_images(prompt.clone(), images);
        self.run_turn_input(prompt, input)
    }

    /// Run a turn with a host-constructed input. Automatic wakeups use this to
    /// attach provenance metadata without letting display text forge it.
    pub fn run_turn_input(&self, prompt: String, input: InputMessage) -> TurnHandle {
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
            if let Err(error) = model.validate_model_available().await {
                let _ = tx.send(TurnEvent::Failed(format!(
                    "model availability check: {error:#}"
                )));
                let _ = tx.send(TurnEvent::Done {
                    result: None,
                    success: false,
                });
                return;
            }
            let before = match handles.runtime.messages(session_id).await {
                Ok(m) => m.len(),
                Err(e) => {
                    let _ = tx.send(TurnEvent::Failed(format!("load history: {e}")));
                    let _ = tx.send(TurnEvent::Done {
                        result: None,
                        success: false,
                    });
                    return;
                }
            };
            let events_before = match handles.runtime.events().await {
                Ok(e) => e.len(),
                Err(_) => 0,
            };

            let turn_handles = handles.clone();
            let mut turn =
                tokio::spawn(
                    async move { turn_handles.run_checkpointed_turn(&prompt, input).await },
                );

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
                                session_id,
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
                let _ = tx.send(TurnEvent::Done {
                    result: None,
                    success: false,
                });
                return;
            }

            // Drain any tail events emitted between the last broadcast
            // poll and the turn's actual completion.
            catch_up_events(
                &handles,
                session_id,
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
                    let _ = tx.send(TurnEvent::Done {
                        result: None,
                        success: false,
                    });
                    return;
                }
            };
            let response = match result {
                Ok(r) => r,
                Err(e) => {
                    let _ = tx.send(TurnEvent::Failed(format!("{e}")));
                    let _ = tx.send(TurnEvent::Done {
                        result: None,
                        success: false,
                    });
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
                    text: response.response.clone(),
                });
            }
            if !response.success
                && let Some(err) = &response.error
            {
                out.push(ChatLine {
                    author: Author::System,
                    text: format!("turn error: {err}"),
                });
            }
            let _ = tx.send(TurnEvent::Lines(out));
            let success = response.success;
            let _ = tx.send(TurnEvent::Done {
                result: Some(response),
                success,
            });
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
        workspace: Arc<crate::exec::workspace_host::WorkspaceHost>,
    ) -> TurnHandle {
        let (tx, rx) = mpsc::unbounded_channel::<TurnEvent>();
        let (cancel_tx, mut cancel_rx) = oneshot::channel::<()>();
        let sandbox = self.handles.sandbox.clone();
        let approval_gate = self.handles.sandbox_approval_gate.clone();
        let approval_policy = self.handles.approval_policy;
        let control = self.handles.control.clone();

        tokio::spawn(async move {
            let tool = BashTool::with_policy(
                Workspace::new(workspace),
                sandbox,
                approval_policy,
                approval_gate,
            )
            .with_control(control);
            let run = tool.execute(serde_json::json!({
                "command": command,
                // Direct shell output is not persisted through a tool-call
                // lifecycle, so render a useful bounded window inline.
                "output": "normal",
            }));
            let success = tokio::select! {
                result = run => {
                    let success = shell_result_succeeded(&result);
                    let _ = tx.send(TurnEvent::Lines(shell_result_lines(result)));
                    success
                }
                _ = &mut cancel_rx => {
                    let _ = tx.send(TurnEvent::Lines(vec![ChatLine {
                        author: Author::System,
                        text: "turn cancelled".into(),
                    }]));
                    false
                }
            };
            let _ = tx.send(TurnEvent::Done {
                result: None,
                success,
            });
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
    session_id: SessionId,
    cursor: &mut usize,
    emitted_events: &mut HashSet<String>,
    router: &mut DeltaRouter,
    tx: &mpsc::UnboundedSender<TurnEvent>,
) {
    let events = handles.runtime.events().await.unwrap_or_default();
    route_catch_up_events(&events, session_id, cursor, emitted_events, router, tx);
}

fn route_catch_up_events(
    events: &[Event],
    session_id: SessionId,
    cursor: &mut usize,
    emitted_events: &mut HashSet<String>,
    router: &mut DeltaRouter,
    tx: &mpsc::UnboundedSender<TurnEvent>,
) {
    let mut lines = Vec::new();
    // Only scan the suffix persisted since the last catch-up; `emitted_events`
    // still de-dupes overlap with events already delivered live.
    for event in events.iter().skip(*cursor) {
        // The runtime event store is shared by the root and every child
        // session. Match the live broadcast path's boundary: child narration,
        // titles, tokens, and activity belong in the agent panel, never the
        // root transcript after a lagged-receiver catch-up.
        if event.session_id != session_id {
            continue;
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use everruns_core::{EventContext, ToolCompletedData};
    use everruns_provider::DriverId;
    use everruns_provider::error::Result as EverrunsResult;
    use everruns_provider::{
        ChatDriver, DiscoveredModel, LlmCallConfig, LlmMessage, LlmResponseStream,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn catch_up_routes_only_the_active_session() {
        let root = SessionId::new();
        let child = SessionId::new();
        let event = |session_id, call: &str, narration: &str| {
            Event::new(
                session_id,
                EventContext::empty(),
                ToolCompletedData::success(
                    call.to_string(),
                    "write_session_title".to_string(),
                    vec![ContentPart::text("{}")],
                    None,
                )
                .with_narration(Some(narration.to_string())),
            )
        };
        let events = vec![
            event(root, "root_title", "Updated session title: Root"),
            event(child, "child_title", "Updated session title: Child"),
        ];
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut cursor = 0;
        let mut emitted = HashSet::new();
        let mut router = DeltaRouter::default();

        route_catch_up_events(&events, root, &mut cursor, &mut emitted, &mut router, &tx);

        let mut lines = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let TurnEvent::Lines(batch) = event {
                lines.extend(batch.into_iter().map(|line| line.text));
            }
        }
        assert_eq!(cursor, 2, "foreign events still advance the shared cursor");
        assert_eq!(emitted.len(), 1, "foreign event ids are not claimed");
        assert!(lines.iter().any(|line| line.contains("Root")));
        assert!(lines.iter().all(|line| !line.contains("Child")));
    }

    #[tokio::test]
    async fn unavailable_discovered_model_fails_before_turn_is_persisted_or_sent() {
        struct ModelListingDriver {
            chat_calls: Arc<AtomicUsize>,
        }

        #[async_trait]
        impl ChatDriver for ModelListingDriver {
            async fn chat_completion_stream(
                &self,
                _endpoint: &everruns_provider::runtime_provider::ProviderEndpoint,
                _messages: Vec<LlmMessage>,
                _config: &LlmCallConfig,
            ) -> EverrunsResult<LlmResponseStream> {
                self.chat_calls.fetch_add(1, Ordering::SeqCst);
                panic!("unavailable model must be rejected before provider send")
            }

            async fn list_models(
                &self,
                _endpoint: &everruns_provider::runtime_provider::ProviderEndpoint,
            ) -> EverrunsResult<Option<Vec<DiscoveredModel>>> {
                Ok(Some(vec![DiscoveredModel {
                    model_id: "different-model".to_string(),
                    display_name: None,
                    created_at: None,
                    owned_by: None,
                    capabilities: vec!["chat".to_string()],
                    discovered_profile: None,
                }]))
            }
        }

        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let settings = Arc::new(crate::config::SettingsStore::open(
            sessions.path().join("settings.toml"),
        ));
        let built = crate::runtime::build_with_options(
            workspace.path().to_path_buf(),
            crate::runtime::ProviderChoice::Sim,
            None,
            sessions.path().to_path_buf(),
            settings,
            crate::runtime::BuildOptions::default(),
        )
        .await
        .expect("build runtime");
        let chat_calls = Arc::new(AtomicUsize::new(0));
        let mut model = built.model;
        let captured_calls = chat_calls.clone();
        model
            .driver_registry
            .register_or_replace(DriverId::LlmSim, move |_config| {
                Box::new(ModelListingDriver {
                    chat_calls: captured_calls.clone(),
                })
            });
        let runtime = built.handles.runtime.clone();
        let session_id = built.handles.session_id;
        let session = Session::new(built.handles, model);

        let mut turn = session.run_turn("keep this ask".to_string(), Vec::new());
        let mut failure = None;
        while let Some(event) = turn.events.recv().await {
            match event {
                TurnEvent::Failed(message) => failure = Some(message),
                TurnEvent::Done { .. } => break,
                _ => {}
            }
        }

        assert_eq!(chat_calls.load(Ordering::SeqCst), 0);
        assert!(
            failure
                .as_deref()
                .is_some_and(|message| message.contains("not available"))
        );
        assert!(
            runtime
                .messages(session_id)
                .await
                .expect("messages")
                .is_empty(),
            "the rejected ask must remain resumable instead of entering history"
        );
    }

    #[tokio::test]
    async fn cancelling_agent_turn_finishes_without_host_transcript_status() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let settings = Arc::new(crate::config::SettingsStore::open(
            sessions.path().join("settings.toml"),
        ));
        let built = crate::runtime::build_with_options(
            workspace.path().to_path_buf(),
            crate::runtime::ProviderChoice::Sim,
            None,
            sessions.path().to_path_buf(),
            settings,
            crate::runtime::BuildOptions::default(),
        )
        .await
        .expect("build runtime");
        let session = Session::new(built.handles, built.model);

        let mut turn = session.run_turn("keep working".to_string(), Vec::new());
        turn.cancel.send(()).expect("cancel turn");

        let mut transcript = Vec::new();
        let mut completed = false;
        while let Some(event) = turn.events.recv().await {
            match event {
                TurnEvent::Lines(lines) => transcript.extend(lines),
                TurnEvent::Done { result: None, .. } => {
                    completed = true;
                    break;
                }
                TurnEvent::Done {
                    result: Some(result),
                    ..
                } => {
                    panic!("cancelled turn unexpectedly completed: {result:?}")
                }
                _ => {}
            }
        }

        assert!(
            completed,
            "cancelled turn must still reach its terminal event"
        );
        assert!(
            transcript.is_empty(),
            "cancelled turn must not add host status to the transcript: {transcript:?}"
        );
    }
}

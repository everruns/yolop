//! ACP protocol v2 front end.
//!
//! Protocol v2 (draft) is what Zed's Delta requires of external agents. It
//! changes the lifecycle more than the payloads: `session/prompt` answers with
//! a `messageId` as soon as the message is inserted, and the turn's progress
//! and end travel as `state_update` notifications (`running`, `requires_action`,
//! `idle` with a stop reason), so an agent can work before, between and after
//! prompts. Messages carry ids, tool calls are upserts, plans have ids, modes
//! became a `mode` config option, and the client no longer offers files or
//! terminals.
//!
//! Decision: v2 is a translation layer over the v1 session core in
//! [`super`], not a second turn loop. Handlers here map v2 requests onto the
//! shared helpers (`open_session`, `reopen_session`, `run_client_turn`,
//! `set_config_value`), and [`Link`] translates every v1 `session/update` the
//! core emits into its v2 shape. The SDK's protocol router picks this
//! implementation for clients that request v2 or newer; v1 clients keep the
//! v1 implementation untouched.
//!
//! See `knowledge/specs/acp.md`, "Protocol v2".

mod activity;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

use crate::exec::worktree::WorktreeManager;
use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::v1::schema as v1s;
use agent_client_protocol::v2::schema as s;
use agent_client_protocol::v2::{self as acp2, ConnectionTo};
use agent_client_protocol::{Agent, Client, ConnectTo};
use everruns_contracts::runtime::session_task::SessionTask;
use everruns_contracts::tool_types::ToolCall as RuntimeToolCall;
use everruns_contracts::typed_id::SessionId as RuntimeSessionId;
use everruns_core::events::Event as RuntimeEvent;
use everruns_core::session_task::SessionTaskRegistry;
use everruns_core::{EventData, McpServerTransportType, ScopedMcpServer, ScopedMcpServers};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use super::{
    Peer, PeerLink, RuntimeFactory, Server, SessionExtras, StopReason, detect_client_identity,
    emit_config_options, internal_error, invalid_params, offer_setup_page, open_session,
    prompt_input, reopen_session, replay_session_history, run_client_turn, session_config_options,
    set_config_value,
};
use crate::capabilities::ToolObserver;
use crate::runtime::ClientSystemPrompt;

/// `-32002`: the session is unknown or no longer active on this connection.
const SESSION_NOT_FOUND: i32 = -32002;
/// The id of the one item plan yolop keeps per session (its todo list).
const PLAN_ID: &str = "todos";

/// The v2 side of a connection: the SDK handle plus the per-session state v2
/// adds on top of v1 (message ids, foreground work behind `state_update`,
/// terminals, file snapshots, background work and usage).
pub(in crate::editor::acp) struct Link {
    cx: ConnectionTo<Client>,
    sessions: StdMutex<HashMap<String, LinkSession>>,
    /// Ids only need to be unique per session and entity type; a process
    /// epoch keeps them unique across reconnects to the same saved session.
    epoch: u64,
    next_id: AtomicU64,
}

#[derive(Default)]
struct LinkSession {
    /// Id of the message the next chunk of each kind appends to. Any other
    /// update closes the open message, so text before and after a tool call
    /// stays two messages in the client, in order.
    agent_message: Option<String>,
    agent_thought: Option<String>,
    user_message: Option<String>,
    /// The agent message a guardrail replacement revises.
    last_agent_message: Option<String>,
    /// History replay in progress: each message starts with an empty
    /// whole-message update, as v2 asks of replayed chunked messages.
    replaying: bool,
    /// Foreground work started and not yet finished: queued prompts plus a
    /// running prompt or wake turn. `idle` is reported when it returns to 0.
    active: usize,
    /// Permission requests and forms in flight, so `requires_action` lasts
    /// until the last one is answered.
    waiting: usize,
    /// Set by `register`: the workspace (for terminal cwds and edit paths) and
    /// the task registry (for background work).
    worktree: Option<Arc<WorktreeManager>>,
    tasks: Option<Arc<dyn SessionTaskRegistry>>,
    runtime_session: Option<RuntimeSessionId>,
    /// Background tasks listed in the last snapshot, by task id.
    background: Vec<String>,
    /// Shell tool calls shown as terminals: output received so far.
    terminals: HashMap<String, Vec<u8>>,
    /// Files each edit tool call is about to change, captured before it ran.
    snapshots: HashMap<String, Vec<activity::FileSnapshot>>,
    /// Diff content waiting for the tool call's completion update.
    diffs: HashMap<String, Value>,
    /// Token usage of the foreground work since the last `idle`.
    usage: Option<s::Usage>,
    /// Tool calls the client cancelled: their final status is `cancelled`.
    cancelled: HashSet<String>,
    /// The additional workspace folders the session was opened with.
    directories: Vec<PathBuf>,
}

impl Link {
    fn new(cx: ConnectionTo<Client>) -> Self {
        let epoch = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_millis() as u64)
            .unwrap_or_default();
        Self {
            cx,
            sessions: StdMutex::new(HashMap::new()),
            epoch,
            next_id: AtomicU64::new(1),
        }
    }

    fn new_id(&self, prefix: &str) -> String {
        let n = self.next_id.fetch_add(1, Ordering::Relaxed);
        format!("{prefix}_{:x}_{n}", self.epoch)
    }

    fn with_state<T>(&self, session_id: &str, f: impl FnOnce(&mut LinkSession) -> T) -> T {
        let mut sessions = self.sessions.lock().unwrap();
        f(sessions.entry(session_id.to_string()).or_default())
    }

    fn send(&self, session_id: &str, update: s::SessionUpdate) {
        let notification = s::UpdateSessionNotification::new(session_id.to_string(), update);
        if let Err(err) = self.cx.send_notification(notification) {
            tracing::warn!(%err, "acp v2: failed to send session update");
        }
    }

    /// Send an update written as v2 JSON. Every such shape is pinned by a test.
    fn send_json(&self, session_id: &str, update: Value) {
        match serde_json::from_value::<s::SessionUpdate>(update) {
            Ok(update) => self.send(session_id, update),
            Err(err) => tracing::warn!(%err, "acp v2: malformed update"),
        }
    }

    /// Record what the session runs in, once its runtime is built.
    pub(super) fn register(
        &self,
        session_id: &str,
        worktree: Arc<WorktreeManager>,
        tasks: Arc<dyn SessionTaskRegistry>,
    ) {
        let runtime_session = session_id.parse::<RuntimeSessionId>().ok();
        self.with_state(session_id, |state| {
            state.worktree = Some(worktree);
            state.tasks = Some(tasks);
            state.runtime_session = runtime_session;
        });
    }

    fn root(&self, session_id: &str) -> Option<PathBuf> {
        self.with_state(session_id, |state| {
            state
                .worktree
                .as_ref()
                .map(|worktree| worktree.active_root())
        })
    }

    pub(super) fn session_update(&self, session_id: &str, update: v1s::SessionUpdate) {
        let translated = self.with_state(session_id, |state| {
            translate_update(state, update, || self.new_id("msg"))
        });
        match translated {
            Ok(updates) => {
                for update in updates {
                    self.send(session_id, update);
                }
            }
            Err(err) => tracing::warn!(%err, "acp v2: could not translate session update"),
        }
    }

    /// Send a complete user message, as the echo of an inserted prompt.
    fn user_message(&self, session_id: &str, message_id: &str, content: Vec<s::ContentBlock>) {
        self.close_messages(session_id);
        self.send(
            session_id,
            s::SessionUpdate::UserMessage(
                s::UserMessage::new(message_id.to_string()).content(content),
            ),
        );
    }

    fn close_messages(&self, session_id: &str) {
        self.with_state(session_id, LinkSession::close_messages);
    }

    fn set_replaying(&self, session_id: &str, replaying: bool) {
        self.with_state(session_id, |state| {
            state.replaying = replaying;
            state.close_messages();
        });
    }

    /// Just before the v1 updates for `event`. A shell command's exit and an
    /// edit's diff are settled here, so the completion update carries them.
    pub(super) fn before_event(&self, session_id: &str, event: &RuntimeEvent) {
        let EventData::ToolCompleted(data) = &event.data else {
            return;
        };
        let id = data.tool_call_id.clone();
        let (terminal, snapshots, cancelled) = self.with_state(session_id, |state| {
            (
                state.terminals.get(&id).cloned(),
                state.snapshots.remove(&id),
                state.cancelled.contains(&id),
            )
        });
        if let Some(streamed) = terminal {
            let result = tool_result_value(data);
            let (captured, exit_code) = activity::shell_result(result.as_ref());
            let output = if streamed.is_empty() {
                captured
            } else {
                streamed
            };
            let mut exit = json!({});
            if cancelled {
                // The command's process group was killed.
                exit["signal"] = json!("SIGKILL");
            } else if let Some(code) = exit_code {
                exit["exitCode"] = json!(code);
            } else if !data.success {
                exit["exitCode"] = json!(1);
            }
            self.send_json(
                session_id,
                json!({
                    "sessionUpdate": "terminal_update",
                    "terminalId": id,
                    "output": { "data": activity::base64(&output) },
                    "exitStatus": exit,
                    "capabilities": null,
                }),
            );
        }
        if let Some(diff) = snapshots.as_deref().and_then(activity::diff_content) {
            self.with_state(session_id, |state| state.diffs.insert(id, diff));
        }
    }

    /// Just after the v1 updates for `event`: what v1 has no update for.
    pub(super) fn after_event(&self, session_id: &str, event: &RuntimeEvent) {
        match &event.data {
            EventData::ToolStarted(data) => {
                let Some(command) =
                    activity::shell_command(&data.tool_call.name, &data.tool_call.arguments)
                else {
                    return;
                };
                let id = data.tool_call.id.clone();
                let cwd = self.root(session_id);
                self.with_state(session_id, |state| {
                    state.terminals.insert(id.clone(), Vec::new())
                });
                let mut terminal = json!({
                    "sessionUpdate": "terminal_update",
                    "terminalId": id,
                    "command": command,
                    "capabilities": { "cancel": {} },
                });
                if let Some(cwd) = cwd.filter(|cwd| cwd.is_absolute()) {
                    terminal["cwd"] = json!(cwd);
                }
                self.send_json(session_id, terminal);
                self.send_json(
                    session_id,
                    json!({
                        "sessionUpdate": "tool_call_update",
                        "toolCallId": id,
                        "name": "bash",
                        "content": [{ "type": "terminal", "terminalId": id }],
                        "capabilities": { "cancel": {} },
                    }),
                );
            }
            EventData::ToolOutputDelta(data) => {
                if data.delta.is_empty() {
                    return;
                }
                let terminal = self.with_state(session_id, |state| {
                    state.terminals.get_mut(&data.tool_call_id).map(|output| {
                        output.extend_from_slice(data.delta.as_bytes());
                    })
                });
                let update = if terminal.is_some() {
                    json!({
                        "sessionUpdate": "terminal_output_chunk",
                        "terminalId": data.tool_call_id,
                        "data": activity::base64(data.delta.as_bytes()),
                    })
                } else {
                    json!({
                        "sessionUpdate": "tool_call_content_chunk",
                        "toolCallId": data.tool_call_id,
                        "content": { "type": "content", "content": { "type": "text", "text": data.delta } },
                    })
                };
                self.send_json(session_id, update);
            }
            EventData::TurnCompleted(data) => {
                if let Some(usage) = &data.usage {
                    self.with_state(session_id, |state| add_usage(&mut state.usage, usage));
                }
            }
            EventData::OutputMessageReplaced(data) => {
                let id = self.with_state(session_id, |state| {
                    state
                        .agent_message
                        .clone()
                        .or_else(|| state.last_agent_message.clone())
                });
                if let Some(id) = id {
                    self.send_json(
                        session_id,
                        json!({
                            "sessionUpdate": "agent_message",
                            "messageId": id,
                            "content": [{ "type": "text", "text": data.replacement }],
                        }),
                    );
                }
            }
            _ => {}
        }
    }

    /// The approval gate's observer: capture the files an edit is about to
    /// change. Runs before the tool, unlike the event stream.
    pub(super) fn before_tool(&self, session_id: &str, tool_call: &RuntimeToolCall) {
        let Some(root) = self.root(session_id) else {
            return;
        };
        let Some(paths) = activity::edited_paths(&tool_call.name, &tool_call.arguments, &root)
        else {
            return;
        };
        let snapshots = paths
            .into_iter()
            .map(|path| activity::FileSnapshot {
                before: activity::read_snapshot(&path),
                path,
            })
            .collect();
        self.with_state(session_id, |state| {
            state.snapshots.insert(tool_call.id.clone(), snapshots)
        });
    }

    /// Re-read the session's background tasks: announce new ones as tool
    /// calls and settle finished ones, so the next state update can list
    /// exactly what is still running.
    async fn refresh_background(&self, session_id: &str) {
        let Some((tasks, runtime_session)) = self.with_state(session_id, |state| {
            Some((state.tasks.clone()?, state.runtime_session?))
        }) else {
            return;
        };
        let Ok(all) = tasks.list(runtime_session, None).await else {
            return;
        };
        let updates = self.with_state(session_id, |state| {
            let mut updates = Vec::new();
            let running: Vec<&SessionTask> = all
                .iter()
                .filter(|task| !task.state.is_terminal())
                .collect();
            for task in &running {
                if !state.background.contains(&task.id) {
                    updates.push(activity::task_update(task));
                }
            }
            for listed in &state.background {
                if running.iter().any(|task| &task.id == listed) {
                    continue;
                }
                if let Some(task) = all.iter().find(|task| &task.id == listed) {
                    updates.push(activity::task_update(task));
                } else {
                    updates.push(json!({
                        "sessionUpdate": "tool_call_update",
                        "toolCallId": activity::task_tool_call_id(listed),
                        "status": "completed",
                    }));
                }
            }
            state.background = running.iter().map(|task| task.id.clone()).collect();
            updates
        });
        for update in updates {
            self.send_json(session_id, update);
        }
    }

    fn background(&self, session_id: &str) -> Option<Vec<s::BackgroundWork>> {
        self.with_state(session_id, |state| {
            state.tasks.as_ref()?;
            Some(
                state
                    .background
                    .iter()
                    .map(|task| {
                        s::BackgroundWork::ToolCall(s::BackgroundToolCall::new(
                            activity::task_tool_call_id(task),
                        ))
                    })
                    .collect(),
            )
        })
    }

    fn running(&self, session_id: &str) -> s::SessionUpdate {
        s::SessionUpdate::StateUpdate(s::StateUpdate::Running(
            s::RunningStateUpdate::new().background(self.background(session_id)),
        ))
    }

    fn requires_action(&self, session_id: &str) -> s::SessionUpdate {
        s::SessionUpdate::StateUpdate(s::StateUpdate::RequiresAction(
            s::RequiresActionStateUpdate::new().background(self.background(session_id)),
        ))
    }

    fn idle(&self, session_id: &str, stop: Option<StopReason>) -> s::SessionUpdate {
        let usage = self.with_state(session_id, |state| state.usage.take());
        let mut update = s::IdleStateUpdate::new()
            .background(self.background(session_id))
            .usage(usage);
        if let Some(stop) = stop {
            update = update.stop_reason(stop_reason(stop));
        }
        s::SessionUpdate::StateUpdate(s::StateUpdate::Idle(update))
    }

    pub(super) async fn work_started(&self, session_id: &str) {
        let first = self.with_state(session_id, |state| {
            state.active += 1;
            state.active == 1
        });
        if first {
            self.refresh_background(session_id).await;
            self.send(session_id, self.running(session_id));
        }
    }

    pub(super) async fn work_finished(&self, session_id: &str, stop: StopReason) {
        let last = self.with_state(session_id, |state| {
            state.active = state.active.saturating_sub(1);
            if state.active == 0 {
                state.close_messages();
            }
            state.active == 0
        });
        if last {
            self.refresh_background(session_id).await;
            self.send(session_id, self.idle(session_id, Some(stop)));
        }
    }

    /// Background work changed outside foreground work (a task finished
    /// while the session was idle): report the new list.
    pub(super) async fn background_changed(&self, session_id: &str) {
        self.refresh_background(session_id).await;
        let idle = self.with_state(session_id, |state| state.active == 0);
        if idle {
            self.send(session_id, self.idle(session_id, None));
        }
    }

    /// The current state, sent before a `session/resume` response.
    async fn state_snapshot(&self, session_id: &str) {
        self.refresh_background(session_id).await;
        let (active, waiting) = self.with_state(session_id, |state| (state.active, state.waiting));
        let update = match (active, waiting) {
            (0, _) => self.idle(session_id, None),
            (_, 0) => self.running(session_id),
            _ => self.requires_action(session_id),
        };
        self.send(session_id, update);
    }

    pub(super) async fn request_permission(
        &self,
        request: v1s::RequestPermissionRequest,
    ) -> Result<Option<String>, agent_client_protocol::Error> {
        let session_id = request.session_id.to_string();
        let cwd = self.root(&session_id);
        let (upsert, request) = permission_request(request, cwd.as_deref())?;
        if let Some(upsert) = upsert {
            self.close_messages(&session_id);
            self.send_json(&session_id, upsert);
        }
        self.set_waiting(&session_id, true);
        let response = self.cx.send_request(request).block_task().await;
        self.set_waiting(&session_id, false);
        Ok(match response?.outcome {
            s::RequestPermissionOutcome::Selected(selected) => Some(selected.option_id.to_string()),
            s::RequestPermissionOutcome::Cancelled(_) => None,
            // Never read an unknown outcome as approval.
            _ => Some(String::new()),
        })
    }

    fn set_waiting(&self, session_id: &str, waiting: bool) {
        let transition = self.with_state(session_id, |state| {
            if waiting {
                state.waiting += 1;
                (state.waiting == 1).then_some(true)
            } else {
                state.waiting = state.waiting.saturating_sub(1);
                (state.waiting == 0 && state.active > 0).then_some(false)
            }
        });
        match transition {
            Some(true) => self.send(session_id, self.requires_action(session_id)),
            Some(false) => self.send(session_id, self.running(session_id)),
            None => {}
        }
    }

    /// Form elicitation for `ask_user`. The v1 and v2 form payloads share
    /// their wire shape, so the request and its answer convert through JSON.
    pub(in crate::editor::acp) async fn create_elicitation(
        &self,
        request: v1s::CreateElicitationRequest,
    ) -> Result<v1s::ElicitationAction, agent_client_protocol::Error> {
        let session_id = match &request.mode {
            v1s::ElicitationMode::Form(form) => match &form.scope {
                v1s::ElicitationScope::Session(scope) => Some(scope.session_id.to_string()),
                _ => None,
            },
            _ => None,
        };
        let request: s::CreateElicitationRequest = convert(&request)?;
        if let Some(session_id) = &session_id {
            self.set_waiting(session_id, true);
        }
        let response = self.cx.send_request(request).block_task().await;
        if let Some(session_id) = &session_id {
            self.set_waiting(session_id, false);
        }
        convert(&response?.action)
    }

    fn forget(&self, session_id: &str) {
        self.sessions.lock().unwrap().remove(session_id);
    }

    /// `tool_call/cancel` and `terminal/cancel` (a terminal's id is its
    /// call's). `false` when the call is unknown, final or not cancellable.
    ///
    /// TODO(everruns#5384): only shell commands register until the upstream
    /// act loop cancels any call; then accept every running tool call and
    /// cancel through `everruns_core::engine::tool_cancel`.
    fn cancel_call(&self, session_id: &str, tool_call_id: &str) -> bool {
        let Some(runtime) = self.with_state(session_id, |state| {
            state
                .terminals
                .contains_key(tool_call_id)
                .then_some(state.runtime_session)
                .flatten()
        }) else {
            return false;
        };
        if !crate::runtime::tool_cancel::shared().cancel(runtime, tool_call_id) {
            return false;
        }
        self.with_state(session_id, |state| {
            state.cancelled.insert(tool_call_id.to_string())
        });
        true
    }

    fn set_directories(&self, session_id: &str, directories: Vec<PathBuf>) {
        self.with_state(session_id, |state| state.directories = directories);
    }

    fn directories(&self, session_id: &str) -> Vec<PathBuf> {
        self.with_state(session_id, |state| state.directories.clone())
    }
}

impl LinkSession {
    fn close_messages(&mut self) {
        if let Some(id) = self.agent_message.take() {
            self.last_agent_message = Some(id);
        }
        self.agent_thought = None;
        self.user_message = None;
    }
}

fn add_usage(total: &mut Option<s::Usage>, usage: &everruns_core::TokenUsage) {
    let input = u64::from(usage.input_tokens);
    let output = u64::from(usage.output_tokens);
    let read = usage.cache_read_tokens.map(u64::from);
    let write = usage.cache_creation_tokens.map(u64::from);
    let sum = |a: Option<u64>, b: Option<u64>| match (a, b) {
        (None, None) => None,
        (a, b) => Some(a.unwrap_or(0) + b.unwrap_or(0)),
    };
    let next = match total.take() {
        Some(previous) => {
            let input = previous.input_tokens + input;
            let output = previous.output_tokens + output;
            s::Usage::new(input + output, input, output)
                .cached_read_tokens(sum(previous.cached_read_tokens, read))
                .cached_write_tokens(sum(previous.cached_write_tokens, write))
        }
        None => s::Usage::new(input + output, input, output)
            .cached_read_tokens(read)
            .cached_write_tokens(write),
    };
    *total = Some(next);
}

/// The JSON payload a tool returned, the runtime's convention of stashing
/// structured results as JSON text in the first text part.
fn tool_result_value(data: &everruns_core::ToolCompletedData) -> Option<Value> {
    data.result.as_ref()?.iter().find_map(|part| match part {
        everruns_core::ContentPart::Text(text) => serde_json::from_str(&text.text).ok(),
        _ => None,
    })
}

fn stop_reason(stop: StopReason) -> s::StopReason {
    match stop {
        StopReason::EndTurn => s::StopReason::EndTurn,
        StopReason::MaxTokens => s::StopReason::MaxTokens,
        StopReason::MaxTurnRequests => s::StopReason::MaxTurnRequests,
        StopReason::Refusal => s::StopReason::Refusal,
        StopReason::Cancelled => s::StopReason::Cancelled,
        _ => s::StopReason::EndTurn,
    }
}

/// Serialize one protocol's value and read it back as the other's. Used only
/// where the two versions share a wire shape; a mismatch is an internal error,
/// and the translation tests pin each shape yolop sends.
fn convert<A: Serialize, B: DeserializeOwned>(
    value: &A,
) -> Result<B, agent_client_protocol::Error> {
    let value = serde_json::to_value(value).map_err(|e| internal_error(e.to_string()))?;
    serde_json::from_value(value).map_err(|e| internal_error(e.to_string()))
}

/// Translate one v1 `session/update` into its v2 form: usually one update,
/// none for updates v2 dropped (`current_mode_update`: the `mode` config
/// option replaces it), two when a replayed message opens with its marker.
fn translate_update(
    state: &mut LinkSession,
    update: v1s::SessionUpdate,
    mut new_id: impl FnMut() -> String,
) -> Result<Vec<s::SessionUpdate>, agent_client_protocol::Error> {
    let mut value = serde_json::to_value(&update).map_err(|e| internal_error(e.to_string()))?;
    let kind = value
        .get("sessionUpdate")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let Some(object) = value.as_object_mut() else {
        return Ok(Vec::new());
    };
    let mut before = None;
    match kind.as_str() {
        "agent_message_chunk" | "agent_thought_chunk" | "user_message_chunk" => {
            let (open, whole) = match kind.as_str() {
                "agent_message_chunk" => (&mut state.agent_message, "agent_message"),
                "agent_thought_chunk" => (&mut state.agent_thought, "agent_thought"),
                _ => (&mut state.user_message, "user_message"),
            };
            let opened = open.is_none();
            let id = open.get_or_insert_with(&mut new_id).clone();
            if opened && state.replaying {
                before = Some(json!({ "sessionUpdate": whole, "messageId": id, "content": [] }));
            }
            // A different kind of chunk starts a new message of its own.
            if kind != "agent_message_chunk"
                && let Some(closed) = state.agent_message.take()
            {
                state.last_agent_message = Some(closed);
            }
            if kind != "agent_thought_chunk" {
                state.agent_thought = None;
            }
            if kind != "user_message_chunk" {
                state.user_message = None;
            }
            // v1 may carry its own optional id; v2 requires the link's.
            object.insert("messageId".into(), Value::String(id));
        }
        "tool_call" | "tool_call_update" => {
            state.close_messages();
            // v2 has no separate create: the first upsert for an id creates it.
            object.insert("sessionUpdate".into(), json!("tool_call_update"));
            let id = object
                .get("toolCallId")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            // A concrete content array replaces the call's content, so it
            // keeps the terminal and gains the edit's diff.
            if let Some(Value::Array(content)) = object.get_mut("content") {
                if let Some(diff) = state.diffs.remove(&id) {
                    content.insert(0, diff);
                }
                if state.terminals.contains_key(&id) {
                    content.insert(0, json!({ "type": "terminal", "terminalId": id }));
                }
            }
            let status = object
                .get("status")
                .and_then(Value::as_str)
                .map(str::to_string);
            let status = status.as_deref();
            if matches!(status, Some("completed" | "failed")) {
                // A final status ends every control the call offered.
                if state.terminals.contains_key(&id) {
                    object.insert("capabilities".into(), Value::Null);
                }
                if status == Some("failed") && state.cancelled.remove(&id) {
                    object.insert("status".into(), json!("cancelled"));
                }
            }
        }
        "plan" => {
            state.close_messages();
            let entries = object.remove("entries").unwrap_or_else(|| json!([]));
            let meta = object.remove("_meta");
            object.clear();
            object.insert("sessionUpdate".into(), json!("plan_update"));
            object.insert(
                "plan".into(),
                json!({ "type": "items", "planId": PLAN_ID, "entries": entries }),
            );
            if let Some(meta) = meta {
                object.insert("_meta".into(), meta);
            }
        }
        "available_commands_update" => {
            if let Some(Value::Array(commands)) = object.get_mut("availableCommands") {
                for command in commands {
                    v2_command(command);
                }
            }
        }
        "config_option_update" => {
            if let Some(Value::Array(options)) = object.get_mut("configOptions") {
                for option in options {
                    v2_config_option(option);
                }
            }
        }
        "session_info_update" | "usage_update" => {}
        // v2 removed modes; the `mode` config option carries the level.
        "current_mode_update" => return Ok(Vec::new()),
        other => {
            tracing::debug!(kind = other, "acp v2: dropping update with no v2 form");
            return Ok(Vec::new());
        }
    }
    let mut updates = Vec::new();
    for value in before.into_iter().chain(std::iter::once(value)) {
        updates.push(
            serde_json::from_value(value).map_err(|e| internal_error(format!("{kind}: {e}")))?,
        );
    }
    Ok(updates)
}

/// v1 command → v2: commands and their input gain a `type` discriminator.
fn v2_command(command: &mut Value) {
    let Some(command) = command.as_object_mut() else {
        return;
    };
    command.insert("type".into(), json!("command"));
    if let Some(Value::Object(input)) = command.get_mut("input")
        && !input.contains_key("type")
    {
        input.insert("type".into(), json!("text"));
    }
}

/// v1 config option → v2: `id` is `configId` and a group's `group` is `groupId`.
fn v2_config_option(option: &mut Value) {
    let Some(option) = option.as_object_mut() else {
        return;
    };
    if let Some(id) = option.remove("id") {
        option.insert("configId".into(), id);
    }
    if let Some(Value::Array(entries)) = option.get_mut("options") {
        for entry in entries {
            if let Some(entry) = entry.as_object_mut()
                && let Some(group) = entry.remove("group")
            {
                entry.insert("groupId".into(), group);
            }
        }
    }
}

fn config_options(options: Vec<v1s::SessionConfigOption>) -> Vec<s::SessionConfigOption> {
    options
        .into_iter()
        .filter_map(|option| {
            let mut value = serde_json::to_value(option).ok()?;
            v2_config_option(&mut value);
            serde_json::from_value(value)
                .inspect_err(|err| tracing::warn!(%err, "acp v2: config option did not convert"))
                .ok()
        })
        .collect()
}

fn available_commands(commands: Vec<v1s::AvailableCommand>) -> Vec<s::AvailableCommand> {
    commands
        .into_iter()
        .filter_map(|command| {
            let mut value = serde_json::to_value(command).ok()?;
            v2_command(&mut value);
            serde_json::from_value(value)
                .inspect_err(|err| tracing::warn!(%err, "acp v2: command did not convert"))
                .ok()
        })
        .collect()
}

/// v1 `session/request_permission` → v2. A shell command becomes a
/// `command` subject (what will run, and where); any other gated tool call
/// becomes a `tool_call` subject. The tool's name is the required `title`.
///
/// Returns the request and, for a command, the tool call upsert to send
/// first, so the subject's `toolCallId` names a call the client knows.
fn permission_request(
    request: v1s::RequestPermissionRequest,
    cwd: Option<&Path>,
) -> Result<(Option<Value>, s::RequestPermissionRequest), agent_client_protocol::Error> {
    let title = request
        .tool_call
        .fields
        .title
        .clone()
        .unwrap_or_else(|| "Run tool".to_string());
    let mut tool_call =
        serde_json::to_value(&request.tool_call).map_err(|e| internal_error(e.to_string()))?;
    let options =
        serde_json::to_value(&request.options).map_err(|e| internal_error(e.to_string()))?;
    let command = (request.tool_call.fields.kind == Some(v1s::ToolKind::Execute))
        .then(|| {
            request
                .tool_call
                .fields
                .raw_input
                .as_ref()?
                .get("command")?
                .as_str()
                .map(str::to_string)
        })
        .flatten();
    let (upsert, subject) = match (command, cwd.filter(|cwd| cwd.is_absolute())) {
        (Some(command), Some(cwd)) => {
            let id = request.tool_call.tool_call_id.to_string();
            tool_call["sessionUpdate"] = json!("tool_call_update");
            (
                Some(tool_call),
                json!({ "type": "command", "command": command, "cwd": cwd, "toolCallId": id }),
            )
        }
        _ => (None, json!({ "type": "tool_call", "toolCall": tool_call })),
    };
    let request = serde_json::from_value(json!({
        "sessionId": request.session_id.to_string(),
        "title": format!("Allow {title}?"),
        "subject": subject,
        "options": options,
    }))
    .map_err(|e| internal_error(e.to_string()))?;
    Ok((upsert, request))
}

/// v2 prompt content → the v1 blocks the core reads. Same wire shape.
fn prompt_blocks(
    blocks: &[s::ContentBlock],
) -> Result<Vec<v1s::ContentBlock>, agent_client_protocol::Error> {
    blocks
        .iter()
        .map(|block| convert(block).map_err(|e| invalid_params(e.message.clone())))
        .collect()
}

fn scoped_mcp_servers(
    servers: &[s::McpServer],
) -> Result<ScopedMcpServers, agent_client_protocol::Error> {
    let mut scoped = ScopedMcpServers::new();
    let mut names = HashSet::new();
    for server in servers {
        let (name, entry) = match server {
            s::McpServer::Http(http) => (
                http.name.clone(),
                ScopedMcpServer {
                    transport_type: McpServerTransportType::Http,
                    url: http.url.clone(),
                    headers: http
                        .headers
                        .iter()
                        .map(|h| (h.name.clone(), h.value.clone()))
                        .collect(),
                    ..ScopedMcpServer::default()
                },
            ),
            s::McpServer::Stdio(stdio) => (
                stdio.name.clone(),
                ScopedMcpServer {
                    transport_type: McpServerTransportType::Stdio,
                    command: Some(stdio.command.0.to_string_lossy().into_owned()),
                    args: stdio.args.clone(),
                    env: stdio
                        .env
                        .iter()
                        .map(|e| (e.name.clone(), e.value.clone()))
                        .collect(),
                    ..ScopedMcpServer::default()
                },
            ),
            other => {
                return Err(invalid_params(format!(
                    "unsupported mcp transport for server: {other:?}"
                )));
            }
        };
        if !names.insert(name.clone()) {
            return Err(invalid_params(format!(
                "duplicate mcp server name `{name}`"
            )));
        }
        scoped.insert(name, entry);
    }
    Ok(scoped)
}

fn session_not_found(session_id: &str) -> agent_client_protocol::Error {
    agent_client_protocol::Error::new(SESSION_NOT_FOUND, "Session not found")
        .data(json!({ "sessionId": session_id }))
}

fn initialize_response(auth_methods: Vec<v1s::AuthMethod>) -> s::InitializeResponse {
    let auth_methods = auth_methods
        .iter()
        .filter_map(|method| {
            // Yolop offers only agent-run login; anything else has no v2
            // form yet and is skipped, loudly so a drift is visible.
            let v1s::AuthMethod::Agent(agent) = method else {
                tracing::warn!(id = %method.id(), "acp v2: skipping non-agent auth method");
                return None;
            };
            let mut value = serde_json::to_value(agent)
                .inspect_err(|err| tracing::warn!(%err, "acp v2: auth method did not serialize"))
                .ok()?;
            let object = value.as_object_mut()?;
            if let Some(id) = object.remove("id") {
                object.insert("methodId".into(), id);
            }
            object.insert("type".into(), json!("agent"));
            serde_json::from_value(value)
                .inspect_err(|err| tracing::warn!(%err, "acp v2: auth method did not convert"))
                .ok()
        })
        .collect();
    s::InitializeResponse::new(
        ProtocolVersion::V2,
        s::Implementation::new("yolop", env!("CARGO_PKG_VERSION")).title("Yolop"),
    )
    .auth_methods(auth_methods)
    .capabilities(
        s::AgentCapabilities::new().session(
            s::SessionCapabilities::new()
                .prompt(
                    s::PromptCapabilities::new()
                        .image(s::PromptImageCapabilities::new())
                        .embedded_context(s::PromptEmbeddedContextCapabilities::new()),
                )
                .mcp(
                    s::McpCapabilities::new()
                        .stdio(s::McpStdioCapabilities::new())
                        .http(s::McpHttpCapabilities::new()),
                )
                .delete(s::SessionDeleteCapabilities::new())
                .additional_directories(s::SessionAdditionalDirectoriesCapabilities::new())
                .system_prompt(
                    s::SessionSystemPromptCapabilities::new().mode(
                        s::SessionSystemPromptModeCapabilities::new()
                            .override_(s::SessionSystemPromptOverrideCapabilities::new()),
                    ),
                ),
        ),
    )
}

/// The protocol v2 agent. Handlers capture the same [`Server`] the v1 agent
/// does; the router runs only one of them on a connection.
pub(super) fn agent<F: RuntimeFactory>(
    server: Arc<Server<F>>,
    next_tool_id: Arc<AtomicI64>,
) -> impl ConnectTo<Client> {
    // The SDK hands every handler the same connection, but only once one is
    // running; the link is built from the first handler that needs it.
    let link: Arc<StdMutex<Option<Arc<Link>>>> = Arc::new(StdMutex::new(None));
    let peer_for = {
        let link = link.clone();
        move |cx: &ConnectionTo<Client>| -> (Arc<Link>, Arc<Peer>) {
            let link = link
                .lock()
                .unwrap()
                .get_or_insert_with(|| Arc::new(Link::new(cx.clone())))
                .clone();
            let peer = Arc::new(Peer {
                link: PeerLink::V2(link.clone()),
                next_id: next_tool_id.clone(),
            });
            (link, peer)
        }
    };

    Agent
        .v2()
        .name("yolop")
        .on_receive_request(
            {
                let server = server.clone();
                async move |params: s::InitializeRequest, responder: acp2::Responder<_>, _cx| {
                    let identity = detect_client_identity(
                        Some(&v1s::Implementation::new(
                            params.info.name.clone(),
                            params.info.version.clone(),
                        )),
                        params.meta.as_ref(),
                    );
                    server.factory.note_client_identity(identity);
                    let forms = params
                        .capabilities
                        .elicitation
                        .as_ref()
                        .is_some_and(|elicitation| elicitation.form.is_some());
                    server.elicitation_forms.store(forms, Ordering::Release);
                    responder.respond(initialize_response(server.factory.auth_methods()))
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let server = server.clone();
                let peer_for = peer_for.clone();
                async move |params: s::LoginAuthRequest, responder: acp2::Responder<_>, cx| {
                    let method_id = params.method_id.to_string();
                    if !server
                        .factory
                        .auth_methods()
                        .iter()
                        .any(|method| method.id().to_string() == method_id)
                    {
                        return responder.respond_with_error(invalid_params(format!(
                            "unknown authentication method `{method_id}`"
                        )));
                    }
                    match server.factory.authenticate(&method_id).await {
                        Ok(()) => {
                            responder.respond(s::LoginAuthResponse::new())?;
                            let (_, peer) = peer_for(&cx);
                            for session in server.sessions() {
                                emit_config_options(&peer, &session).await;
                            }
                            Ok(())
                        }
                        Err(err) => responder
                            .respond_with_error(internal_error(format!("auth/login: {err}"))),
                    }
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let server = server.clone();
                let peer_for = peer_for.clone();
                async move |_params: s::LogoutAuthRequest, responder: acp2::Responder<_>, cx| {
                    match server.factory.logout().await {
                        Ok(()) => {
                            responder.respond(s::LogoutAuthResponse::new())?;
                            let (_, peer) = peer_for(&cx);
                            for session in server.sessions() {
                                emit_config_options(&peer, &session).await;
                            }
                            Ok(())
                        }
                        Err(err) => responder
                            .respond_with_error(internal_error(format!("auth/logout: {err}"))),
                    }
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let server = server.clone();
                let peer_for = peer_for.clone();
                async move |params: s::NewSessionRequest, responder: acp2::Responder<_>, cx| {
                    let (link, peer) = peer_for(&cx);
                    let mcp = match scoped_mcp_servers(&params.mcp_servers) {
                        Ok(mcp) => mcp,
                        Err(err) => return responder.respond_with_error(err),
                    };
                    let extras = match session_extras(
                        &link,
                        params.system_prompt.as_ref(),
                        &params.additional_directories,
                    ) {
                        Ok(extras) => extras,
                        Err(err) => return responder.respond_with_error(err),
                    };
                    let directories = extras.additional_directories.clone();
                    let session =
                        match open_session(&server, &peer, params.cwd.0, mcp, extras).await {
                            Ok(session) => session,
                            Err(err) => return responder.respond_with_error(err),
                        };
                    link.set_directories(&session.acp_id, directories);
                    let options = session_config_options(&session).await;
                    *session.last_config_options.lock().unwrap() = Some(options.clone());
                    let commands = session.commands.lock().unwrap().clone();
                    responder.respond(
                        s::NewSessionResponse::new(session.acp_id.clone())
                            .config_options(config_options(options))
                            .available_commands(available_commands(super::available_commands(
                                &commands,
                            ))),
                    )?;
                    offer_setup_page(&server, &peer, &session).await;
                    super::maybe_spawn_settings_watcher(&server, &peer);
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let server = server.clone();
                let peer_for = peer_for.clone();
                async move |params: s::ResumeSessionRequest, responder: acp2::Responder<_>, cx| {
                    let (link, peer) = peer_for(&cx);
                    let session_id = params.session_id.to_string();
                    let replay = match &params.replay_from {
                        None => false,
                        Some(s::ReplayFrom::Start(_)) => true,
                        Some(_) => {
                            return responder.respond_with_error(invalid_params(
                                "unsupported replayFrom cursor; only `start` is supported",
                            ));
                        }
                    };
                    let mcp = match scoped_mcp_servers(&params.mcp_servers) {
                        Ok(mcp) => mcp,
                        Err(err) => return responder.respond_with_error(err),
                    };
                    let extras = match session_extras(
                        &link,
                        params.system_prompt.as_ref(),
                        &params.additional_directories,
                    ) {
                        Ok(extras) => extras,
                        Err(err) => return responder.respond_with_error(err),
                    };
                    let directories = extras.additional_directories.clone();
                    // v2 answers an unknown session with `-32002`, so check
                    // before `reopen_session`, whose v1 error is invalid params.
                    let known = server.session(&session_id).is_some()
                        || match session_id.parse::<RuntimeSessionId>() {
                            Ok(id) => server.factory.session_exists(id).await,
                            Err(_) => false,
                        };
                    if !known {
                        return responder.respond_with_error(session_not_found(&session_id));
                    }
                    // The request's folders are the session's complete list.
                    // A live session built with others is rebuilt with these.
                    if let Some(live) = server.session(&session_id)
                        && link.directories(&session_id) != directories
                    {
                        close_live_session(&server, &link, &live).await;
                    }
                    let session = match reopen_session(
                        &server,
                        &peer,
                        &session_id,
                        params.cwd.0,
                        mcp,
                        extras,
                    )
                    .await
                    {
                        Ok(session) => session,
                        Err(err) => return responder.respond_with_error(err),
                    };
                    session.closed.store(false, Ordering::Release);
                    link.set_directories(&session_id, directories);
                    if replay {
                        link.set_replaying(&session_id, true);
                        let replayed = replay_session_history(&peer, &session).await;
                        link.set_replaying(&session_id, false);
                        if let Err(err) = replayed {
                            return responder.respond_with_error(err);
                        }
                    }
                    // Current state, never history: the snapshot follows the
                    // replay and precedes the response.
                    link.state_snapshot(&session_id).await;
                    let options = session_config_options(&session).await;
                    *session.last_config_options.lock().unwrap() = Some(options.clone());
                    let commands = session.commands.lock().unwrap().clone();
                    responder.respond(
                        s::ResumeSessionResponse::new()
                            .config_options(config_options(options))
                            .available_commands(available_commands(super::available_commands(
                                &commands,
                            ))),
                    )?;
                    super::maybe_spawn_settings_watcher(&server, &peer);
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let server = server.clone();
                async move |params: s::ListSessionsRequest, responder: acp2::Responder<_>, _cx| {
                    let cwd = params.cwd.as_ref().map(|cwd| cwd.0.clone());
                    let sessions = server
                        .factory
                        .list_sessions(cwd.as_deref())
                        .await
                        .into_iter()
                        .map(session_info)
                        .collect();
                    responder.respond(s::ListSessionsResponse::new(sessions))
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let server = server.clone();
                let peer_for = peer_for.clone();
                async move |params: s::CloseSessionRequest, responder: acp2::Responder<_>, cx| {
                    let (link, _) = peer_for(&cx);
                    let session_id = params.session_id.to_string();
                    let Some(session) = server.session(&session_id) else {
                        return responder.respond_with_error(session_not_found(&session_id));
                    };
                    close_live_session(&server, &link, &session).await;
                    responder.respond(s::CloseSessionResponse::new())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let server = server.clone();
                let peer_for = peer_for.clone();
                async move |params: s::DeleteSessionRequest, responder: acp2::Responder<_>, cx| {
                    let (link, _) = peer_for(&cx);
                    let session_id = params.session_id.to_string();
                    let Ok(runtime_id) = session_id.parse::<RuntimeSessionId>() else {
                        return responder.respond_with_error(session_not_found(&session_id));
                    };
                    let live = server.session(&session_id);
                    if live.is_none() && !server.factory.session_exists(runtime_id).await {
                        return responder.respond_with_error(session_not_found(&session_id));
                    }
                    // Deleting an active session closes it first, the same way
                    // `session/close` does.
                    if let Some(session) = live {
                        close_live_session(&server, &link, &session).await;
                    }
                    match server.factory.delete_session(runtime_id).await {
                        Ok(()) => responder.respond(s::DeleteSessionResponse::new()),
                        Err(err) => responder
                            .respond_with_error(internal_error(format!("session/delete: {err}"))),
                    }
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let server = server.clone();
                let peer_for = peer_for.clone();
                async move |params: s::PromptRequest, responder: acp2::Responder<_>, cx| {
                    let (link, peer) = peer_for(&cx);
                    let session_id = params.session_id.to_string();
                    let Some(session) = server.session(&session_id) else {
                        return responder.respond_with_error(session_not_found(&session_id));
                    };
                    // Everything that can reject the prompt happens before the
                    // response: an error means the message was not inserted.
                    let blocks = match prompt_blocks(&params.prompt) {
                        Ok(blocks) => blocks,
                        Err(err) => return responder.respond_with_error(err),
                    };
                    let input = match prompt_input(&session.model, &blocks) {
                        Ok(input) => input,
                        Err(err) => {
                            return responder.respond_with_error(invalid_params(format!(
                                "invalid prompt image: {err}"
                            )));
                        }
                    };
                    let message_id = link.new_id("user");
                    // Count the work before answering, so a `session/close`
                    // racing this prompt still sees it.
                    peer.work_started(&session_id).await;
                    if let Err(err) = responder.respond(s::PromptResponse::new(message_id.clone()))
                    {
                        peer.work_finished(&session_id, StopReason::Cancelled).await;
                        return Err(err);
                    }
                    link.user_message(&session_id, &message_id, params.prompt);
                    // The turn runs off the dispatch loop so `session/cancel`,
                    // permission answers and further prompts keep flowing.
                    // Prompts for a busy session queue on its turn lock.
                    tokio::spawn({
                        let server = server.clone();
                        async move {
                            let stop =
                                run_client_turn(&server, peer.clone(), session, &blocks, input)
                                    .await;
                            peer.work_finished(&session_id, stop).await;
                        }
                    });
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let server = server.clone();
                async move |params: s::SetSessionConfigOptionRequest,
                            responder: acp2::Responder<_>,
                            _cx| {
                    let session_id = params.session_id.to_string();
                    let Some(session) = server.session(&session_id) else {
                        return responder.respond_with_error(session_not_found(&session_id));
                    };
                    let s::SessionConfigOptionValue::Id { value } = &params.value else {
                        return responder.respond_with_error(invalid_params(
                            "session config options take a value id",
                        ));
                    };
                    if let Err(err) = set_config_value(
                        &session,
                        &params.config_id.to_string(),
                        &value.to_string(),
                    )
                    .await
                    {
                        return responder.respond_with_error(err);
                    }
                    let options = session_config_options(&session).await;
                    *session.last_config_options.lock().unwrap() = Some(options.clone());
                    responder.respond(s::SetSessionConfigOptionResponse::new(config_options(
                        options,
                    )))
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let peer_for = peer_for.clone();
                async move |params: s::CancelToolCallRequest, responder: acp2::Responder<_>, cx| {
                    let (link, _) = peer_for(&cx);
                    let session_id = params.session_id.to_string();
                    let tool_call_id = params.tool_call_id.to_string();
                    if link.cancel_call(&session_id, &tool_call_id) {
                        responder.respond(s::CancelToolCallResponse::new())
                    } else {
                        responder.respond_with_error(invalid_params(format!(
                            "tool call `{tool_call_id}` is not running or cannot be cancelled"
                        )))
                    }
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let peer_for = peer_for.clone();
                async move |params: s::CancelTerminalRequest, responder: acp2::Responder<_>, cx| {
                    let (link, _) = peer_for(&cx);
                    let session_id = params.session_id.to_string();
                    let terminal_id = params.terminal_id.to_string();
                    if link.cancel_call(&session_id, &terminal_id) {
                        responder.respond(s::CancelTerminalResponse::new())
                    } else {
                        responder.respond_with_error(invalid_params(format!(
                            "terminal `{terminal_id}` is not running or cannot be cancelled"
                        )))
                    }
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            {
                let server = server.clone();
                async move |params: s::CancelSessionNotification, _cx| {
                    if let Some(session) = server.session(&params.session_id.to_string()) {
                        session.trigger_cancel();
                    }
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_notification!(),
        )
}

/// Stop queued and running work, wait for the running turn to send its
/// final updates (and `idle: cancelled`), then forget the session. Shared by
/// `session/close` and `session/delete`.
async fn close_live_session<F: RuntimeFactory>(
    server: &Arc<Server<F>>,
    link: &Link,
    session: &Arc<super::Session>,
) {
    session.closed.store(true, Ordering::Release);
    session.trigger_cancel();
    let _turn = session.turn_lock.lock().await;
    server.sessions.lock().unwrap().remove(&session.acp_id);
    session.handles.flush_trace_exporters().await;
    link.forget(&session.acp_id);
}

/// What a v2 session adds to its runtime: the link's file snapshot observer
/// and the client's system prompt, if it sent one.
fn session_extras(
    link: &Arc<Link>,
    system_prompt: Option<&s::SystemPrompt>,
    additional_directories: &[s::AbsolutePath],
) -> Result<SessionExtras, agent_client_protocol::Error> {
    let observer: ToolObserver = {
        let link = Arc::downgrade(link);
        Arc::new(
            move |session_id: RuntimeSessionId, tool_call: &RuntimeToolCall| {
                if let Some(link) = link.upgrade() {
                    link.before_tool(&session_id.to_string(), tool_call);
                }
            },
        )
    };
    Ok(SessionExtras {
        tool_observer: Some(observer),
        system_prompt: system_prompt.map(client_system_prompt).transpose()?,
        additional_directories: workspace_folders(additional_directories)?,
    })
}

/// `additionalDirectories` → existing directories. A folder that is not
/// there is the client's error, reported before the session opens.
fn workspace_folders(
    directories: &[s::AbsolutePath],
) -> Result<Vec<PathBuf>, agent_client_protocol::Error> {
    directories
        .iter()
        .map(|directory| {
            let path = directory.0.clone();
            if path.is_absolute() && path.is_dir() {
                Ok(path)
            } else {
                Err(invalid_params(format!(
                    "additionalDirectories: not an existing absolute directory: {}",
                    path.display()
                )))
            }
        })
        .collect()
}

/// v2 `systemPrompt` → the runtime's. Text blocks only: the system prompt is
/// instructions, and the harness has nowhere to put an image or resource.
fn client_system_prompt(
    prompt: &s::SystemPrompt,
) -> Result<ClientSystemPrompt, agent_client_protocol::Error> {
    let mut text = Vec::new();
    for block in &prompt.content {
        match block {
            s::ContentBlock::Text(block) => text.push(block.text.as_str()),
            _ => return Err(invalid_params("systemPrompt supports text content only")),
        }
    }
    let text = text.join("\n\n");
    match &prompt.mode {
        s::SystemPromptMode::Append => Ok(ClientSystemPrompt::Append(text)),
        s::SystemPromptMode::Override => Ok(ClientSystemPrompt::Override(text)),
        other => Err(invalid_params(format!(
            "unsupported systemPrompt mode `{other:?}`"
        ))),
    }
}

/// Remove a saved session's directory. The handler has already checked the
/// session exists.
pub(in crate::editor::acp) async fn delete_saved_session(
    sessions_dir: &Path,
    session_id: RuntimeSessionId,
) -> anyhow::Result<()> {
    let dir = crate::runtime::session_log::session_dir_path(sessions_dir, session_id);
    tokio::fs::remove_dir_all(&dir).await?;
    Ok(())
}

/// A saved session, as `session/list` reports it.
pub(crate) struct SavedSession {
    pub session_id: String,
    pub cwd: PathBuf,
    pub updated_at: Option<std::time::SystemTime>,
}

fn session_info(saved: SavedSession) -> s::SessionInfo {
    let mut info = s::SessionInfo::new(saved.session_id, saved.cwd);
    if let Some(updated) = saved.updated_at {
        let updated: chrono::DateTime<chrono::Utc> = updated.into();
        info = info.updated_at(updated.to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
    }
    info
}

/// Saved sessions under `sessions_dir`, newest first, optionally only those
/// whose workspace is `cwd`. Sessions with no recorded workspace are skipped:
/// v2 requires an absolute `cwd` for each.
pub(in crate::editor::acp) fn list_saved_sessions(
    sessions_dir: &Path,
    cwd: Option<&Path>,
) -> Vec<SavedSession> {
    let Ok(entries) = std::fs::read_dir(sessions_dir) else {
        return Vec::new();
    };
    let mut sessions = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().to_str()?.to_string();
            let id = name
                .parse::<everruns_contracts::typed_id::SessionId>()
                .ok()?;
            let dir = entry.path();
            let workspace = crate::runtime::session_log::read_session_workspace(&dir)
                .ok()
                .flatten()?;
            if !workspace.is_absolute() || cwd.is_some_and(|cwd| cwd != workspace) {
                return None;
            }
            let modified = crate::runtime::saved_sessions::modified(&dir, id);
            Some(SavedSession {
                session_id: name,
                cwd: workspace,
                updated_at: Some(modified),
            })
        })
        .collect::<Vec<_>>();
    sessions.sort_by_key(|session| std::cmp::Reverse(session.updated_at));
    sessions
}

#[cfg(test)]
mod tests {
    use super::*;

    fn translate(state: &mut LinkSession, update: v1s::SessionUpdate) -> Value {
        let mut updates = translate_all(state, update);
        assert_eq!(updates.len(), 1, "one v2 update: {updates:?}");
        updates.remove(0)
    }

    fn translate_all(state: &mut LinkSession, update: v1s::SessionUpdate) -> Vec<Value> {
        // Ids come from the link's counter; a fresh one per call is enough
        // to tell an open message (same id) from a new one.
        static NEXT: AtomicU64 = AtomicU64::new(1);
        translate_update(state, update, || {
            format!("id{}", NEXT.fetch_add(1, Ordering::Relaxed))
        })
        .expect("translates")
        .into_iter()
        .map(|update| serde_json::to_value(update).unwrap())
        .collect()
    }

    fn text(text: &str) -> v1s::ContentChunk {
        v1s::ContentChunk::new(v1s::ContentBlock::Text(v1s::TextContent::new(text)))
    }

    #[test]
    fn chunks_share_a_message_id_until_another_update_closes_it() {
        let mut state = LinkSession::default();
        let first = translate(&mut state, v1s::SessionUpdate::AgentMessageChunk(text("a")));
        let second = translate(&mut state, v1s::SessionUpdate::AgentMessageChunk(text("b")));
        assert_eq!(first["messageId"], second["messageId"]);
        assert_eq!(first["sessionUpdate"], "agent_message_chunk");

        let tool = translate(
            &mut state,
            v1s::SessionUpdate::ToolCall(v1s::ToolCall::new("call_1", "Read file")),
        );
        assert_eq!(tool["sessionUpdate"], "tool_call_update");
        assert_eq!(tool["toolCallId"], "call_1");
        assert_eq!(tool["title"], "Read file");

        let after = translate(&mut state, v1s::SessionUpdate::AgentMessageChunk(text("c")));
        assert_ne!(
            after["messageId"], first["messageId"],
            "the tool call closed it"
        );

        let thought = translate(&mut state, v1s::SessionUpdate::AgentThoughtChunk(text("t")));
        let resumed = translate(&mut state, v1s::SessionUpdate::AgentMessageChunk(text("d")));
        assert_ne!(thought["messageId"], after["messageId"]);
        assert_ne!(
            resumed["messageId"], after["messageId"],
            "a thought closes it too"
        );
    }

    #[test]
    fn plan_becomes_an_items_plan_update() {
        let mut state = LinkSession::default();
        let plan = translate(
            &mut state,
            v1s::SessionUpdate::Plan(v1s::Plan::new(vec![v1s::PlanEntry::new(
                "write tests",
                v1s::PlanEntryPriority::Medium,
                v1s::PlanEntryStatus::InProgress,
            )])),
        );
        assert_eq!(plan["sessionUpdate"], "plan_update");
        assert_eq!(plan["plan"]["type"], "items");
        assert_eq!(plan["plan"]["planId"], PLAN_ID);
        assert_eq!(plan["plan"]["entries"][0]["content"], "write tests");
    }

    #[test]
    fn commands_and_config_options_gain_v2_discriminators() {
        let mut state = LinkSession::default();
        let commands = translate(
            &mut state,
            v1s::SessionUpdate::AvailableCommandsUpdate(v1s::AvailableCommandsUpdate::new(vec![
                v1s::AvailableCommand::new("setup", "Connect a provider").input(
                    v1s::AvailableCommandInput::Unstructured(v1s::UnstructuredCommandInput::new(
                        "provider",
                    )),
                ),
            ])),
        );
        let command = &commands["availableCommands"][0];
        assert_eq!(command["type"], "command");
        assert_eq!(command["input"]["type"], "text");
        assert_eq!(command["input"]["hint"], "provider");

        let options = translate(
            &mut state,
            v1s::SessionUpdate::ConfigOptionUpdate(v1s::ConfigOptionUpdate::new(vec![
                v1s::SessionConfigOption::select(
                    "mode",
                    "Approval",
                    "normal",
                    vec![v1s::SessionConfigSelectOption::new("normal", "Normal")],
                )
                .category(v1s::SessionConfigOptionCategory::Mode),
            ])),
        );
        let option = &options["configOptions"][0];
        assert_eq!(option["configId"], "mode");
        assert_eq!(option["category"], "mode");
        assert_eq!(option["currentValue"], "normal");
    }

    #[test]
    fn mode_updates_have_no_v2_form() {
        let mut state = LinkSession::default();
        let update = translate_update(
            &mut state,
            v1s::SessionUpdate::CurrentModeUpdate(v1s::CurrentModeUpdate::new("off")),
            || "id".into(),
        )
        .unwrap();
        assert!(update.is_empty());
    }

    #[test]
    fn replayed_messages_open_with_an_empty_whole_message() {
        let mut state = LinkSession {
            replaying: true,
            ..LinkSession::default()
        };
        let first = translate_all(&mut state, v1s::SessionUpdate::AgentMessageChunk(text("a")));
        assert_eq!(first.len(), 2);
        assert_eq!(first[0]["sessionUpdate"], "agent_message");
        assert_eq!(first[0]["messageId"], first[1]["messageId"]);
        assert_eq!(first[1]["sessionUpdate"], "agent_message_chunk");
        let next = translate_all(&mut state, v1s::SessionUpdate::AgentMessageChunk(text("b")));
        assert_eq!(next.len(), 1, "the open message takes more chunks");
    }

    #[test]
    fn a_completed_edit_carries_its_diff_and_a_shell_call_its_terminal() {
        let mut state = LinkSession::default();
        state
            .diffs
            .insert("edit".into(), json!({ "type": "diff", "changes": [] }));
        state.terminals.insert("sh".into(), b"out".to_vec());
        let done = |id: &str| {
            v1s::SessionUpdate::ToolCallUpdate(v1s::ToolCallUpdate::new(
                id.to_string(),
                v1s::ToolCallUpdateFields::new()
                    .status(v1s::ToolCallStatus::Completed)
                    .content(vec![v1s::ToolCallContent::from("done")]),
            ))
        };
        let edit = translate(&mut state, done("edit"));
        assert_eq!(edit["content"][0]["type"], "diff");
        assert_eq!(edit["content"][1]["type"], "content");
        let shell = translate(&mut state, done("sh"));
        assert_eq!(shell["content"][0]["type"], "terminal");
        assert_eq!(shell["content"][0]["terminalId"], "sh");
    }

    #[test]
    fn link_only_updates_parse_as_v2() {
        // The updates the link writes as JSON, so a schema change fails here
        // rather than being dropped with a warning at runtime.
        for update in [
            json!({ "sessionUpdate": "terminal_update", "terminalId": "t", "command": "ls", "cwd": "/w",
                    "capabilities": { "cancel": {} } }),
            json!({ "sessionUpdate": "terminal_update", "terminalId": "t",
                    "output": { "data": activity::base64(b"hi") }, "exitStatus": { "exitCode": 0 },
                    "capabilities": null }),
            json!({ "sessionUpdate": "terminal_update", "terminalId": "t",
                    "exitStatus": { "signal": "SIGKILL" }, "capabilities": null }),
            json!({ "sessionUpdate": "terminal_output_chunk", "terminalId": "t", "data": "aGk=" }),
            json!({ "sessionUpdate": "tool_call_update", "toolCallId": "t", "name": "bash",
                    "content": [{ "type": "terminal", "terminalId": "t" }],
                    "capabilities": { "cancel": {} } }),
            json!({ "sessionUpdate": "tool_call_content_chunk", "toolCallId": "t",
                    "content": { "type": "content", "content": { "type": "text", "text": "x" } } }),
            json!({ "sessionUpdate": "agent_message", "messageId": "m",
                    "content": [{ "type": "text", "text": "redacted" }] }),
            json!({ "sessionUpdate": "tool_call_update", "toolCallId": "e",
                    "content": [{ "type": "diff", "changes": [{ "operation": "modify", "path": "/w/a", "fileType": "text" }],
                                  "patch": { "format": "git_patch", "text": "diff --git a/w/a b/w/a\n" } }] }),
        ] {
            let parsed: s::SessionUpdate = serde_json::from_value(update.clone())
                .unwrap_or_else(|err| panic!("{update}: {err}"));
            let round = serde_json::to_value(parsed).unwrap();
            assert_eq!(round["sessionUpdate"], update["sessionUpdate"]);
            // Malformed controls fall back to none, so check they survive.
            assert_eq!(
                round.get("capabilities"),
                update.get("capabilities"),
                "{update}"
            );
            assert_eq!(
                round.get("exitStatus"),
                update.get("exitStatus"),
                "{update}"
            );
        }
    }

    #[test]
    fn a_cancelled_shell_call_ends_cancelled_without_controls() {
        let mut state = LinkSession::default();
        state.terminals.insert("sh".into(), Vec::new());
        state.terminals.insert("other".into(), Vec::new());
        state.cancelled.insert("sh".into());
        let failed = |id: &str| {
            v1s::SessionUpdate::ToolCallUpdate(v1s::ToolCallUpdate::new(
                id.to_string(),
                v1s::ToolCallUpdateFields::new().status(v1s::ToolCallStatus::Failed),
            ))
        };
        let shell = translate(&mut state, failed("sh"));
        assert_eq!(shell["status"], "cancelled");
        assert_eq!(shell.get("capabilities"), Some(&Value::Null));
        assert!(!state.cancelled.contains("sh"));
        let other = translate(&mut state, failed("other"));
        assert_eq!(
            other["status"], "failed",
            "only a cancelled call reads cancelled"
        );
    }

    #[test]
    fn usage_accumulates_until_idle() {
        let mut total = None;
        let usage = |input, output| everruns_core::TokenUsage {
            input_tokens: input,
            output_tokens: output,
            ..Default::default()
        };
        add_usage(&mut total, &usage(10, 2));
        add_usage(&mut total, &usage(5, 1));
        let total = total.unwrap();
        assert_eq!(
            (total.input_tokens, total.output_tokens, total.total_tokens),
            (15, 3, 18)
        );
    }

    #[test]
    fn system_prompts_take_text_and_both_modes() {
        let text = |t: &str| s::ContentBlock::Text(s::TextContent::new(t));
        let append = s::SystemPrompt::new(vec![text("a"), text("b")]);
        assert!(matches!(
            client_system_prompt(&append).unwrap(),
            ClientSystemPrompt::Append(text) if text == "a\n\nb"
        ));
        let replace = s::SystemPrompt::new(vec![text("only")]).mode(s::SystemPromptMode::Override);
        assert!(matches!(
            client_system_prompt(&replace).unwrap(),
            ClientSystemPrompt::Override(text) if text == "only"
        ));
    }

    #[test]
    fn shell_permission_requests_name_the_command_as_their_subject() {
        let (upsert, request) = permission_request(
            v1s::RequestPermissionRequest::new(
                "session",
                v1s::ToolCallUpdate::new(
                    "call_1",
                    v1s::ToolCallUpdateFields::new()
                        .title("bash".to_string())
                        .kind(v1s::ToolKind::Execute)
                        .raw_input(json!({ "command": "cargo test" })),
                ),
                vec![v1s::PermissionOption::new(
                    "allow_once",
                    "Allow",
                    v1s::PermissionOptionKind::AllowOnce,
                )],
            ),
            Some(Path::new("/work")),
        )
        .unwrap();
        let value = serde_json::to_value(request).unwrap();
        assert_eq!(value["subject"]["type"], "command");
        assert_eq!(value["subject"]["command"], "cargo test");
        assert_eq!(value["subject"]["cwd"], "/work");
        assert_eq!(value["subject"]["toolCallId"], "call_1");
        let upsert = upsert.expect("the tool call is announced first");
        assert_eq!(upsert["sessionUpdate"], "tool_call_update");
        let _: s::SessionUpdate = serde_json::from_value(upsert).unwrap();
    }

    #[test]
    fn permission_requests_name_the_tool_call_as_their_subject() {
        let (upsert, request) = permission_request(
            v1s::RequestPermissionRequest::new(
                "session",
                v1s::ToolCallUpdate::new(
                    "call_1",
                    v1s::ToolCallUpdateFields::new().title("bash".to_string()),
                ),
                vec![v1s::PermissionOption::new(
                    "allow_once",
                    "Allow",
                    v1s::PermissionOptionKind::AllowOnce,
                )],
            ),
            Some(Path::new("/work")),
        )
        .unwrap();
        assert!(upsert.is_none());
        let value = serde_json::to_value(request).unwrap();
        assert_eq!(value["title"], "Allow bash?");
        assert_eq!(value["subject"]["type"], "tool_call");
        assert_eq!(value["subject"]["toolCall"]["toolCallId"], "call_1");
        assert_eq!(value["options"][0]["optionId"], "allow_once");
    }

    #[test]
    fn duplicate_mcp_server_names_are_invalid_params() {
        let server = s::McpServer::Http(s::McpServerHttp::new("docs", "https://example.com/mcp"));
        let err = scoped_mcp_servers(&[server.clone(), server]).unwrap_err();
        assert_eq!(err.code, agent_client_protocol::ErrorCode::INVALID_PARAMS);
    }
}

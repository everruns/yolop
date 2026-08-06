//! Wake seam for everruns background tasks.
//!
//! everruns' `spawn_background` runs a background-capable tool detached from the
//! turn and, on completion, signals the session by calling
//! `platform_store.send_message(session_id, message)` (see
//! `SessionBackgroundSink::signal_session` in everruns-core). Without a platform
//! store that call is a silent no-op, which is why background completions never
//! reached the yolop agent.
//!
//! yolop installs a [`LocalPlatformStore`](everruns_local::LocalPlatformStore)
//! backed by [`WakeRunner`]. For a live host session the runner routes the
//! completion message to the host's unbounded channel; the host (TUI event loop
//! or ACP session loop) runs it as an ordinary streamed turn when idle. Child
//! sub-agent sessions have no terminal host, so the same runner creates and
//! drives those turns synchronously through the in-process runtime.

use async_trait::async_trait;
use everruns_core::error::{AgentLoopError, Result};
use everruns_core::message::MessageRole;
use everruns_core::message_retriever::InputMessage;
use everruns_core::platform_store::{PlatformCreateSessionRequest, PlatformMessage};
use everruns_core::session::Session;
use everruns_core::session_task::{SessionTask, SessionTaskRegistry};
use everruns_core::typed_id::{AgentId, HarnessId, SessionId};
use everruns_core::{TaskTransition, wake_text_for};
use everruns_local::LocalSessionRunner;
use everruns_runtime::{InProcessRuntime, RuntimeSessionStore, SessionBuilder};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use tokio::sync::{Mutex as AsyncMutex, mpsc};

/// Sender half of a session's wake channel. The `LocalPlatformStore`'s
/// `send_message` pushes a background-completion message here; the host drains
/// the paired [`WakeReceiver`] and runs a turn.
pub type WakeSender = mpsc::UnboundedSender<WakeMessage>;
/// Receiver half a host drains to react to finished background tasks.
pub type WakeReceiver = mpsc::UnboundedReceiver<WakeMessage>;

const MAX_WAKE_BATCH_BYTES: usize = 32 * 1024;
const MAX_WAKE_BATCH_MESSAGES: usize = 256;
const MAX_HANDOFF_FIELD_BYTES: usize = 4 * 1024;
pub(crate) const HANDOFF_METADATA_KEY: &str = "yolop.background_handoff";

/// Host-derived continuation state for one completed task. The task registry,
/// not completion prose, owns identity, scope, status, and artifact references.
/// Free-form text remains untrusted result data when rendered for the model.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub(crate) struct TaskHandoff {
    pub task_id: String,
    pub kind: String,
    pub title: String,
    pub requested_scope: String,
    pub status: String,
    pub execution_summary: String,
    pub changed_state_references: Vec<String>,
    pub validation_evidence: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub(crate) struct WakeHandoff {
    pub version: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_ask: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_goal: Option<String>,
    pub tasks: Vec<TaskHandoff>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub omitted_tasks: usize,
}

/// A raw durable wake plus an optional registry-authenticated compact handoff.
/// `handoff == None` deliberately means "use ordinary full-history context".
#[derive(Clone, Debug, PartialEq)]
pub struct WakeMessage {
    raw: String,
    handoff: Option<WakeHandoff>,
}

impl WakeMessage {
    #[cfg(test)]
    pub(crate) fn unstructured(raw: impl Into<String>) -> Self {
        Self {
            raw: raw.into(),
            handoff: None,
        }
    }

    pub(crate) fn with_active_goal(mut self, goal: Option<String>) -> Self {
        if let Some(handoff) = self.handoff.as_mut() {
            handoff.active_goal = goal.map(|value| truncate_field(&value));
        }
        self
    }

    pub(crate) fn with_active_ask(mut self, ask: Option<String>) -> Self {
        if let Some(handoff) = self.handoff.as_mut() {
            handoff.active_ask = ask.map(|value| truncate_field(&value));
        }
        self
    }
}

/// Drain the completions already queued for one idle host into one model turn.
/// The task registry remains the durable source of truth when an unusually
/// large burst exceeds the prompt cap.
pub(crate) fn coalesce_pending_wakes(
    first: WakeMessage,
    receiver: &mut WakeReceiver,
) -> WakeMessage {
    let mut messages = vec![first];
    while messages.len() < MAX_WAKE_BATCH_MESSAGES
        && let Ok(message) = receiver.try_recv()
    {
        messages.push(message);
    }
    let count = messages.len();
    if count == 1 {
        return messages.pop().expect("wake batch has first message");
    }

    let mut raw = format!("{count} background tasks finished:");
    let mut omitted = 0;
    let mut tasks = Vec::new();
    let mut task_bytes = 0usize;
    let mut omitted_tasks = 0usize;
    let mut compact = true;
    let mut active_ask = None;
    let mut active_goal = None;
    for (index, item) in messages.into_iter().enumerate() {
        let section = format!("\n\n--- task {} ---\n{}", index + 1, item.raw);
        if raw.len() + section.len() <= MAX_WAKE_BATCH_BYTES {
            raw.push_str(&section);
        } else {
            omitted += 1;
        }
        match item.handoff {
            Some(handoff) => {
                active_ask = active_ask.or(handoff.active_ask);
                active_goal = active_goal.or(handoff.active_goal);
                omitted_tasks = omitted_tasks.saturating_add(handoff.omitted_tasks);
                for task in handoff.tasks {
                    let bytes = serde_json::to_vec(&task).map_or(MAX_WAKE_BATCH_BYTES, |v| v.len());
                    if task_bytes.saturating_add(bytes) <= MAX_WAKE_BATCH_BYTES {
                        task_bytes += bytes;
                        tasks.push(task);
                    } else {
                        omitted_tasks += 1;
                    }
                }
            }
            None => compact = false,
        }
    }
    if omitted > 0 {
        raw.push_str(&format!(
            "\n\n{omitted} additional completion(s) omitted from this prompt; inspect list_tasks for their durable results."
        ));
    }
    WakeMessage {
        raw,
        handoff: compact.then_some(WakeHandoff {
            version: 1,
            active_ask,
            active_goal,
            tasks,
            omitted_tasks,
        }),
    }
}

fn wake_routes() -> &'static Mutex<HashMap<SessionId, WakeSender>> {
    static ROUTES: OnceLock<Mutex<HashMap<SessionId, WakeSender>>> = OnceLock::new();
    ROUTES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Local platform runner for host wakes and child sub-agent sessions.
///
/// Every terminal-host session registers a wake sender for its lifetime.
/// Messages targeting one of those sessions are queued for the host. All other
/// messages target child sessions and run synchronously through `runtime`.
pub struct WakeRunner {
    session_id: SessionId,
    wake_tx: WakeSender,
    runtime: Arc<OnceLock<Weak<InProcessRuntime>>>,
    sessions: Arc<dyn RuntimeSessionStore>,
    tasks: Option<Arc<dyn SessionTaskRegistry>>,
    child_turn_locks: Mutex<HashMap<SessionId, Arc<AsyncMutex<()>>>>,
}

impl WakeRunner {
    pub fn new(
        session_id: SessionId,
        wake_tx: WakeSender,
        runtime: Arc<OnceLock<Weak<InProcessRuntime>>>,
        sessions: Arc<dyn RuntimeSessionStore>,
        tasks: Option<Arc<dyn SessionTaskRegistry>>,
    ) -> Self {
        wake_routes()
            .lock()
            .unwrap()
            .insert(session_id, wake_tx.clone());
        Self {
            session_id,
            wake_tx,
            runtime,
            sessions,
            tasks,
            child_turn_locks: Mutex::new(HashMap::new()),
        }
    }

    fn runtime(&self) -> Result<Arc<InProcessRuntime>> {
        self.runtime
            .get()
            .ok_or_else(|| AgentLoopError::config("runtime not initialized yet"))
            .and_then(|runtime| {
                runtime
                    .upgrade()
                    .ok_or_else(|| AgentLoopError::config("runtime has shut down"))
            })
    }

    fn child_turn_lock(&self, session_id: SessionId) -> Arc<AsyncMutex<()>> {
        self.child_turn_locks
            .lock()
            .unwrap()
            .entry(session_id)
            .or_insert_with(|| Arc::new(AsyncMutex::new(())))
            .clone()
    }

    async fn wake_message(&self, session_id: SessionId, content: &str) -> WakeMessage {
        let Some(tasks) = self.tasks.as_ref() else {
            return WakeMessage {
                raw: truncate_bytes(content, MAX_WAKE_BATCH_BYTES),
                handoff: None,
            };
        };
        let task = if content.starts_with("Task \"") {
            match task_id_from_wake(content) {
                Some(task_id) => {
                    tasks
                        .get(session_id, task_id)
                        .await
                        .ok()
                        .flatten()
                        .filter(|task| {
                            wake_text_for(task, TaskTransition::Terminal).as_deref()
                                == Some(content)
                        })
                }
                None => None,
            }
        } else if content.starts_with("Background run ") {
            let result_path = wake_field(content, "result_path");
            match (result_path, tasks.list(session_id, None).await) {
                (Some(path), Ok(tasks)) => tasks.into_iter().find(|task| {
                    task.result_path.as_deref() == Some(path)
                        && background_completion_text(task).as_deref() == Some(content)
                }),
                _ => None,
            }
        } else {
            None
        };
        let active_goal = self
            .sessions
            .get_session(session_id)
            .await
            .ok()
            .flatten()
            .and_then(|session| session.goal)
            .map(|goal| truncate_field(&goal));
        WakeMessage {
            raw: truncate_bytes(content, MAX_WAKE_BATCH_BYTES),
            handoff: task.and_then(task_handoff).map(|task| WakeHandoff {
                version: 1,
                active_ask: None,
                active_goal,
                tasks: vec![task],
                omitted_tasks: 0,
            }),
        }
    }
}

impl Drop for WakeRunner {
    fn drop(&mut self) {
        let mut routes = wake_routes().lock().unwrap();
        if routes
            .get(&self.session_id)
            .is_some_and(|current| current.same_channel(&self.wake_tx))
        {
            routes.remove(&self.session_id);
        }
    }
}

/// Frame a raw everruns completion message as an `[automatic]` wake prompt,
/// mirroring the yolop-native `wake_prompt` framing: make clear it is not a user
/// message and point the model at the run's result before it continues.
pub fn frame_wake_prompt(message: &WakeMessage) -> String {
    format!(
        "[automatic] Background work you started has finished:\n\n{}\n\nThis is not a \
         user message. Read the run's result if useful (its `result_path`/`log_path` are session \
         files you can read), then continue the work it was for or report the result.",
        message.raw
    )
}

/// Persist the raw wake exactly as received while attaching a host-only marker
/// used to select a compact provider view. User/model text cannot forge this
/// provenance because hosts call this constructor only for routed wakes.
pub fn input_for_wake(message: &WakeMessage) -> InputMessage {
    let mut input = InputMessage::user(frame_wake_prompt(message));
    if let Some(handoff) = &message.handoff
        && let Ok(value) = serde_json::to_value(handoff)
    {
        input.metadata = Some(std::collections::HashMap::from([(
            HANDOFF_METADATA_KEY.to_string(),
            value,
        )]));
        input.tags.push("automatic_background_wake".to_string());
    }
    input
}

fn task_handoff(task: SessionTask) -> Option<TaskHandoff> {
    let summary = task.summary.as_deref()?.trim();
    if summary.is_empty() {
        return None;
    }
    let requested_scope = requested_scope(&task)?;
    let mut references = task
        .artifacts
        .iter()
        .filter_map(|artifact| artifact.path.clone().or_else(|| artifact.url.clone()))
        .collect::<Vec<_>>();
    if let Some(path) = &task.result_path {
        references.push(path.clone());
        if let Some(dir) = path.strip_suffix("/result.json") {
            references.push(format!("{dir}/output.log"));
        }
    }
    references.sort();
    references.dedup();
    references.truncate(16);
    Some(TaskHandoff {
        task_id: truncate_field(&task.id),
        kind: truncate_field(&task.kind),
        title: truncate_field(&task.display_name),
        requested_scope,
        status: task.state.to_string(),
        execution_summary: truncate_field(summary),
        changed_state_references: references
            .into_iter()
            .map(|value| truncate_field(&value))
            .collect(),
        validation_evidence: truncate_field(summary),
    })
}

fn requested_scope(task: &SessionTask) -> Option<String> {
    let selected = match task.kind.as_str() {
        "background_tool" => json!({
            "tool": task.spec.get("tool")?,
            "arguments": task.spec.get("arguments")?,
        }),
        "subagent" | "session" => json!({
            "instructions": task.spec.get("instructions")?,
            "mode": task.spec.get("mode"),
        }),
        _ if !task.spec.is_null() => task.spec.clone(),
        _ => return None,
    };
    serde_json::to_string(&selected)
        .ok()
        .map(|value| truncate_field(&value))
}

fn task_id_from_wake(content: &str) -> Option<&str> {
    let line = content.lines().next()?;
    let open = line.rfind('(')?;
    let close = line[open + 1..].find(')')? + open + 1;
    let id = line[open + 1..close].trim();
    (!id.is_empty()).then_some(id)
}

fn background_completion_text(task: &SessionTask) -> Option<String> {
    let result_path = task.result_path.as_deref()?;
    let artifact_dir = result_path.strip_suffix("/result.json")?;
    let run_id = artifact_dir.rsplit('/').next()?;
    let tool = task.spec.get("tool")?.as_str()?;
    let summary = task.summary.as_deref()?;
    let status = match task.state.to_string().as_str() {
        "succeeded" => "completed",
        "failed" => "failed",
        "canceled" => "canceled",
        _ => return None,
    };
    Some(format!(
        "Background run {status}.\n- run_id: {run_id}\n- title: {}\n- tool: {tool}\n- summary: {summary}\n- result_path: {result_path}\n- log_path: {artifact_dir}/output.log",
        task.display_name
    ))
}

fn wake_field<'a>(content: &'a str, name: &str) -> Option<&'a str> {
    let prefix = format!("- {name}: ");
    content
        .lines()
        .find_map(|line| line.strip_prefix(&prefix))
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn truncate_field(value: &str) -> String {
    truncate_bytes(value, MAX_HANDOFF_FIELD_BYTES)
}

fn truncate_bytes(value: &str, max: usize) -> String {
    if value.len() <= max {
        return value.to_string();
    }
    let mut end = max;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n…[truncated]", &value[..end])
}

fn is_zero(value: &usize) -> bool {
    *value == 0
}

#[async_trait]
impl LocalSessionRunner for WakeRunner {
    async fn routable_session_ids(&self) -> Result<Option<Vec<SessionId>>> {
        Ok(Some(
            wake_routes().lock().unwrap().keys().copied().collect(),
        ))
    }

    async fn send_message(&self, session_id: SessionId, content: &str) -> Result<()> {
        let wake = self.wake_message(session_id, content).await;
        let sender = wake_routes().lock().unwrap().get(&session_id).cloned();
        if let Some(sender) = sender {
            return sender.send(wake).map_err(|_| {
                let mut routes = wake_routes().lock().unwrap();
                if routes
                    .get(&session_id)
                    .is_some_and(|current| current.same_channel(&sender))
                {
                    routes.remove(&session_id);
                }
                AgentLoopError::tool(format!(
                    "session {session_id} wake receiver is closed; wake remains retryable"
                ))
            });
        }

        if self.sessions.get_session(session_id).await?.is_none() {
            return Err(AgentLoopError::session_not_found(session_id));
        }
        let turn_lock = self.child_turn_lock(session_id);
        let _guard = turn_lock.lock().await;
        let result = self
            .runtime()?
            .run_turn(session_id, input_for_wake(&wake))
            .await?;
        if result.success {
            Ok(())
        } else {
            Err(AgentLoopError::tool(format!(
                "child turn failed: {}",
                result.error.unwrap_or_default()
            )))
        }
    }

    async fn create_session(
        &self,
        harness_id: HarnessId,
        agent_id: Option<AgentId>,
        title: Option<&str>,
        _locale: Option<&str>,
        parent_session_id: Option<SessionId>,
    ) -> Result<Session> {
        let mut session = SessionBuilder::new(harness_id)
            .id(SessionId::new())
            .title(title.unwrap_or("sub-agent"))
            .build();
        session.agent_id = agent_id;
        session.parent_session_id = parent_session_id;
        self.sessions.add_session(session.clone()).await?;
        Ok(session)
    }

    async fn create_session_with_options(
        &self,
        request: PlatformCreateSessionRequest,
    ) -> Result<Session> {
        if request.parent_session_id.is_none() {
            return Err(AgentLoopError::tool(
                "detached local sub-agent sessions are not supported; use lifetime=linked",
            ));
        }
        // `seed` applies to detached sessions. The unified tool schema exposes
        // it for every target, so tolerate it on linked children instead of
        // turning an otherwise valid spawn into an internal error.
        let mut session = SessionBuilder::new(request.harness_id)
            .id(SessionId::new())
            .title(request.title.as_deref().unwrap_or("sub-agent"))
            .build();
        session.agent_id = request.agent_id;
        session.parent_session_id = request.parent_session_id;
        session.goal = request.goal;
        self.sessions.add_session(session.clone()).await?;
        Ok(session)
    }

    async fn list_sessions(
        &self,
        _limit: Option<usize>,
        _agent_id: Option<AgentId>,
    ) -> Result<Vec<Session>> {
        Ok(Vec::new())
    }

    async fn get_session(&self, session_id: SessionId) -> Result<Option<Session>> {
        self.sessions.get_session(session_id).await
    }

    async fn get_messages(
        &self,
        session_id: SessionId,
        limit: Option<usize>,
    ) -> Result<Vec<PlatformMessage>> {
        let messages = self.runtime()?.messages(session_id).await?;
        let mut mapped: Vec<_> = messages
            .iter()
            .map(|message| PlatformMessage {
                role: match &message.role {
                    MessageRole::Agent => "agent".to_string(),
                    MessageRole::User => "user".to_string(),
                    other => format!("{other:?}").to_lowercase(),
                },
                content: message.text().unwrap_or_default().to_string(),
                created_at: message.created_at,
            })
            .collect();
        if let Some(limit) = limit {
            let skip = mapped.len().saturating_sub(limit);
            mapped.drain(..skip);
        }
        Ok(mapped)
    }

    async fn get_session_status(&self, session_id: SessionId) -> Result<Option<String>> {
        Ok(self
            .sessions
            .get_session(session_id)
            .await?
            // Child turns run synchronously in `send_message`; success returns
            // only after the turn has completed, while failures return an
            // error there. Use the explicit terminal state for foreground
            // children too: unlike the background watcher, that path does not
            // translate the local runner's bare `idle` status.
            .map(|_| "completed".to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use everruns_core::session_task::{
        CreateSessionTask, SessionTaskState, TaskWakePolicy, new_session_task,
    };
    use everruns_runtime::RuntimeBackends;

    fn runner(session_id: SessionId, tx: WakeSender) -> WakeRunner {
        let backends = RuntimeBackends::in_memory();
        WakeRunner::new(
            session_id,
            tx,
            Arc::new(OnceLock::new()),
            backends.session_store,
            None,
        )
    }

    #[tokio::test]
    async fn routes_wakes_to_the_target_session() {
        let session_a = SessionId::from_seed(910_001);
        let session_b = SessionId::from_seed(910_002);
        let (tx_a, mut rx_a) = mpsc::unbounded_channel();
        let (tx_b, mut rx_b) = mpsc::unbounded_channel();
        let runner_a = runner(session_a, tx_a);
        let _runner_b = runner(session_b, tx_b);

        runner_a
            .send_message(session_b, "for b")
            .await
            .expect("route to active session b");

        assert_eq!(rx_b.recv().await.map(|wake| wake.raw), Some("for b".into()));
        assert!(rx_a.try_recv().is_err());
    }

    #[tokio::test]
    async fn inactive_session_wakes_remain_retryable() {
        let active = SessionId::from_seed(910_003);
        let inactive = SessionId::from_seed(910_004);
        let (tx, _rx) = mpsc::unbounded_channel();
        let runner = runner(active, tx);

        let error = runner
            .send_message(inactive, "later")
            .await
            .expect_err("inactive session must reject delivery");

        assert!(error.to_string().contains("not found"));
    }

    #[tokio::test]
    async fn reports_only_live_host_sessions_as_routable() {
        let session_a = SessionId::from_seed(910_005);
        let session_b = SessionId::from_seed(910_006);
        let (tx_a, _rx_a) = mpsc::unbounded_channel();
        let (tx_b, _rx_b) = mpsc::unbounded_channel();
        let runner_a = runner(session_a, tx_a);
        let runner_b = runner(session_b, tx_b);

        let routable = runner_a
            .routable_session_ids()
            .await
            .expect("read routable sessions")
            .expect("wake runner must scope schedule claims");
        assert!(routable.contains(&session_a));
        assert!(routable.contains(&session_b));

        drop(runner_b);
        let routable = runner_a
            .routable_session_ids()
            .await
            .expect("read routable sessions after route closes")
            .expect("wake runner must scope schedule claims");
        assert!(routable.contains(&session_a));
        assert!(!routable.contains(&session_b));
    }

    #[test]
    fn queued_completion_burst_coalesces_without_losing_results() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        tx.send(WakeMessage::unstructured("task_2 result_path=/results/2"))
            .unwrap();
        tx.send(WakeMessage::unstructured("task_3 result_path=/results/3"))
            .unwrap();

        let batch = coalesce_pending_wakes(
            WakeMessage::unstructured("task_1 result_path=/results/1"),
            &mut rx,
        );

        assert!(
            batch.raw.starts_with("3 background tasks finished"),
            "three wakes should cost one model turn"
        );
        for result_path in ["/results/1", "/results/2", "/results/3"] {
            assert!(
                batch.raw.contains(result_path),
                "every queued result must be delivered in the coalesced prompt"
            );
        }
        assert!(
            rx.try_recv().is_err(),
            "the burst must be drained exactly once"
        );
    }

    fn terminal_task(id: &str, state: SessionTaskState, summary: Option<&str>) -> SessionTask {
        let mut task = new_session_task(
            CreateSessionTask {
                session_id: SessionId::from_seed(910_099),
                id: Some(id.to_string()),
                kind: "background_tool".to_string(),
                display_name: format!("validation {id}"),
                spec: json!({
                    "tool": "bash",
                    "arguments": {"command": "cargo test"},
                }),
                state: SessionTaskState::Running,
                links: Default::default(),
                wake_policy: TaskWakePolicy::Silent,
            },
            Utc::now(),
        );
        task.state = state;
        task.summary = summary.map(str::to_string);
        task.result_path = Some(format!("/.background/{id}/result.json"));
        task
    }

    #[test]
    fn failed_canceled_and_missing_summaries_have_safe_handoff_behavior() {
        let failed = task_handoff(terminal_task(
            "task_failed",
            SessionTaskState::Failed,
            Some("tests failed: assertion mismatch"),
        ))
        .expect("failed task has decisive evidence");
        assert_eq!(failed.status, "failed");
        assert!(failed.validation_evidence.contains("assertion mismatch"));

        let canceled = task_handoff(terminal_task(
            "task_canceled",
            SessionTaskState::Canceled,
            Some("Canceled by request."),
        ))
        .expect("canceled task has a bounded handoff");
        assert_eq!(canceled.status, "canceled");

        assert!(
            task_handoff(terminal_task(
                "task_missing",
                SessionTaskState::Succeeded,
                None,
            ))
            .is_none(),
            "missing summary must select the full-history fallback"
        );
    }

    #[test]
    fn handoff_fields_and_batches_are_bounded_in_notification_order() {
        let first = task_handoff(terminal_task(
            "task_1",
            SessionTaskState::Succeeded,
            Some(&"a".repeat(MAX_HANDOFF_FIELD_BYTES * 2)),
        ))
        .unwrap();
        let second = task_handoff(terminal_task(
            "task_2",
            SessionTaskState::Succeeded,
            Some("second"),
        ))
        .unwrap();
        assert!(first.execution_summary.len() < MAX_HANDOFF_FIELD_BYTES + 32);

        let structured = |task| WakeMessage {
            raw: "completion".to_string(),
            handoff: Some(WakeHandoff {
                version: 1,
                active_ask: None,
                active_goal: None,
                tasks: vec![task],
                omitted_tasks: 0,
            }),
        };
        let (tx, mut rx) = mpsc::unbounded_channel();
        tx.send(structured(second)).unwrap();
        let batch = coalesce_pending_wakes(structured(first), &mut rx);
        let ids = batch
            .handoff
            .unwrap()
            .tasks
            .into_iter()
            .map(|task| task.task_id)
            .collect::<Vec<_>>();
        assert_eq!(ids, vec!["task_1", "task_2"]);
    }
}

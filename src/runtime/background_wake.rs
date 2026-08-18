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
//! backed by Everruns' [`HostRoutedRunner`](everruns_local::HostRoutedRunner).
//! Its inner [`WakeRunner`] enriches host wakes with Yolop's authenticated task
//! handoff and drives child sessions synchronously through the in-process
//! runtime.

use async_trait::async_trait;
use everruns_core::ExecutionSession;
use everruns_core::InputMessage;
use everruns_core::MessageRole;
use everruns_core::{PlatformCreateSessionRequest, PlatformMessage};
use everruns_core::{SessionTask, SessionTaskRegistry};
use everruns_core::{TaskTransition, wake_text_for};
use everruns_host::{InProcessRuntime, RuntimeSessionStore, SessionBuilder};
use everruns_local::{HostRoutedRunner, LocalSessionRunner, WakeRoutes};
use everruns_provider::typed_id::{AgentId, HarnessId, SessionId};
use everruns_provider::{AgentLoopError, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use tokio::sync::{Mutex as AsyncMutex, mpsc};

/// Sender half of a session's wake channel. The `LocalPlatformStore`'s
/// `send_message` pushes a background-completion message here; the host drains
/// the paired [`WakeReceiver`] and runs a turn.
pub(crate) type WakeSender = mpsc::UnboundedSender<WakeMessage>;
/// Receiver half a host drains to react to finished background tasks.
pub struct WakeReceiver {
    inner: mpsc::UnboundedReceiver<WakeMessage>,
    _route: Option<HostWakeRoute>,
}

impl WakeReceiver {
    pub async fn recv(&mut self) -> Option<WakeMessage> {
        self.inner.recv().await
    }

    pub fn try_recv(&mut self) -> std::result::Result<WakeMessage, mpsc::error::TryRecvError> {
        self.inner.try_recv()
    }

    #[cfg(test)]
    pub(crate) fn unrouted(inner: mpsc::UnboundedReceiver<WakeMessage>) -> Self {
        Self {
            inner,
            _route: None,
        }
    }
}

/// Owns one upstream host route and the task that enriches its raw messages.
pub struct HostWakeRoute {
    routes: WakeRoutes,
    session_id: SessionId,
    bridge: tokio::task::JoinHandle<()>,
}

impl Drop for HostWakeRoute {
    fn drop(&mut self) {
        self.routes.unregister(self.session_id);
        self.bridge.abort();
    }
}

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
    kind: WakeKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WakeKind {
    Background,
    Coordination,
}

impl WakeMessage {
    #[cfg(test)]
    pub(crate) fn unstructured(raw: impl Into<String>) -> Self {
        Self {
            raw: raw.into(),
            handoff: None,
            kind: WakeKind::Background,
        }
    }

    pub(crate) fn coordination(prompt: impl Into<String>) -> Self {
        Self {
            raw: prompt.into(),
            handoff: None,
            kind: WakeKind::Coordination,
        }
    }

    pub(crate) fn is_coordination(&self) -> bool {
        self.kind == WakeKind::Coordination
    }

    pub(crate) fn notice(&self) -> &'static str {
        if self.is_coordination() {
            "↻ coordination message received, waking agent"
        } else {
            "↻ background task finished, waking agent to review"
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
    // Coordination messages are complete typed prompts. Keep their ordering
    // and identity intact instead of blending them into a background-task
    // burst; the next idle boundary drains the following message.
    if first.kind == WakeKind::Coordination {
        return first;
    }
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
        kind: WakeKind::Background,
    }
}

/// Local platform runner for host wakes and child sub-agent sessions.
/// Host routing itself is owned by Everruns' [`HostRoutedRunner`].
pub struct WakeRunner {
    runtime: Arc<OnceLock<Weak<InProcessRuntime>>>,
    sessions: Arc<dyn RuntimeSessionStore>,
    tasks: Option<Arc<dyn SessionTaskRegistry>>,
    child_turn_locks: Mutex<HashMap<SessionId, Arc<AsyncMutex<()>>>>,
}

impl WakeRunner {
    pub fn new(
        runtime: Arc<OnceLock<Weak<InProcessRuntime>>>,
        sessions: Arc<dyn RuntimeSessionStore>,
        tasks: Option<Arc<dyn SessionTaskRegistry>>,
    ) -> Self {
        Self {
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
                kind: WakeKind::Background,
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
            kind: WakeKind::Background,
        }
    }
}

/// Register a live host session with Everruns' route registry and translate
/// raw route messages into Yolop's authenticated wake representation.
pub fn register_host_route(
    runner: Arc<HostRoutedRunner<WakeRunner>>,
    session_id: SessionId,
) -> (WakeSender, WakeReceiver) {
    let routes = runner.routes().clone();
    let mut raw = routes.register(session_id);
    let (wake_tx, wake_rx) = mpsc::unbounded_channel();
    let bridge_tx = wake_tx.clone();
    let bridge_runner = runner.clone();
    let bridge = tokio::spawn(async move {
        while let Some(content) = raw.recv().await {
            let wake = bridge_runner
                .inner()
                .wake_message(session_id, &content)
                .await;
            if bridge_tx.send(wake).is_err() {
                break;
            }
        }
    });
    (
        wake_tx,
        WakeReceiver {
            inner: wake_rx,
            _route: Some(HostWakeRoute {
                routes,
                session_id,
                bridge,
            }),
        },
    )
}

/// Frame a raw everruns completion message as an `[automatic]` wake prompt,
/// mirroring the yolop-native `wake_prompt` framing: make clear it is not a user
/// message and point the model at the run's result before it continues.
pub fn frame_wake_prompt(message: &WakeMessage) -> String {
    if message.kind == WakeKind::Coordination {
        return message.raw.clone();
    }
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
    if message.kind == WakeKind::Coordination {
        input.tags.push("automatic_coordination".to_string());
        return input;
    }
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
        Ok(Some(Vec::new()))
    }

    async fn send_message(&self, session_id: SessionId, content: &str) -> Result<()> {
        let wake = self.wake_message(session_id, content).await;
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
    ) -> Result<ExecutionSession> {
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
    ) -> Result<ExecutionSession> {
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
    ) -> Result<Vec<ExecutionSession>> {
        Ok(Vec::new())
    }

    async fn get_session(&self, session_id: SessionId) -> Result<Option<ExecutionSession>> {
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
    use everruns_core::session_task::new_session_task;
    use everruns_core::{CreateSessionTask, SessionTaskState, TaskWakePolicy};
    use everruns_host::HostBackends;

    fn runner() -> WakeRunner {
        let backends = HostBackends::in_memory();
        WakeRunner::new(Arc::new(OnceLock::new()), backends.session_store, None)
    }

    #[tokio::test]
    async fn upstream_router_delivers_an_enriched_host_wake() {
        let session_id = SessionId::from_seed(910_001);
        let runner = Arc::new(HostRoutedRunner::new(runner(), WakeRoutes::new()));
        let (_, mut wakes) = register_host_route(runner.clone(), session_id);

        runner
            .send_message(session_id, "for host")
            .await
            .expect("route to active host");

        assert_eq!(
            wakes.recv().await.map(|wake| wake.raw),
            Some("for host".into())
        );
    }

    #[tokio::test]
    async fn inactive_session_wakes_remain_retryable() {
        let inactive = SessionId::from_seed(910_004);
        let runner = runner();

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
        let runner = Arc::new(HostRoutedRunner::new(runner(), WakeRoutes::new()));
        let (_, rx_a) = register_host_route(runner.clone(), session_a);
        let (_, rx_b) = register_host_route(runner.clone(), session_b);

        let routable = runner
            .routable_session_ids()
            .await
            .expect("read routable sessions")
            .expect("wake runner must scope schedule claims");
        assert!(routable.contains(&session_a));
        assert!(routable.contains(&session_b));

        drop(rx_b);
        let routable = runner
            .routable_session_ids()
            .await
            .expect("read routable sessions after route closes")
            .expect("wake runner must scope schedule claims");
        assert!(routable.contains(&session_a));
        assert!(!routable.contains(&session_b));
        drop(rx_a);
    }

    #[test]
    fn queued_completion_burst_coalesces_without_losing_results() {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut rx = WakeReceiver::unrouted(rx);
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

    #[test]
    fn coordination_wakes_keep_typed_framing_and_distinct_presentation() {
        let wake = WakeMessage::coordination("[automatic] assigned task_dispatch");

        assert!(wake.is_coordination());
        assert_eq!(
            frame_wake_prompt(&wake),
            "[automatic] assigned task_dispatch"
        );
        assert_eq!(
            wake.notice(),
            "↻ coordination message received, waking agent"
        );
        assert_eq!(input_for_wake(&wake).tags, vec!["automatic_coordination"]);
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
            kind: WakeKind::Background,
        };
        let (tx, rx) = mpsc::unbounded_channel();
        let mut rx = WakeReceiver::unrouted(rx);
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

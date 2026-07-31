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
use everruns_core::platform_store::{PlatformCreateSessionRequest, PlatformMessage};
use everruns_core::session::Session;
use everruns_core::typed_id::{AgentId, HarnessId, SessionId};
use everruns_local::LocalSessionRunner;
use everruns_runtime::{InProcessRuntime, RuntimeSessionStore, SessionBuilder};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use tokio::sync::{Mutex as AsyncMutex, mpsc};

/// Sender half of a session's wake channel. The `LocalPlatformStore`'s
/// `send_message` pushes a background-completion message here; the host drains
/// the paired [`WakeReceiver`] and runs a turn.
pub type WakeSender = mpsc::UnboundedSender<String>;
/// Receiver half a host drains to react to finished background tasks.
pub type WakeReceiver = mpsc::UnboundedReceiver<String>;

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
    child_turn_locks: Mutex<HashMap<SessionId, Arc<AsyncMutex<()>>>>,
}

impl WakeRunner {
    pub fn new(
        session_id: SessionId,
        wake_tx: WakeSender,
        runtime: Arc<OnceLock<Weak<InProcessRuntime>>>,
        sessions: Arc<dyn RuntimeSessionStore>,
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
pub fn frame_wake_prompt(message: &str) -> String {
    format!(
        "[automatic] A background task you started has finished:\n\n{message}\n\nThis is not a \
         user message. Read the run's result if useful (its `result_path`/`log_path` are session \
         files you can read), then continue the work it was for or report the result."
    )
}

#[async_trait]
impl LocalSessionRunner for WakeRunner {
    async fn routable_session_ids(&self) -> Result<Option<Vec<SessionId>>> {
        Ok(Some(
            wake_routes().lock().unwrap().keys().copied().collect(),
        ))
    }

    async fn send_message(&self, session_id: SessionId, content: &str) -> Result<()> {
        let sender = wake_routes().lock().unwrap().get(&session_id).cloned();
        if let Some(sender) = sender {
            return sender.send(content.to_string()).map_err(|_| {
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
        let result = self.runtime()?.run_text_turn(session_id, content).await?;
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
    use everruns_runtime::RuntimeBackends;

    fn runner(session_id: SessionId, tx: WakeSender) -> WakeRunner {
        let backends = RuntimeBackends::in_memory();
        WakeRunner::new(
            session_id,
            tx,
            Arc::new(OnceLock::new()),
            backends.session_store,
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

        assert_eq!(rx_b.recv().await.as_deref(), Some("for b"));
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
}

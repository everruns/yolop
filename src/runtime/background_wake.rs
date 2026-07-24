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
//! backed by [`WakeRunner`]. For a live host session the runner does NOT run a
//! turn synchronously (the behavior the `LocalSessionRunner` contract assumes,
//! which only fits child sub-agent sessions) — that would re-enter the session
//! from a detached background task's context, bypassing the host's streaming
//! turn loop. Instead it routes the completion message by session id to the
//! host's unbounded channel; the host (TUI event loop or ACP session loop) runs
//! it as an ordinary streamed turn when idle. See knowledge/specs/background.md.

use async_trait::async_trait;
use everruns_core::error::{AgentLoopError, Result};
use everruns_core::platform_store::PlatformMessage;
use everruns_core::session::Session;
use everruns_core::typed_id::{AgentId, HarnessId, SessionId};
use everruns_local::LocalSessionRunner;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use tokio::sync::mpsc;

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

/// `LocalSessionRunner` whose `send_message` routes a wake to the target host
/// session instead of running a turn inline. Every built session registers its
/// sender for its lifetime. This matters because the local schedule runner
/// claims from an org-wide store and may encounter another live ACP session's
/// schedule.
///
/// The other trait methods are unused today: a background *tool* run signals its
/// own (parent) session, so only `send_message` is exercised. Child sub-agent
/// sessions — which would drive `create_session`/`get_*` and expect a
/// synchronous `send_message` — are handled separately if/when the sub-agent
/// path migrates onto this runner.
pub struct WakeRunner {
    session_id: SessionId,
    wake_tx: WakeSender,
}

impl WakeRunner {
    pub fn new(session_id: SessionId, wake_tx: WakeSender) -> Self {
        wake_routes()
            .lock()
            .unwrap()
            .insert(session_id, wake_tx.clone());
        Self {
            session_id,
            wake_tx,
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
        let Some(sender) = sender else {
            return Err(AgentLoopError::tool(format!(
                "session {session_id} is not active; wake remains retryable"
            )));
        };
        sender.send(content.to_string()).map_err(|_| {
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
        })
    }

    async fn create_session(
        &self,
        _harness_id: HarnessId,
        _agent_id: Option<AgentId>,
        _title: Option<&str>,
        _locale: Option<&str>,
        _parent_session_id: Option<SessionId>,
    ) -> Result<Session> {
        Err(unsupported("create_session"))
    }

    async fn list_sessions(
        &self,
        _limit: Option<usize>,
        _agent_id: Option<AgentId>,
    ) -> Result<Vec<Session>> {
        Ok(Vec::new())
    }

    async fn get_session(&self, _session_id: SessionId) -> Result<Option<Session>> {
        Ok(None)
    }

    async fn get_messages(
        &self,
        _session_id: SessionId,
        _limit: Option<usize>,
    ) -> Result<Vec<PlatformMessage>> {
        Ok(Vec::new())
    }

    async fn get_session_status(&self, _session_id: SessionId) -> Result<Option<String>> {
        Ok(None)
    }
}

fn unsupported(op: &str) -> AgentLoopError {
    AgentLoopError::tool(format!(
        "the local wake runner does not support '{op}' (background completions only)"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn routes_wakes_to_the_target_session() {
        let session_a = SessionId::from_seed(910_001);
        let session_b = SessionId::from_seed(910_002);
        let (tx_a, mut rx_a) = mpsc::unbounded_channel();
        let (tx_b, mut rx_b) = mpsc::unbounded_channel();
        let runner_a = WakeRunner::new(session_a, tx_a);
        let _runner_b = WakeRunner::new(session_b, tx_b);

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
        let runner = WakeRunner::new(active, tx);

        let error = runner
            .send_message(inactive, "later")
            .await
            .expect_err("inactive session must reject delivery");

        assert!(error.to_string().contains("not active"));
    }

    #[tokio::test]
    async fn reports_only_live_host_sessions_as_routable() {
        let session_a = SessionId::from_seed(910_005);
        let session_b = SessionId::from_seed(910_006);
        let (tx_a, _rx_a) = mpsc::unbounded_channel();
        let (tx_b, _rx_b) = mpsc::unbounded_channel();
        let runner_a = WakeRunner::new(session_a, tx_a);
        let runner_b = WakeRunner::new(session_b, tx_b);

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

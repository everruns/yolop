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
//! turn loop. Instead it hands the completion message to the host over an
//! unbounded channel; the host (TUI event loop or ACP session loop) runs it as
//! an ordinary streamed turn when idle. See specs/background.md.

use async_trait::async_trait;
use everruns_core::error::{AgentLoopError, Result};
use everruns_core::platform_store::PlatformMessage;
use everruns_core::session::Session;
use everruns_core::typed_id::{AgentId, HarnessId, SessionId};
use everruns_local::LocalSessionRunner;
use tokio::sync::mpsc;

/// Sender half of a session's wake channel. The `LocalPlatformStore`'s
/// `send_message` pushes a background-completion message here; the host drains
/// the paired [`WakeReceiver`] and runs a turn.
pub type WakeSender = mpsc::UnboundedSender<String>;
/// Receiver half a host drains to react to finished background tasks.
pub type WakeReceiver = mpsc::UnboundedReceiver<String>;

/// `LocalSessionRunner` whose `send_message` enqueues a wake for the host
/// session instead of running a turn inline. One per built session; `wake_tx`
/// feeds that session's [`WakeReceiver`].
///
/// The other trait methods are unused today: a background *tool* run signals its
/// own (parent) session, so only `send_message` is exercised. Child sub-agent
/// sessions — which would drive `create_session`/`get_*` and expect a
/// synchronous `send_message` — are handled separately if/when the sub-agent
/// path migrates onto this runner.
pub struct WakeRunner {
    wake_tx: WakeSender,
}

impl WakeRunner {
    pub fn new(wake_tx: WakeSender) -> Self {
        Self { wake_tx }
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
    async fn send_message(&self, _session_id: SessionId, content: &str) -> Result<()> {
        // Non-blocking hand-off to the host loop. A closed receiver (host gone)
        // is not an error — the process is winding down.
        let _ = self.wake_tx.send(content.to_string());
        Ok(())
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

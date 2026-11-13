//! Lets an editor cancel one running shell command without cancelling its
//! turn (ACP v2 `tool_call/cancel` and `terminal/cancel`).
//!
//! TODO(everruns#5384): everruns-core now does this in the act loop for every
//! inline tool call (`everruns_core::engine::tool_cancel`, with
//! `everruns::Session::cancel_tool_call`), and completes a cancelled call with
//! status `cancelled`. Once a release carries it, delete this module, drop the
//! race in `BashTool::run_command`, cancel through the upstream registry, and
//! advertise `capabilities.cancel` on every ACP v2 tool call, not only shell
//! commands. The API here mirrors upstream's so the switch is mechanical.
//!
//! Until then only `bash` registers: it races its command against the token
//! and kills the command's process group when the token fires, keeping the
//! output read so far.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use everruns_contracts::typed_id::SessionId;
use tokio_util::sync::CancellationToken;

/// What the model reads for a call that was cancelled.
pub(crate) const CANCELLED_MESSAGE: &str =
    "The user cancelled this tool call before it finished. Its effects may be partial.";

type Key = (SessionId, String);

/// The running tool calls an editor may cancel, by session and call id.
#[derive(Default)]
pub(crate) struct ToolCallCancels {
    calls: Mutex<HashMap<Key, (u64, CancellationToken)>>,
    next: AtomicU64,
}

impl ToolCallCancels {
    /// Cancel a running call. `false` when no such call is running (it is
    /// unknown, already finished, or not cancellable).
    pub(crate) fn cancel(&self, session_id: SessionId, tool_call_id: &str) -> bool {
        let calls = self.calls.lock().unwrap_or_else(|e| e.into_inner());
        match calls.get(&(session_id, tool_call_id.to_string())) {
            Some((_, token)) if !token.is_cancelled() => {
                token.cancel();
                true
            }
            _ => false,
        }
    }

    /// Track a call until the returned registration drops.
    pub(crate) fn register(
        self: &Arc<Self>,
        session_id: SessionId,
        tool_call_id: &str,
    ) -> Registration {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let token = CancellationToken::new();
        let key = (session_id, tool_call_id.to_string());
        self.calls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key.clone(), (id, token.clone()));
        Registration {
            cancels: self.clone(),
            key,
            id,
            token,
        }
    }
}

/// The process-wide registry: the ACP server and the tools share it.
pub(crate) fn shared() -> Arc<ToolCallCancels> {
    static SHARED: OnceLock<Arc<ToolCallCancels>> = OnceLock::new();
    SHARED.get_or_init(Default::default).clone()
}

/// A call tracked by [`ToolCallCancels`]; dropping it forgets the call.
pub(crate) struct Registration {
    cancels: Arc<ToolCallCancels>,
    key: Key,
    id: u64,
    token: CancellationToken,
}

impl Registration {
    /// Fires when the editor cancels the call.
    pub(crate) fn token(&self) -> &CancellationToken {
        &self.token
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        let mut calls = self.cancels.calls.lock().unwrap_or_else(|e| e.into_inner());
        // A retried call may have registered the same id since; leave it.
        if calls.get(&self.key).is_some_and(|(id, _)| *id == self.id) {
            calls.remove(&self.key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_registered_call_cancels_once_and_is_forgotten_when_it_ends() {
        let cancels = Arc::new(ToolCallCancels::default());
        let session = SessionId::new();
        let registration = cancels.register(session, "call_1");
        assert!(!cancels.cancel(session, "call_2"), "unknown call");
        assert!(!cancels.cancel(SessionId::new(), "call_1"), "other session");
        assert!(cancels.cancel(session, "call_1"));
        assert!(registration.token().is_cancelled());
        assert!(!cancels.cancel(session, "call_1"), "already cancelled");
        drop(registration);
        let older = cancels.register(session, "call_1");
        let newer = cancels.register(session, "call_1");
        drop(older);
        assert!(cancels.cancel(session, "call_1"), "newer registration kept");
        drop(newer);
    }
}

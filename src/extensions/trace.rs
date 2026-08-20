//! Agentic-trace forwarding: fan the session's live event stream out to any
//! enabled extension that declares the `trace` facet, as fire-and-forget
//! `trace/event` notifications. The extension server maps events to spans and
//! exports them to a tracing backend (Logfire, an OTLP collector, …).
//!
//! This mirrors `capabilities::herdr`'s `start_monitor`: one spawned task per
//! trace extension consuming a `broadcast::Receiver<Event>`, filtered to the
//! session. Forwarding is observe-only and best-effort — a slow or crashed
//! exporter can never stall the agent loop (the host never awaits a reply),
//! and a forward failure degrades to a warning.

use super::manager::ExtensionProcess;
use everruns_core::{
    Event, OUTPUT_MESSAGE_DELTA, REASON_THINKING_DELTA, TOOL_OUTPUT_DELTA, TOOL_PROGRESS,
};
use everruns_provider::typed_id::SessionId;
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::{broadcast, watch};

/// High-frequency streaming deltas carry no span-shaped meaning on their own and
/// would flood the exporter (and the broadcast channel); the paired
/// `*.started`/`*.completed` events already bound the same work. Everything else
/// — turn/reason/act/tool lifecycle and `llm.generation` — is forwarded.
fn is_forwardable(event_type: &str) -> bool {
    !matches!(
        event_type,
        OUTPUT_MESSAGE_DELTA | REASON_THINKING_DELTA | TOOL_OUTPUT_DELTA | TOOL_PROGRESS
    )
}

/// Map one host event to `trace/event` params (the `yolop_yep::TraceEventParams`
/// shape). `context` and `data` are carried verbatim so the exporter can stitch
/// spans and read token usage without the host modelling every event type.
pub(crate) fn trace_event_params(event: &Event) -> Value {
    serde_json::json!({
        "event_type": event.event_type,
        "id": event.id.to_string(),
        "ts": event.ts.to_rfc3339(),
        "session_id": event.session_id.to_string(),
        "context": serde_json::to_value(&event.context).unwrap_or(Value::Null),
        "data": serde_json::to_value(&event.data).unwrap_or(Value::Null),
    })
}

/// A trace-declaring extension paired with its live-server process, captured
/// while the harness is assembled (before `session_id` and the event broadcast
/// exist) and started once they do. Keeps `ExtensionProcess` out of the runtime
/// wiring — the runtime only holds this handle and calls [`TraceForwarder::start`].
pub(crate) struct TraceForwarder {
    name: String,
    process: Arc<ExtensionProcess>,
}

impl TraceForwarder {
    /// Build a forwarder for `capability` iff it declares the `trace` facet.
    /// `config` is the extension's effective harness config, so the trace
    /// server shares the same process (and Logfire token) as its other facets.
    pub(crate) fn for_capability(
        capability: &super::capability::ExtensionCapability,
        config: &serde_json::Value,
    ) -> Option<Self> {
        use everruns_core::Capability;
        capability.trace_process(config).map(|process| Self {
            name: capability.name().to_string(),
            process,
        })
    }

    /// Begin forwarding this session's events to the extension server.
    /// The returned handle completes once forwarding has stopped and the
    /// server has been flushed; teardown awaits it so the final events survive.
    pub(crate) fn start(
        self,
        session_id: SessionId,
        events: broadcast::Receiver<Event>,
        stop: watch::Receiver<bool>,
    ) -> tokio::task::JoinHandle<()> {
        spawn_forwarder(self.name, self.process, session_id, events, stop)
    }
}

/// Spawn the forwarder task: for `session_id`, push every forwardable event to
/// `process` as a `trace/event` notification. Ends when the broadcast closes
/// (session teardown).
pub(crate) fn spawn_forwarder(
    ext_name: String,
    process: Arc<ExtensionProcess>,
    session_id: SessionId,
    mut events: broadcast::Receiver<Event>,
    mut stop: watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // One forwarding step, shared by the live loop and the drain below.
        async fn forward(
            ext_name: &str,
            process: &ExtensionProcess,
            session_id: SessionId,
            event: Event,
        ) {
            if event.session_id != session_id || !is_forwardable(&event.event_type) {
                return;
            }
            if let Err(err) = process.send_trace_event(trace_event_params(&event)).await {
                tracing::warn!(
                    target: "yolop::ext", ext = %ext_name,
                    "trace/event forward failed: {err}"
                );
            }
        }

        loop {
            let received = tokio::select! {
                received = events.recv() => received,
                // Teardown asked us to stop. The broadcast may still be open
                // (the runtime outlives this task), so this, not stream close,
                // is what normally ends forwarding.
                _ = stop.changed() => {
                    // Everything already buffered still belongs to this
                    // session's trace, `turn.completed` above all. Drain it
                    // before flushing, or stopping would drop exactly the
                    // events the flush exists to save.
                    while let Ok(event) = events.try_recv() {
                        forward(&ext_name, &process, session_id, event).await;
                    }
                    break;
                }
            };
            match received {
                Ok(event) => forward(&ext_name, &process, session_id, event).await,
                // The exporter fell behind the broadcast buffer: skip the gap
                // rather than block the agent. A lossy trace beats a stalled run.
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::debug!(
                        target: "yolop::ext", ext = %ext_name,
                        "trace forwarder lagged, dropped {n} events"
                    );
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
        // The stream closed: the session is tearing down. Flush before the
        // process is dropped, or the tail of the stream dies with the child.
        // `turn.completed` is the one that matters most: it closes the trace's
        // root span, so losing it exports children with no root.
        process.shutdown_gracefully().await;
    })
}

/// Stops the session's trace forwarders and waits for their final flush.
///
/// Forwarding is fire-and-forget and the servers are `kill_on_drop`, so without
/// an explicit stop-and-wait the tail of the event stream dies with the child
/// at teardown. `turn.completed` is the casualty that matters: it closes the
/// trace's root span, so traces exported with no root and short runs exported
/// nothing at all.
pub struct TraceFlush {
    stop: watch::Sender<bool>,
    handles: Vec<tokio::task::JoinHandle<()>>,
}

impl TraceFlush {
    pub(crate) fn new(
        stop: watch::Sender<bool>,
        handles: Vec<tokio::task::JoinHandle<()>>,
    ) -> Self {
        Self { stop, handles }
    }

    /// Signal every forwarder to stop, then wait for each to flush its server.
    /// Bounded: a server that will not answer must not hold up process exit.
    pub async fn flush(self) {
        const BUDGET: std::time::Duration = std::time::Duration::from_secs(5);
        if self.handles.is_empty() {
            return;
        }
        let _ = self.stop.send(true);
        for handle in self.handles {
            if tokio::time::timeout(BUDGET, handle).await.is_err() {
                tracing::debug!(
                    target: "yolop::ext",
                    "trace forwarder did not flush within {BUDGET:?}; exiting anyway"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use everruns_core::{EventContext, EventRequest};

    #[test]
    fn deltas_are_dropped_lifecycle_is_kept() {
        assert!(is_forwardable("turn.started"));
        assert!(is_forwardable("tool.completed"));
        assert!(is_forwardable("llm.generation"));
        assert!(!is_forwardable(OUTPUT_MESSAGE_DELTA));
        assert!(!is_forwardable(TOOL_OUTPUT_DELTA));
        assert!(!is_forwardable(TOOL_PROGRESS));
    }

    #[test]
    fn params_carry_type_ids_and_verbatim_payload() {
        let session = SessionId::new();
        let request = EventRequest {
            event_type: "tool.completed".into(),
            ts: chrono::Utc::now(),
            session_id: session,
            context: EventContext::empty(),
            data: everruns_core::events::EventData::unsupported(
                "tool.completed".into(),
                serde_json::json!({ "tool_name": "bash" }),
            ),
            metadata: None,
            tags: None,
        };
        let event = request.into_event(everruns_provider::typed_id::EventId::new(), 1);
        let params = trace_event_params(&event);
        assert_eq!(params["event_type"], "tool.completed");
        assert_eq!(params["session_id"], session.to_string());
        assert_eq!(params["id"], event.id.to_string());
        assert!(params["ts"].as_str().unwrap().contains('T'));
        // The event `context` is carried through verbatim as a JSON object for
        // the exporter to stitch spans from (`data` likewise; here the fixture
        // uses an unsupported variant, which upstream serializes as null).
        assert!(params["context"].is_object(), "{params}");
    }
}

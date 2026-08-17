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
use tokio::sync::broadcast;

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
    pub(crate) fn start(self, session_id: SessionId, events: broadcast::Receiver<Event>) {
        spawn_forwarder(self.name, self.process, session_id, events);
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
) {
    tokio::spawn(async move {
        loop {
            match events.recv().await {
                Ok(event) if event.session_id == session_id => {
                    if !is_forwardable(&event.event_type) {
                        continue;
                    }
                    if let Err(err) = process.send_trace_event(trace_event_params(&event)).await {
                        tracing::warn!(
                            target: "yolop::ext", ext = %ext_name,
                            "trace/event forward failed: {err}"
                        );
                    }
                }
                Ok(_) => {}
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
    });
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

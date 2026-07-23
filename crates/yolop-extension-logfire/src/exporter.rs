//! The stateful trace exporter: folds the host's stream of agentic
//! `trace/event` notifications into OpenTelemetry spans and hands finished
//! batches to a sink as OTLP/HTTP JSON.
//!
//! Kept transport-free on purpose — the sink is a plain `FnMut(&str)` that
//! receives the serialized OTLP body — so the event→span logic is unit-tested
//! without a network (see the tests below) and `main.rs` only supplies the HTTP
//! POST. All work is best-effort: a malformed or unknown event is skipped, never
//! fatal, so a live agent run is never disturbed by the exporter.

use serde_json::{Value, json};
use std::collections::HashMap;

/// Families of events we render as spans. Everything else (session/task/file/
/// budget/context/message-delta) is ignored so the trace stays span-shaped.
/// Each is a `<family>.<phase>` event; `started` opens a span, a terminal phase
/// closes it.
const SPAN_FAMILIES: &[&str] = &["turn", "reason", "act", "tool"];

/// Terminal phases that close an open span (or, unpaired, emit a point span).
fn is_terminal_phase(phase: &str) -> bool {
    matches!(
        phase,
        "completed" | "failed" | "cancelled" | "sealed" | "recovered"
    )
}

/// One event, split into the fields the exporter reasons about.
struct Parsed<'a> {
    family: &'a str,
    phase: &'a str,
    ts_nanos: i128,
    session_id: &'a str,
    context: &'a Value,
    data: &'a Value,
    event_type: &'a str,
    event_id: &'a str,
}

/// An in-progress span awaiting its terminal event.
struct OpenSpan {
    span_id: [u8; 8],
    parent: Option<[u8; 8]>,
    name: String,
    start_nanos: i128,
    attributes: Vec<(String, Value)>,
}

/// A closed span ready to serialize into OTLP.
struct FinishedSpan {
    span_id: [u8; 8],
    parent: Option<[u8; 8]>,
    name: String,
    start_nanos: i128,
    end_nanos: i128,
    attributes: Vec<(String, Value)>,
    /// OTLP status code: 0 unset, 2 error.
    status: u8,
}

/// Folds events into spans and flushes finished batches through `sink`.
pub struct SpanExporter<F: FnMut(&str)> {
    service_name: String,
    sink: F,
    /// One trace per session, derived deterministically from the session id.
    trace_id: Option<[u8; 16]>,
    session_id: String,
    /// Open spans keyed by `<family>:<correlation-id>`.
    open: HashMap<String, OpenSpan>,
    /// The current turn's span id, so child spans (reason/act/tool) parent to it.
    turn_span: HashMap<String, [u8; 8]>,
    finished: Vec<FinishedSpan>,
}

/// Cap the buffer so a run that never hits a turn boundary can't grow unbounded.
const MAX_BUFFERED_SPANS: usize = 256;

impl<F: FnMut(&str)> SpanExporter<F> {
    pub fn new(service_name: impl Into<String>, sink: F) -> Self {
        Self {
            service_name: service_name.into(),
            sink,
            trace_id: None,
            session_id: String::new(),
            open: HashMap::new(),
            turn_span: HashMap::new(),
            finished: Vec::new(),
        }
    }

    /// Handle one `trace/event`. Never panics: unparseable or out-of-family
    /// events are dropped.
    pub fn handle(&mut self, params: &yolop_yep::TraceEventParams) {
        let Some(parsed) = parse(params) else {
            return;
        };
        if self.trace_id.is_none() {
            self.trace_id = Some(trace_id_for(parsed.session_id));
            self.session_id = parsed.session_id.to_string();
        }
        if !SPAN_FAMILIES.contains(&parsed.family) {
            return;
        }

        if parsed.phase == "started" {
            self.open_span(&parsed);
        } else if is_terminal_phase(parsed.phase) {
            self.close_span(&parsed);
        }

        // Flush at turn boundaries — one POST per turn — and as a backstop when
        // the buffer grows large without a boundary.
        if parsed.family == "turn" && is_terminal_phase(parsed.phase) {
            self.flush();
        } else if self.finished.len() >= MAX_BUFFERED_SPANS {
            self.flush();
        }
    }

    fn open_span(&mut self, parsed: &Parsed) {
        let key = correlation_key(parsed);
        let span_id = span_id_for(&key);
        let parent = self.parent_for(parsed);
        if parsed.family == "turn" {
            if let Some(turn) = turn_id(parsed.context) {
                self.turn_span.insert(turn.to_string(), span_id);
            }
        }
        self.open.insert(
            key,
            OpenSpan {
                span_id,
                parent,
                name: span_name(parsed),
                start_nanos: parsed.ts_nanos,
                attributes: attributes(parsed),
            },
        );
    }

    fn close_span(&mut self, parsed: &Parsed) {
        let key = correlation_key(parsed);
        let status = if parsed.phase == "failed" { 2 } else { 0 };
        let finished = match self.open.remove(&key) {
            Some(open) => FinishedSpan {
                span_id: open.span_id,
                parent: open.parent,
                name: open.name,
                start_nanos: open.start_nanos,
                end_nanos: parsed.ts_nanos,
                attributes: merge_attrs(open.attributes, attributes(parsed)),
                status,
            },
            // Unpaired terminal (missed/absent `started`): a point span so the
            // event is still recorded rather than silently dropped.
            None => FinishedSpan {
                span_id: span_id_for(&key),
                parent: self.parent_for(parsed),
                name: span_name(parsed),
                start_nanos: parsed.ts_nanos,
                end_nanos: parsed.ts_nanos,
                attributes: attributes(parsed),
                status,
            },
        };
        if parsed.family == "turn" {
            if let Some(turn) = turn_id(parsed.context) {
                self.turn_span.remove(turn);
            }
        }
        self.finished.push(finished);
    }

    /// Parent a child (reason/act/tool) to its enclosing turn span, if open.
    fn parent_for(&self, parsed: &Parsed) -> Option<[u8; 8]> {
        if parsed.family == "turn" {
            return None;
        }
        turn_id(parsed.context).and_then(|t| self.turn_span.get(t).copied())
    }

    /// Serialize the finished spans as one OTLP/HTTP JSON body and hand it to
    /// the sink. A no-op when there is nothing buffered.
    pub fn flush(&mut self) {
        if self.finished.is_empty() {
            return;
        }
        let Some(trace_id) = self.trace_id else {
            self.finished.clear();
            return;
        };
        let spans = std::mem::take(&mut self.finished);
        let body = otlp_body(&self.service_name, &self.session_id, trace_id, &spans);
        (self.sink)(&body);
    }
}

// ---------------------------------------------------------------------------
// Parsing / naming / attributes

fn parse(params: &yolop_yep::TraceEventParams) -> Option<Parsed<'_>> {
    let (family, phase) = params
        .event_type
        .split_once('.')
        .unwrap_or((params.event_type.as_str(), ""));
    let ts_nanos = params
        .ts
        .parse::<chrono::DateTime<chrono::Utc>>()
        .ok()?
        .timestamp_nanos_opt()?
        .into();
    Some(Parsed {
        family,
        phase,
        ts_nanos,
        session_id: &params.session_id,
        context: &params.context,
        data: &params.data,
        event_type: &params.event_type,
        event_id: &params.id,
    })
}

fn turn_id(context: &Value) -> Option<&str> {
    context.get("turn_id").and_then(Value::as_str)
}

/// A stable per-span key so `started` and its terminal event coincide. Prefers
/// the most specific correlation id available for the family and falls back to
/// the event id (which makes an unpaired point span rather than a wrong pair).
fn correlation_key(parsed: &Parsed) -> String {
    let candidates: &[&str] = match parsed.family {
        "tool" => &["tool_call_id", "exec_id", "call_id"],
        "turn" => &["turn_id"],
        _ => &["exec_id", "turn_id"],
    };
    for key in candidates {
        if let Some(id) = parsed.context.get(*key).and_then(Value::as_str) {
            return format!("{}:{id}", parsed.family);
        }
    }
    format!("{}:{}", parsed.family, parsed.event_id)
}

fn span_name(parsed: &Parsed) -> String {
    match parsed.family {
        "tool" => {
            let tool = parsed
                .data
                .get("tool_name")
                .or_else(|| parsed.data.get("name"))
                .and_then(Value::as_str);
            match tool {
                Some(name) => format!("tool {name}"),
                None => "tool".to_string(),
            }
        }
        other => other.to_string(),
    }
}

/// Base attributes for a span: the concrete event type plus any scalar fields
/// from the event `data` (token usage, model, tool name, …), namespaced.
fn attributes(parsed: &Parsed) -> Vec<(String, Value)> {
    let mut attrs = vec![("yolop.event_type".to_string(), json!(parsed.event_type))];
    if let Some(map) = parsed.data.as_object() {
        for (key, value) in map {
            if value.is_object() || value.is_array() {
                continue; // keep attributes flat and cheap
            }
            attrs.push((format!("yolop.{key}"), value.clone()));
        }
    }
    attrs
}

fn merge_attrs(
    mut base: Vec<(String, Value)>,
    extra: Vec<(String, Value)>,
) -> Vec<(String, Value)> {
    for (k, v) in extra {
        if !base.iter().any(|(bk, _)| *bk == k) {
            base.push((k, v));
        }
    }
    base
}

// ---------------------------------------------------------------------------
// Deterministic ids (FNV-1a; no rng dependency, reproducible in tests)

fn fnv1a_64(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for &b in bytes {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

fn trace_id_for(session_id: &str) -> [u8; 16] {
    // Two independent 64-bit hashes give a 128-bit id that is non-zero and
    // stable per session (all of a session's spans share one trace).
    let hi = fnv1a_64(session_id.as_bytes());
    let lo = fnv1a_64(format!("{session_id}#lo").as_bytes());
    let mut id = [0u8; 16];
    id[..8].copy_from_slice(&hi.to_be_bytes());
    id[8..].copy_from_slice(&lo.to_be_bytes());
    if id == [0u8; 16] {
        id[15] = 1;
    }
    id
}

fn span_id_for(key: &str) -> [u8; 8] {
    let mut id = fnv1a_64(key.as_bytes()).to_be_bytes();
    if id == [0u8; 8] {
        id[7] = 1;
    }
    id
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

// ---------------------------------------------------------------------------
// OTLP/HTTP JSON

fn otlp_attributes(attrs: &[(String, Value)]) -> Vec<Value> {
    attrs
        .iter()
        .map(|(k, v)| json!({ "key": k, "value": otlp_any_value(v) }))
        .collect()
}

fn otlp_any_value(v: &Value) -> Value {
    match v {
        Value::String(s) => json!({ "stringValue": s }),
        Value::Bool(b) => json!({ "boolValue": b }),
        Value::Number(n) if n.is_i64() || n.is_u64() => json!({ "intValue": n.to_string() }),
        Value::Number(n) => json!({ "doubleValue": n.as_f64().unwrap_or(0.0) }),
        other => json!({ "stringValue": other.to_string() }),
    }
}

/// Build the OTLP `POST /v1/traces` JSON body for a batch of finished spans.
fn otlp_body(
    service_name: &str,
    session_id: &str,
    trace_id: [u8; 16],
    spans: &[FinishedSpan],
) -> String {
    let trace_hex = hex(&trace_id);
    let otlp_spans: Vec<Value> = spans
        .iter()
        .map(|span| {
            json!({
                "traceId": trace_hex,
                "spanId": hex(&span.span_id),
                "parentSpanId": span.parent.map(|p| hex(&p)).unwrap_or_default(),
                "name": span.name,
                // SPAN_KIND_INTERNAL
                "kind": 1,
                "startTimeUnixNano": span.start_nanos.max(0).to_string(),
                "endTimeUnixNano": span.end_nanos.max(0).to_string(),
                "attributes": otlp_attributes(&span.attributes),
                "status": { "code": span.status },
            })
        })
        .collect();

    json!({
        "resourceSpans": [{
            "resource": {
                "attributes": otlp_attributes(&[
                    ("service.name".to_string(), json!(service_name)),
                    ("session.id".to_string(), json!(session_id)),
                ]),
            },
            "scopeSpans": [{
                "scope": { "name": "yolop-extension-logfire" },
                "spans": otlp_spans,
            }],
        }],
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    fn ev(event_type: &str, ts: &str, ctx: Value, data: Value) -> yolop_yep::TraceEventParams {
        yolop_yep::TraceEventParams {
            event_type: event_type.to_string(),
            id: format!("id-{event_type}"),
            ts: ts.to_string(),
            session_id: "sess-1".to_string(),
            context: ctx,
            data,
        }
    }

    /// A collector sink capturing each flushed OTLP body as parsed JSON.
    fn recorder() -> (Rc<RefCell<Vec<Value>>>, impl FnMut(&str)) {
        let out = Rc::new(RefCell::new(Vec::new()));
        let sink_out = out.clone();
        let sink = move |body: &str| {
            sink_out
                .borrow_mut()
                .push(serde_json::from_str(body).expect("valid json body"));
        };
        (out, sink)
    }

    #[test]
    fn a_turn_with_a_tool_flushes_one_batch_of_nested_spans() {
        let (out, sink) = recorder();
        let mut exporter = SpanExporter::new("yolop", sink);
        let turn = json!({ "turn_id": "t1" });
        let tool_ctx = json!({ "turn_id": "t1", "tool_call_id": "c1" });

        exporter.handle(&ev(
            "turn.started",
            "2024-01-01T00:00:00Z",
            turn.clone(),
            json!({}),
        ));
        exporter.handle(&ev(
            "tool.started",
            "2024-01-01T00:00:01Z",
            tool_ctx.clone(),
            json!({ "tool_name": "bash" }),
        ));
        exporter.handle(&ev(
            "tool.completed",
            "2024-01-01T00:00:02Z",
            tool_ctx,
            json!({ "tool_name": "bash", "ok": true }),
        ));
        // No flush yet — only a turn boundary flushes.
        assert!(out.borrow().is_empty());

        exporter.handle(&ev(
            "turn.completed",
            "2024-01-01T00:00:03Z",
            turn,
            json!({}),
        ));

        let batches = out.borrow();
        assert_eq!(batches.len(), 1, "one POST at the turn boundary");
        let spans = &batches[0]["resourceSpans"][0]["scopeSpans"][0]["spans"];
        let spans = spans.as_array().unwrap();
        assert_eq!(spans.len(), 2, "turn + tool");

        let turn_span = spans.iter().find(|s| s["name"] == "turn").unwrap();
        let tool_span = spans.iter().find(|s| s["name"] == "tool bash").unwrap();
        // Same trace, tool parented to the turn.
        assert_eq!(turn_span["traceId"], tool_span["traceId"]);
        assert_eq!(tool_span["parentSpanId"], turn_span["spanId"]);
        assert_eq!(turn_span["parentSpanId"], "");
        // Real duration, not a point.
        assert_ne!(tool_span["startTimeUnixNano"], tool_span["endTimeUnixNano"]);
        // Data scalars became attributes.
        let attrs = tool_span["attributes"].as_array().unwrap();
        assert!(
            attrs
                .iter()
                .any(|a| a["key"] == "yolop.tool_name" && a["value"]["stringValue"] == "bash")
        );
    }

    #[test]
    fn failed_event_marks_error_status() {
        let (out, sink) = recorder();
        let mut exporter = SpanExporter::new("yolop", sink);
        let turn = json!({ "turn_id": "t9" });
        exporter.handle(&ev(
            "turn.started",
            "2024-01-01T00:00:00Z",
            turn.clone(),
            json!({}),
        ));
        exporter.handle(&ev("turn.failed", "2024-01-01T00:00:01Z", turn, json!({})));
        let batches = out.borrow();
        let span = &batches[0]["resourceSpans"][0]["scopeSpans"][0]["spans"][0];
        assert_eq!(span["status"]["code"], 2, "ERROR");
    }

    #[test]
    fn non_span_families_are_ignored() {
        let (out, sink) = recorder();
        let mut exporter = SpanExporter::new("yolop", sink);
        // session/file/budget events don't open spans; flushing yields nothing.
        exporter.handle(&ev(
            "session.started",
            "2024-01-01T00:00:00Z",
            json!({}),
            json!({}),
        ));
        exporter.handle(&ev(
            "file.written",
            "2024-01-01T00:00:00Z",
            json!({}),
            json!({}),
        ));
        exporter.flush();
        assert!(out.borrow().is_empty());
    }

    #[test]
    fn ids_are_deterministic_and_nonzero() {
        assert_eq!(trace_id_for("sess-1"), trace_id_for("sess-1"));
        assert_ne!(trace_id_for("sess-1"), [0u8; 16]);
        assert_eq!(span_id_for("turn:t1"), span_id_for("turn:t1"));
        assert_ne!(span_id_for("turn:t1"), span_id_for("turn:t2"));
    }

    #[test]
    fn unparseable_timestamp_is_skipped_not_fatal() {
        let (out, sink) = recorder();
        let mut exporter = SpanExporter::new("yolop", sink);
        exporter.handle(&ev(
            "turn.started",
            "not-a-date",
            json!({ "turn_id": "t1" }),
            json!({}),
        ));
        exporter.flush();
        assert!(out.borrow().is_empty());
    }
}

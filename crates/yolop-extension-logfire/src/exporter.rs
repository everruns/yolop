//! Folds the host's stream of agentic `trace/event` notifications into
//! OpenTelemetry spans and emits them through the official OTLP exporter.
//!
//! The lifecycle events are *already-completed* facts (a `*.started` then a
//! terminal `*.completed`/`*.failed`), so we reconstruct spans with their real
//! start/end timestamps via the tracer's `SpanBuilder` and parent context —
//! rather than the usual "start a span, do work, end it" flow. One trace per
//! **turn**: `turn.*` is the root span and `reason`/`act`/`tool`/`llm` spans
//! nest under their turn's context. All work is best-effort: an unparseable or
//! out-of-family event is skipped, never fatal.

use opentelemetry::trace::{Span, SpanKind, Status, TraceContextExt, Tracer};
use opentelemetry::{Context, KeyValue};
use opentelemetry_sdk::trace::{SdkTracer, SdkTracerProvider, Span as SdkSpan};
use serde_json::Value;
use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Paired families: a `started` opens a span, a terminal phase closes it.
const PAIRED_FAMILIES: &[&str] = &["turn", "reason", "act", "tool"];

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
    ts: SystemTime,
    context: &'a Value,
    data: &'a Value,
    event_type: &'a str,
    event_id: &'a str,
}

/// Folds events into OpenTelemetry spans emitted through `tracer`.
pub struct TraceExporter {
    tracer: SdkTracer,
    /// Kept alive for the program's lifetime; dropping it flushes/shuts down
    /// the span processor.
    _provider: SdkTracerProvider,
    /// Open spans keyed by `<family>:<correlation-id>`, closed on their
    /// terminal event.
    open: HashMap<String, SdkSpan>,
    /// Parent context for a turn's children, keyed by turn id.
    turn_cx: HashMap<String, Context>,
}

impl TraceExporter {
    pub fn new(tracer: SdkTracer, provider: SdkTracerProvider) -> Self {
        Self {
            tracer,
            _provider: provider,
            open: HashMap::new(),
            turn_cx: HashMap::new(),
        }
    }

    /// Handle one `trace/event`. Never panics: unparseable or out-of-family
    /// events are dropped.
    pub fn handle(&mut self, params: &yolop_yep::TraceEventParams) {
        let Some(parsed) = parse(params) else {
            return;
        };
        match (parsed.family, parsed.phase) {
            (f, "started") if PAIRED_FAMILIES.contains(&f) => self.open_span(&parsed),
            (f, p) if PAIRED_FAMILIES.contains(&f) && is_terminal_phase(p) => {
                self.close_span(&parsed)
            }
            // `llm.generation` is a point event: a zero-duration span under the
            // turn, carrying model/token attributes.
            ("llm", _) => self.point_span(&parsed),
            _ => {}
        }
    }

    fn open_span(&mut self, parsed: &Parsed) {
        let parent_cx = self.parent_cx(parsed);
        let span = self
            .tracer
            .span_builder(span_name(parsed))
            .with_kind(SpanKind::Internal)
            .with_start_time(parsed.ts)
            .with_attributes(attributes(parsed))
            .start_with_context(&self.tracer, &parent_cx);
        // A turn is a root span; record its context so its children nest under it.
        if parsed.family == "turn"
            && let Some(turn) = turn_id(parsed.context)
        {
            let cx = Context::new().with_remote_span_context(span.span_context().clone());
            self.turn_cx.insert(turn.to_string(), cx);
        }
        self.open.insert(correlation_key(parsed), span);
    }

    fn close_span(&mut self, parsed: &Parsed) {
        match self.open.remove(&correlation_key(parsed)) {
            Some(mut span) => {
                span.set_attributes(attributes(parsed));
                if parsed.phase == "failed" {
                    span.set_status(Status::error("failed"));
                }
                span.end_with_timestamp(parsed.ts);
            }
            // Unpaired terminal (absent/missed `started`): a point span so the
            // event is still recorded rather than silently dropped.
            None => {
                let mut span = self.point(parsed);
                if parsed.phase == "failed" {
                    span.set_status(Status::error("failed"));
                }
                span.end_with_timestamp(parsed.ts);
            }
        }
        if parsed.family == "turn"
            && let Some(turn) = turn_id(parsed.context)
        {
            self.turn_cx.remove(turn);
        }
    }

    fn point_span(&mut self, parsed: &Parsed) {
        let mut span = self.point(parsed);
        span.end_with_timestamp(parsed.ts);
    }

    /// Start a zero-duration span at the event's timestamp, parented to its turn.
    fn point(&self, parsed: &Parsed) -> SdkSpan {
        let parent_cx = self.parent_cx(parsed);
        self.tracer
            .span_builder(span_name(parsed))
            .with_kind(SpanKind::Internal)
            .with_start_time(parsed.ts)
            .with_attributes(attributes(parsed))
            .start_with_context(&self.tracer, &parent_cx)
    }

    /// Parent context: a turn is a root (empty context, new trace); a child
    /// parents to its turn's span if that turn is open.
    fn parent_cx(&self, parsed: &Parsed) -> Context {
        if parsed.family == "turn" {
            return Context::new();
        }
        turn_id(parsed.context)
            .and_then(|t| self.turn_cx.get(t).cloned())
            .unwrap_or_default()
    }
}

// ---------------------------------------------------------------------------
// Parsing / naming / attributes (pure, unit-tested)

fn parse(params: &yolop_yep::TraceEventParams) -> Option<Parsed<'_>> {
    let (family, phase) = params
        .event_type
        .split_once('.')
        .unwrap_or((params.event_type.as_str(), ""));
    let ts = ts_to_system_time(&params.ts)?;
    Some(Parsed {
        family,
        phase,
        ts,
        context: &params.context,
        data: &params.data,
        event_type: &params.event_type,
        event_id: &params.id,
    })
}

fn ts_to_system_time(ts: &str) -> Option<SystemTime> {
    let nanos = ts
        .parse::<chrono::DateTime<chrono::Utc>>()
        .ok()?
        .timestamp_nanos_opt()?;
    (nanos >= 0).then(|| UNIX_EPOCH + Duration::from_nanos(nanos as u64))
}

fn turn_id(context: &Value) -> Option<&str> {
    context.get("turn_id").and_then(Value::as_str)
}

/// A stable per-span key so `started` and its terminal event coincide. Prefers
/// the most specific correlation id for the family, falling back to the event
/// id (an unpaired point span rather than a wrong pair).
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
        "tool" => match parsed
            .data
            .get("tool_name")
            .or_else(|| parsed.data.get("name"))
            .and_then(Value::as_str)
        {
            Some(name) => format!("tool {name}"),
            None => "tool".to_string(),
        },
        // `llm.generation` etc. read best as the full event type.
        "llm" => parsed.event_type.to_string(),
        other => other.to_string(),
    }
}

/// Span attributes: the concrete event type plus scalar `data` fields (token
/// usage, model, tool name, exit code, …), namespaced under `yolop.`.
fn attributes(parsed: &Parsed) -> Vec<KeyValue> {
    let mut attrs = vec![KeyValue::new(
        "yolop.event_type",
        parsed.event_type.to_string(),
    )];
    if let Some(map) = parsed.data.as_object() {
        for (key, value) in map {
            let name = format!("yolop.{key}");
            match value {
                Value::String(s) => attrs.push(KeyValue::new(name, s.clone())),
                Value::Bool(b) => attrs.push(KeyValue::new(name, *b)),
                Value::Number(n) if n.is_i64() => {
                    attrs.push(KeyValue::new(name, n.as_i64().unwrap()))
                }
                Value::Number(n) if n.is_u64() => {
                    attrs.push(KeyValue::new(name, n.as_u64().unwrap() as i64))
                }
                Value::Number(n) => attrs.push(KeyValue::new(name, n.as_f64().unwrap_or(0.0))),
                // Objects/arrays/null: skip, keeping attributes flat and cheap.
                _ => {}
            }
        }
    }
    attrs
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry::trace::{SpanId, Status as OtelStatus, TracerProvider as _};
    use opentelemetry_sdk::trace::{InMemorySpanExporter, InMemorySpanExporterBuilder};

    fn exporter() -> (TraceExporter, InMemorySpanExporter) {
        let mem = InMemorySpanExporterBuilder::new().build();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(mem.clone())
            .build();
        let tracer = provider.tracer("test");
        (TraceExporter::new(tracer, provider), mem)
    }

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

    #[test]
    fn a_turn_with_a_tool_produces_one_trace_with_nested_spans() {
        let (mut exp, mem) = exporter();
        let turn = serde_json::json!({ "turn_id": "t1" });
        let tool_ctx = serde_json::json!({ "turn_id": "t1", "tool_call_id": "c1" });

        exp.handle(&ev(
            "turn.started",
            "2024-01-01T00:00:00Z",
            turn.clone(),
            serde_json::json!({}),
        ));
        exp.handle(&ev(
            "tool.started",
            "2024-01-01T00:00:01Z",
            tool_ctx.clone(),
            serde_json::json!({ "tool_name": "bash" }),
        ));
        exp.handle(&ev(
            "tool.completed",
            "2024-01-01T00:00:02Z",
            tool_ctx,
            serde_json::json!({ "tool_name": "bash", "exit_code": 0 }),
        ));
        exp.handle(&ev(
            "turn.completed",
            "2024-01-01T00:00:03Z",
            turn,
            serde_json::json!({}),
        ));

        let spans = mem.get_finished_spans().expect("finished spans");
        assert_eq!(spans.len(), 2, "turn + tool");
        let turn_span = spans.iter().find(|s| s.name == "turn").expect("turn span");
        let tool_span = spans
            .iter()
            .find(|s| s.name == "tool bash")
            .expect("tool span");

        // One trace; tool nested under the turn; turn is the root.
        assert_eq!(
            turn_span.span_context.trace_id(),
            tool_span.span_context.trace_id()
        );
        assert_eq!(tool_span.parent_span_id, turn_span.span_context.span_id());
        assert_eq!(turn_span.parent_span_id, SpanId::INVALID);
        // Real duration.
        assert!(tool_span.end_time > tool_span.start_time);
        // Data scalars became attributes (string + int).
        assert!(
            tool_span
                .attributes
                .iter()
                .any(|kv| kv.key.as_str() == "yolop.tool_name" && kv.value.as_str() == "bash")
        );
        assert!(
            tool_span
                .attributes
                .iter()
                .any(|kv| kv.key.as_str() == "yolop.exit_code")
        );
    }

    #[test]
    fn llm_generation_is_a_point_span_under_its_turn() {
        let (mut exp, mem) = exporter();
        let turn = serde_json::json!({ "turn_id": "t1" });
        exp.handle(&ev(
            "turn.started",
            "2024-01-01T00:00:00Z",
            turn.clone(),
            serde_json::json!({}),
        ));
        exp.handle(&ev(
            "llm.generation",
            "2024-01-01T00:00:01Z",
            serde_json::json!({ "turn_id": "t1" }),
            serde_json::json!({ "model": "gpt", "input_tokens": 42, "output_tokens": 7 }),
        ));
        exp.handle(&ev(
            "turn.completed",
            "2024-01-01T00:00:02Z",
            turn,
            serde_json::json!({}),
        ));

        let spans = mem.get_finished_spans().unwrap();
        let llm = spans
            .iter()
            .find(|s| s.name == "llm.generation")
            .expect("llm span");
        let turn_span = spans.iter().find(|s| s.name == "turn").unwrap();
        assert_eq!(llm.parent_span_id, turn_span.span_context.span_id());
        assert!(
            llm.attributes
                .iter()
                .any(|kv| kv.key.as_str() == "yolop.input_tokens")
        );
    }

    #[test]
    fn failed_event_sets_error_status() {
        let (mut exp, mem) = exporter();
        let turn = serde_json::json!({ "turn_id": "t9" });
        exp.handle(&ev(
            "turn.started",
            "2024-01-01T00:00:00Z",
            turn.clone(),
            serde_json::json!({}),
        ));
        exp.handle(&ev(
            "turn.failed",
            "2024-01-01T00:00:01Z",
            turn,
            serde_json::json!({}),
        ));
        let spans = mem.get_finished_spans().unwrap();
        let turn_span = spans.iter().find(|s| s.name == "turn").unwrap();
        assert!(matches!(turn_span.status, OtelStatus::Error { .. }));
    }

    #[test]
    fn non_span_families_and_bad_timestamps_are_ignored() {
        let (mut exp, mem) = exporter();
        exp.handle(&ev(
            "session.started",
            "2024-01-01T00:00:00Z",
            serde_json::json!({}),
            serde_json::json!({}),
        ));
        exp.handle(&ev(
            "file.written",
            "2024-01-01T00:00:00Z",
            serde_json::json!({}),
            serde_json::json!({}),
        ));
        exp.handle(&ev(
            "turn.started",
            "not-a-date",
            serde_json::json!({ "turn_id": "t1" }),
            serde_json::json!({}),
        ));
        assert!(mem.get_finished_spans().unwrap().is_empty());
    }
}

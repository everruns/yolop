//! Folds the host's stream of agentic `trace/event` notifications into
//! OpenTelemetry spans and emits them through the official OTLP exporter.
//!
//! The lifecycle events are *already-completed* facts (a `*.started` then a
//! terminal `*.completed`/`*.failed`), so we reconstruct spans with their real
//! start/end timestamps via the tracer's `SpanBuilder` and parent context —
//! rather than the usual "start a span, do work, end it" flow.
//!
//! Hierarchy comes from the host, which already computes it: every event
//! carries `context.parent_span_id`, so `llm.generation` nests under its
//! `reason` span and `tool.*` under its `act` span. `turn.*` is the root and
//! carries no span id of its own, so we mint one and its children name it by
//! `turn_id`. Re-deriving parenting from `turn_id` alone, as this exporter
//! once did, flattens every trace into one level.
//!
//! Attributes follow the Gen-AI semantic conventions (see [`crate::gen_ai`]),
//! so the spans read the same as the ones the host's own OTel and Braintrust
//! backends emit. All work is best-effort: an unparseable or out-of-family
//! event is skipped, never fatal.

use crate::gen_ai;
use opentelemetry::trace::{Span, SpanKind, Status, TraceContextExt, Tracer};
use opentelemetry::{Context, KeyValue};
use opentelemetry_sdk::trace::{SdkTracer, SdkTracerProvider, Span as SdkSpan};
use serde_json::Value;
use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Paired families: a `started` opens a span, a terminal phase closes it.
const PAIRED_FAMILIES: &[&str] = &["turn", "reason", "act", "tool", "thinking"];

/// Model-authored content, which spans never carry.
///
/// The Gen-AI conventions put prompts, completions, and reasoning behind an
/// explicit content-capture opt-in, and this exporter has none: `thinking`
/// holds the model's full chain-of-thought, so exporting it would put reasoning
/// text into a tracing backend as a side effect of enabling the extension.
fn is_model_content(key: &str) -> bool {
    matches!(key, "thinking")
}

/// Families that are an agentic phase of a turn, and so own their `exec_id`.
fn is_phase_family(family: &str) -> bool {
    matches!(family, "reason" | "act" | "turn")
}

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
    session_id: &'a str,
    /// This event's own span id, absent on `turn.*`.
    span_id: Option<&'a str>,
    /// The parent the host computed. Equal to `turn_id` for a turn's direct
    /// children, since the turn has no span id to name.
    parent_span_id: Option<&'a str>,
    turn_id: Option<&'a str>,
    exec_id: Option<&'a str>,
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
    /// Span context by the id its children name it with: a span's own
    /// `span_id`, and additionally the `turn_id` for a turn's minted root.
    span_cx: HashMap<String, Context>,
    /// Ids registered during a turn, so the whole trace's contexts are dropped
    /// when the turn closes rather than accumulating for the session's life.
    turn_ids: HashMap<String, Vec<String>>,
}

impl TraceExporter {
    pub fn new(tracer: SdkTracer, provider: SdkTracerProvider) -> Self {
        Self {
            tracer,
            _provider: provider,
            open: HashMap::new(),
            span_cx: HashMap::new(),
            turn_ids: HashMap::new(),
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
        self.register_cx(parsed, &span);
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
            && let Some(turn) = parsed.turn_id
            && let Some(ids) = self.turn_ids.remove(turn)
        {
            for id in ids {
                self.span_cx.remove(&id);
            }
        }
    }

    fn point_span(&mut self, parsed: &Parsed) {
        let mut span = self.point(parsed);
        span.end_with_timestamp(parsed.ts);
    }

    /// Record a span's context under the ids its children will name it by, and
    /// remember those ids so the turn can drop them when it ends.
    fn register_cx(&mut self, parsed: &Parsed, span: &SdkSpan) {
        let cx = Context::new().with_remote_span_context(span.span_context().clone());
        let mut keys: Vec<String> = Vec::new();
        if let Some(id) = parsed.span_id {
            keys.push(id.to_string());
        }
        // The turn's root has no span id of its own; its children name it by
        // `turn_id`, so register the root under that instead.
        if parsed.family == "turn"
            && let Some(turn) = parsed.turn_id
        {
            keys.push(turn.to_string());
        }
        // A phase owns its `exec_id`, which is how a child that carries no
        // `parent_span_id` of its own (thinking) finds the phase it belongs to.
        // Only the phase may claim it: an exec groups the phase *and* its
        // children (reason with llm, act with tool), so letting a child
        // register would make it the exec's representative instead.
        if is_phase_family(parsed.family)
            && let Some(exec) = parsed.exec_id
        {
            keys.push(exec.to_string());
        }
        for key in &keys {
            self.span_cx.insert(key.clone(), cx.clone());
        }
        if let Some(turn) = parsed.turn_id {
            self.turn_ids
                .entry(turn.to_string())
                .or_default()
                .extend(keys);
        }
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

    /// Parent context, taken from the hierarchy the host already computed.
    ///
    /// `parent_span_id` names either another span or, for a turn's direct
    /// children, the turn by its `turn_id`. Falling back to the turn keeps a
    /// child attached when its parent's `started` was missed, and a turn (or
    /// an event whose parent is unknown) roots a new trace.
    fn parent_cx(&self, parsed: &Parsed) -> Context {
        if parsed.family == "turn" {
            return Context::new();
        }
        parsed
            .parent_span_id
            .and_then(|id| self.span_cx.get(id).cloned())
            // A child with no parent id of its own belongs to its exec's phase.
            // Skipped for phases themselves, which own that exec id.
            .or_else(|| {
                (!is_phase_family(parsed.family))
                    .then_some(parsed.exec_id)
                    .flatten()
                    .and_then(|e| self.span_cx.get(e).cloned())
            })
            .or_else(|| parsed.turn_id.and_then(|t| self.span_cx.get(t).cloned()))
            .unwrap_or_default()
    }
}

// ---------------------------------------------------------------------------
// Parsing / naming / attributes (pure, unit-tested)

fn parse(params: &yolop_yep::TraceEventParams) -> Option<Parsed<'_>> {
    let ts = ts_to_system_time(&params.ts)?;
    // `reason.thinking.*` is a span in its own right, nested inside its reason
    // phase. Left as family `reason` with phase `thinking.started` it matched
    // neither the `started` nor the terminal arm, so thinking was dropped.
    let (family, phase) = match (params.family(), params.phase()) {
        ("reason", rest) => match rest.strip_prefix("thinking.") {
            Some(phase) => ("thinking", phase),
            None => ("reason", rest),
        },
        (family, phase) => (family, phase),
    };
    Some(Parsed {
        family,
        phase,
        ts,
        context: &params.context,
        data: &params.data,
        event_type: &params.event_type,
        event_id: &params.id,
        session_id: &params.session_id,
        span_id: params.span_id(),
        parent_span_id: params.parent_span_id(),
        turn_id: params.turn_id(),
        exec_id: params.exec_id(),
    })
}

fn ts_to_system_time(ts: &str) -> Option<SystemTime> {
    let nanos = ts
        .parse::<chrono::DateTime<chrono::Utc>>()
        .ok()?
        .timestamp_nanos_opt()?;
    (nanos >= 0).then(|| UNIX_EPOCH + Duration::from_nanos(nanos as u64))
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

/// The Gen-AI operation a family maps to, or `None` for families the spec has
/// no operation for.
fn operation_name(family: &str) -> Option<&'static str> {
    match family {
        "turn" => Some(gen_ai::operation::INVOKE_AGENT),
        "reason" => Some(gen_ai::operation::REASON),
        "act" => Some(gen_ai::operation::ACT),
        "tool" => Some(gen_ai::operation::EXECUTE_TOOL),
        "llm" => Some(gen_ai::operation::CHAT),
        "thinking" => Some(gen_ai::operation::THINKING),
        _ => None,
    }
}

/// Gen-AI semantic-convention attributes for this event.
///
/// Without these the spans carried only a private `yolop.*` namespace, so a
/// tracing UI that reads the conventions showed empty model, token, and cost
/// fields. The LLM values live in `data.metadata`, which the generic `yolop.*`
/// flattening skips because it is an object rather than a scalar.
fn gen_ai_attributes(parsed: &Parsed) -> Vec<KeyValue> {
    let mut attrs = Vec::new();
    if let Some(op) = operation_name(parsed.family) {
        attrs.push(KeyValue::new(gen_ai::OPERATION_NAME, op));
    }
    if !parsed.session_id.is_empty() {
        attrs.push(KeyValue::new(
            gen_ai::CONVERSATION_ID,
            parsed.session_id.to_string(),
        ));
    }
    if let Some(agent) = parsed.data.get("agent_id").and_then(Value::as_str) {
        attrs.push(KeyValue::new(gen_ai::AGENT_ID, agent.to_string()));
    }

    if parsed.family == "tool" {
        if let Some(name) = parsed
            .data
            .get("tool_name")
            .or_else(|| parsed.data.get("name"))
            .and_then(Value::as_str)
        {
            attrs.push(KeyValue::new(gen_ai::TOOL_NAME, name.to_string()));
        }
        if let Some(call) = parsed
            .context
            .get("tool_call_id")
            .or_else(|| parsed.context.get("call_id"))
            .and_then(Value::as_str)
        {
            attrs.push(KeyValue::new(gen_ai::TOOL_CALL_ID, call.to_string()));
        }
    }

    let Some(metadata) = parsed.data.get("metadata") else {
        return attrs;
    };
    if let Some(model) = metadata.get("model").and_then(Value::as_str) {
        attrs.push(KeyValue::new(gen_ai::REQUEST_MODEL, model.to_string()));
        attrs.push(KeyValue::new(gen_ai::RESPONSE_MODEL, model.to_string()));
    }
    if let Some(provider) = metadata.get("provider").and_then(Value::as_str) {
        attrs.push(KeyValue::new(gen_ai::PROVIDER_NAME, provider.to_string()));
        // Both spellings: UIs differ on which they read.
        attrs.push(KeyValue::new(gen_ai::SYSTEM, provider.to_string()));
    }
    if let Some(reasons) = metadata.get("finish_reasons").and_then(Value::as_array) {
        let reasons: Vec<opentelemetry::StringValue> = reasons
            .iter()
            .filter_map(Value::as_str)
            .map(|r| r.to_string().into())
            .collect();
        if !reasons.is_empty() {
            attrs.push(KeyValue::new(
                gen_ai::RESPONSE_FINISH_REASONS,
                opentelemetry::Value::Array(reasons.into()),
            ));
        }
    }
    if let Some(usage) = metadata.get("usage") {
        for (field, key) in [
            ("input_tokens", gen_ai::USAGE_INPUT_TOKENS),
            ("output_tokens", gen_ai::USAGE_OUTPUT_TOKENS),
            ("cache_read_tokens", gen_ai::USAGE_CACHE_READ_TOKENS),
            ("cache_creation_tokens", gen_ai::USAGE_CACHE_CREATION_TOKENS),
        ] {
            if let Some(count) = usage.get(field).and_then(Value::as_i64) {
                attrs.push(KeyValue::new(key, count));
            }
        }
        // Cost has no Gen-AI convention, so it keeps the `yolop.` namespace.
        // Emitted explicitly because it sits two levels deep, below the one
        // level the generic flattening walks.
        if let Some(cost) = usage.get("estimated_cost_usd").and_then(Value::as_f64) {
            attrs.push(KeyValue::new("yolop.usage.estimated_cost_usd", cost));
        }
    }
    attrs
}

/// Span attributes: the Gen-AI conventions, the concrete event type, plus
/// scalar `data` fields (tool name, exit code, …) namespaced under `yolop.`.
fn attributes(parsed: &Parsed) -> Vec<KeyValue> {
    let mut attrs = gen_ai_attributes(parsed);
    attrs.push(KeyValue::new(
        "yolop.event_type",
        parsed.event_type.to_string(),
    ));
    if let Some(map) = parsed.data.as_object() {
        for (key, value) in map {
            if is_model_content(key) {
                continue;
            }
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
                // One level of nesting, flattened: `metadata` holds the LLM
                // model, durations, and cost, which were dropped entirely when
                // objects were skipped outright.
                Value::Object(nested) => {
                    for (sub, value) in nested {
                        let name = format!("{name}.{sub}");
                        match value {
                            Value::String(s) => attrs.push(KeyValue::new(name, s.clone())),
                            Value::Bool(b) => attrs.push(KeyValue::new(name, *b)),
                            Value::Number(n) if n.is_i64() => {
                                attrs.push(KeyValue::new(name, n.as_i64().unwrap()))
                            }
                            Value::Number(n) if n.is_u64() => {
                                attrs.push(KeyValue::new(name, n.as_u64().unwrap() as i64))
                            }
                            Value::Number(n) => {
                                attrs.push(KeyValue::new(name, n.as_f64().unwrap_or(0.0)))
                            }
                            _ => {}
                        }
                    }
                }
                // Arrays/null: skip, keeping attributes flat and cheap.
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

    /// Context shapes copied from a real Anthropic session: the host computes
    /// the tree, and `llm.generation` parents to its `reason` span rather than
    /// straight to the turn. Re-deriving parenting from `turn_id` flattened
    /// this, so every exported trace lost a level.
    #[test]
    fn the_hosts_span_tree_is_preserved_not_flattened() {
        let (mut exp, mem) = exporter();
        let reason_span = "01a01d59-fd57-7083-b7b4-a0c51beaf956";
        let act_span = "01a01d5a-070b-7340-9c38-52256b2f1e7e";

        exp.handle(&ev(
            "turn.started",
            "2024-01-01T00:00:00Z",
            serde_json::json!({ "turn_id": "t1" }),
            serde_json::json!({}),
        ));
        // reason parents to the turn, naming it by turn id (the turn has no
        // span id of its own).
        exp.handle(&ev(
            "reason.started",
            "2024-01-01T00:00:01Z",
            serde_json::json!({
                "turn_id": "t1", "span_id": reason_span, "parent_span_id": "t1"
            }),
            serde_json::json!({}),
        ));
        // The generation nests under reason.
        exp.handle(&ev(
            "llm.generation",
            "2024-01-01T00:00:02Z",
            serde_json::json!({
                "turn_id": "t1", "span_id": "gen-1", "parent_span_id": reason_span
            }),
            serde_json::json!({ "metadata": { "model": "claude-opus-4-8" } }),
        ));
        exp.handle(&ev(
            "reason.completed",
            "2024-01-01T00:00:03Z",
            serde_json::json!({
                "turn_id": "t1", "span_id": reason_span, "parent_span_id": "t1"
            }),
            serde_json::json!({}),
        ));
        exp.handle(&ev(
            "act.started",
            "2024-01-01T00:00:04Z",
            serde_json::json!({
                "turn_id": "t1", "span_id": act_span, "parent_span_id": "t1"
            }),
            serde_json::json!({}),
        ));
        // The tool nests under act, not under the turn.
        exp.handle(&ev(
            "tool.started",
            "2024-01-01T00:00:05Z",
            serde_json::json!({
                "turn_id": "t1", "span_id": "tool-1", "parent_span_id": act_span,
                "tool_call_id": "c1"
            }),
            serde_json::json!({ "tool_name": "bash" }),
        ));
        exp.handle(&ev(
            "tool.completed",
            "2024-01-01T00:00:06Z",
            serde_json::json!({
                "turn_id": "t1", "span_id": "tool-1", "parent_span_id": act_span,
                "tool_call_id": "c1"
            }),
            serde_json::json!({ "tool_name": "bash" }),
        ));
        exp.handle(&ev(
            "act.completed",
            "2024-01-01T00:00:07Z",
            serde_json::json!({
                "turn_id": "t1", "span_id": act_span, "parent_span_id": "t1"
            }),
            serde_json::json!({}),
        ));
        exp.handle(&ev(
            "turn.completed",
            "2024-01-01T00:00:08Z",
            serde_json::json!({ "turn_id": "t1" }),
            serde_json::json!({}),
        ));

        let spans = mem.get_finished_spans().expect("spans");
        let by_name = |n: &str| {
            spans
                .iter()
                .find(|s| s.name == n)
                .unwrap_or_else(|| panic!("missing span {n}"))
                .clone()
        };
        let turn = by_name("turn");
        let reason = by_name("reason");
        let act = by_name("act");
        let tool = by_name("tool bash");
        let llm = by_name("llm.generation");

        assert_eq!(turn.parent_span_id, SpanId::INVALID, "turn is the root");
        assert_eq!(reason.parent_span_id, turn.span_context.span_id());
        assert_eq!(act.parent_span_id, turn.span_context.span_id());
        assert_eq!(
            llm.parent_span_id,
            reason.span_context.span_id(),
            "the generation belongs to its reason span, not the turn"
        );
        assert_eq!(
            tool.parent_span_id,
            act.span_context.span_id(),
            "the tool call belongs to its act span, not the turn"
        );
        // Still one trace.
        for span in [&reason, &act, &tool, &llm] {
            assert_eq!(span.span_context.trace_id(), turn.span_context.trace_id());
        }
    }

    /// `reason.thinking.*` carries no span id of its own, only the `exec_id` of
    /// the reason phase it belongs to. Left as family `reason` with phase
    /// `thinking.started` it matched neither arm and was dropped entirely.
    #[test]
    fn thinking_becomes_a_span_nested_in_its_reason_phase() {
        let (mut exp, mem) = exporter();
        let reason_span = "reason-span-1";
        let exec = "exec_1";

        exp.handle(&ev(
            "turn.started",
            "2024-01-01T00:00:00Z",
            serde_json::json!({ "turn_id": "t1" }),
            serde_json::json!({}),
        ));
        exp.handle(&ev(
            "reason.started",
            "2024-01-01T00:00:01Z",
            serde_json::json!({
                "turn_id": "t1", "exec_id": exec,
                "span_id": reason_span, "parent_span_id": "t1"
            }),
            serde_json::json!({}),
        ));
        // Exactly the shape the host emits: no span id, no parent id, just the
        // exec it belongs to.
        exp.handle(&ev(
            "reason.thinking.started",
            "2024-01-01T00:00:02Z",
            serde_json::json!({ "turn_id": "t1", "exec_id": exec }),
            serde_json::json!({ "model": "claude-opus-4-8" }),
        ));
        exp.handle(&ev(
            "reason.thinking.completed",
            "2024-01-01T00:00:03Z",
            serde_json::json!({ "turn_id": "t1", "exec_id": exec }),
            serde_json::json!({ "thinking": "secret chain of thought" }),
        ));
        // Close the phase so it is exported and can be compared against.
        exp.handle(&ev(
            "reason.completed",
            "2024-01-01T00:00:04Z",
            serde_json::json!({
                "turn_id": "t1", "exec_id": exec,
                "span_id": reason_span, "parent_span_id": "t1"
            }),
            serde_json::json!({}),
        ));

        let spans = mem.get_finished_spans().expect("spans");
        let thinking = spans
            .iter()
            .find(|s| s.name == "thinking")
            .expect("thinking must produce a span");
        let reason = spans
            .iter()
            .find(|s| s.name == "reason")
            .map(|s| s.span_context.span_id());

        // Paired into one span with real duration, under its reason phase.
        assert!(thinking.end_time > thinking.start_time);
        assert_eq!(
            Some(thinking.parent_span_id),
            reason,
            "thinking belongs to the reason phase it ran inside"
        );
        assert!(
            thinking
                .attributes
                .iter()
                .any(|kv| kv.key.as_str() == "gen_ai.operation.name"
                    && kv.value.to_string() == "thinking")
        );
    }

    /// Reasoning text is model-authored content, and the conventions put
    /// content behind an opt-in this exporter does not have. Enabling the
    /// extension must not ship chain-of-thought to a tracing backend.
    #[test]
    fn thinking_text_is_never_exported() {
        let (mut exp, mem) = exporter();
        exp.handle(&ev(
            "reason.thinking.started",
            "2024-01-01T00:00:02Z",
            serde_json::json!({ "turn_id": "t1", "exec_id": "e1" }),
            serde_json::json!({}),
        ));
        exp.handle(&ev(
            "reason.thinking.completed",
            "2024-01-01T00:00:03Z",
            serde_json::json!({ "turn_id": "t1", "exec_id": "e1" }),
            serde_json::json!({ "thinking": "secret chain of thought" }),
        ));

        let spans = mem.get_finished_spans().expect("spans");
        for span in &spans {
            for kv in &span.attributes {
                assert!(
                    !kv.value.to_string().contains("secret chain of thought"),
                    "reasoning text leaked into attribute {}",
                    kv.key.as_str()
                );
            }
        }
    }

    /// The values a tracing UI reads. They live in `data.metadata`, which the
    /// generic `yolop.*` flattening skipped as an object, so these fields
    /// showed up empty.
    #[test]
    fn llm_generation_carries_the_gen_ai_conventions() {
        let (mut exp, mem) = exporter();
        exp.handle(&ev(
            "llm.generation",
            "2024-01-01T00:00:02Z",
            serde_json::json!({ "turn_id": "t1", "span_id": "gen-1" }),
            serde_json::json!({
                "metadata": {
                    "model": "claude-opus-4-8",
                    "provider": "anthropic",
                    "finish_reasons": ["tool_calls"],
                    "usage": {
                        "input_tokens": 2,
                        "output_tokens": 69,
                        "cache_read_tokens": 16089,
                        "cache_creation_tokens": 0,
                        "estimated_cost_usd": 0.0097795
                    }
                }
            }),
        ));

        let spans = mem.get_finished_spans().expect("spans");
        let span = spans.first().expect("one span");
        let attr = |key: &str| {
            span.attributes
                .iter()
                .find(|kv| kv.key.as_str() == key)
                .map(|kv| kv.value.to_string())
        };

        assert_eq!(attr("gen_ai.operation.name").as_deref(), Some("chat"));
        assert_eq!(
            attr("gen_ai.request.model").as_deref(),
            Some("claude-opus-4-8")
        );
        assert_eq!(
            attr("gen_ai.response.model").as_deref(),
            Some("claude-opus-4-8")
        );
        assert_eq!(attr("gen_ai.provider.name").as_deref(), Some("anthropic"));
        assert_eq!(attr("gen_ai.system").as_deref(), Some("anthropic"));
        assert_eq!(attr("gen_ai.usage.input_tokens").as_deref(), Some("2"));
        assert_eq!(attr("gen_ai.usage.output_tokens").as_deref(), Some("69"));
        assert_eq!(
            attr("gen_ai.usage.cache_read_tokens").as_deref(),
            Some("16089")
        );
        assert_eq!(attr("gen_ai.conversation.id").as_deref(), Some("sess-1"));
        assert!(
            attr("gen_ai.response.finish_reasons").is_some(),
            "finish reasons must be recorded"
        );
        // Cost survives the one-level flattening rather than being dropped.
        assert!(
            attr("yolop.usage.estimated_cost_usd").is_some(),
            "attributes were: {:?}",
            span.attributes
                .iter()
                .map(|kv| kv.key.as_str().to_string())
                .collect::<Vec<_>>()
        );
    }

    /// A tool span names the tool and its call id per the conventions.
    #[test]
    fn a_tool_span_carries_the_gen_ai_tool_attributes() {
        let (mut exp, mem) = exporter();
        exp.handle(&ev(
            "tool.completed",
            "2024-01-01T00:00:05Z",
            serde_json::json!({ "turn_id": "t1", "tool_call_id": "call-42" }),
            serde_json::json!({ "tool_name": "bash" }),
        ));
        let spans = mem.get_finished_spans().expect("spans");
        let span = spans.first().expect("one span");
        let attr = |key: &str| {
            span.attributes
                .iter()
                .find(|kv| kv.key.as_str() == key)
                .map(|kv| kv.value.to_string())
        };
        assert_eq!(
            attr("gen_ai.operation.name").as_deref(),
            Some("execute_tool")
        );
        assert_eq!(attr("gen_ai.tool.name").as_deref(), Some("bash"));
        assert_eq!(attr("gen_ai.tool.call.id").as_deref(), Some("call-42"));
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

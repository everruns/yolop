//! Translation from everruns runtime events into ACP `session/update`s.
//!
//! The runtime emits a rich event stream (reasoning deltas, message deltas,
//! tool lifecycle, todo writes). ACP wants a narrower vocabulary: assistant
//! message chunks, thought chunks, tool calls, tool-call updates, and plans.
//! [`Translator`] is the pure, per-turn state machine that performs that
//! mapping. Keeping it free of I/O is what makes the wire behaviour fully
//! unit-testable without a live model.

use std::collections::HashSet;

use everruns_core::events::{Event as RuntimeEvent, EventData, ToolCompletedData};
use everruns_core::message::{ContentPart, MessageRole};
use serde_json::Value;

use super::protocol::{
    self, ContentBlock, Plan, PlanEntry, PlanEntryPriority, PlanEntryStatus, SessionUpdate,
    ToolCall, ToolCallContent, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields, ToolKind,
};

/// The runtime's todo tool. write_todos updates are surfaced as ACP plans
/// rather than opaque tool calls so editors render them in their plan UI.
const WRITE_TODOS: &str = "write_todos";

/// Per-turn translator. Construct one per `session/prompt` so the
/// per-message streaming flag and event-dedup set reset between turns.
#[derive(Default)]
pub struct Translator {
    /// Whether the in-flight assistant message has streamed any delta. When
    /// a provider streams (Anthropic), we forward deltas and suppress the
    /// terminal full-text chunk to avoid duplication. When it does not
    /// (fixed llmsim, some OpenAI paths), we synthesise one chunk from the
    /// completed message instead.
    current_message_streamed: bool,
    /// Event ids already translated. The runtime can redeliver the same
    /// event across the live broadcast and the catch-up drain; dedup keeps
    /// the client from seeing doubles.
    seen: HashSet<String>,
}

impl Translator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Translate a single runtime event into zero or more ACP updates.
    /// Returns an empty vec for events with no client-visible mapping.
    pub fn on_event(&mut self, event: &RuntimeEvent) -> Vec<SessionUpdate> {
        if !self.seen.insert(event.id.to_string()) {
            return Vec::new();
        }
        match &event.data {
            EventData::OutputMessageStarted(_) => {
                self.current_message_streamed = false;
                Vec::new()
            }
            EventData::OutputMessageDelta(data) => {
                if data.delta.is_empty() {
                    return Vec::new();
                }
                self.current_message_streamed = true;
                vec![SessionUpdate::AgentMessageChunk(protocol::text_chunk(
                    &data.delta,
                ))]
            }
            EventData::OutputMessageCompleted(data) => {
                if self.current_message_streamed {
                    return Vec::new();
                }
                if data.message.role != MessageRole::Agent || data.message.has_tool_calls() {
                    return Vec::new();
                }
                match data.message.text().map(str::trim) {
                    Some(text) if !text.is_empty() => {
                        vec![SessionUpdate::AgentMessageChunk(protocol::text_chunk(text))]
                    }
                    _ => Vec::new(),
                }
            }
            EventData::ReasonThinkingDelta(data) => {
                if data.delta.is_empty() {
                    return Vec::new();
                }
                vec![SessionUpdate::AgentThoughtChunk(protocol::text_chunk(
                    &data.delta,
                ))]
            }
            EventData::ReasonItem(data) => data
                .summary
                .iter()
                .filter_map(|segment| {
                    let trimmed = segment.trim();
                    (!trimmed.is_empty())
                        .then(|| SessionUpdate::AgentThoughtChunk(protocol::text_chunk(trimmed)))
                })
                .collect(),
            EventData::ToolStarted(data) => {
                let name = data.tool_call.name.as_str();
                if name == WRITE_TODOS {
                    return plan_from_value(&data.tool_call.arguments)
                        .map(|entries| vec![SessionUpdate::Plan(Plan::new(entries))])
                        .unwrap_or_default();
                }
                let title = data
                    .narration
                    .as_deref()
                    .or(data.display_name.as_deref())
                    .unwrap_or(name)
                    .to_string();
                vec![SessionUpdate::ToolCall(
                    ToolCall::new(data.tool_call.id.clone(), title)
                        .kind(tool_kind(name))
                        .status(ToolCallStatus::InProgress)
                        .raw_input(non_null(data.tool_call.arguments.clone())),
                )]
            }
            EventData::ToolCompleted(data) => {
                if data.tool_name == WRITE_TODOS {
                    // The result echoes the authoritative todo list with
                    // updated statuses; re-emit the plan so the client's view
                    // reflects completion.
                    return result_value(data)
                        .as_ref()
                        .and_then(plan_from_value)
                        .map(|entries| vec![SessionUpdate::Plan(Plan::new(entries))])
                        .unwrap_or_default();
                }
                let status = if data.success {
                    ToolCallStatus::Completed
                } else {
                    ToolCallStatus::Failed
                };
                let content = tool_result_content(data)
                    .map(|block| vec![ToolCallContent::Content(protocol::Content::new(block))])
                    .unwrap_or_default();
                vec![SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                    data.tool_call_id.clone(),
                    ToolCallUpdateFields::new().status(status).content(content),
                ))]
            }
            _ => Vec::new(),
        }
    }
}

/// Map a runtime tool name to an ACP tool kind so editors can pick the right
/// affordance. Unknown tools fall back to `other`.
fn tool_kind(name: &str) -> ToolKind {
    match name {
        "read_file" | "stat_file" | "list_directory" => ToolKind::Read,
        "grep" | "grep_files" | "duckduckgo_search" => ToolKind::Search,
        "edit_file" | "write_file" | "create_directory" => ToolKind::Edit,
        "delete_file" => ToolKind::Delete,
        "bash" => ToolKind::Execute,
        "web_fetch" => ToolKind::Fetch,
        _ => ToolKind::Other,
    }
}

/// One concise text block summarising a finished tool call, or `None` when
/// there is nothing worth surfacing.
fn tool_result_content(data: &ToolCompletedData) -> Option<ContentBlock> {
    let summary = crate::app::summarize_tool_result(data);
    let trimmed = summary.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(protocol::text_block(trimmed))
    }
}

/// Parse the runtime's `{ "todos": [...] }` shape (used by both the
/// write_todos arguments and its result) into ACP plan entries.
fn plan_from_value(value: &Value) -> Option<Vec<PlanEntry>> {
    let todos = value.get("todos")?.as_array()?;
    let entries = todos
        .iter()
        .filter_map(|todo| {
            let content = todo.get("content").and_then(Value::as_str)?;
            if content.trim().is_empty() {
                return None;
            }
            let status = match todo.get("status").and_then(Value::as_str) {
                Some("completed") => PlanEntryStatus::Completed,
                Some("in_progress") => PlanEntryStatus::InProgress,
                _ => PlanEntryStatus::Pending,
            };
            Some(PlanEntry::new(content, PlanEntryPriority::Medium, status))
        })
        .collect::<Vec<_>>();
    Some(entries)
}

/// Decode the JSON payload a tool returned, if any. Mirrors the runtime's
/// convention of stashing structured results as JSON text in the first
/// content part.
fn result_value(data: &ToolCompletedData) -> Option<Value> {
    let parts = data.result.as_ref()?;
    for part in parts {
        if let ContentPart::Text(t) = part
            && let Ok(v) = serde_json::from_str::<Value>(&t.text)
        {
            return Some(v);
        }
    }
    None
}

fn non_null(value: Value) -> Option<Value> {
    if value.is_null() { None } else { Some(value) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use everruns_core::events::{
        Event, EventContext, OutputMessageCompletedData, OutputMessageDeltaData,
        ReasonThinkingDeltaData, ToolCompletedData, ToolStartedData,
    };
    use everruns_core::message::Message;
    use everruns_core::tool_types::ToolCall;
    use everruns_core::typed_id::{EventId, SessionId, TurnId};
    use serde_json::json;

    fn event(data: EventData) -> Event {
        Event {
            id: EventId::new(),
            event_type: data.event_type().to_string(),
            ts: Utc::now(),
            session_id: SessionId::new(),
            context: EventContext::empty(),
            data,
            metadata: None,
            tags: None,
            sequence: None,
        }
    }

    #[test]
    fn streaming_deltas_become_message_chunks() {
        let mut t = Translator::new();
        let updates = t.on_event(&event(EventData::OutputMessageDelta(
            OutputMessageDeltaData {
                turn_id: TurnId::new(),
                delta: "Hel".into(),
                accumulated: "Hel".into(),
            },
        )));
        assert_eq!(
            updates,
            vec![SessionUpdate::AgentMessageChunk(protocol::text_chunk(
                "Hel"
            ))]
        );
    }

    #[test]
    fn completed_message_suppressed_after_streaming() {
        let mut t = Translator::new();
        let _ = t.on_event(&event(EventData::OutputMessageDelta(
            OutputMessageDeltaData {
                turn_id: TurnId::new(),
                delta: "Hi".into(),
                accumulated: "Hi".into(),
            },
        )));
        let completed = t.on_event(&event(EventData::OutputMessageCompleted(
            OutputMessageCompletedData {
                message: Message::assistant("Hi"),
                metadata: None,
                usage: None,
                error_code: None,
                error_fields: None,
            },
        )));
        assert!(
            completed.is_empty(),
            "streamed text must not be re-sent: {completed:?}"
        );
    }

    #[test]
    fn completed_message_synthesised_when_not_streamed() {
        let mut t = Translator::new();
        let updates = t.on_event(&event(EventData::OutputMessageCompleted(
            OutputMessageCompletedData {
                message: Message::assistant("full answer"),
                metadata: None,
                usage: None,
                error_code: None,
                error_fields: None,
            },
        )));
        assert_eq!(
            updates,
            vec![SessionUpdate::AgentMessageChunk(protocol::text_chunk(
                "full answer"
            ))]
        );
    }

    #[test]
    fn thinking_deltas_become_thought_chunks() {
        let mut t = Translator::new();
        let updates = t.on_event(&event(EventData::ReasonThinkingDelta(
            ReasonThinkingDeltaData {
                turn_id: TurnId::new(),
                delta: "pondering".into(),
                accumulated: "pondering".into(),
            },
        )));
        assert_eq!(
            updates,
            vec![SessionUpdate::AgentThoughtChunk(protocol::text_chunk(
                "pondering"
            ))]
        );
    }

    #[test]
    fn tool_started_maps_kind_and_in_progress_status() {
        let mut t = Translator::new();
        let updates = t.on_event(&event(EventData::ToolStarted(ToolStartedData {
            tool_call: ToolCall {
                id: "call_1".into(),
                name: "bash".into(),
                arguments: json!({ "command": "ls" }),
            },
            tool_call_fingerprint: None,
            display_name: Some("Bash".into()),
            narration: Some("Listing files".into()),
        })));
        assert_eq!(
            updates,
            vec![SessionUpdate::ToolCall(
                protocol::ToolCall::new("call_1", "Listing files")
                    .kind(ToolKind::Execute)
                    .status(ToolCallStatus::InProgress)
                    .raw_input(json!({ "command": "ls" })),
            )]
        );
    }

    #[test]
    fn tool_completed_failure_maps_to_failed_status() {
        let mut t = Translator::new();
        let updates = t.on_event(&event(EventData::ToolCompleted(ToolCompletedData {
            tool_call_id: "call_1".into(),
            tool_name: "bash".into(),
            tool_call_fingerprint: None,
            tool_result_fingerprint: None,
            display_name: None,
            success: false,
            status: "error".into(),
            result: None,
            error: Some("boom".into()),
            duration_ms: None,
            capability_id: None,
            capability_name: None,
            narration: None,
        })));
        assert_eq!(updates.len(), 1);
        match &updates[0] {
            SessionUpdate::ToolCallUpdate(update) => {
                assert_eq!(update.tool_call_id.to_string(), "call_1");
                assert_eq!(update.fields.status, Some(ToolCallStatus::Failed));
                assert_eq!(
                    update.fields.content,
                    Some(vec![protocol::content("error: boom")])
                );
            }
            other => panic!("expected tool_call_update, got {other:?}"),
        }
    }

    #[test]
    fn write_todos_started_becomes_plan() {
        let mut t = Translator::new();
        let updates = t.on_event(&event(EventData::ToolStarted(ToolStartedData {
            tool_call: ToolCall {
                id: "call_todos".into(),
                name: "write_todos".into(),
                arguments: json!({
                    "todos": [
                        { "content": "first", "status": "completed" },
                        { "content": "second", "status": "in_progress" },
                        { "content": "third", "status": "pending" },
                    ]
                }),
            },
            tool_call_fingerprint: None,
            display_name: None,
            narration: None,
        })));
        assert_eq!(
            updates,
            vec![SessionUpdate::Plan(Plan::new(vec![
                PlanEntry::new(
                    "first",
                    PlanEntryPriority::Medium,
                    PlanEntryStatus::Completed,
                ),
                PlanEntry::new(
                    "second",
                    PlanEntryPriority::Medium,
                    PlanEntryStatus::InProgress,
                ),
                PlanEntry::new("third", PlanEntryPriority::Medium, PlanEntryStatus::Pending,),
            ]))]
        );
    }

    #[test]
    fn duplicate_event_id_is_ignored() {
        let mut t = Translator::new();
        let ev = event(EventData::OutputMessageDelta(OutputMessageDeltaData {
            turn_id: TurnId::new(),
            delta: "x".into(),
            accumulated: "x".into(),
        }));
        assert_eq!(t.on_event(&ev).len(), 1);
        assert_eq!(t.on_event(&ev).len(), 0, "second delivery must be ignored");
    }
}

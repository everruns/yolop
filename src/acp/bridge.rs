//! Translation from everruns runtime events into ACP `session/update`s.
//!
//! The runtime emits a rich event stream (reasoning deltas and summaries,
//! message deltas, tool lifecycle, todo writes). ACP wants a narrower
//! vocabulary: assistant message chunks, thought chunks, tool calls,
//! tool-call updates, and plans.
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
    /// Tool calls already introduced to the client. Persisted history keeps
    /// completed assistant messages and tool completions, but not the
    /// transient `tool.started` events used by the live bridge.
    started_tool_calls: HashSet<String>,
    /// Replay mode is used only for `session/load`, where ACP requires the
    /// agent to stream the historical user turns back to the client. Live
    /// `session/prompt` handling leaves this off because the client already
    /// has the prompt it just sent.
    replay_history: bool,
}

impl Translator {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn for_replay() -> Self {
        Self {
            replay_history: true,
            ..Self::default()
        }
    }

    /// Translate a single runtime event into zero or more ACP updates.
    /// Returns an empty vec for events with no client-visible mapping.
    pub fn on_event(&mut self, event: &RuntimeEvent) -> Vec<SessionUpdate> {
        if !self.seen.insert(event.id.to_string()) {
            return Vec::new();
        }
        match &event.data {
            EventData::InputMessage(data) => {
                if !self.replay_history {
                    return Vec::new();
                }
                if data.message.role != MessageRole::User {
                    return Vec::new();
                }
                match data.message.text().map(str::trim) {
                    Some(text) if !text.is_empty() => {
                        vec![SessionUpdate::UserMessageChunk(protocol::text_chunk(text))]
                    }
                    _ => Vec::new(),
                }
            }
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
                if data.message.role != MessageRole::Agent {
                    return Vec::new();
                }
                let mut updates = data
                    .message
                    .text()
                    .map(str::trim)
                    .filter(|text| !text.is_empty())
                    .map(|text| vec![SessionUpdate::AgentMessageChunk(protocol::text_chunk(text))])
                    .unwrap_or_default();
                if data.message.has_tool_calls() {
                    if !self.replay_history {
                        return updates;
                    }
                    updates.extend(data.message.content.iter().filter_map(|part| {
                        match part {
                            ContentPart::ToolCall(call) if call.name == WRITE_TODOS => {
                                plan_from_value(&call.arguments)
                                    .map(|entries| SessionUpdate::Plan(Plan::new(entries)))
                            }
                            ContentPart::ToolCall(call)
                                if self.started_tool_calls.insert(call.id.clone()) =>
                            {
                                Some(SessionUpdate::ToolCall(
                                    ToolCall::new(call.id.clone(), call.name.clone())
                                        .kind(tool_kind(&call.name))
                                        .status(ToolCallStatus::InProgress)
                                        .raw_input(non_null(call.arguments.clone())),
                                ))
                            }
                            _ => None,
                        }
                    }));
                }
                updates
            }
            EventData::ReasonThinkingDelta(data) => {
                if data.delta.is_empty() {
                    return Vec::new();
                }
                vec![SessionUpdate::AgentThoughtChunk(protocol::text_chunk(
                    &data.delta,
                ))]
            }
            EventData::ReasonItem(data) => {
                // Provider-curated summaries are safe public narration, unlike
                // plaintext extended thinking. ACP has no commentary update,
                // so match the terminal projection and use an agent message.
                let narration = data
                    .summary
                    .iter()
                    .map(|segment| segment.trim())
                    .filter(|segment| !segment.is_empty())
                    .collect::<Vec<_>>()
                    .join("\n\n");
                if narration.is_empty() {
                    Vec::new()
                } else {
                    vec![SessionUpdate::AgentMessageChunk(protocol::text_chunk(
                        narration,
                    ))]
                }
            }
            EventData::ToolStarted(data) => {
                let name = data.tool_call.name.as_str();
                if name == WRITE_TODOS {
                    return plan_from_value(&data.tool_call.arguments)
                        .map(|entries| vec![SessionUpdate::Plan(Plan::new(entries))])
                        .unwrap_or_default();
                }
                if !self.started_tool_calls.insert(data.tool_call.id.clone()) {
                    return Vec::new();
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
                let title = data
                    .narration
                    .as_deref()
                    .or(data.display_name.as_deref())
                    .unwrap_or(&data.tool_name)
                    .to_string();
                let mut updates = Vec::new();
                if self.replay_history && self.started_tool_calls.insert(data.tool_call_id.clone())
                {
                    updates.push(SessionUpdate::ToolCall(
                        ToolCall::new(data.tool_call_id.clone(), title.clone())
                            .kind(tool_kind(&data.tool_name))
                            .status(ToolCallStatus::InProgress),
                    ));
                }
                let mut fields = ToolCallUpdateFields::new().status(status).content(content);
                if self.replay_history {
                    fields = fields.title(title);
                }
                updates.push(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                    data.tool_call_id.clone(),
                    fields,
                )));
                updates
            }
            _ => Vec::new(),
        }
    }
}

/// Map built-in and dynamically prefixed MCP tool names to ACP's semantic
/// categories. Unknown tools are executable actions, which is more useful to
/// clients than ACP's omitted `other` default.
fn tool_kind(name: &str) -> ToolKind {
    let name = everruns_core::parse_mcp_tool_name(name)
        .map(|(_, tool)| tool)
        .unwrap_or_else(|| name.to_string());
    let normalized = name.to_ascii_lowercase();
    let tokens = normalized
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|token| !token.is_empty())
        .collect::<Vec<_>>();
    let has = |values: &[&str]| tokens.iter().any(|token| values.contains(token));

    if has(&["delete", "remove", "unlink"]) {
        ToolKind::Delete
    } else if has(&["move", "rename"]) {
        ToolKind::Move
    } else if has(&[
        "search", "find", "grep", "query", "lookup", "map", "symbols",
    ]) {
        ToolKind::Search
    } else if has(&["fetch", "browse", "download", "http", "web"]) {
        ToolKind::Fetch
    } else if has(&["read", "list", "stat", "get", "show", "inspect", "view"]) {
        ToolKind::Read
    } else if has(&[
        "edit", "write", "create", "update", "set", "upsert", "patch", "replace", "insert",
        "enable", "disable",
    ]) {
        ToolKind::Edit
    } else if has(&["think", "plan", "todo"]) {
        ToolKind::Think
    } else if has(&["switch"]) {
        ToolKind::SwitchMode
    } else {
        ToolKind::Execute
    }
}

/// One concise text block summarising a finished tool call, or `None` when
/// there is nothing worth surfacing.
fn tool_result_content(data: &ToolCompletedData) -> Option<ContentBlock> {
    let summary = crate::transcript::summarize_tool_result(data);
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
        Event, EventContext, OutputMessageCompletedData, OutputMessageDeltaData, ReasonItemData,
        ReasonThinkingDeltaData, ToolCompletedData, ToolStartedData,
    };
    use everruns_core::message::{ExecutionPhase, Message};
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
                phase: None,
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
                phase: None,
            },
        )));
        let completed = t.on_event(&event(EventData::OutputMessageCompleted(
            OutputMessageCompletedData {
                message: Message::assistant("Hi"),
                metadata: None,
                usage: None,
                error_code: None,
                error_fields: None,
                error_disclosure: None,
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
                error_disclosure: None,
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
    fn completed_tool_call_message_keeps_public_commentary() {
        let mut t = Translator::new();
        let mut message = Message::assistant_with_tools(
            "I’ll inspect the event bridge next.",
            vec![ToolCall {
                id: "call_1".into(),
                name: "read_file".into(),
                arguments: json!({ "path": "src/acp/bridge.rs" }),
            }],
        );
        message.phase = Some(ExecutionPhase::Commentary);
        let updates = t.on_event(&event(EventData::OutputMessageCompleted(
            OutputMessageCompletedData {
                message,
                metadata: None,
                usage: None,
                error_code: None,
                error_fields: None,
                error_disclosure: None,
            },
        )));

        assert_eq!(
            updates,
            vec![SessionUpdate::AgentMessageChunk(protocol::text_chunk(
                "I’ll inspect the event bridge next."
            ))]
        );
    }

    #[test]
    fn reasoning_summaries_render_as_public_narration() {
        let mut t = Translator::new();
        let updates = t.on_event(&event(EventData::ReasonItem(ReasonItemData {
            turn_id: TurnId::new(),
            provider: "openai".into(),
            model: Some("gpt-5".into()),
            item_id: "reason_1".into(),
            encrypted_content: Some("opaque".into()),
            summary: vec![
                "**Investigating event semantics**".into(),
                " **Comparing ACP projections** ".into(),
            ],
            token_count: None,
        })));

        assert_eq!(
            updates,
            vec![SessionUpdate::AgentMessageChunk(protocol::text_chunk(
                "**Investigating event semantics**\n\n**Comparing ACP projections**"
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
    fn tool_started_uses_in_progress_status_with_semantic_kind() {
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
        let serialized = serde_json::to_value(&updates[0]).unwrap();
        assert_eq!(serialized["status"], "in_progress");
        assert_eq!(
            match &updates[0] {
                SessionUpdate::ToolCall(call) => call.kind,
                other => panic!("expected tool call, got {other:?}"),
            },
            ToolKind::Execute
        );
        assert_eq!(serialized["kind"], "execute");
    }

    #[test]
    fn tool_kind_classifies_builtin_and_mcp_tools() {
        assert_eq!(tool_kind("read_file"), ToolKind::Read);
        assert_eq!(tool_kind("repo_map"), ToolKind::Search);
        assert_eq!(tool_kind("web_fetch"), ToolKind::Fetch);
        assert_eq!(tool_kind("edit_file"), ToolKind::Edit);
        assert_eq!(tool_kind("bash"), ToolKind::Execute);
        assert_eq!(tool_kind("mcp_paseo__list_files"), ToolKind::Read);
        assert_eq!(tool_kind("mcp_paseo__search_files"), ToolKind::Search);
        assert_eq!(tool_kind("mcp_paseo__build_repo_map"), ToolKind::Search);
        assert_eq!(tool_kind("mcp_paseo__custom_action"), ToolKind::Execute);
    }

    #[test]
    fn mcp_tool_started_emits_semantic_kind_on_the_acp_update() {
        let mut translator = Translator::new();
        let updates = translator.on_event(&event(EventData::ToolStarted(ToolStartedData {
            tool_call: ToolCall {
                id: "call_1".into(),
                name: "mcp_paseo__build_repo_map".into(),
                arguments: json!({ "path": "src" }),
            },
            tool_call_fingerprint: None,
            display_name: Some("Build repo map".into()),
            narration: Some("Build repo map: src".into()),
        })));

        let serialized = serde_json::to_value(&updates[0]).unwrap();
        assert_eq!(serialized["title"], "Build repo map: src");
        assert_eq!(serialized["kind"], "search");
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
    fn tool_completed_failure_with_result_payload_still_includes_error_content() {
        let mut t = Translator::new();
        let updates = t.on_event(&event(EventData::ToolCompleted(ToolCompletedData {
            tool_call_id: "call_1".into(),
            tool_name: "web_fetch".into(),
            tool_call_fingerprint: None,
            tool_result_fingerprint: None,
            display_name: Some("Web Fetch".into()),
            success: false,
            status: "error".into(),
            result: Some(vec![ContentPart::text(json!({ "ok": false }).to_string())]),
            error: Some("Invalid URL: must start with http:// or https://".into()),
            duration_ms: None,
            capability_id: None,
            capability_name: None,
            narration: None,
        })));
        assert_eq!(updates.len(), 1);
        match &updates[0] {
            SessionUpdate::ToolCallUpdate(update) => {
                assert_eq!(update.fields.status, Some(ToolCallStatus::Failed));
                assert_eq!(
                    update.fields.content,
                    Some(vec![protocol::content(
                        "error: Invalid URL: must start with http:// or https://"
                    )])
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
            phase: None,
        }));
        assert_eq!(t.on_event(&ev).len(), 1);
        assert_eq!(t.on_event(&ev).len(), 0, "second delivery must be ignored");
    }

    #[test]
    fn replay_mode_emits_user_messages() {
        let mut t = Translator::for_replay();
        let updates = t.on_event(&event(EventData::InputMessage(
            everruns_core::events::InputMessageData::new(Message::user("prior prompt")),
        )));
        assert_eq!(
            updates,
            vec![SessionUpdate::UserMessageChunk(protocol::text_chunk(
                "prior prompt"
            ))]
        );
    }

    #[test]
    fn replay_mode_reconstructs_tool_calls_from_completed_agent_messages() {
        let mut t = Translator::for_replay();
        let message = Message::assistant_with_tools(
            "",
            vec![ToolCall {
                id: "call_1".into(),
                name: "bash".into(),
                arguments: json!({ "command": "ls" }),
            }],
        );

        let updates = t.on_event(&event(EventData::OutputMessageCompleted(
            OutputMessageCompletedData {
                message,
                metadata: None,
                usage: None,
                error_code: None,
                error_fields: None,
                error_disclosure: None,
            },
        )));

        assert_eq!(
            updates,
            vec![SessionUpdate::ToolCall(
                protocol::ToolCall::new("call_1", "bash")
                    .kind(ToolKind::Execute)
                    .status(ToolCallStatus::InProgress)
                    .raw_input(json!({ "command": "ls" })),
            )]
        );
    }

    #[test]
    fn replay_mode_does_not_duplicate_a_reconstructed_tool_start() {
        let mut t = Translator::for_replay();
        let call = ToolCall {
            id: "call_1".into(),
            name: "bash".into(),
            arguments: json!({ "command": "ls" }),
        };
        let completed = event(EventData::OutputMessageCompleted(
            OutputMessageCompletedData {
                message: Message::assistant_with_tools("", vec![call.clone()]),
                metadata: None,
                usage: None,
                error_code: None,
                error_fields: None,
                error_disclosure: None,
            },
        ));
        let started = event(EventData::ToolStarted(ToolStartedData {
            tool_call: call,
            tool_call_fingerprint: None,
            display_name: Some("Bash".into()),
            narration: Some("Listing files".into()),
        }));

        assert_eq!(t.on_event(&completed).len(), 1);
        assert!(t.on_event(&started).is_empty());
    }

    #[test]
    fn replay_mode_never_emits_an_orphaned_tool_completion() {
        let mut t = Translator::for_replay();
        let updates = t.on_event(&event(EventData::ToolCompleted(ToolCompletedData {
            tool_call_id: "call_1".into(),
            tool_name: "bash".into(),
            tool_call_fingerprint: None,
            tool_result_fingerprint: None,
            display_name: Some("Bash".into()),
            success: true,
            status: "success".into(),
            result: None,
            error: None,
            duration_ms: None,
            capability_id: None,
            capability_name: None,
            narration: Some("Listed files".into()),
        })));

        assert_eq!(updates.len(), 2);
        match (&updates[0], &updates[1]) {
            (SessionUpdate::ToolCall(call), SessionUpdate::ToolCallUpdate(update)) => {
                assert_eq!(call.tool_call_id.to_string(), "call_1");
                assert_eq!(call.title, "Listed files");
                assert_eq!(update.tool_call_id.to_string(), "call_1");
                assert_eq!(update.fields.title.as_deref(), Some("Listed files"));
                assert_eq!(update.fields.status, Some(ToolCallStatus::Completed));
            }
            other => panic!("expected tool call followed by completion, got {other:?}"),
        }
    }

    #[test]
    fn live_mode_suppresses_user_messages() {
        let mut t = Translator::new();
        let updates = t.on_event(&event(EventData::InputMessage(
            everruns_core::events::InputMessageData::new(Message::user("current prompt")),
        )));
        assert!(updates.is_empty());
    }
}

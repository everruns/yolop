//! The TUI view model and the single place that interprets `everruns_core`
//! runtime events / tool results.
//!
//! This module owns the display data types (`ChatLine`, `Author`,
//! `ActivityStatus`, `StreamPreview`, `StreamKind`) and the `TurnEvent` channel
//! protocol that [`crate::runtime::session::Session`] emits while a turn runs. It is the
//! one boundary that knows about `EventData` / `Message`; the rest of the TUI
//! (`crate::tui`) consumes only the view-model types defined here. Functions are
//! pure over runtime types — no terminal I/O. Presentation concerns (colors)
//! live in `crate::tui::render`.

use everruns_core::events::{Event as RuntimeEvent, EventData, ToolCompletedData, ToolStartedData};
use everruns_core::message::{ContentPart, Message, MessageRole};
use everruns_core::tools::ToolExecutionResult;
use serde_json::Value;
use std::collections::HashMap;
use tokio::sync::mpsc;

// ---------- view-model types ----------

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Author {
    User,
    Assistant,
    Narration,
    Tool,
    ToolDetail,
    Stderr,
    Sandbox,
    Diff,
    System,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChatLine {
    pub author: Author,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamPreview {
    pub kind: StreamKind,
    pub text: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamKind {
    Assistant,
    Tool,
}

#[derive(Clone, Debug)]
pub struct ActivityStatus {
    pub text: String,
    pub(crate) fallback: bool,
}

/// Events emitted by [`crate::runtime::session::Session`] while a turn (or `!shell`
/// command) runs. The TUI drains these from the per-turn channel; it never sees
/// the underlying runtime events.
#[derive(Debug)]
pub(crate) enum TurnEvent {
    Lines(Vec<ChatLine>),
    Activity(ActivityStatus),
    /// Replace the live streaming preview shown above the input.
    /// `None` clears the preview.
    Stream(Option<StreamPreview>),
    Tokens(u64),
    /// Prompt tokens the latest LLM generation consumed — the current fill of
    /// the model's context window. Replaces (not accumulates) the prior value.
    ContextUsed(u32),
    Done,
    Failed(String),
}

/// Tracks the most recently active delta stream so we can drop the streaming
/// preview as soon as a matching `*.completed` arrives. Per-turn state — one
/// `DeltaRouter` per turn.
#[derive(Default)]
pub(crate) struct DeltaRouter {
    last_assistant_turn: Option<everruns_core::typed_id::TurnId>,
    last_tool_call: Option<String>,
    write_todos_args: HashMap<String, Value>,
}

// ---------- live event translation ----------

pub(crate) fn handle_live_event(
    event: &RuntimeEvent,
    emitted_events: &mut std::collections::HashSet<String>,
    router: &mut DeltaRouter,
    tx: &mpsc::UnboundedSender<TurnEvent>,
) {
    if !emitted_events.insert(event.id.to_string()) {
        return;
    }

    match &event.data {
        EventData::OutputMessageDelta(data) => {
            router.last_assistant_turn = Some(data.turn_id);
            let _ = tx.send(TurnEvent::Stream(Some(StreamPreview {
                kind: StreamKind::Assistant,
                text: data.accumulated.clone(),
            })));
            return;
        }
        EventData::OutputMessageCompleted(_) | EventData::OutputMessageReplaced(_)
            if router.last_assistant_turn.is_some() =>
        {
            router.last_assistant_turn = None;
            let _ = tx.send(TurnEvent::Stream(None));
        }
        EventData::ToolOutputDelta(data) => {
            router.last_tool_call = Some(data.tool_call_id.clone());
            let text = format!(
                "{} [{}] {}",
                data.tool_name,
                data.stream,
                data.delta.trim_end()
            );
            let _ = tx.send(TurnEvent::Stream(Some(StreamPreview {
                kind: StreamKind::Tool,
                text,
            })));
            return;
        }
        EventData::ToolCompleted(data)
            if router.last_tool_call.as_deref() == Some(data.tool_call_id.as_str()) =>
        {
            router.last_tool_call = None;
            let _ = tx.send(TurnEvent::Stream(None));
        }
        _ => {}
    }

    if let Some(tokens) = tokens_for_event(event) {
        let _ = tx.send(TurnEvent::Tokens(tokens));
    }
    if let Some(context) = context_tokens_for_event(event) {
        let _ = tx.send(TurnEvent::ContextUsed(context));
    }
    remember_write_todos_args(event, router);
    if let Some(activity) = status_for_event(event) {
        let _ = tx.send(TurnEvent::Activity(activity));
    }
    let lines = lines_for_event_with_router(event, router);
    if !lines.is_empty() {
        let _ = tx.send(TurnEvent::Lines(lines));
    }
}

pub(crate) fn remember_write_todos_args(event: &RuntimeEvent, router: &mut DeltaRouter) {
    if let EventData::ToolStarted(data) = &event.data
        && data.tool_call.name == "write_todos"
    {
        router
            .write_todos_args
            .insert(data.tool_call.id.clone(), data.tool_call.arguments.clone());
    }
}

pub(crate) fn tokens_for_event(event: &RuntimeEvent) -> Option<u64> {
    match &event.data {
        EventData::ReasonItem(data) => data.token_count.map(u64::from),
        _ => None,
    }
}

/// Prompt-side token count for an LLM generation — the full context sent to the
/// model (non-cached input plus both cache buckets, which are disjoint from
/// `input_tokens`). This is the current context-window fill, taken from the most
/// recent generation rather than accumulated across the turn.
pub(crate) fn context_tokens_for_event(event: &RuntimeEvent) -> Option<u32> {
    let EventData::LlmGeneration(data) = &event.data else {
        return None;
    };
    let usage = data.metadata.usage.as_ref()?;
    Some(
        usage
            .input_tokens
            .saturating_add(usage.cache_read_tokens.unwrap_or(0))
            .saturating_add(usage.cache_creation_tokens.unwrap_or(0)),
    )
}

pub(crate) fn lines_for_event_with_router(
    event: &RuntimeEvent,
    router: &mut DeltaRouter,
) -> Vec<ChatLine> {
    match &event.data {
        EventData::ToolCompleted(data) if data.tool_name == "write_todos" => {
            todo_lines_for_result_or_args(data, &mut router.write_todos_args)
        }
        _ => lines_for_event(event),
    }
}

pub fn lines_for_event(event: &RuntimeEvent) -> Vec<ChatLine> {
    match &event.data {
        EventData::ReasonStarted(_) => Vec::new(),
        EventData::ReasonCompleted(data) => {
            // Render pre-tool-call narration. When there are no tool calls
            // the final assistant message will arrive via the message loop
            // shortly, so we'd duplicate it; keep the has_tool_calls gate.
            if data.success && data.has_tool_calls {
                let mut lines = Vec::new();
                if let Some(text) = data
                    .text_preview
                    .as_deref()
                    .map(str::trim)
                    .filter(|text| !text.is_empty())
                {
                    lines.push(ChatLine {
                        author: Author::Narration,
                        text: text.to_string(),
                    });
                }
                lines
            } else {
                Vec::new()
            }
        }
        EventData::ReasonItem(data) => data
            .summary
            .iter()
            .filter_map(|segment| {
                let trimmed = segment.trim();
                (!trimmed.is_empty()).then(|| ChatLine {
                    author: Author::Narration,
                    text: trimmed.to_string(),
                })
            })
            .collect(),
        EventData::OutputMessageCompleted(_) => Vec::new(),
        EventData::ToolCompleted(data) => {
            if data.tool_name == "write_todos" {
                return todo_lines_for_result(data);
            }
            let marker = if data.success { "✓" } else { "✗" };
            let label = data
                .narration
                .as_deref()
                .or(data.display_name.as_deref())
                .unwrap_or(data.tool_name.as_str());
            let summary = summarize_tool_result(data);
            let mut lines = vec![ChatLine {
                author: Author::Tool,
                text: if summary.is_empty() {
                    format!("{marker} {label}")
                } else {
                    format!("{marker} {label}  {summary}")
                },
            }];
            if data.tool_name == "bash"
                && let Some(result) = result_value(data)
            {
                append_bash_diagnostics(&mut lines, &result);
            }
            if (data.tool_name == "edit_file" || data.tool_name == "ast_edit")
                && let Some(diff) = extract_field(data, "diff")
            {
                for line in diff.lines().take(40) {
                    lines.push(ChatLine {
                        author: Author::Diff,
                        text: line.to_string(),
                    });
                }
            }
            lines
        }
        EventData::ContextCompacted(data) => vec![ChatLine {
            author: Author::System,
            text: compacted_summary_line(data),
        }],
        _ => Vec::new(),
    }
}

/// One-line summary of a completed compaction, e.g.
/// `⊟ compacted context · 142 → 38 msgs · observation_masking+aggressive_trim · 120ms`.
fn compacted_summary_line(data: &everruns_core::events::ContextCompactedData) -> String {
    let saved = data.messages_before.saturating_sub(data.messages_after);
    let mut line = format!(
        "⊟ compacted context · {} → {} msgs",
        data.messages_before, data.messages_after
    );
    if saved > 0 {
        line.push_str(&format!(" (−{saved})"));
    }
    if !data.strategy_used.is_empty() {
        line.push_str(&format!(" · {}", data.strategy_used));
    }
    line.push_str(&format!(" · {}", format_duration_ms(data.duration_ms)));
    line
}

/// Compact millisecond duration: `120ms`, `1.2s`, `1m03s`.
fn format_duration_ms(ms: u64) -> String {
    if ms < 1_000 {
        return format!("{ms}ms");
    }
    let secs = ms / 1_000;
    if secs < 60 {
        return format!("{:.1}s", ms as f64 / 1_000.0);
    }
    format!("{}m{:02}s", secs / 60, secs % 60)
}

fn append_bash_diagnostics(lines: &mut Vec<ChatLine>, result: &Value) {
    if let Some(stderr) = result
        .get("stderr")
        .and_then(Value::as_str)
        .map(str::trim_end)
        .filter(|stderr| !stderr.is_empty())
    {
        lines.push(ChatLine {
            author: Author::Stderr,
            text: format!("stderr:\n{stderr}"),
        });
    }
    if result.get("sandbox_denial").and_then(Value::as_str) == Some("likely") {
        lines.push(ChatLine {
            author: Author::Sandbox,
            text: "native sandbox likely blocked this operation".into(),
        });
    }
}

pub(crate) fn lines_for_replayed_event(event: &RuntimeEvent) -> Vec<ChatLine> {
    match &event.data {
        EventData::InputMessage(data) => message_line(Author::User, &data.message)
            .into_iter()
            .collect(),
        EventData::OutputMessageCompleted(data) => {
            if data.message.role == MessageRole::Agent {
                message_line(Author::Assistant, &data.message)
                    .into_iter()
                    .collect()
            } else {
                Vec::new()
            }
        }
        _ => lines_for_event(event),
    }
}

/// Assistant text lines produced by a turn, read from the messages appended
/// after `skip`. Mirrors the message-loop reconciliation the session does at
/// end of turn.
pub(crate) fn assistant_lines_since(messages: &[Message], skip: usize) -> Vec<ChatLine> {
    let mut out = Vec::new();
    for msg in messages.iter().skip(skip) {
        if msg.role == MessageRole::Agent
            && !msg.has_tool_calls()
            && let Some(text) = msg.text()
        {
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                out.push(ChatLine {
                    author: Author::Assistant,
                    text: trimmed.to_string(),
                });
            }
        }
    }
    out
}

pub(crate) fn message_line(author: Author, message: &Message) -> Option<ChatLine> {
    let image_count = message
        .content
        .iter()
        .filter(|part| matches!(part, ContentPart::Image(_)))
        .count();
    let text = message.text().unwrap_or_default();
    let text = crate::tui::input::image_input::user_display_text(text, image_count);
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    Some(ChatLine {
        author,
        text: text.to_string(),
    })
}

pub fn status_for_event(event: &RuntimeEvent) -> Option<ActivityStatus> {
    match &event.data {
        EventData::ReasonStarted(_) => Some(fallback_status("thinking")),
        EventData::ReasonCompleted(data) => {
            if !data.success {
                let err = data.error.as_deref().unwrap_or("reasoning failed");
                return Some(activity_status(format!(
                    "reasoning failed: {}",
                    first_line(err, 100)
                )));
            }
            data.has_tool_calls
                .then(|| activity_status(format!("planned {} tool call(s)", data.tool_call_count)))
        }
        EventData::ActStarted(data) => data
            .headline
            .clone()
            .or_else(|| Some(format!("running {} tool(s)", data.tool_calls.len())))
            .map(activity_status),
        EventData::ActCompleted(data) => data
            .headline
            .clone()
            .or_else(|| {
                Some(format!(
                    "tools finished: {} ok, {} failed",
                    data.success_count, data.error_count
                ))
            })
            .map(activity_status),
        EventData::ToolStarted(data) => {
            Some(activity_status(format!("→ {}", tool_started_label(data))))
        }
        EventData::ToolProgress(data) => Some(activity_status(format!(
            "… {}: {}",
            data.display_name
                .as_deref()
                .unwrap_or(data.tool_name.as_str()),
            first_line(&data.message, 100)
        ))),
        EventData::ToolCallRequested(data) => Some(activity_status(format!(
            "waiting for {} client tool result(s)",
            data.tool_calls.len()
        ))),
        EventData::OutputMessageStarted(data) => {
            let iteration = data.iteration.unwrap_or(1);
            Some(activity_status(format!(
                "iteration {iteration}: writing response"
            )))
        }
        EventData::ReasonThinkingStarted(_) => Some(fallback_status("thinking deeply")),
        EventData::ContextCompacting(data) => Some(activity_status(format!(
            "compacting context ({})…",
            data.reason
        ))),
        EventData::TurnCancelled(_) => Some(activity_status("turn cancelled")),
        EventData::TurnFailed(data) => Some(activity_status(format!(
            "turn failed: {}",
            first_line(&data.error, 100)
        ))),
        _ => None,
    }
}

pub(crate) fn activity_status(text: impl Into<String>) -> ActivityStatus {
    ActivityStatus {
        text: text.into(),
        fallback: false,
    }
}

pub(crate) fn fallback_status(text: impl Into<String>) -> ActivityStatus {
    ActivityStatus {
        text: text.into(),
        fallback: true,
    }
}

pub(crate) fn result_value(data: &ToolCompletedData) -> Option<Value> {
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

pub(crate) fn extract_field(data: &ToolCompletedData, field: &str) -> Option<String> {
    let v = result_value(data)?;
    v.get(field).and_then(|s| s.as_str()).map(str::to_string)
}

pub(crate) const MAX_RENDERED_TODOS: usize = 20;
pub(crate) const MAX_TODO_TEXT_CHARS: usize = 160;

pub(crate) fn truncate_chars(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let truncated: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{truncated}…")
    } else {
        truncated
    }
}

pub(crate) fn todo_lines_for_result(data: &ToolCompletedData) -> Vec<ChatLine> {
    todo_lines_for_result_or_args(data, &mut HashMap::new())
}

fn todo_lines_for_result_or_args(
    data: &ToolCompletedData,
    write_todos_args: &mut HashMap<String, Value>,
) -> Vec<ChatLine> {
    let cached_args = write_todos_args.remove(&data.tool_call_id);
    let result = result_value(data);
    let has_todos = |value: &&Value| value.get("todos").and_then(Value::as_array).is_some();
    let Some(v) = result
        .as_ref()
        .filter(has_todos)
        .or_else(|| cached_args.as_ref().filter(has_todos))
        .or(result.as_ref())
        .or(cached_args.as_ref())
    else {
        let marker = if data.success { "✓" } else { "✗" };
        return vec![ChatLine {
            author: Author::Tool,
            text: format!(
                "{marker} {}",
                data.display_name.as_deref().unwrap_or("Write Todos")
            ),
        }];
    };
    let Some(todos) = v.get("todos").and_then(Value::as_array) else {
        return vec![ChatLine {
            author: Author::Tool,
            text: summarize_tool_result(data),
        }];
    };

    let total = todos.len();
    let completed = todos
        .iter()
        .filter(|todo| todo.get("status").and_then(Value::as_str) == Some("completed"))
        .count();
    let summary = format!("{completed} of {total} todos completed");
    let mut rendered_todos = Vec::new();
    for todo in todos.iter().take(MAX_RENDERED_TODOS) {
        let status = todo
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("pending");
        let content = todo.get("content").and_then(Value::as_str).unwrap_or("");
        let active_form = todo
            .get("activeForm")
            .and_then(Value::as_str)
            .unwrap_or(content);
        let (icon, text) = match status {
            "completed" => ("✓", content),
            "in_progress" => ("›", active_form),
            _ => ("○", content),
        };
        rendered_todos.push(format!(
            "{icon} {}",
            truncate_chars(text, MAX_TODO_TEXT_CHARS)
        ));
    }

    let mut lines = if rendered_todos.len() <= 3 {
        let inline_todos = rendered_todos.join("  ");
        vec![ChatLine {
            author: Author::Tool,
            text: if inline_todos.is_empty() {
                summary
            } else {
                format!("{summary}  {inline_todos}")
            },
        }]
    } else {
        let mut lines = vec![ChatLine {
            author: Author::Tool,
            text: summary,
        }];
        lines.extend(rendered_todos.into_iter().map(|text| ChatLine {
            author: Author::ToolDetail,
            text,
        }));
        lines
    };

    let omitted = total.saturating_sub(MAX_RENDERED_TODOS);
    if omitted > 0 {
        lines.push(ChatLine {
            author: Author::ToolDetail,
            text: format!("… {omitted} more todo(s) omitted"),
        });
    }

    if let Some(warning) = v.get("warning").and_then(Value::as_str) {
        lines.push(ChatLine {
            author: Author::ToolDetail,
            text: format!("warning: {}", truncate_chars(warning, MAX_TODO_TEXT_CHARS)),
        });
    }

    lines
}

fn tool_started_label(data: &ToolStartedData) -> String {
    data.narration
        .clone()
        .or_else(|| data.display_name.clone())
        .unwrap_or_else(|| data.tool_call.name.clone())
}

/// One-line summary of a tool result, used in the transcript and `--print` output.
pub fn summarize_tool_result(data: &ToolCompletedData) -> String {
    if !data.success
        && let Some(err) = &data.error
    {
        return format!("error: {}", first_line(err, 120));
    }
    let Some(v) = result_value(data) else {
        if let Some(err) = &data.error {
            return format!("error: {}", first_line(err, 120));
        }
        return String::new();
    };
    // Field names match the built-in `session_file_system` capability's
    // result shapes. See crates/core/src/capabilities/file_system.rs.
    match data.tool_name.as_str() {
        "write_todos" => {
            let completed = v.get("completed").and_then(Value::as_u64).unwrap_or(0);
            let total = v.get("total_tasks").and_then(Value::as_u64).unwrap_or(0);
            format!("{completed}/{total} completed")
        }
        "read_file" => {
            let path = v.get("path").and_then(Value::as_str).unwrap_or("");
            let total = v.get("total_lines").and_then(Value::as_u64).unwrap_or(0);
            let shown = v.get("lines_shown");
            let start = shown
                .and_then(|s| s.get("start"))
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let end = shown
                .and_then(|s| s.get("end"))
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let count = end.saturating_sub(start.saturating_sub(1));
            format!("{path} ({count}/{total} lines)")
        }
        "write_file" => {
            let path = v.get("path").and_then(Value::as_str).unwrap_or("");
            let bytes = v.get("size_bytes").and_then(Value::as_u64).unwrap_or(0);
            format!("{path} ({bytes} bytes)")
        }
        "edit_file" => {
            let path = v.get("path").and_then(Value::as_str).unwrap_or("");
            let n = v.get("applied_edits").and_then(Value::as_u64).unwrap_or(0);
            format!("{path} ({n} edit(s))")
        }
        "ast_edit" => {
            let n = v.get("count").and_then(Value::as_u64).unwrap_or(0);
            let files = v.get("files_changed").and_then(Value::as_u64).unwrap_or(0);
            let preview = v.get("dry_run").and_then(Value::as_bool).unwrap_or(true);
            let mode = if preview { "preview" } else { "applied" };
            format!("{n} replacement(s) in {files} file(s) ({mode})")
        }
        "list_directory" => {
            let path = v.get("path").and_then(Value::as_str).unwrap_or("");
            let n = v.get("count").and_then(Value::as_u64).unwrap_or(0);
            format!("{path} ({n} entries)")
        }
        "grep_files" => {
            let pattern = v.get("pattern").and_then(Value::as_str).unwrap_or("");
            let n = v.get("match_count").and_then(Value::as_u64).unwrap_or(0);
            format!("/{pattern}/ ({n} match(es))")
        }
        "delete_file" => {
            let path = v.get("path").and_then(Value::as_str).unwrap_or("");
            format!("{path} (deleted)")
        }
        "stat_file" => {
            let path = v.get("path").and_then(Value::as_str).unwrap_or("");
            let size = v.get("size_bytes").and_then(Value::as_u64).unwrap_or(0);
            format!("{path} ({size} bytes)")
        }
        "bash" => {
            let cmd = v
                .get("command")
                .and_then(Value::as_str)
                .map(|c| first_line(c, 80))
                .unwrap_or_default();
            let code = v
                .get("exit_code")
                .and_then(Value::as_i64)
                .map(|c| c.to_string())
                .unwrap_or_else(|| "?".into());
            format!("`{cmd}` exit={code}")
        }
        _ => String::new(),
    }
}

/// First line of `s`, truncated to `max` characters with an ellipsis. Truncates
/// on `char` boundaries (via [`truncate_chars`]) so non-ASCII error/status text
/// never panics on a mid-codepoint byte index.
pub(crate) fn first_line(s: &str, max: usize) -> String {
    truncate_chars(s.lines().next().unwrap_or(""), max)
}

// ---------- `!shell` result translation ----------

pub(crate) fn shell_result_lines(result: ToolExecutionResult) -> Vec<ChatLine> {
    match result {
        ToolExecutionResult::Success(value) => shell_success_lines(&value),
        ToolExecutionResult::SuccessWithImages { result, .. } => shell_success_lines(&result),
        ToolExecutionResult::ToolError(message) => vec![ChatLine {
            author: Author::System,
            text: format!("shell failed: {message}"),
        }],
        ToolExecutionResult::InternalError(_) => vec![ChatLine {
            author: Author::System,
            text: "shell failed: internal error".into(),
        }],
        ToolExecutionResult::ConnectionRequired { provider } => vec![ChatLine {
            author: Author::System,
            text: format!("shell failed: connection required for {provider}"),
        }],
    }
}

fn shell_success_lines(value: &Value) -> Vec<ChatLine> {
    let exit_code = value.get("exit_code").and_then(Value::as_i64).unwrap_or(-1);
    let success = value
        .get("success")
        .and_then(Value::as_bool)
        .unwrap_or(exit_code == 0);
    let mut out = vec![ChatLine {
        author: Author::Tool,
        text: format!("shell exited with code {exit_code}"),
    }];

    for (label, author) in [("stdout", Author::ToolDetail), ("stderr", Author::Stderr)] {
        if let Some(text) = value.get(label).and_then(Value::as_str) {
            let text = text.trim_end();
            if !text.is_empty() {
                out.push(ChatLine {
                    author,
                    text: format!("{label}:\n{text}"),
                });
            }
        }
    }
    if value.get("sandbox_denial").and_then(Value::as_str) == Some("likely") {
        out.push(ChatLine {
            author: Author::Sandbox,
            text: "native sandbox likely blocked this operation".into(),
        });
    }
    if value
        .get("truncated")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || value
            .get("output_limited")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    {
        out.push(ChatLine {
            author: Author::System,
            text: "shell output was truncated".into(),
        });
    }
    if out.len() == 1 {
        out.push(ChatLine {
            author: Author::ToolDetail,
            text: "(no output)".into(),
        });
    }
    if !success {
        out.push(ChatLine {
            author: Author::System,
            text: "shell command exited non-zero".into(),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use everruns_core::{events::ToolStartedData, tool_types::ToolCall};
    use serde_json::json;

    fn started_tool(name: &str, arguments: serde_json::Value) -> ToolStartedData {
        ToolStartedData {
            tool_call: ToolCall {
                id: "call-1".to_owned(),
                name: name.to_owned(),
                arguments,
            },
            display_name: Some("Hardcoded label".to_owned()),
            narration: None,
            tool_call_fingerprint: None,
        }
    }

    #[test]
    fn shell_denial_distinguishes_stderr_and_explains_native_sandbox() {
        let lines = shell_success_lines(&json!({
            "exit_code": 1,
            "success": false,
            "stdout": "",
            "stderr": "touch: /outside: Operation not permitted\n",
            "sandbox": "native",
            "sandbox_denial": "likely"
        }));

        assert!(lines.iter().any(|line| {
            line.author == Author::Stderr && line.text.contains("Operation not permitted")
        }));
        assert!(lines.iter().any(|line| {
            line.author == Author::Sandbox
                && line.text == "native sandbox likely blocked this operation"
        }));
    }

    #[test]
    fn model_bash_denial_renders_stderr_and_sandbox_explanation() {
        let data = ToolCompletedData::success(
            "call-1".into(),
            "bash".into(),
            vec![ContentPart::text(
                json!({
                    "command": "touch /outside",
                    "exit_code": 1,
                    "success": false,
                    "stderr": "touch: Operation not permitted",
                    "sandbox": "native",
                    "sandbox_denial": "likely"
                })
                .to_string(),
            )],
            None,
        );
        let event = RuntimeEvent::new(
            everruns_core::typed_id::SessionId::new(),
            everruns_core::events::EventContext::empty(),
            data,
        );

        let lines = lines_for_event(&event);

        assert!(lines.iter().any(|line| line.author == Author::Stderr));
        assert!(lines.iter().any(|line| line.author == Author::Sandbox));
    }

    #[test]
    fn tool_started_label_prefers_runtime_narration() {
        let mut data = started_tool("tool_search", json!({ "query": "hooks" }));
        data.narration = Some("Runtime narration".to_owned());

        assert_eq!(tool_started_label(&data), "Runtime narration");
    }

    #[test]
    fn tool_started_label_falls_back_to_display_name() {
        let data = started_tool("tool_search", json!({ "query": "hooks" }));

        assert_eq!(tool_started_label(&data), "Hardcoded label");
    }

    #[test]
    fn tool_started_label_falls_back_to_tool_name() {
        let mut data = started_tool("tool_search", json!({ "query": "hooks" }));
        data.display_name = None;

        assert_eq!(tool_started_label(&data), "tool_search");
    }

    #[test]
    fn failed_unknown_tool_with_result_payload_still_summarizes_error() {
        let data = ToolCompletedData {
            tool_call_id: "call-1".into(),
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
        };

        assert_eq!(
            summarize_tool_result(&data),
            "error: Invalid URL: must start with http:// or https://"
        );
    }

    fn event(data: impl Into<EventData>) -> RuntimeEvent {
        RuntimeEvent::new(
            everruns_core::typed_id::SessionId::new(),
            everruns_core::events::EventContext::empty(),
            data.into(),
        )
    }

    #[test]
    fn context_compacted_renders_a_system_summary_line() {
        use everruns_core::events::{CompactionStepData, ContextCompactedData};

        let event = event(ContextCompactedData {
            strategy_used: "observation_masking+aggressive_trim".into(),
            messages_before: 142,
            messages_after: 38,
            duration_ms: 120,
            steps: vec![CompactionStepData {
                strategy: "observation_masking".into(),
                messages_after: 96,
                duration_ms: 4,
            }],
        });

        let lines = lines_for_event(&event);
        assert_eq!(lines.len(), 1, "expected one summary line: {lines:?}");
        let line = &lines[0];
        assert_eq!(line.author, Author::System);
        assert!(line.text.contains("142 → 38 msgs"), "{}", line.text);
        assert!(line.text.contains("(−104)"), "{}", line.text);
        assert!(
            line.text.contains("observation_masking+aggressive_trim"),
            "{}",
            line.text
        );
        assert!(line.text.contains("120ms"), "{}", line.text);
    }

    #[test]
    fn context_compacted_survives_replay() {
        use everruns_core::events::ContextCompactedData;

        let event = event(ContextCompactedData {
            strategy_used: "native".into(),
            messages_before: 50,
            messages_after: 12,
            duration_ms: 2_400,
            steps: Vec::new(),
        });

        let lines = lines_for_replayed_event(&event);
        assert_eq!(lines.len(), 1, "replay should render the notice: {lines:?}");
        assert_eq!(lines[0].author, Author::System);
        // sub-minute duration falls back to seconds formatting.
        assert!(lines[0].text.contains("2.4s"), "{}", lines[0].text);
    }

    #[test]
    fn context_compacting_reports_reason_as_activity() {
        use everruns_core::events::{CompactionReason, ContextCompactingData};

        let event = event(ContextCompactingData {
            reason: CompactionReason::ProactiveBudget,
            strategy: "auto".into(),
            messages_before: 142,
        });

        let status = status_for_event(&event).expect("compacting should set activity");
        assert!(
            status.text.contains("compacting context"),
            "{}",
            status.text
        );
        assert!(status.text.contains("proactive_budget"), "{}", status.text);
    }

    #[test]
    fn duration_formatting_scales_by_magnitude() {
        assert_eq!(format_duration_ms(120), "120ms");
        assert_eq!(format_duration_ms(2_400), "2.4s");
        assert_eq!(format_duration_ms(63_000), "1m03s");
    }
}

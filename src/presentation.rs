//! Terminal-independent presentation model for user-visible yolop output.
//!
//! Runtime events are translated into transcript view-model entries in
//! `crate::transcript`; this module turns those entries plus live app state into
//! the semantic output hosts display. It intentionally has no ratatui/crossterm
//! dependency so transcript wording and status values can be tested without a
//! terminal buffer.

use crate::session_tasks_view::BackgroundCounts;
use crate::transcript::{Author, ChatLine, StreamKind, StreamPreview};
use crate::version::VERSION_DETAILS;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PresentedTranscriptLine {
    pub author: Author,
    pub label: Option<&'static str>,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PresentationState {
    pub stream_preview: Option<StreamPreview>,
    pub busy: bool,
    pub turn_activity: Option<String>,
    pub model_id: String,
    pub provider_name: String,
    pub reasoning_effort: Option<String>,
    pub session_id: String,
    pub lines_count: usize,
    pub session_tokens: Option<u64>,
    pub status_layout: StatusLayout,
    pub hooks_summary: String,
    pub approval_mode: String,
    pub background: Option<BackgroundCounts>,
    pub goal_indicator: Option<String>,
    pub ask_indicator: Option<String>,
    pub worktree_compact: Option<String>,
    pub worktree_expanded: Option<(String, String)>,
    /// Live status pushed by extensions over `status/changed`, as
    /// `(extension_name, status_text)` pairs. Rendered as its own status field.
    pub extension_status: Vec<(String, String)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StatusLayout {
    Compact,
    Expanded,
}

impl StatusLayout {
    pub(crate) fn base_row_count(self) -> u16 {
        match self {
            Self::Compact => 1,
            Self::Expanded => 4,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StatusLine {
    pub fields: Vec<StatusField>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StatusField {
    pub label: Option<&'static str>,
    pub value: String,
}

impl Author {
    pub(crate) fn label(&self) -> &'static str {
        match self {
            Author::User => "you",
            Author::Assistant => "agent",
            Author::Narration => "note",
            Author::Tool => "tool",
            Author::ToolDetail => "",
            Author::Diff => "diff",
            Author::System => "system",
        }
    }
}

impl StreamKind {
    pub(crate) fn label(self) -> &'static str {
        match self {
            StreamKind::Assistant => "agent",
            StreamKind::Tool => "tool",
        }
    }
}

impl PresentationState {
    pub(crate) fn status_row_count(&self) -> u16 {
        self.status_layout.base_row_count() + self.worktree_extra_status_rows()
    }

    fn worktree_extra_status_rows(&self) -> u16 {
        u16::from(self.status_layout == StatusLayout::Expanded && self.worktree_expanded.is_some())
    }

    pub(crate) fn status_lines(&self) -> Vec<StatusLine> {
        match self.status_layout {
            StatusLayout::Compact => compact_status_lines(self),
            StatusLayout::Expanded => expanded_status_lines(self),
        }
    }

    pub(crate) fn activity_text(&self) -> Option<&str> {
        self.busy
            .then(|| self.turn_activity.as_deref().unwrap_or("thinking"))
    }
}

pub(crate) fn present_transcript_line(chat: &ChatLine) -> PresentedTranscriptLine {
    let label = match chat.author {
        Author::ToolDetail => None,
        _ => Some(chat.author.label()),
    };
    PresentedTranscriptLine {
        author: chat.author.clone(),
        label,
        text: chat.text.clone(),
    }
}

#[cfg(test)]
pub(crate) fn plain_transcript_line(chat: &ChatLine) -> String {
    let line = present_transcript_line(chat);
    match line.label {
        Some(label) => format!("{label} › {}", line.text),
        None => format!("           {}", line.text),
    }
}

fn compact_status_lines(state: &PresentationState) -> Vec<StatusLine> {
    vec![StatusLine {
        fields: status_contributions(state)
            .into_iter()
            .flatten()
            .collect::<Vec<_>>(),
    }]
}

fn expanded_status_lines(state: &PresentationState) -> Vec<StatusLine> {
    let mut counts = vec![
        status_value(message_count_label(state.lines_count)),
        status_field("tokens", token_label(state.session_tokens)),
    ];
    if let Some(bg) = background_label(state.background, false) {
        counts.push(status_field("bg", bg));
    }
    // Extension-pushed status shares the counts row so the expanded layout's
    // fixed row budget is unchanged.
    counts.extend(extension_status_fields(state));

    let mut lines = vec![
        StatusLine {
            fields: vec![
                status_value("[collapse ↑]"),
                status_field("provider", state.provider_name.clone()),
                status_field("model", state.model_id.clone()),
            ],
        },
        StatusLine {
            fields: vec![
                status_field("effort", effort_label(state)),
                status_field("approval", state.approval_mode.clone()),
                status_field("hooks", state.hooks_summary.clone()),
                status_field("goal", goal_label(state)),
                status_field("ask", ask_label(state)),
            ],
        },
        StatusLine { fields: counts },
        StatusLine {
            fields: vec![
                status_field("session", state.session_id.clone()),
                status_field("version", VERSION_DETAILS),
            ],
        },
    ];
    if let Some((branch, path)) = &state.worktree_expanded {
        lines.push(StatusLine {
            fields: vec![
                status_field("worktree", branch.clone()),
                status_field("path", path.clone()),
            ],
        });
    }
    lines
}

fn status_contributions(state: &PresentationState) -> Vec<Vec<StatusField>> {
    let toggle_label = match state.status_layout {
        StatusLayout::Compact => "[expand ↓]",
        StatusLayout::Expanded => "[collapse ↑]",
    };
    let mut counts = vec![status_value(message_count_label(state.lines_count))];
    if let Some(bg) = background_label(state.background, true) {
        counts.push(status_field("bg", bg));
    }
    if let Some(wt) = &state.worktree_compact {
        counts.push(status_field("wt", wt.clone()));
    }
    let mut groups = vec![
        vec![
            status_value(toggle_label),
            status_value(state.provider_name.clone()),
            status_value(state.model_id.clone()),
        ],
        vec![
            status_field("effort", effort_label(state)),
            status_field("approval", state.approval_mode.clone()),
        ],
        vec![
            status_field("goal", goal_label(state)),
            status_field("ask", ask_label(state)),
        ],
        counts,
    ];
    let ext = extension_status_fields(state);
    if !ext.is_empty() {
        groups.push(ext);
    }
    groups
}

/// One status field per extension that pushed a `status/changed`, labelled with
/// the extension name (e.g. `git-guard › 1423 chars`).
fn extension_status_fields(state: &PresentationState) -> Vec<StatusField> {
    state
        .extension_status
        .iter()
        .map(|(name, status)| status_value(format!("{name}: {status}")))
        .collect()
}

fn goal_label(state: &PresentationState) -> String {
    state.goal_indicator.clone().unwrap_or_else(|| "—".into())
}

fn ask_label(state: &PresentationState) -> String {
    state.ask_indicator.clone().unwrap_or_else(|| "—".into())
}

fn background_label(counts: Option<BackgroundCounts>, compact: bool) -> Option<String> {
    let counts = counts?;
    if counts.total == 0 {
        return None;
    }
    if compact {
        Some(format!(
            "{} run/{} sched/{}",
            counts.running, counts.scheduled, counts.total
        ))
    } else {
        Some(format!(
            "{} running · {} scheduled · {} total",
            counts.running, counts.scheduled, counts.total
        ))
    }
}

fn status_value(value: impl Into<String>) -> StatusField {
    StatusField {
        label: None,
        value: value.into(),
    }
}

fn status_field(label: &'static str, value: impl Into<String>) -> StatusField {
    StatusField {
        label: Some(label),
        value: value.into(),
    }
}

fn effort_label(state: &PresentationState) -> String {
    state
        .reasoning_effort
        .clone()
        .unwrap_or_else(|| "n/a".to_string())
}

fn token_label(tokens: Option<u64>) -> String {
    tokens
        .map(|tokens| tokens.to_string())
        .unwrap_or_else(|| "n/a".to_string())
}

fn message_count_label(count: usize) -> String {
    format!("{count} msgs")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcript::lines_for_event;
    use everruns_core::events::{Event as RuntimeEvent, EventContext, ToolCompletedData};
    use everruns_core::message::ContentPart;
    use everruns_core::typed_id::SessionId;
    use serde_json::json;

    fn state() -> PresentationState {
        PresentationState {
            stream_preview: None,
            busy: false,
            turn_activity: None,
            model_id: "gpt-5.5".to_string(),
            provider_name: "openai".to_string(),
            reasoning_effort: Some("medium".to_string()),
            session_id: "sess_123".to_string(),
            lines_count: 3,
            session_tokens: Some(42),
            status_layout: StatusLayout::Compact,
            hooks_summary: "none".to_string(),
            approval_mode: "normal".to_string(),
            background: None,
            goal_indicator: None,
            ask_indicator: None,
            worktree_compact: None,
            worktree_expanded: None,
            extension_status: Vec::new(),
        }
    }

    #[test]
    fn extension_status_renders_in_both_layouts() {
        let flatten = |lines: Vec<StatusLine>| -> String {
            lines
                .iter()
                .flat_map(|l| l.fields.iter())
                .map(|f| f.value.clone())
                .collect::<Vec<_>>()
                .join(" · ")
        };
        let mut s = state();
        s.extension_status = vec![("git-guard".to_string(), "1423 chars".to_string())];

        s.status_layout = StatusLayout::Compact;
        assert!(
            flatten(s.status_lines()).contains("git-guard: 1423 chars"),
            "compact layout must show the extension status"
        );

        s.status_layout = StatusLayout::Expanded;
        assert!(
            flatten(s.status_lines()).contains("git-guard: 1423 chars"),
            "expanded layout must show the extension status"
        );

        // No extensions → no stray field.
        s.extension_status.clear();
        assert!(!flatten(s.status_lines()).contains("git-guard"));
    }

    #[test]
    fn plain_transcript_line_exposes_user_visible_tool_output() {
        let line = ChatLine {
            author: Author::Tool,
            text: "✓ Bash `git status --short` exit=0".to_string(),
        };

        assert_eq!(
            plain_transcript_line(&line),
            "tool › ✓ Bash `git status --short` exit=0"
        );
    }

    #[test]
    fn plain_transcript_line_keeps_fallback_wording_visible_to_tests() {
        let mut data = ToolCompletedData::success(
            "call_bash".to_string(),
            "bash".to_string(),
            vec![ContentPart::text(
                json!({
                    "command": "git status --short",
                    "exit_code": 0
                })
                .to_string(),
            )],
            None,
        );
        data.narration = Some("Ran Bash".to_string());
        let event = RuntimeEvent::new(SessionId::new(), EventContext::empty(), data);
        let lines = lines_for_event(&event);

        let rendered = plain_transcript_line(&lines[0]);

        assert_eq!(rendered, "tool › ✓ Ran Bash  `git status --short` exit=0");
    }

    #[test]
    fn session_task_narration_is_visible_in_live_activity_and_transcript() {
        use crate::capabilities::narration::{narrate_spawn_background, narrate_wait_task};
        use crate::transcript::status_for_event;
        use everruns_core::events::ToolStartedData;
        use everruns_core::tool_narration::ToolNarrationPhase;
        use everruns_core::tool_types::ToolCall;

        let wait_call = ToolCall {
            id: "call_wait".to_string(),
            name: "wait_task".to_string(),
            arguments: json!({ "task_id": "task_ci_watch" }),
        };
        let wait_narration = narrate_wait_task(&wait_call, ToolNarrationPhase::Started);
        let started = RuntimeEvent::new(
            SessionId::new(),
            EventContext::empty(),
            ToolStartedData {
                tool_call: wait_call,
                display_name: Some("Wait Task".to_string()),
                narration: Some(wait_narration.clone()),
                tool_call_fingerprint: None,
            },
        );
        assert_eq!(
            status_for_event(&started).map(|status| status.text),
            Some("→ Wait for task: task_ci_watch".to_string()),
            "live activity must prefer human narration over display_name"
        );

        let spawn_call = ToolCall {
            id: "call_spawn".to_string(),
            name: "spawn_background".to_string(),
            arguments: json!({
                "tool": "bash",
                "title": "Wait for CI",
                "args": { "command": "gh pr checks --watch" }
            }),
        };
        let mut completed = ToolCompletedData::success(
            "call_spawn".to_string(),
            "spawn_background".to_string(),
            vec![ContentPart::text(
                json!({ "task_id": "task_ci_watch", "state": "running" }).to_string(),
            )],
            None,
        );
        completed.display_name = Some("Spawn Background".to_string());
        completed.narration = Some(narrate_spawn_background(
            &spawn_call,
            ToolNarrationPhase::Completed,
        ));
        let event = RuntimeEvent::new(SessionId::new(), EventContext::empty(), completed);
        let rendered = plain_transcript_line(&lines_for_event(&event)[0]);
        assert_eq!(
            rendered, "tool › ✓ Spawn background: Wait for CI",
            "transcript must not fall back to 'Ran Spawn Background'"
        );
        assert_ne!(wait_narration, "Wait Task");
    }

    #[test]
    fn status_model_exposes_compact_status_values_without_terminal() {
        let lines = state().status_lines();

        assert_eq!(lines.len(), 1);
        let values = lines[0]
            .fields
            .iter()
            .map(|field| (field.label, field.value.as_str()))
            .collect::<Vec<_>>();
        assert!(values.contains(&(None, "[expand ↓]")));
        assert!(values.contains(&(None, "openai")));
        assert!(values.contains(&(None, "gpt-5.5")));
        assert!(values.contains(&(Some("effort"), "medium")));
        assert!(values.contains(&(Some("approval"), "normal")));
        assert!(values.contains(&(Some("goal"), "—")));
        assert!(values.contains(&(None, "3 msgs")));
    }

    #[test]
    fn status_model_exposes_expanded_background_and_session_values() {
        let model = PresentationState {
            status_layout: StatusLayout::Expanded,
            background: Some(BackgroundCounts {
                running: 2,
                scheduled: 1,
                total: 5,
            }),
            ..state()
        };

        let lines = model.status_lines();

        assert_eq!(lines.len(), 4);
        assert_eq!(lines[0].fields[1].label, Some("provider"));
        assert_eq!(lines[0].fields[1].value, "openai");
        assert_eq!(lines[2].fields[2].label, Some("bg"));
        assert_eq!(
            lines[2].fields[2].value,
            "2 running · 1 scheduled · 5 total"
        );
        assert_eq!(lines[3].fields[0].label, Some("session"));
        assert_eq!(lines[3].fields[0].value, "sess_123");
        assert_eq!(lines[3].fields[1].label, Some("version"));
        assert_eq!(lines[3].fields[1].value, VERSION_DETAILS);
    }
}

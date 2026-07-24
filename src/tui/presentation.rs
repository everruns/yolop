//! Terminal-independent presentation model for user-visible yolop output.
//!
//! Runtime events are translated into transcript view-model entries in
//! `crate::tui::transcript`; this module turns those entries plus live app state into
//! the semantic output hosts display. It intentionally has no ratatui/crossterm
//! dependency so transcript wording and status values can be tested without a
//! terminal buffer.

use crate::tui::session_tasks_view::BackgroundCounts;
use crate::tui::transcript::{Author, ChatLine, StreamKind, StreamPreview};
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
    /// Messages accepted by the composer while the active turn is running.
    pub queued_messages: usize,
    pub turn_activity: Option<String>,
    pub model_id: String,
    pub provider_name: String,
    pub reasoning_effort: Option<String>,
    pub session_id: String,
    pub lines_count: usize,
    pub session_tokens: Option<u64>,
    /// Wall-clock seconds since the active turn began, or `None` when idle.
    /// Drives the live elapsed timer on the busy indicator.
    pub turn_elapsed_secs: Option<u64>,
    /// Prompt tokens the most recent generation consumed (context-window fill).
    pub context_used_tokens: Option<u32>,
    /// The active model's context-window size, when known.
    pub context_window_tokens: Option<u32>,
    /// Whole percent (0–100) of the context window at which proactive compaction
    /// is expected to trigger, when compaction is active. Drives the threshold
    /// mark on the context gauge. `None` hides the mark.
    pub compaction_budget_percent: Option<u8>,
    pub status_layout: StatusLayout,
    pub hooks_summary: String,
    pub approval_mode: String,
    pub background: Option<BackgroundCounts>,
    pub goal_indicator: Option<String>,
    pub ask_indicator: Option<String>,
    pub worktree_compact: Option<String>,
    pub worktree_expanded: Option<(String, String)>,
    /// Turn-scoped status text contributed by the agent through the TUI host.
    pub agent_status: Option<String>,
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
    pub action: Option<StatusAction>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StatusAction {
    ToggleLayout,
    OpenModel,
    OpenEffort,
    OpenBackground,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StatusSection {
    pub title: &'static str,
    pub fields: Vec<StatusField>,
}

impl Author {
    pub(crate) fn label(&self) -> &'static str {
        match self {
            Author::User => "you",
            Author::Assistant => "agent",
            Author::Narration => "note",
            Author::Tool => "tool",
            Author::ToolDetail => "",
            Author::Stderr => "",
            Author::Sandbox => "sandbox",
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

    pub(crate) fn expanded_status_sections(&self) -> Vec<StatusSection> {
        let mut session = vec![
            status_value(message_count_label(self.lines_count)),
            status_field("tokens", token_label(self.session_tokens)),
        ];
        if let Some(ctx) = context_label(
            self.context_used_tokens,
            self.context_window_tokens,
            self.compaction_budget_percent,
        ) {
            session.push(status_field("ctx", ctx));
        }
        if let Some(bg) = background_label(self.background, false) {
            session.push(status_field_action(
                "background",
                bg,
                StatusAction::OpenBackground,
            ));
        }
        session.push(status_field("goal", goal_label(self)));
        session.push(status_field("ask", ask_label(self)));
        if let Some(status) = self
            .agent_status
            .as_deref()
            .or_else(|| self.activity_text())
            .filter(|status| !status.is_empty())
        {
            session.push(status_field("agent", status));
        }

        let mut workspace = vec![
            status_field("hooks", self.hooks_summary.clone()),
            status_field("session", self.session_id.clone()),
            status_field("version", VERSION_DETAILS),
        ];
        if let Some((branch, path)) = &self.worktree_expanded {
            workspace.insert(0, status_field("path", path.clone()));
            workspace.insert(0, status_field("worktree", branch.clone()));
        }
        workspace.extend(extension_status_fields(self));

        vec![
            StatusSection {
                title: "Runtime",
                fields: vec![
                    status_value_action("[collapse ↑]", StatusAction::ToggleLayout),
                    status_field("provider", self.provider_name.clone()),
                    status_field_action("model", self.model_id.clone(), StatusAction::OpenModel),
                    status_field_action("effort", effort_label(self), StatusAction::OpenEffort),
                    status_field("approval", self.approval_mode.clone()),
                ],
            },
            StatusSection {
                title: "Session",
                fields: session,
            },
            StatusSection {
                title: "Workspace",
                fields: workspace,
            },
        ]
    }

    pub(crate) fn activity_text(&self) -> Option<&str> {
        self.busy
            .then(|| self.turn_activity.as_deref().unwrap_or("thinking"))
    }
}

pub(crate) fn present_transcript_line(chat: &ChatLine) -> PresentedTranscriptLine {
    let label = match chat.author {
        Author::ToolDetail | Author::Stderr => None,
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
    if let Some(ctx) = context_label(
        state.context_used_tokens,
        state.context_window_tokens,
        state.compaction_budget_percent,
    ) {
        counts.push(status_field("ctx", ctx));
    }
    if let Some(bg) = background_label(state.background, false) {
        counts.push(status_field_action("bg", bg, StatusAction::OpenBackground));
    }
    // Extension-pushed status shares the counts row so the expanded layout's
    // fixed row budget is unchanged.
    counts.extend(extension_status_fields(state));

    let mut lines = vec![
        StatusLine {
            fields: vec![
                status_value_action("[collapse ↑]", StatusAction::ToggleLayout),
                status_field("provider", state.provider_name.clone()),
                status_field_action("model", state.model_id.clone(), StatusAction::OpenModel),
            ],
        },
        StatusLine {
            fields: vec![
                status_field_action("effort", effort_label(state), StatusAction::OpenEffort),
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
    if let Some(ctx) = context_label(
        state.context_used_tokens,
        state.context_window_tokens,
        state.compaction_budget_percent,
    ) {
        counts.push(status_field("ctx", ctx));
    }
    if let Some(bg) = background_label(state.background, true) {
        counts.push(status_field_action("bg", bg, StatusAction::OpenBackground));
    }
    if let Some(wt) = &state.worktree_compact {
        counts.push(status_field("wt", wt.clone()));
    }
    let mut groups = vec![
        vec![
            status_value_action(toggle_label, StatusAction::ToggleLayout),
            status_value(state.provider_name.clone()),
            status_value_action(state.model_id.clone(), StatusAction::OpenModel),
        ],
        vec![
            status_field_action("effort", effort_label(state), StatusAction::OpenEffort),
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
        action: None,
    }
}

fn status_field(label: &'static str, value: impl Into<String>) -> StatusField {
    StatusField {
        label: Some(label),
        value: value.into(),
        action: None,
    }
}

fn status_value_action(value: impl Into<String>, action: StatusAction) -> StatusField {
    StatusField {
        label: None,
        value: value.into(),
        action: Some(action),
    }
}

fn status_field_action(
    label: &'static str,
    value: impl Into<String>,
    action: StatusAction,
) -> StatusField {
    StatusField {
        label: Some(label),
        value: value.into(),
        action: Some(action),
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

/// Number of cells in the inline context-gauge bar.
const CONTEXT_GAUGE_CELLS: u32 = 8;

/// Context-window fill as a small gauge, e.g. `██╿█░░░░ 45% (90k/200k)`. `None`
/// until we have both a usage sample and a known window size.
///
/// When `compaction_budget_percent` is set, a `╿` tick marks the fraction of the
/// window at which proactive compaction is expected to trigger. The mark is a
/// guide, not a guarantee: the gauge fill is real prompt-token usage, while
/// compaction actually triggers on a char/4 token *estimate* of the message
/// history (everruns-core `should_compact_proactively`), so the true trigger
/// point can drift from the mark.
pub(crate) fn context_label(
    used: Option<u32>,
    window: Option<u32>,
    compaction_budget_percent: Option<u8>,
) -> Option<String> {
    let (used, window) = (used?, window?);
    if window == 0 {
        return None;
    }
    let pct = ((u64::from(used) * 100) / u64::from(window)).min(100) as u32;
    let bar = context_gauge_bar(pct, compaction_budget_percent);
    Some(format!(
        "{bar} {pct}% ({}/{})",
        compact_token_count(used),
        compact_token_count(window)
    ))
}

/// Render the `CONTEXT_GAUGE_CELLS`-wide gauge: filled cells for `used_pct`, a
/// `╿` tick at the compaction threshold when known (it overrides the cell it
/// lands on). Both `used_pct` and `threshold` are whole percents (0–100).
fn context_gauge_bar(used_pct: u32, threshold: Option<u8>) -> String {
    let filled = (used_pct * CONTEXT_GAUGE_CELLS)
        .div_ceil(100)
        .min(CONTEXT_GAUGE_CELLS);
    let tick = threshold
        .filter(|&t| t <= 100)
        .map(|t| (u32::from(t) * CONTEXT_GAUGE_CELLS / 100).min(CONTEXT_GAUGE_CELLS - 1));
    (0..CONTEXT_GAUGE_CELLS)
        .map(|i| {
            if Some(i) == tick {
                '╿'
            } else if i < filled {
                '█'
            } else {
                '░'
            }
        })
        .collect()
}

/// Render a token count compactly: `900`, `90k`, `1.2M`.
fn compact_token_count(tokens: u32) -> String {
    match tokens {
        n if n < 1_000 => n.to_string(),
        n if n < 1_000_000 => format!("{}k", n / 1_000),
        n => format!("{:.1}M", n as f64 / 1_000_000.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::transcript::lines_for_event;
    use everruns_core::events::{Event as RuntimeEvent, EventContext, ToolCompletedData};
    use everruns_core::message::ContentPart;
    use everruns_core::typed_id::SessionId;
    use serde_json::json;

    fn state() -> PresentationState {
        PresentationState {
            stream_preview: None,
            busy: false,
            queued_messages: 0,
            turn_activity: None,
            model_id: "gpt-5.5".to_string(),
            provider_name: "openai".to_string(),
            reasoning_effort: Some("medium".to_string()),
            session_id: "sess_123".to_string(),
            lines_count: 3,
            session_tokens: Some(42),
            turn_elapsed_secs: None,
            context_used_tokens: None,
            context_window_tokens: None,
            compaction_budget_percent: None,
            status_layout: StatusLayout::Compact,
            hooks_summary: "none".to_string(),
            approval_mode: "normal".to_string(),
            background: None,
            goal_indicator: None,
            ask_indicator: None,
            worktree_compact: None,
            worktree_expanded: None,
            agent_status: None,
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
    fn expanded_sections_group_runtime_session_and_workspace_status() {
        let model = PresentationState {
            status_layout: StatusLayout::Expanded,
            agent_status: Some("running tests 3/8".to_string()),
            worktree_expanded: Some(("codex/status-drawer".into(), "…/bb69/yolop".into())),
            ..state()
        };

        let sections = model.expanded_status_sections();

        assert_eq!(
            sections
                .iter()
                .map(|section| section.title)
                .collect::<Vec<_>>(),
            vec!["Runtime", "Session", "Workspace"]
        );
        assert!(sections[0].fields.iter().any(|field| {
            field.label == Some("model") && field.action == Some(StatusAction::OpenModel)
        }));
        assert!(sections[0].fields.iter().any(|field| {
            field.label == Some("effort") && field.action == Some(StatusAction::OpenEffort)
        }));
        assert!(
            sections[1]
                .fields
                .iter()
                .any(|field| field.label == Some("agent") && field.value == "running tests 3/8")
        );
        assert!(
            sections[2]
                .fields
                .iter()
                .any(|field| field.label == Some("worktree"))
        );
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
    fn presentation_distinguishes_stderr_detail_from_sandbox_notice() {
        let stderr = present_transcript_line(&ChatLine {
            author: Author::Stderr,
            text: "stderr:\nOperation not permitted".to_string(),
        });
        let sandbox = present_transcript_line(&ChatLine {
            author: Author::Sandbox,
            text: "native sandbox likely blocked this operation".to_string(),
        });

        assert_eq!(stderr.label, None);
        assert_eq!(sandbox.label, Some("sandbox"));
        assert_eq!(sandbox.text, "native sandbox likely blocked this operation");
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
        use crate::tui::transcript::status_for_event;
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
    fn context_label_formats_percentage_and_compact_counts() {
        // Without a compaction budget the gauge has no threshold tick.
        assert_eq!(
            context_label(Some(90_000), Some(200_000), None).as_deref(),
            Some("████░░░░ 45% (90k/200k)")
        );
        assert_eq!(
            context_label(Some(1_500_000), Some(2_000_000), None).as_deref(),
            Some("██████░░ 75% (1.5M/2.0M)")
        );
        // Clamped to 100% and safe against a zero/absent window.
        assert_eq!(
            context_label(Some(300), Some(200), None).as_deref(),
            Some("████████ 100% (300/200)")
        );
        assert_eq!(context_label(Some(10), None, None), None);
        assert_eq!(context_label(None, Some(200_000), None), None);
        assert_eq!(context_label(Some(10), Some(0), None), None);
    }

    #[test]
    fn context_label_marks_the_compaction_threshold() {
        // 45% fill (4/8 cells), threshold 20% → tick lands on cell index 1.
        assert_eq!(
            context_label(Some(90_000), Some(200_000), Some(20)).as_deref(),
            Some("█╿██░░░░ 45% (90k/200k)")
        );
        // Threshold clamps into range; 100% lands on the final cell.
        assert_eq!(
            context_label(Some(200_000), Some(200_000), Some(100)).as_deref(),
            Some("███████╿ 100% (200k/200k)")
        );
        // An out-of-range budget is ignored rather than panicking.
        assert_eq!(
            context_label(Some(50_000), Some(200_000), Some(150)).as_deref(),
            Some("██░░░░░░ 25% (50k/200k)")
        );
    }

    #[test]
    fn status_shows_context_gauge_when_known() {
        let model = PresentationState {
            context_used_tokens: Some(50_000),
            context_window_tokens: Some(200_000),
            ..state()
        };
        let values = model
            .status_lines()
            .into_iter()
            .flat_map(|line| line.fields)
            .map(|field| (field.label, field.value))
            .collect::<Vec<_>>();
        assert!(
            values
                .iter()
                .any(|(label, value)| *label == Some("ctx") && value == "██░░░░░░ 25% (50k/200k)"),
            "expected a ctx gauge in {values:?}"
        );
    }

    #[test]
    fn status_gauge_marks_compaction_threshold_in_both_layouts() {
        // Fullscreen and inline share this status path, so proving the mark
        // lands in the rendered `ctx` field covers both modes.
        let base = PresentationState {
            context_used_tokens: Some(50_000),
            context_window_tokens: Some(200_000),
            compaction_budget_percent: Some(20),
            ..state()
        };
        for layout in [StatusLayout::Compact, StatusLayout::Expanded] {
            let model = PresentationState {
                status_layout: layout,
                ..base.clone()
            };
            let ctx = model
                .status_lines()
                .into_iter()
                .flat_map(|line| line.fields)
                .find(|field| field.label == Some("ctx"))
                .map(|field| field.value)
                .unwrap_or_default();
            assert!(
                ctx.contains('╿'),
                "{layout:?} ctx gauge should carry the compaction threshold mark: {ctx:?}"
            );
        }
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

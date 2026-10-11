use everruns_contracts::typed_id::SessionId;
use everruns_core::session_task::TASK_KIND_ASSIGNMENT;
use everruns_core::{SessionStore, SessionTask, SessionTaskRegistry, TokenUsage};
use everruns_core::{SessionTaskState, TASK_KIND_MONITOR, TASK_KIND_SUBAGENT};
use std::collections::HashSet;

const MAX_TREE_SESSIONS: usize = 256;
const MAX_TREE_ROWS: usize = 2_048;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct BackgroundCounts {
    pub(crate) running: usize,
    pub(crate) scheduled: usize,
    pub(crate) total: usize,
}

#[derive(Clone, Debug)]
pub(crate) struct TaskTreeRow {
    pub(crate) task: SessionTask,
    pub(crate) prefix: String,
    pub(crate) depth: usize,
    pub(crate) branch_usage: Option<TokenUsage>,
    parent: Option<usize>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct TaskTree {
    pub(crate) rows: Vec<TaskTreeRow>,
    pub(crate) errors: Vec<String>,
}

impl TaskTree {
    pub(crate) fn counts(&self) -> Option<BackgroundCounts> {
        counts(self.rows.iter().map(|row| &row.task))
    }

    pub(crate) fn selected(&self, selected: usize) -> Option<&SessionTask> {
        self.rows.get(selected).map(|row| &row.task)
    }

    /// Sub-agents or coordination threads: delegated work the rail opens for.
    pub(crate) fn has_delegated_work(&self) -> bool {
        self.rows
            .iter()
            .any(|row| row.task.kind == TASK_KIND_SUBAGENT || row.task.kind == TASK_KIND_ASSIGNMENT)
    }
}

/// Terminal-independent model for the live activity rail shared by both TUI
/// renderers. It deliberately describes tasks rather than terminal chrome.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ActivityRail {
    /// A coordinator's threads, one row per thread (its latest assignment).
    pub(crate) threads: ActivityCounts,
    pub(crate) agents: ActivityCounts,
    pub(crate) background: ActivityCounts,
    pub(crate) rows: Vec<ActivityRailRow>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ActivityCounts {
    pub(crate) total: usize,
    pub(crate) active: usize,
    pub(crate) succeeded: usize,
    pub(crate) failed: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ActivityRailRow {
    Section {
        kind: ActivitySection,
        counts: ActivityCounts,
    },
    Task(ActivityTaskRow),
    Warning(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ActivitySection {
    Threads,
    Agents,
    Background,
}

impl ActivitySection {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Threads => "THREADS",
            Self::Agents => "AGENTS",
            Self::Background => "BACKGROUND",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ActivityTaskRow {
    /// Index in [`TaskTree::rows`], retained so sidebar selection maps back to
    /// the task registry without a second identity scheme.
    pub(crate) task_index: usize,
    pub(crate) depth: usize,
    pub(crate) name: String,
    pub(crate) kind: ActivityTaskKind,
    pub(crate) status: ActivityStatus,
    pub(crate) usage: Option<String>,
    pub(crate) canceling: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ActivityTaskKind {
    /// A coordination thread, shown by its latest assignment. Its process runs
    /// separately, so it has no child rows here.
    Thread,
    Agent,
    Background,
    Monitor,
}

impl ActivityTaskKind {
    fn is_background(self) -> bool {
        matches!(self, Self::Background | Self::Monitor)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ActivityStatus {
    Queued,
    Running,
    AwaitingInput,
    Succeeded,
    Failed,
    Canceled,
}

impl ActivityStatus {
    pub(crate) fn icon(self, kind: ActivityTaskKind) -> &'static str {
        if kind == ActivityTaskKind::Monitor
            && !matches!(self, Self::Succeeded | Self::Failed | Self::Canceled)
        {
            return "◷";
        }
        match self {
            Self::Queued => "○",
            Self::Running => "●",
            Self::AwaitingInput => "?",
            Self::Succeeded => "✓",
            Self::Failed => "✕",
            Self::Canceled => "–",
        }
    }

    pub(crate) fn label(self, kind: ActivityTaskKind) -> &'static str {
        // The coordination board's vocabulary: what the thread needs from the
        // person, not the task machinery.
        if kind == ActivityTaskKind::Thread {
            return match self {
                Self::Queued | Self::Running => "working",
                Self::AwaitingInput | Self::Failed => "needs you",
                Self::Succeeded => "ready for review",
                Self::Canceled => "closed",
            };
        }
        if kind == ActivityTaskKind::Monitor
            && !matches!(self, Self::Succeeded | Self::Failed | Self::Canceled)
        {
            return "waiting";
        }
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::AwaitingInput => "needs input",
            Self::Succeeded => "done",
            Self::Failed => "failed",
            Self::Canceled => "canceled",
        }
    }
}

impl From<SessionTaskState> for ActivityStatus {
    fn from(value: SessionTaskState) -> Self {
        match value {
            SessionTaskState::Queued => Self::Queued,
            SessionTaskState::Running => Self::Running,
            SessionTaskState::AwaitingInput => Self::AwaitingInput,
            SessionTaskState::Succeeded => Self::Succeeded,
            SessionTaskState::Failed => Self::Failed,
            SessionTaskState::Canceled => Self::Canceled,
        }
    }
}

fn activity_task_row(task_index: usize, row: &TaskTreeRow) -> ActivityTaskRow {
    let kind = if row.task.kind == TASK_KIND_ASSIGNMENT {
        ActivityTaskKind::Thread
    } else if row.task.kind == TASK_KIND_SUBAGENT {
        ActivityTaskKind::Agent
    } else if row.task.kind == TASK_KIND_MONITOR {
        ActivityTaskKind::Monitor
    } else {
        ActivityTaskKind::Background
    };
    ActivityTaskRow {
        task_index,
        depth: row.depth,
        name: row.task.display_name.clone(),
        status: row.task.state.into(),
        kind,
        usage: if kind == ActivityTaskKind::Thread {
            checklist(&row.task)
        } else {
            row.branch_usage.as_ref().map(|usage| {
                let mut value = format!("{} tok", format_tokens(usage.total_tokens() as u64));
                if let Some(cost) = usage.effective_cost_usd() {
                    value.push_str(&format!(" · ${cost:.4}"));
                }
                value
            })
        },
        canceling: row.task.cancel_requested_at.is_some() && !row.task.state.is_terminal(),
    }
}

/// A thread's checklist progress, as the coordinator's `list_threads` reports it.
fn checklist(task: &SessionTask) -> Option<String> {
    let steps = &task.progress.as_ref()?.steps;
    if steps.is_empty() {
        return None;
    }
    let done = steps
        .iter()
        .filter(|step| {
            step.status == everruns_contracts::runtime::background::ProgressStepStatus::Done
        })
        .count();
    Some(format!("{done}/{} steps", steps.len()))
}

fn activity_counts<'a>(rows: impl Iterator<Item = &'a ActivityTaskRow>) -> ActivityCounts {
    let mut counts = ActivityCounts::default();
    for row in rows {
        counts.total += 1;
        if matches!(
            row.status,
            ActivityStatus::Queued | ActivityStatus::Running | ActivityStatus::AwaitingInput
        ) {
            counts.active += 1;
        }
        if row.status == ActivityStatus::Succeeded {
            counts.succeeded += 1;
        }
        if row.status == ActivityStatus::Failed {
            counts.failed += 1;
        }
    }
    counts
}

/// Group sub-agents and every other session task into one stable activity
/// model. Background commands and monitors remain visible while agents run.
pub(crate) fn activity_rail(tree: &TaskTree) -> Option<ActivityRail> {
    if tree.rows.is_empty() && tree.errors.is_empty() {
        return None;
    }
    // A thread works one assignment at a time; its board row is the latest.
    let latest_assignment: HashSet<usize> = {
        let mut latest = std::collections::HashMap::new();
        for (index, row) in tree.rows.iter().enumerate() {
            if row.task.kind == TASK_KIND_ASSIGNMENT {
                latest.insert(row.task.links.child_session_id, index);
            }
        }
        latest.into_values().collect()
    };
    let task_rows: Vec<_> = tree
        .rows
        .iter()
        .enumerate()
        .filter(|(index, row)| {
            row.task.kind != TASK_KIND_ASSIGNMENT || latest_assignment.contains(index)
        })
        .map(|(index, row)| activity_task_row(index, row))
        .collect();
    let threads = activity_counts(
        task_rows
            .iter()
            .filter(|row| row.kind == ActivityTaskKind::Thread),
    );
    let agents = activity_counts(
        task_rows
            .iter()
            .filter(|row| row.kind == ActivityTaskKind::Agent),
    );
    let background = activity_counts(task_rows.iter().filter(|row| row.kind.is_background()));
    let mut rows = Vec::with_capacity(task_rows.len() + 3 + tree.errors.len());
    for (section, counts, is_member) in [
        (ActivitySection::Threads, threads, ActivityTaskKind::Thread),
        (ActivitySection::Agents, agents, ActivityTaskKind::Agent),
        (
            ActivitySection::Background,
            background,
            ActivityTaskKind::Background,
        ),
    ] {
        if counts.total == 0 {
            continue;
        }
        rows.push(ActivityRailRow::Section {
            kind: section,
            counts,
        });
        rows.extend(task_rows.iter().filter_map(|row| {
            let matches = match section {
                ActivitySection::Threads | ActivitySection::Agents => row.kind == is_member,
                ActivitySection::Background => row.kind.is_background(),
            };
            matches.then(|| ActivityRailRow::Task(row.clone()))
        }));
    }
    rows.extend(tree.errors.iter().cloned().map(ActivityRailRow::Warning));
    Some(ActivityRail {
        threads,
        agents,
        background,
        rows,
    })
}

impl ActivityRail {
    pub(crate) fn selectable_task_indices(&self) -> Vec<usize> {
        self.rows
            .iter()
            .filter_map(|row| match row {
                ActivityRailRow::Task(row) => Some(row.task_index),
                _ => None,
            })
            .collect()
    }

    pub(crate) fn body_row_for_task(&self, task_index: usize) -> Option<usize> {
        self.rows.iter().position(
            |row| matches!(row, ActivityRailRow::Task(task) if task.task_index == task_index),
        )
    }

    pub(crate) fn active(&self) -> usize {
        self.threads.active + self.agents.active + self.background.active
    }
}

/// Plain-text projection used by tests and cancellation feedback. Terminal
/// renderers consume the structured model so colors never depend on substrings.
#[cfg(test)]
pub(crate) fn render_activity_rail(tree: &TaskTree, selected: Option<usize>) -> String {
    let Some(panel) = activity_rail(tree) else {
        return "No activity in this session.".to_string();
    };
    let mut out = String::new();
    for row in panel.rows {
        match row {
            ActivityRailRow::Section { kind, counts } => {
                out.push_str(&format!("{} {}\n", kind.label(), counts.total));
            }
            ActivityRailRow::Task(row) => {
                out.push_str(if selected == Some(row.task_index) {
                    "> "
                } else {
                    "  "
                });
                out.push_str(&"  ".repeat(row.depth));
                out.push_str(row.status.icon(row.kind));
                out.push(' ');
                out.push_str(&row.name);
                out.push_str(" — ");
                out.push_str(if row.canceling {
                    "canceling"
                } else {
                    row.status.label(row.kind)
                });
                if let Some(usage) = row.usage {
                    out.push_str(" · ");
                    out.push_str(&usage);
                }
                out.push('\n');
            }
            ActivityRailRow::Warning(warning) => {
                out.push_str("! ");
                out.push_str(&warning);
                out.push('\n');
            }
        }
    }
    out
}

enum Visit {
    ExecutionSession {
        session_id: SessionId,
        parent: Option<usize>,
        ancestors_last: Vec<bool>,
    },
    Task {
        task: Box<SessionTask>,
        parent: Option<usize>,
        ancestors_last: Vec<bool>,
        is_last: bool,
    },
}

/// Load the current session's task descendants. A subagent task links its child
/// session; tasks owned by that session are rendered beneath it, recursively.
pub(crate) async fn load_task_tree(
    root_session_id: SessionId,
    registry: &dyn SessionTaskRegistry,
    sessions: &dyn SessionStore,
) -> TaskTree {
    let mut tree = TaskTree::default();
    let mut stack = vec![Visit::ExecutionSession {
        session_id: root_session_id,
        parent: None,
        ancestors_last: Vec::new(),
    }];
    let mut visited = HashSet::new();
    let mut sessions_truncated = false;
    let mut rows_truncated = false;

    while let Some(visit) = stack.pop() {
        match visit {
            Visit::ExecutionSession {
                session_id,
                parent,
                ancestors_last,
            } => {
                if visited.len() >= MAX_TREE_SESSIONS {
                    if !sessions_truncated {
                        tree.errors.push(format!(
                            "task tree stopped after {MAX_TREE_SESSIONS} sessions"
                        ));
                        sessions_truncated = true;
                    }
                    continue;
                }
                if !visited.insert(session_id) {
                    tree.errors
                        .push(format!("task tree cycle at session {session_id}"));
                    continue;
                }
                match registry.list(session_id, None).await {
                    Ok(mut tasks) => {
                        tasks.sort_by(|a, b| {
                            a.created_at
                                .cmp(&b.created_at)
                                .then_with(|| a.id.cmp(&b.id))
                        });
                        let count = tasks.len();
                        for (index, task) in tasks.into_iter().enumerate().rev() {
                            stack.push(Visit::Task {
                                task: Box::new(task),
                                parent,
                                ancestors_last: ancestors_last.clone(),
                                is_last: index + 1 == count,
                            });
                        }
                    }
                    Err(error) => tree.errors.push(format!("session {session_id}: {error}")),
                }
            }
            Visit::Task {
                task,
                parent,
                ancestors_last,
                is_last,
            } => {
                if tree.rows.len() >= MAX_TREE_ROWS {
                    if !rows_truncated {
                        tree.errors
                            .push(format!("task tree stopped after {MAX_TREE_ROWS} tasks"));
                        rows_truncated = true;
                    }
                    continue;
                }
                let task = *task;
                let mut prefix = String::new();
                for ancestor_last in &ancestors_last {
                    prefix.push_str(if *ancestor_last { "   " } else { "│  " });
                }
                prefix.push_str(if is_last { "└─ " } else { "├─ " });

                // A coordination thread is its own process and session log;
                // its tasks are not this session's to list.
                let (child_session_id, branch_usage) = if task.kind == TASK_KIND_ASSIGNMENT {
                    (None, None)
                } else if let Some(child_id) = task.links.child_session_id {
                    match sessions.get_session(child_id).await {
                        Ok(Some(session))
                            if session.parent_session_id == Some(task.session_id)
                                || session.forked_from_session_id == Some(task.session_id) =>
                        {
                            (Some(child_id), session.usage)
                        }
                        Ok(Some(_)) => {
                            tree.errors.push(format!(
                                "linked session {child_id} is not a child of {}",
                                task.session_id
                            ));
                            (None, None)
                        }
                        Ok(None) => {
                            tree.errors
                                .push(format!("child session {child_id} not found"));
                            (None, None)
                        }
                        Err(error) => {
                            tree.errors
                                .push(format!("child session {child_id}: {error}"));
                            (None, None)
                        }
                    }
                } else {
                    (None, None)
                };
                let row_index = tree.rows.len();
                tree.rows.push(TaskTreeRow {
                    task,
                    prefix,
                    depth: ancestors_last.len(),
                    branch_usage,
                    parent,
                });

                if let Some(child_id) = child_session_id {
                    let mut child_ancestors = ancestors_last;
                    child_ancestors.push(is_last);
                    stack.push(Visit::ExecutionSession {
                        session_id: child_id,
                        parent: Some(row_index),
                        ancestors_last: child_ancestors,
                    });
                }
            }
        }
    }

    // A task linked to a child session owns that session's usage plus all
    // descendant child-session usage. Fold leaves upward for branch rollups.
    for index in (0..tree.rows.len()).rev() {
        let Some(parent) = tree.rows[index].parent else {
            continue;
        };
        let Some(usage) = tree.rows[index].branch_usage.clone() else {
            continue;
        };
        match tree.rows[parent].branch_usage.as_mut() {
            Some(parent_usage) => parent_usage.add(&usage),
            None => tree.rows[parent].branch_usage = Some(usage),
        }
    }

    tree
}

/// Running tools, scheduled monitors, and total history for the status bar.
/// Returns `None` when the session has no tasks.
fn counts<'a>(tasks: impl Iterator<Item = &'a SessionTask>) -> Option<BackgroundCounts> {
    let mut counts = BackgroundCounts {
        running: 0,
        scheduled: 0,
        total: 0,
    };
    for task in tasks {
        counts.total += 1;
        if !task.state.is_terminal() {
            if task.kind == TASK_KIND_MONITOR {
                counts.scheduled += 1;
            } else {
                counts.running += 1;
            }
        }
    }
    (counts.total > 0).then_some(counts)
}

/// Human-readable tree for `/background` and both TUI renderers. `selected`
/// adds an interactive cursor without changing the command's plain-text form.
pub(crate) fn render_task_tree(tree: &TaskTree, selected: Option<usize>) -> String {
    if tree.rows.is_empty() {
        if tree.errors.is_empty() {
            return "No background tasks in this session.".to_string();
        }
        return format!(
            "No background tasks in this session.\n\nSession task registry: {}",
            tree.errors.join("; ")
        );
    }

    let counts = tree.counts().expect("non-empty task tree has counts");
    let mut out = format!(
        "{} task(s): {} running, {} scheduled\n",
        counts.total, counts.running, counts.scheduled
    );
    for (index, row) in tree.rows.iter().enumerate() {
        let cursor = if selected == Some(index) { "> " } else { "  " };
        let task = &row.task;
        out.push_str(cursor);
        out.push_str(&row.prefix);
        out.push_str(&format!(
            "[{}] {} {}: {}",
            task.id, task.kind, task.state, task.display_name
        ));
        if let Some(usage) = &row.branch_usage {
            out.push_str(&format!(
                " · {} tok",
                format_tokens(usage.total_tokens() as u64)
            ));
            if let Some(cost) = usage.effective_cost_usd() {
                out.push_str(&format!(" · ${cost:.4}"));
            }
        }
        if task.cancel_requested_at.is_some() && !task.state.is_terminal() {
            out.push_str(" · canceling");
        }
        if let Some(detail) = task_detail(task) {
            out.push_str(" -- ");
            out.push_str(detail);
        }
        if let Some(path) = task.result_path.as_deref() {
            out.push_str(&format!(" (result: {path})"));
        }
        out.push('\n');
    }
    if !tree.errors.is_empty() {
        out.push_str("\nWarnings: ");
        out.push_str(&tree.errors.join("; "));
        out.push('\n');
    }
    out.push_str("\nInspect tasks with list_tasks/get_task; cancel with cancel_task.");
    out
}

fn task_detail(task: &SessionTask) -> Option<&str> {
    if task.state.is_terminal() {
        task.summary
            .as_deref()
            .or_else(|| task.error.as_ref().map(|error| error.message.as_str()))
    } else {
        task.state_detail
            .as_deref()
            .or(task.summary.as_deref())
            .or_else(|| task.error.as_ref().map(|error| error.message.as_str()))
    }
}

fn format_tokens(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}m", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{:.1}k", tokens as f64 / 1_000.0)
    } else {
        tokens.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use chrono::Utc;
    use everruns::local::{LocalSessionTaskRegistry, SqliteDb};
    use everruns_contracts::typed_id::{HarnessId, SessionId};
    use everruns_core::host::SessionBuilder;
    use everruns_core::session_task::new_session_task;
    use everruns_core::{CreateSessionTask, ExecutionSession};
    use everruns_core::{
        SessionTaskState, TASK_KIND_BACKGROUND_TOOL, TASK_KIND_MONITOR, TASK_KIND_SUBAGENT,
        TaskLinks,
    };
    use serde_json::json;
    use std::collections::HashMap;

    fn task(id: &str, state: SessionTaskState, name: &str) -> SessionTask {
        task_with_kind(id, TASK_KIND_BACKGROUND_TOOL, state, name)
    }

    fn task_with_kind(id: &str, kind: &str, state: SessionTaskState, name: &str) -> SessionTask {
        let mut task = new_session_task(
            CreateSessionTask {
                session_id: SessionId::from_seed(42),
                id: Some(id.to_string()),
                kind: kind.to_string(),
                display_name: name.to_string(),
                spec: json!({}),
                state,
                links: Default::default(),
                wake_policy: Default::default(),
            },
            Utc::now(),
        );
        task.summary = Some("done".to_string());
        task
    }

    fn tree(tasks: Vec<SessionTask>) -> TaskTree {
        TaskTree {
            rows: tasks
                .into_iter()
                .enumerate()
                .map(|(index, task)| TaskTreeRow {
                    task,
                    prefix: if index == 0 { "├─ " } else { "└─ " }.into(),
                    depth: 0,
                    branch_usage: None,
                    parent: None,
                })
                .collect(),
            errors: Vec::new(),
        }
    }

    #[test]
    fn counts_reflect_active_and_total_session_tasks() {
        let tree = tree(vec![
            task("task_a", SessionTaskState::Running, "run"),
            task_with_kind(
                "task_monitor",
                TASK_KIND_MONITOR,
                SessionTaskState::Running,
                "scheduled review",
            ),
            task("task_b", SessionTaskState::Succeeded, "done"),
        ]);

        assert_eq!(
            tree.counts(),
            Some(BackgroundCounts {
                running: 1,
                scheduled: 1,
                total: 3,
            })
        );
        assert_eq!(TaskTree::default().counts(), None);
    }

    #[test]
    fn render_lists_tree_rows_and_selection() {
        let tree = tree(vec![task(
            "task_a",
            SessionTaskState::Succeeded,
            "write marker",
        )]);
        let rendered = render_task_tree(&tree, Some(0));

        assert!(rendered.contains("1 task(s): 0 running, 0 scheduled"));
        assert!(rendered.contains("> ├─ [task_a] background_tool succeeded: write marker"));
    }

    #[test]
    fn render_formats_branch_usage() {
        let mut tree = tree(vec![task(
            "task_agent",
            SessionTaskState::Running,
            "research",
        )]);
        tree.rows[0].branch_usage =
            Some(TokenUsage::new(12_000, 345).with_effective_cost(Some(0.12345)));
        let rendered = render_task_tree(&tree, None);
        assert!(rendered.contains("12.3k tok · $0.1235"));
    }

    #[test]
    fn activity_rail_groups_agents_background_work_and_monitors() {
        let tree = tree(vec![
            task_with_kind(
                "task_lead",
                TASK_KIND_SUBAGENT,
                SessionTaskState::Running,
                "Flight Lead",
            ),
            task_with_kind(
                "task_worker",
                TASK_KIND_SUBAGENT,
                SessionTaskState::Succeeded,
                "Orbit Clock",
            ),
            task("task_shell", SessionTaskState::Running, "cargo test"),
            task_with_kind(
                "task_monitor",
                TASK_KIND_MONITOR,
                SessionTaskState::Running,
                "CI monitor",
            ),
        ]);

        let panel = activity_rail(&tree).expect("tasks produce an activity rail");
        assert_eq!(
            (
                panel.agents.total,
                panel.agents.active,
                panel.agents.succeeded,
                panel.agents.failed
            ),
            (2, 1, 1, 0)
        );
        assert_eq!((panel.background.total, panel.background.active), (2, 2));
        assert_eq!(panel.selectable_task_indices(), vec![0, 1, 2, 3]);
        assert_eq!(panel.body_row_for_task(0), Some(1));
        assert_eq!(panel.body_row_for_task(2), Some(4));
        assert!(tree.has_delegated_work());

        let rendered = render_activity_rail(&tree, Some(0));
        assert!(rendered.contains("AGENTS 2"));
        assert!(rendered.contains("> ● Flight Lead — running"));
        assert!(rendered.contains("✓ Orbit Clock — done"));
        assert!(rendered.contains("BACKGROUND 2"));
        assert!(rendered.contains("● cargo test — running"));
        assert!(rendered.contains("◷ CI monitor — waiting"));
    }

    #[test]
    fn render_empty_reports_no_tasks() {
        assert_eq!(
            render_task_tree(&TaskTree::default(), None),
            "No background tasks in this session."
        );
    }

    struct Sessions(HashMap<SessionId, ExecutionSession>);

    #[async_trait]
    impl SessionStore for Sessions {
        async fn get_session(
            &self,
            session_id: SessionId,
        ) -> everruns_contracts::error::Result<Option<ExecutionSession>> {
            Ok(self.0.get(&session_id).cloned())
        }
    }

    #[tokio::test]
    async fn load_nests_child_tasks_and_rolls_usage_to_branch_root() {
        let root_id = SessionId::from_seed(700);
        let child_id = SessionId::from_seed(701);
        let grandchild_id = SessionId::from_seed(702);
        let registry = LocalSessionTaskRegistry::new(
            SqliteDb::open_in_memory().expect("in-memory task database"),
        )
        .expect("task registry");

        registry
            .create(CreateSessionTask {
                session_id: root_id,
                id: Some("task_parent".into()),
                kind: TASK_KIND_SUBAGENT.into(),
                display_name: "research".into(),
                spec: json!({}),
                state: SessionTaskState::Running,
                links: TaskLinks {
                    child_session_id: Some(child_id),
                    ..Default::default()
                },
                wake_policy: Default::default(),
            })
            .await
            .unwrap();
        registry
            .create(CreateSessionTask {
                session_id: child_id,
                id: Some("task_child".into()),
                kind: TASK_KIND_SUBAGENT.into(),
                display_name: "verify".into(),
                spec: json!({}),
                state: SessionTaskState::Running,
                links: TaskLinks {
                    child_session_id: Some(grandchild_id),
                    ..Default::default()
                },
                wake_policy: Default::default(),
            })
            .await
            .unwrap();

        let mut child = SessionBuilder::new(HarnessId::from_seed(1))
            .id(child_id)
            .build();
        child.parent_session_id = Some(root_id);
        child.usage = Some(TokenUsage::new(100, 20).with_effective_cost(Some(0.01)));
        let mut grandchild = SessionBuilder::new(HarnessId::from_seed(1))
            .id(grandchild_id)
            .build();
        grandchild.parent_session_id = Some(child_id);
        grandchild.usage = Some(TokenUsage::new(50, 5).with_effective_cost(Some(0.02)));
        let sessions = Sessions(HashMap::from([
            (child_id, child),
            (grandchild_id, grandchild),
        ]));

        let tree = load_task_tree(root_id, &registry, &sessions).await;

        assert_eq!(tree.rows.len(), 2);
        assert_eq!(tree.rows[0].task.id, "task_parent");
        assert_eq!(tree.rows[1].task.id, "task_child");
        assert!(tree.rows[1].prefix.starts_with("   "));
        let root_usage = tree.rows[0].branch_usage.as_ref().unwrap();
        assert_eq!(root_usage.total_tokens(), 175);
        assert_eq!(root_usage.effective_cost_usd(), Some(0.03));
    }

    /// A coordinator's board: one row per thread (its latest assignment), in
    /// the board's words, with checklist progress. A thread runs in its own
    /// process, so the coordinator's session store not knowing it is normal.
    #[tokio::test]
    async fn coordination_threads_form_a_board_by_their_latest_assignment() {
        use everruns_contracts::runtime::background::{ProgressStep, ProgressStepStatus};
        let root_id = SessionId::from_seed(720);
        let parser = SessionId::from_seed(721);
        let docs = SessionId::from_seed(722);
        let registry = LocalSessionTaskRegistry::new(
            SqliteDb::open_in_memory().expect("in-memory task database"),
        )
        .expect("task registry");
        let assign =
            |id: &str, thread: SessionId, title: &str, state: SessionTaskState| CreateSessionTask {
                session_id: root_id,
                id: Some(id.into()),
                kind: TASK_KIND_ASSIGNMENT.into(),
                display_name: title.into(),
                spec: json!({}),
                state,
                links: TaskLinks {
                    child_session_id: Some(thread),
                    ..Default::default()
                },
                wake_policy: Default::default(),
            };
        for task in [
            assign("asg_1", parser, "Parser tests", SessionTaskState::Succeeded),
            assign("asg_2", docs, "Docs pass", SessionTaskState::AwaitingInput),
            assign("asg_3", parser, "Parser fuzzing", SessionTaskState::Running),
        ] {
            registry.create(task).await.unwrap();
            // Distinct creation times keep "latest" well defined.
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        registry
            .update(
                root_id,
                "asg_3",
                everruns_core::SessionTaskUpdate {
                    progress: Some(
                        everruns_contracts::runtime::background::BackgroundProgress {
                            current: None,
                            total: None,
                            unit: None,
                            label: None,
                            steps: vec![
                                ProgressStep {
                                    title: "seed corpus".into(),
                                    status: ProgressStepStatus::Done,
                                },
                                ProgressStep {
                                    title: "run".into(),
                                    status: ProgressStepStatus::InProgress,
                                },
                            ],
                        },
                    ),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        let tree = load_task_tree(root_id, &registry, &Sessions(HashMap::new())).await;
        assert!(tree.errors.is_empty(), "{:?}", tree.errors);
        let rail = activity_rail(&tree).expect("assignments produce a board");
        assert_eq!((rail.threads.total, rail.threads.active), (2, 2));
        let rendered = render_activity_rail(&tree, None);
        assert!(rendered.contains("THREADS 2"), "{rendered}");
        assert!(rendered.contains("? Docs pass — needs you"), "{rendered}");
        assert!(
            rendered.contains("● Parser fuzzing — working · 1/2 steps"),
            "{rendered}"
        );
        assert!(!rendered.contains("Parser tests"), "{rendered}");
        assert!(!rendered.contains("BACKGROUND"), "{rendered}");
        assert!(tree.has_delegated_work(), "the rail opens for a board");
    }

    #[tokio::test]
    async fn load_rejects_linked_sessions_outside_the_task_lineage() {
        let root_id = SessionId::from_seed(710);
        let unrelated_id = SessionId::from_seed(711);
        let registry = LocalSessionTaskRegistry::new(
            SqliteDb::open_in_memory().expect("in-memory task database"),
        )
        .expect("task registry");
        registry
            .create(CreateSessionTask {
                session_id: root_id,
                id: Some("task_parent".into()),
                kind: TASK_KIND_SUBAGENT.into(),
                display_name: "research".into(),
                spec: json!({}),
                state: SessionTaskState::Running,
                links: TaskLinks {
                    child_session_id: Some(unrelated_id),
                    ..Default::default()
                },
                wake_policy: Default::default(),
            })
            .await
            .unwrap();
        let mut unrelated = SessionBuilder::new(HarnessId::from_seed(1))
            .id(unrelated_id)
            .build();
        unrelated.usage = Some(TokenUsage::new(1_000, 0));
        let sessions = Sessions(HashMap::from([(unrelated_id, unrelated)]));

        let tree = load_task_tree(root_id, &registry, &sessions).await;

        assert_eq!(tree.rows.len(), 1);
        assert!(tree.rows[0].branch_usage.is_none());
        assert!(
            tree.errors
                .iter()
                .any(|error| error.contains("is not a child"))
        );
    }
}

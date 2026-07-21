use everruns_core::SessionTask;
use everruns_core::session_task::TASK_KIND_MONITOR;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct BackgroundCounts {
    pub(crate) running: usize,
    pub(crate) scheduled: usize,
    pub(crate) total: usize,
}

/// Running tools, scheduled monitors, and total history for the status bar.
/// Returns `None` when the session has no tasks.
pub(crate) fn counts(tasks: &[SessionTask]) -> Option<BackgroundCounts> {
    let scheduled = tasks
        .iter()
        .filter(|task| task.kind == TASK_KIND_MONITOR && !task.state.is_terminal())
        .count();
    let running = tasks
        .iter()
        .filter(|task| task.kind != TASK_KIND_MONITOR && !task.state.is_terminal())
        .count();
    let total = tasks.len();
    (total > 0).then_some(BackgroundCounts {
        running,
        scheduled,
        total,
    })
}

/// Human-readable list of the session's everruns tasks for the `/background`
/// command and the TUI panel. Detached background work runs through
/// `spawn_background`; inspect and control it with `list_tasks`/`get_task`/
/// `cancel_task`.
pub(crate) fn render_task_list(tasks: &[SessionTask], task_error: Option<&str>) -> String {
    if tasks.is_empty() {
        if let Some(error) = task_error {
            return format!(
                "No background tasks in this session.\n\nSession task registry: {error}"
            );
        }
        return "No background tasks in this session.".to_string();
    }

    let counts = counts(tasks).expect("non-empty task list has counts");
    let mut out = format!(
        "{} background task(s): {} running, {} scheduled:\n",
        counts.total, counts.running, counts.scheduled
    );
    let mut sorted = tasks.to_vec();
    sorted.sort_by(|a, b| {
        b.updated_at
            .cmp(&a.updated_at)
            .then_with(|| b.created_at.cmp(&a.created_at))
    });
    for task in sorted {
        out.push_str(&format!(
            "- [{}] {} {}: {}",
            task.id, task.kind, task.state, task.display_name
        ));
        if task.state.is_terminal()
            && let Some(summary) = task.summary.as_deref()
        {
            out.push_str(" -- ");
            out.push_str(summary);
        } else if let Some(detail) = task.state_detail.as_deref() {
            out.push_str(" -- ");
            out.push_str(detail);
        } else if let Some(summary) = task.summary.as_deref() {
            out.push_str(" -- ");
            out.push_str(summary);
        } else if let Some(error) = task.error.as_ref() {
            out.push_str(" -- ");
            out.push_str(&error.message);
        }
        if let Some(path) = task.result_path.as_deref() {
            out.push_str(&format!(" (result: {path})"));
        }
        out.push('\n');
    }
    out.push_str("\nInspect tasks with list_tasks/get_task; cancel with cancel_task.");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use everruns_core::session_task::{
        SessionTaskState, TASK_KIND_BACKGROUND_TOOL, TASK_KIND_MONITOR, new_session_task,
    };
    use everruns_core::{CreateSessionTask, SessionId};
    use serde_json::json;

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

    #[test]
    fn counts_reflect_active_and_total_session_tasks() {
        let tasks = vec![
            task("task_a", SessionTaskState::Running, "run"),
            task_with_kind(
                "task_monitor",
                TASK_KIND_MONITOR,
                SessionTaskState::Running,
                "scheduled review",
            ),
            task("task_b", SessionTaskState::Succeeded, "done"),
        ];

        assert_eq!(
            counts(&tasks),
            Some(BackgroundCounts {
                running: 1,
                scheduled: 1,
                total: 3,
            })
        );
        assert_eq!(counts(&[]), None);
    }

    #[test]
    fn render_lists_session_tasks() {
        let tasks = vec![task("task_a", SessionTaskState::Succeeded, "write marker")];
        let rendered = render_task_list(&tasks, None);

        assert!(rendered.contains("1 background task(s): 0 running, 0 scheduled"));
        assert!(rendered.contains("[task_a] background_tool succeeded: write marker"));
        assert!(rendered.contains("cancel with cancel_task"));
    }

    #[test]
    fn render_empty_reports_no_tasks() {
        assert_eq!(
            render_task_list(&[], None),
            "No background tasks in this session."
        );
    }
}

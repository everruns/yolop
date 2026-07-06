use everruns_core::SessionTask;

/// `(active, total)` counts for the status bar, or `None` when there are no
/// tasks. Active = non-terminal.
pub(crate) fn counts(tasks: &[SessionTask]) -> Option<(usize, usize)> {
    let active = tasks
        .iter()
        .filter(|task| !task.state.is_terminal())
        .count();
    let total = tasks.len();
    (total > 0).then_some((active, total))
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

    let active = tasks
        .iter()
        .filter(|task| !task.state.is_terminal())
        .count();
    let mut out = format!("{} background task(s), {} active:\n", tasks.len(), active);
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
        if let Some(detail) = task.state_detail.as_deref() {
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
        SessionTaskState, TASK_KIND_BACKGROUND_TOOL, new_session_task,
    };
    use everruns_core::{CreateSessionTask, SessionId};
    use serde_json::json;

    fn task(id: &str, state: SessionTaskState, name: &str) -> SessionTask {
        let mut task = new_session_task(
            CreateSessionTask {
                session_id: SessionId::from_seed(42),
                id: Some(id.to_string()),
                kind: TASK_KIND_BACKGROUND_TOOL.to_string(),
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
            task("task_b", SessionTaskState::Succeeded, "done"),
        ];

        assert_eq!(counts(&tasks), Some((1, 2)));
        assert_eq!(counts(&[]), None);
    }

    #[test]
    fn render_lists_session_tasks() {
        let tasks = vec![task("task_a", SessionTaskState::Succeeded, "write marker")];
        let rendered = render_task_list(&tasks, None);

        assert!(rendered.contains("1 background task(s), 0 active"));
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

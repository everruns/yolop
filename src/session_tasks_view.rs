use everruns_core::SessionTask;

pub(crate) fn combined_counts(
    tasks: &[SessionTask],
    legacy_counts: (usize, usize),
) -> Option<(usize, usize)> {
    let active = tasks
        .iter()
        .filter(|task| !task.state.is_terminal())
        .count()
        .saturating_add(legacy_counts.0);
    let total = tasks.len().saturating_add(legacy_counts.1);
    (total > 0).then_some((active, total))
}

pub(crate) fn render_combined_task_list(
    tasks: &[SessionTask],
    task_error: Option<&str>,
    legacy_counts: (usize, usize),
    legacy_body: &str,
) -> String {
    let has_session_tasks = !tasks.is_empty();
    let has_legacy_tasks = legacy_counts.1 > 0;

    if !has_session_tasks && !has_legacy_tasks {
        if let Some(error) = task_error {
            return format!(
                "No background tasks in this session.\n\nSession task registry: {error}"
            );
        }
        return "No background tasks in this session.".to_string();
    }

    let mut out = String::new();
    if has_session_tasks {
        let active = tasks
            .iter()
            .filter(|task| !task.state.is_terminal())
            .count();
        out.push_str(&format!(
            "{} Everruns session task(s), {} active:\n",
            tasks.len(),
            active
        ));
        let mut sorted = tasks.to_vec();
        sorted.sort_by(|a, b| {
            b.updated_at
                .cmp(&a.updated_at)
                .then_with(|| b.created_at.cmp(&a.created_at))
        });
        for task in sorted {
            out.push_str("- ");
            out.push_str(&format!(
                "[{}] {} {}: {}",
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
        out.push_str("\nInspect Everruns tasks with list_tasks/get_task.");
    } else if let Some(error) = task_error {
        out.push_str("Everruns session tasks could not be loaded:\n");
        out.push_str(error);
    }

    if has_legacy_tasks {
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str("Legacy Yolop background tasks:\n");
        out.push_str(legacy_body);
    }

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
    fn counts_include_session_tasks_and_legacy_tasks() {
        let tasks = vec![
            task("task_a", SessionTaskState::Running, "run"),
            task("task_b", SessionTaskState::Succeeded, "done"),
        ];

        assert_eq!(combined_counts(&tasks, (1, 3)), Some((2, 5)));
        assert_eq!(combined_counts(&[], (0, 0)), None);
    }

    #[test]
    fn render_prefers_everruns_tasks_and_keeps_legacy_section() {
        let tasks = vec![task("task_a", SessionTaskState::Succeeded, "write marker")];
        let rendered =
            render_combined_task_list(&tasks, None, (0, 1), "1 background task(s), 0 running:");

        assert!(rendered.contains("Everruns session task"));
        assert!(rendered.contains("[task_a] background_tool succeeded: write marker"));
        assert!(rendered.contains("Legacy Yolop background tasks"));
    }
}

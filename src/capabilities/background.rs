// The `background` capability — a thin surface over everruns session tasks.
//
// Detached background work (e.g. `gh pr checks --watch` waiting on CI) runs
// through everruns' `spawn_background`, which wraps the background-capable
// `bash` tool: it streams to a session-file log, tracks a `background_tool`
// session task, and on completion signals the session. yolop delivers that
// signal to the host as a proactive wake turn via the platform-store wake seam
// (see `crate::background_wake`).
//
// This capability adds only the `/background` command, which lists the
// session's everruns tasks. The model inspects and controls them with the
// everruns `list_tasks` / `get_task` / `cancel_task` tools (the `session_tasks`
// capability). See specs/background.md.

use crate::session_tasks_view::render_task_list;
use async_trait::async_trait;
use everruns_core::capabilities::{Capability, CapabilityStatus, SystemPromptContext};
use everruns_core::command::{
    CommandDescriptor, CommandExecutionContext, CommandResult, CommandSource, ExecuteCommandRequest,
};
use everruns_core::session_task::SessionTaskRegistry;
use everruns_core::session_task::TASK_KIND_MONITOR;
use everruns_core::typed_id::SessionId;
use std::sync::Arc;

pub(crate) const BACKGROUND_CAPABILITY_ID: &str = "background";

// Prompt-side half of the poll-proofing seam (the runtime-enforced half is
// `progress_guard`'s Waiting class): waiting on an external event must cost
// zero turns, so steer the model to detach the wait and rely on the
// completion wake instead of foreground watches or poll-sleep turns.
const BACKGROUND_SYSTEM_PROMPT: &str = "<capability id=\"background\">\n\
    Waiting on an external event — a CI run, a PR review window, a deploy, a long \
    build — must not consume turns. Do not run watch commands in the foreground and \
    do not poll status across turns. Start one blocking watch detached via \
    `spawn_background` (e.g. `gh pr checks --watch`, `gh run watch --exit-status`, \
    or `until <check>; do sleep 30; done`), say what you are waiting for, and end \
    the turn: completion wakes you with the result. You can keep working on other \
    steps while it runs. In one-shot (`-p`) runs there is no wake — block on the \
    spawned task with `wait_task` instead of ending the turn. To inspect background \
    state, call `list_tasks` once without kind or state filters; scheduled work is a \
    `monitor`, not a `background_tool`. Scheduled monitors are obligations you own: \
    before finishing work, cancel any monitor whose purpose is satisfied, superseded, \
    or no longer needed; keep it armed only when its future wake is still required. \
    Treat `disarmed: true` or a terminal task state from `cancel_task` as completed \
    cancellation; `cancellation_pending: true` means cooperative shutdown is still in \
    progress.\n\
    </capability>";

pub(crate) struct BackgroundCapability {
    pub(crate) session_id: SessionId,
    pub(crate) task_registry: Arc<dyn SessionTaskRegistry>,
}

#[async_trait]
impl Capability for BackgroundCapability {
    fn id(&self) -> &str {
        BACKGROUND_CAPABILITY_ID
    }
    fn name(&self) -> &str {
        "Background execution"
    }
    fn description(&self) -> &str {
        "List detached background tasks (started with `spawn_background`, e.g. waiting for CI) and \
         their status. Completions wake the agent automatically; inspect results with \
         `get_task`/`list_tasks`."
    }
    fn status(&self) -> CapabilityStatus {
        CapabilityStatus::Available
    }
    fn category(&self) -> Option<&str> {
        Some("Execution")
    }

    async fn system_prompt_contribution(&self, _ctx: &SystemPromptContext) -> Option<String> {
        let active_monitors = self
            .task_registry
            .list(self.session_id, None)
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|task| task.kind == TASK_KIND_MONITOR && !task.state.is_terminal())
            .map(|task| task.id)
            .collect::<Vec<_>>();
        let mut prompt = BACKGROUND_SYSTEM_PROMPT
            .strip_suffix("</capability>")
            .unwrap_or(BACKGROUND_SYSTEM_PROMPT)
            .to_string();
        if !active_monitors.is_empty() {
            prompt.push_str(&format!(
                "Active scheduled monitor obligations in this session: {}. Reconcile each before reporting its parent work complete.\n",
                active_monitors.join(", ")
            ));
        }
        prompt.push_str("</capability>");
        Some(prompt)
    }

    fn system_prompt_preview(&self) -> Option<String> {
        Some(
            "<capability id=\"background\">\nDetach waits on external events via `spawn_background`; completion wakes the agent.\n</capability>"
                .to_string(),
        )
    }

    fn commands(&self) -> Vec<CommandDescriptor> {
        vec![CommandDescriptor {
            name: "background".to_string(),
            description: "list background tasks and their status".to_string(),
            source: CommandSource::System,
            args: Vec::new(),
        }]
    }

    async fn execute_command(
        &self,
        request: &ExecuteCommandRequest,
        _ctx: &CommandExecutionContext,
    ) -> everruns_core::Result<CommandResult> {
        if request.name != "background" {
            return Err(everruns_core::AgentLoopError::config(format!(
                "{} cannot execute /{}",
                self.id(),
                request.name
            )));
        }
        let (tasks, task_error) = match self.task_registry.list(self.session_id, None).await {
            Ok(tasks) => (tasks, None),
            Err(err) => (Vec::new(), Some(err.to_string())),
        };
        Ok(CommandResult {
            success: true,
            message: render_task_list(&tasks, task_error.as_deref()),
            error_code: None,
            error_fields: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use everruns_core::session_task::{
        CreateSessionTask, NewTaskMessage, SessionTask, SessionTaskFilter, SessionTaskUpdate,
        TaskMessage,
    };

    /// The prompt contribution tolerates an empty registry.
    struct StubRegistry {
        tasks: Vec<SessionTask>,
    }

    #[async_trait]
    impl SessionTaskRegistry for StubRegistry {
        async fn create(&self, _input: CreateSessionTask) -> everruns_core::Result<SessionTask> {
            unimplemented!("stub")
        }
        async fn update(
            &self,
            _session_id: SessionId,
            _task_id: &str,
            _update: SessionTaskUpdate,
        ) -> everruns_core::Result<Option<SessionTask>> {
            unimplemented!("stub")
        }
        async fn get(
            &self,
            _session_id: SessionId,
            _task_id: &str,
        ) -> everruns_core::Result<Option<SessionTask>> {
            unimplemented!("stub")
        }
        async fn list(
            &self,
            _session_id: SessionId,
            _filter: Option<&SessionTaskFilter>,
        ) -> everruns_core::Result<Vec<SessionTask>> {
            Ok(self.tasks.clone())
        }
        async fn request_cancel(
            &self,
            _session_id: SessionId,
            _task_id: &str,
        ) -> everruns_core::Result<Option<SessionTask>> {
            unimplemented!("stub")
        }
        async fn record_message(
            &self,
            _session_id: SessionId,
            _task_id: &str,
            _message: NewTaskMessage,
        ) -> everruns_core::Result<TaskMessage> {
            unimplemented!("stub")
        }
        async fn list_messages(
            &self,
            _session_id: SessionId,
            _task_id: &str,
            _limit: Option<u32>,
            _after_id: Option<&str>,
        ) -> everruns_core::Result<Vec<TaskMessage>> {
            unimplemented!("stub")
        }
    }

    #[tokio::test]
    async fn system_prompt_teaches_detached_waits_per_host() {
        let capability = BackgroundCapability {
            session_id: SessionId::new(),
            task_registry: Arc::new(StubRegistry { tasks: vec![] }),
        };
        let ctx = SystemPromptContext::without_file_store(SessionId::new());

        let prompt = capability
            .system_prompt_contribution(&ctx)
            .await
            .expect("background capability contributes a prompt");
        // Interactive hosts detach and get woken; one-shot runs must block
        // instead because `-p` exits before any wake can be delivered.
        assert!(prompt.contains("spawn_background"));
        assert!(prompt.contains("end the turn"));
        assert!(prompt.contains("wait_task"));
        assert!(prompt.contains("cancel any monitor whose purpose is satisfied"));
        assert!(prompt.contains("disarmed: true"));
    }

    #[tokio::test]
    async fn system_prompt_surfaces_active_monitor_obligations() {
        let session_id = SessionId::from_seed(42);
        let task = everruns_core::session_task::new_session_task(
            CreateSessionTask {
                session_id,
                id: Some("task_scheduled_check".into()),
                kind: TASK_KIND_MONITOR.into(),
                display_name: "scheduled check".into(),
                spec: serde_json::json!({}),
                state: everruns_core::session_task::SessionTaskState::Running,
                links: Default::default(),
                wake_policy: everruns_core::session_task::TaskWakePolicy::Silent,
            },
            chrono::Utc::now(),
        );
        let capability = BackgroundCapability {
            session_id,
            task_registry: Arc::new(StubRegistry { tasks: vec![task] }),
        };
        let ctx = SystemPromptContext::without_file_store(session_id);

        let prompt = capability
            .system_prompt_contribution(&ctx)
            .await
            .expect("background capability contributes a prompt");

        assert!(prompt.contains("Active scheduled monitor obligations"));
        assert!(prompt.contains("task_scheduled_check"));
        assert!(prompt.contains("Reconcile each"));
    }
}

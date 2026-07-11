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
    spawned task with `wait_task` instead of ending the turn.\n\
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
        Some(BACKGROUND_SYSTEM_PROMPT.to_string())
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

    /// The prompt contribution never touches the registry.
    struct StubRegistry;

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
            Ok(Vec::new())
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
            task_registry: Arc::new(StubRegistry),
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
    }
}

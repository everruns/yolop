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
use everruns_core::capabilities::{Capability, CapabilityStatus};
use everruns_core::command::{
    CommandDescriptor, CommandExecutionContext, CommandResult, CommandSource, ExecuteCommandRequest,
};
use everruns_core::session_task::SessionTaskRegistry;
use everruns_core::typed_id::SessionId;
use std::sync::Arc;

pub(crate) const BACKGROUND_CAPABILITY_ID: &str = "background";

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

//! `/worktree` — session git worktree status and auto-activation control.

use crate::worktree::WorktreeManager;
use async_trait::async_trait;
use everruns_core::capabilities::{Capability, CapabilityStatus};
use everruns_core::command::{
    CommandArg, CommandDescriptor, CommandExecutionContext, CommandResult, CommandSource,
    ExecuteCommandRequest,
};
use std::sync::Arc;

pub(crate) const WORKTREE_CAPABILITY_ID: &str = "yolop_worktree";
const WORKTREE_COMMAND_NAME: &str = "worktree";

pub(crate) struct WorktreeCapability {
    pub(crate) manager: Arc<WorktreeManager>,
}

#[async_trait]
impl Capability for WorktreeCapability {
    fn id(&self) -> &str {
        WORKTREE_CAPABILITY_ID
    }

    fn name(&self) -> &str {
        "Worktree"
    }

    fn description(&self) -> &str {
        "Show session git worktree status or disable auto-activation."
    }

    fn status(&self) -> CapabilityStatus {
        CapabilityStatus::Available
    }

    fn category(&self) -> Option<&str> {
        Some("System")
    }

    fn commands(&self) -> Vec<CommandDescriptor> {
        vec![CommandDescriptor {
            name: WORKTREE_COMMAND_NAME.to_string(),
            description: "Show worktree status, or pass `off` to disable auto-activation."
                .to_string(),
            source: CommandSource::System,
            args: vec![CommandArg {
                name: "action".to_string(),
                description: "`off` disables auto worktree creation for future turns.".to_string(),
                required: false,
                suggestions: vec!["off".to_string()],
            }],
        }]
    }

    async fn execute_command(
        &self,
        request: &ExecuteCommandRequest,
        _ctx: &CommandExecutionContext,
    ) -> everruns_core::Result<CommandResult> {
        if request.name != WORKTREE_COMMAND_NAME {
            return Err(everruns_core::AgentLoopError::config(format!(
                "{} cannot execute /{}",
                self.id(),
                request.name
            )));
        }

        let arg = request.arguments.as_deref().unwrap_or("").trim();
        let message = if arg.eq_ignore_ascii_case("off") {
            self.manager.disable_auto();
            "worktrees: auto activation disabled for this session (already-active worktrees \
             are unchanged)"
                .to_string()
        } else if arg.is_empty() {
            self.manager.status_message()
        } else {
            return Err(everruns_core::AgentLoopError::config(format!(
                "unknown /worktree argument `{arg}`; use `/worktree` or `/worktree off`"
            )));
        };

        Ok(CommandResult {
            success: true,
            message,
            error_code: None,
            error_fields: None,
        })
    }
}

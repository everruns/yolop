//! `/goal` — autonomous multi-turn work toward a verifiable completion condition.
//!
//! After each agent turn, a separate tool-less evaluator model reads the
//! transcript and decides whether the condition holds. If not, the host starts
//! another turn automatically.

use crate::session_state::goal::{
    GOAL_CAPABILITY_ID, GOAL_COMMAND_NAME, GoalCommandOutcome, GoalStore, evaluate_active_goal,
    evaluation_result_message, format_status, is_goal_evaluate_request,
};
use async_trait::async_trait;
use everruns_core::command::{
    CommandArg, CommandDescriptor, CommandExecutionContext, CommandResult, CommandSource,
    ExecuteCommandRequest,
};
use everruns_core::{Capability, CapabilityStatus};
use std::sync::Arc;

pub(crate) struct GoalCapability {
    pub(crate) store: Arc<GoalStore>,
}

#[async_trait]
impl Capability for GoalCapability {
    fn id(&self) -> &str {
        GOAL_CAPABILITY_ID
    }

    fn name(&self) -> &str {
        "Goal"
    }

    fn description(&self) -> &str {
        "Keep working across turns until a completion condition is met."
    }

    fn status(&self) -> CapabilityStatus {
        CapabilityStatus::Available
    }

    fn icon(&self) -> Option<&str> {
        Some("target")
    }

    fn category(&self) -> Option<&str> {
        Some("System")
    }

    fn commands(&self) -> Vec<CommandDescriptor> {
        vec![CommandDescriptor {
            name: GOAL_COMMAND_NAME.to_string(),
            description: "Set a completion condition and keep working until it is met.".to_string(),
            source: CommandSource::System,
            args: vec![CommandArg {
                name: "condition".to_string(),
                description:
                    "Verifiable end state, `pause`/`resume`, `clear` to stop, or omit for status."
                        .to_string(),
                required: false,
                suggestions: vec![],
            }],
        }]
    }

    async fn execute_command(
        &self,
        request: &ExecuteCommandRequest,
        ctx: &CommandExecutionContext,
    ) -> everruns_provider::error::Result<CommandResult> {
        if request.name != GOAL_COMMAND_NAME {
            return Err(everruns_provider::error::AgentLoopError::config(format!(
                "{} cannot execute /{}",
                self.id(),
                request.name
            )));
        }

        if is_goal_evaluate_request(request) {
            let condition = self.store.active_condition(ctx.session_id).ok_or_else(|| {
                everruns_provider::error::AgentLoopError::config("no active goal to evaluate")
            })?;
            let evaluation = evaluate_active_goal(ctx, &condition).await?;
            self.store
                .record_evaluation(ctx.session_id, &evaluation)
                .map_err(|err| everruns_provider::error::AgentLoopError::config(err.to_string()))?;
            return Ok(CommandResult {
                success: true,
                message: evaluation_result_message(&evaluation),
                error_code: None,
                error_fields: None,
            });
        }

        let outcome = GoalStore::parse_user_args(request.arguments.as_deref())
            .map_err(|err| everruns_provider::error::AgentLoopError::config(err.to_string()))?;

        if let GoalCommandOutcome::Status(_) = &outcome {
            let status = self.store.status(ctx.session_id, None);
            return Ok(CommandResult {
                success: true,
                message: format_status(&status),
                error_code: None,
                error_fields: None,
            });
        }

        let message = self
            .store
            .apply_outcome(ctx.session_id, outcome)
            .map_err(|err| everruns_provider::error::AgentLoopError::config(err.to_string()))?;
        Ok(CommandResult {
            success: true,
            message,
            error_code: None,
            error_fields: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_state::goal::GOAL_EVALUATE_ARG;
    use everruns_core::{
        CommandHost, CommandTurnContext, SessionCompletion, SessionCompletionError,
    };
    use everruns_provider::typed_id::SessionId;
    use std::sync::Mutex;

    struct FakeHost {
        completion: Mutex<String>,
    }

    #[async_trait]
    impl CommandHost for FakeHost {
        async fn turn_context(&self) -> everruns_provider::error::Result<CommandTurnContext> {
            let session_id = SessionId::new();
            Ok(CommandTurnContext {
                session_id,
                messages: vec![everruns_core::message::Message::user(
                    "ran cargo test and all tests passed",
                )],
                system_prompt: "system".into(),
                model: "test-model".into(),
                provider_type: "llmsim".into(),
                resolved_locale: None,
            })
        }

        async fn completion(
            &self,
            _request: everruns_core::command_host::SessionCompletionRequest,
        ) -> std::result::Result<SessionCompletion, SessionCompletionError> {
            Ok(SessionCompletion {
                text: self.completion.lock().expect("lock").clone(),
            })
        }
    }

    #[tokio::test]
    async fn goal_evaluate_marks_goal_achieved() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Arc::new(GoalStore::open(dir.path().to_path_buf()));
        let session_id = SessionId::new();
        store
            .set_active(session_id, "all tests pass".into())
            .expect("set");

        let capability = GoalCapability {
            store: store.clone(),
        };
        let host = Arc::new(FakeHost {
            completion: Mutex::new(
                r#"{"met": true, "reason": "tests passed in transcript"}"#.into(),
            ),
        });
        let ctx = CommandExecutionContext::new(session_id, host);
        let result = capability
            .execute_command(
                &ExecuteCommandRequest {
                    name: GOAL_COMMAND_NAME.to_string(),
                    arguments: Some(GOAL_EVALUATE_ARG.to_string()),
                    controls: None,
                },
                &ctx,
            )
            .await
            .expect("evaluate");
        assert!(result.success);
        let evaluation =
            crate::session_state::goal::parse_evaluation_response(&result.message).expect("parse");
        assert!(evaluation.met);
        assert!(!store.is_active(session_id));
    }
}

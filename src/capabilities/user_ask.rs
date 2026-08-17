//! Experimental `yolop_user_ask` — track the user's request and validate it
//! after each turn.
//!
//! Independent of `/goal`: records what the user wants, allows updates when they
//! pivot, and runs a tool-less evaluator at turn end. Hosts own bounded continuation.

use crate::capabilities::narration::stable_labeled;
use crate::session_state::user_ask::{
    USER_ASK_CAPABILITY_ID, USER_ASK_COMMAND_NAME, UserAskCommandOutcome, UserAskStore,
    evaluate_active_user_ask, evaluation_result_message, format_status,
    is_user_ask_evaluate_request, system_prompt_block,
};
use async_trait::async_trait;
use everruns_core::command::{
    CommandArg, CommandDescriptor, CommandExecutionContext, CommandResult, CommandSource,
    ExecuteCommandRequest,
};
use everruns_core::tool_narration::{ToolNarrationPhase, arg_str, truncate};
use everruns_core::{Capability, CapabilityStatus, SystemPromptContext};
use everruns_core::{Tool, ToolExecutionResult};
use everruns_provider::ToolCall;
use everruns_provider::typed_id::SessionId;
use serde_json::{Value, json};
use std::sync::Arc;

pub(crate) struct UserAskCapability {
    pub(crate) store: Arc<UserAskStore>,
    pub(crate) session_id: SessionId,
}

#[async_trait]
impl Capability for UserAskCapability {
    fn id(&self) -> &str {
        USER_ASK_CAPABILITY_ID
    }

    fn name(&self) -> &str {
        "User ask (experimental)"
    }

    fn description(&self) -> &str {
        "Experimental request tracking across turns with end-of-turn completion evaluation."
    }

    fn status(&self) -> CapabilityStatus {
        CapabilityStatus::Available
    }

    fn icon(&self) -> Option<&str> {
        Some("message-circle-question")
    }

    fn category(&self) -> Option<&str> {
        Some("System")
    }

    fn commands(&self) -> Vec<CommandDescriptor> {
        vec![CommandDescriptor {
            name: USER_ASK_COMMAND_NAME.to_string(),
            description: "Set or inspect the tracked user request.".to_string(),
            source: CommandSource::System,
            args: vec![CommandArg {
                name: "ask".to_string(),
                description: "What the user wants, `clear` to stop tracking, or omit for status."
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
        if request.name != USER_ASK_COMMAND_NAME {
            return Err(everruns_provider::error::AgentLoopError::config(format!(
                "{} cannot execute /{}",
                self.id(),
                request.name
            )));
        }

        if is_user_ask_evaluate_request(request) {
            let ask = self.store.active_text(ctx.session_id).ok_or_else(|| {
                everruns_provider::error::AgentLoopError::config("no active user ask to evaluate")
            })?;
            let evaluation = evaluate_active_user_ask(ctx, &ask).await?;
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

        let outcome = UserAskStore::parse_user_args(request.arguments.as_deref())
            .map_err(|err| everruns_provider::error::AgentLoopError::config(err.to_string()))?;

        if let UserAskCommandOutcome::Status(_) = &outcome {
            let status = self.store.status(ctx.session_id);
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

    async fn system_prompt_contribution(&self, _ctx: &SystemPromptContext) -> Option<String> {
        let block = system_prompt_block(&self.store.status(self.session_id));
        (!block.is_empty()).then_some(block)
    }

    fn system_prompt_preview(&self) -> Option<String> {
        None
    }

    fn tools(&self) -> Vec<Box<dyn Tool>> {
        vec![
            Box::new(SetUserAskTool {
                store: self.store.clone(),
                session_id: self.session_id,
            }),
            Box::new(ClearUserAskTool {
                store: self.store.clone(),
                session_id: self.session_id,
            }),
        ]
    }
}

struct SetUserAskTool {
    store: Arc<UserAskStore>,
    session_id: SessionId,
}

#[async_trait]
impl Tool for SetUserAskTool {
    fn narrate(
        &self,
        tool_call: &ToolCall,
        phase: ToolNarrationPhase,
        locale: Option<&str>,
        _ctx: everruns_core::tool_narration::ToolNarrationContext<'_>,
    ) -> Option<String> {
        let _ = locale;
        let ask = arg_str(&tool_call.arguments, &["ask", "text", "request"])
            .map(|value| truncate(value, 48));
        Some(stable_labeled("Set user ask", ask, phase))
    }

    fn name(&self) -> &str {
        "set_user_ask"
    }

    fn display_name(&self) -> Option<&str> {
        Some("Set user ask")
    }

    fn description(&self) -> &str {
        "Record or replace the user's current request. Use when the user states what they want \
         or changes direction. Keep the summary concise and faithful to their wording."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "ask": {
                    "type": "string",
                    "description": "Concise summary of what the user is asking for."
                }
            },
            "required": ["ask"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, arguments: Value) -> ToolExecutionResult {
        let ask = match arguments.get("ask").and_then(Value::as_str) {
            Some(value) => value.trim(),
            None => return ToolExecutionResult::tool_error("'ask' is required"),
        };
        if ask.is_empty() {
            return ToolExecutionResult::tool_error("'ask' must not be empty");
        }
        match self.store.set_ask(self.session_id, ask.to_string()) {
            Ok(()) => ToolExecutionResult::success(json!({
                "ok": true,
                "ask": ask,
                "message": format!("user ask recorded: {ask}"),
            })),
            Err(err) => ToolExecutionResult::tool_error(err.to_string()),
        }
    }
}

struct ClearUserAskTool {
    store: Arc<UserAskStore>,
    session_id: SessionId,
}

#[async_trait]
impl Tool for ClearUserAskTool {
    fn narrate(
        &self,
        _tool_call: &ToolCall,
        phase: ToolNarrationPhase,
        locale: Option<&str>,
        _ctx: everruns_core::tool_narration::ToolNarrationContext<'_>,
    ) -> Option<String> {
        let _ = locale;
        Some(stable_labeled("Clear user ask", None, phase))
    }

    fn name(&self) -> &str {
        "clear_user_ask"
    }

    fn display_name(&self) -> Option<&str> {
        Some("Clear user ask")
    }

    fn description(&self) -> &str {
        "Stop tracking the current user request. Use only when the user explicitly abandons \
         or withdraws what they were asking for."
    }

    fn parameters_schema(&self) -> Value {
        json!({ "type": "object", "properties": {}, "additionalProperties": false })
    }

    async fn execute(&self, _arguments: Value) -> ToolExecutionResult {
        self.store.clear_active(self.session_id);
        ToolExecutionResult::success(json!({
            "ok": true,
            "message": "user ask cleared",
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_state::user_ask::USER_ASK_EVALUATE_ARG;
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
                    "I upgraded the dependency and ran tests",
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
    async fn user_ask_evaluate_marks_achieved() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Arc::new(UserAskStore::open(dir.path().to_path_buf()));
        let session_id = SessionId::new();
        store
            .set_ask(session_id, "upgrade the dependency".into())
            .expect("set");

        let capability = UserAskCapability {
            store: store.clone(),
            session_id,
        };
        let host = Arc::new(FakeHost {
            completion: Mutex::new(
                r#"{"outcome": "achieved", "reason": "dependency upgraded in transcript"}"#.into(),
            ),
        });
        let ctx = CommandExecutionContext::new(session_id, host);
        let result = capability
            .execute_command(
                &ExecuteCommandRequest {
                    name: USER_ASK_COMMAND_NAME.to_string(),
                    arguments: Some(USER_ASK_EVALUATE_ARG.to_string()),
                    controls: None,
                },
                &ctx,
            )
            .await
            .expect("evaluate");
        assert!(result.success);
        let evaluation = crate::session_state::user_ask::parse_evaluation_response(&result.message)
            .expect("parse");
        assert_eq!(
            evaluation.outcome,
            crate::session_state::user_ask::AskOutcome::Achieved
        );
        assert!(!store.is_active(session_id));
    }
}

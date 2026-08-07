//! Yolop policy around Everruns' shared end-of-turn completion gate.

pub(crate) use everruns_core::turn_completion::{
    CompletionState, ContinuationBudget as CompletionBudget, GateDecision,
};
pub(crate) const CONTINUATION_TAG: &str = "automatic_task_continuation";
pub(crate) const CONTINUATION_METADATA_KEY: &str = "yolop.task_continuation";

pub(crate) fn tag_continuation(
    mut input: everruns_core::message_retriever::InputMessage,
) -> everruns_core::message_retriever::InputMessage {
    input.tags.push(CONTINUATION_TAG.to_string());
    input.metadata.get_or_insert_default().insert(
        CONTINUATION_METADATA_KEY.to_string(),
        serde_json::Value::Bool(true),
    );
    input
}

pub(crate) fn gate_turn(
    result: &everruns_runtime::TurnResult,
    has_active_background: bool,
) -> GateDecision {
    everruns_core::turn_completion::gate_turn(&everruns_core::turn_completion::TurnSummary {
        success: result.success,
        stop_reason: result.stop_reason,
        response: &result.response,
        tool_calls_count: result.tool_calls_count,
        has_active_background,
    })
}

pub(crate) fn continuation_prompt(reason: &str) -> String {
    let reason = reason.trim();
    if reason.is_empty() {
        "[automatic] Continue the active user request from the compact conversation state. Make concrete progress and provide exactly one final answer when finished.".to_string()
    } else {
        format!(
            "[automatic] Continue the active user request from the compact conversation state. {reason} Make concrete progress and provide exactly one final answer when finished."
        )
    }
}

pub(crate) fn evaluation_for_state(
    state: CompletionState,
) -> crate::session_state::user_ask::UserAskEvaluation {
    use crate::session_state::user_ask::AskOutcome;
    let (outcome, reason) = match state {
        CompletionState::Achieved => (AskOutcome::Achieved, "final answer delivered"),
        CompletionState::Blocked => (
            AskOutcome::Blocked,
            "turn was cancelled or needs user input",
        ),
        CompletionState::Failed => (AskOutcome::Failed, "turn ended with a permanent failure"),
        CompletionState::WaitingOnBackground => (
            AskOutcome::WaitingOnBackground,
            "detached work is still running",
        ),
        CompletionState::InProgress => {
            (AskOutcome::InProgress, "turn ended without a final answer")
        }
    };
    crate::session_state::user_ask::UserAskEvaluation {
        outcome,
        reason: reason.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use everruns_core::turn::TurnStopReason;
    use everruns_core::typed_id::TurnId;

    fn result(response: &str, tools: usize, success: bool) -> everruns_runtime::TurnResult {
        everruns_runtime::TurnResult {
            response: response.to_string(),
            iterations: 1,
            tool_calls_count: tools,
            success,
            error: (!success).then(|| "permanent provider error".to_string()),
            stop_reason: if success {
                TurnStopReason::EndTurn
            } else {
                TurnStopReason::Error
            },
            turn_id: TurnId::new(),
        }
    }

    #[test]
    fn trivial_final_is_achieved_without_evaluator() {
        assert_eq!(
            gate_turn(&result("hello", 0, true), false),
            GateDecision::Conclusive(CompletionState::Achieved)
        );
    }

    #[test]
    fn tool_only_turn_continues_unless_background_is_running() {
        assert_eq!(
            gate_turn(&result("", 1, true), false),
            GateDecision::Conclusive(CompletionState::InProgress)
        );
        assert_eq!(
            gate_turn(&result("", 1, true), true),
            GateDecision::Conclusive(CompletionState::WaitingOnBackground)
        );
    }

    #[test]
    fn tool_using_candidate_final_warrants_semantic_evaluation() {
        assert_eq!(
            gate_turn(&result("done", 1, true), false),
            GateDecision::Evaluate
        );
    }

    #[test]
    fn permanent_failure_never_continues() {
        assert_eq!(
            gate_turn(&result("", 0, false), false),
            GateDecision::Conclusive(CompletionState::Failed)
        );
    }

    #[test]
    fn budget_is_strict_for_turns_and_tokens() {
        let mut budget = CompletionBudget::default();
        for _ in 0..everruns_core::turn_completion::DEFAULT_MAX_CONTINUATION_TURNS {
            assert!(budget.observe_turn(1));
        }
        assert!(!budget.observe_turn(1));

        budget.reset();
        let over_limit = everruns_core::turn_completion::DEFAULT_MAX_CONTINUATION_TOKENS + 1;
        assert!(!budget.observe_turn(over_limit));
        assert_eq!(budget.usage(), (1, over_limit));
    }
}

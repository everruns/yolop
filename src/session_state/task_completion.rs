//! Cheap end-of-turn completion gate shared by TUI, print, and ACP hosts.

use std::time::{Duration, Instant};

use everruns_core::turn::TurnStopReason;

pub(crate) const MAX_TASK_TURNS: u32 = 6;
pub(crate) const MAX_TASK_TOKENS: u64 = 64_000;
pub(crate) const MAX_TASK_ELAPSED: Duration = Duration::from_secs(10 * 60);
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CompletionState {
    Achieved,
    Blocked,
    Failed,
    WaitingOnBackground,
    InProgress,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GateDecision {
    Conclusive(CompletionState),
    /// Tool-using work produced a candidate final answer. Mutations and
    /// multi-step work are ambiguous enough to justify semantic evaluation.
    Evaluate,
}

#[derive(Clone, Debug)]
pub(crate) struct CompletionBudget {
    started: Instant,
    turns: u32,
    tokens: u64,
}

impl Default for CompletionBudget {
    fn default() -> Self {
        Self {
            started: Instant::now(),
            turns: 0,
            tokens: 0,
        }
    }
}

impl CompletionBudget {
    pub(crate) fn reset(&mut self) {
        *self = Self::default();
    }

    pub(crate) fn observe_turn(&mut self, tokens: u64) -> bool {
        self.turns = self.turns.saturating_add(1);
        self.tokens = self.tokens.saturating_add(tokens);
        self.turns <= MAX_TASK_TURNS
            && self.tokens <= MAX_TASK_TOKENS
            && self.started.elapsed() <= MAX_TASK_ELAPSED
    }

    #[cfg(test)]
    pub(crate) fn usage(&self) -> (u32, u64) {
        (self.turns, self.tokens)
    }
}

pub(crate) fn gate_turn(
    result: &everruns_runtime::TurnResult,
    has_active_background: bool,
) -> GateDecision {
    if !result.success {
        return GateDecision::Conclusive(match result.stop_reason {
            TurnStopReason::Cancelled => CompletionState::Blocked,
            _ => CompletionState::Failed,
        });
    }

    match result.stop_reason {
        TurnStopReason::Error | TurnStopReason::Refusal => {
            return GateDecision::Conclusive(CompletionState::Failed);
        }
        TurnStopReason::Cancelled => {
            return GateDecision::Conclusive(CompletionState::Blocked);
        }
        TurnStopReason::MaxTokens | TurnStopReason::MaxTurnRequests => {
            return GateDecision::Conclusive(CompletionState::InProgress);
        }
        TurnStopReason::EndTurn => {}
    }

    if has_active_background && result.tool_calls_count > 0 {
        return GateDecision::Conclusive(CompletionState::WaitingOnBackground);
    }

    if result.response.trim().is_empty() {
        return GateDecision::Conclusive(CompletionState::InProgress);
    }

    let normalized = result.response.trim().to_ascii_lowercase();
    if normalized.ends_with('?')
        && ["need", "which", "what", "could you", "please provide"]
            .iter()
            .any(|marker| normalized.contains(marker))
    {
        return GateDecision::Conclusive(CompletionState::Blocked);
    }

    if result.tool_calls_count == 0 {
        GateDecision::Conclusive(CompletionState::Achieved)
    } else {
        GateDecision::Evaluate
    }
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
        for _ in 0..MAX_TASK_TURNS {
            assert!(budget.observe_turn(1));
        }
        assert!(!budget.observe_turn(1));

        budget.reset();
        assert!(!budget.observe_turn(MAX_TASK_TOKENS + 1));
        assert_eq!(budget.usage(), (1, MAX_TASK_TOKENS + 1));
    }
}

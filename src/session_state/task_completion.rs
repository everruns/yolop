//! Yolop's side of the engine completion check.
//!
//! The agent is built with `CompletionCheck::Always` (see `runtime::build`):
//! the engine reviews every finishing turn on the agent's own model and, when
//! the work is unfinished, adds a labeled continuation inside the same turn,
//! bounded by the user's `continuation_max_*` settings. Hosts no longer run a
//! review or loop turns themselves; they read the engine's
//! `turn.completion_checked` events to tell the person what happened.
use everruns_core::events::{
    CompletionCheckAction, CompletionVerdict, Event, EventData, TurnCompletionCheckedData,
};

pub(crate) const CONTINUATION_TAG: &str = "automatic_task_continuation";
pub(crate) const CONTINUATION_METADATA_KEY: &str = "yolop.task_continuation";

pub(crate) fn has_pending_execution(tasks: &[everruns_core::SessionTask]) -> bool {
    use everruns_core::session_task::{SessionTaskState, TASK_KIND_MONITOR};
    // Monitors remain running between future checks. AwaitingInput requires
    // outside action. Neither should keep a host waiting on the request.
    tasks.iter().any(|task| {
        task.kind != TASK_KIND_MONITOR
            && matches!(
                task.state,
                SessionTaskState::Queued | SessionTaskState::Running
            )
    })
}

/// Mark a host-written follow-up (a background wake fallback, a resume after
/// an exhausted budget) as automatic rather than typed by the person.
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

/// The last completion check the engine recorded for `turn_id`.
pub(crate) fn last_check<'a>(
    events: &'a [Event],
    turn_id: &str,
) -> Option<&'a TurnCompletionCheckedData> {
    events.iter().rev().find_map(|event| match &event.data {
        EventData::TurnCompletionChecked(data) if data.turn_id.to_string() == turn_id => Some(data),
        _ => None,
    })
}

/// The review found the turn waiting on something only the person can give.
pub(crate) fn is_blocked(check: &TurnCompletionCheckedData) -> bool {
    check.verdict == Some(CompletionVerdict::Blocked)
}

/// Whether the finished turn waits on the person: the engine's review said
/// so, or (no review ran) the engine's own gate saw the answer end on a
/// question, which it settles without a review or an event. A turn whose
/// detached work is still running waits on that work, not on the person.
pub(crate) fn ended_blocked(events: &[Event], turn: &everruns::Turn, background: bool) -> bool {
    use everruns_core::turn_completion::{CompletionState, GateDecision, TurnSummary, gate_turn};
    match last_check(events, &turn.turn_id) {
        Some(check) => is_blocked(check),
        None => {
            !background
                && turn.success
                && gate_turn(&TurnSummary {
                    success: turn.success,
                    stop_reason: turn.stop_reason,
                    response: &turn.response,
                    tool_calls_count: turn.tool_calls,
                    has_active_background: false,
                }) == GateDecision::Conclusive(CompletionState::Blocked)
        }
    }
}

/// What to tell the person about a check, if anything. An achieved or
/// blocked turn needs no notice: its own answer says it.
pub(crate) fn notice(check: &TurnCompletionCheckedData) -> Option<String> {
    let reason = check.reason.trim();
    match check.action {
        CompletionCheckAction::Finished => None,
        CompletionCheckAction::Continued => Some(format!(
            "↻ Not finished yet: {reason} Continuing automatically ({}/{}).",
            check.continuations_used, check.max_continuations
        )),
        CompletionCheckAction::BudgetExhausted => Some(format!(
            "Automatic continuation budget exhausted after {} continuation{} ({reason}). Send a message to resume with a fresh budget.",
            check.continuations_used,
            if check.continuations_used == 1 {
                ""
            } else {
                "s"
            },
        )),
        CompletionCheckAction::ReviewFailed => {
            Some(format!("Completion review unavailable: {reason}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(
        verdict: Option<CompletionVerdict>,
        action: CompletionCheckAction,
        continuations_used: u32,
    ) -> TurnCompletionCheckedData {
        TurnCompletionCheckedData {
            turn_id: everruns_contracts::typed_id::TurnId::new(),
            verdict,
            reason: "tests still fail.".into(),
            action,
            continuations_used,
            max_continuations: 5,
            tokens_used: 0,
            elapsed_ms: 0,
            continuation_message_id: None,
        }
    }

    #[test]
    fn notices_name_what_the_engine_did() {
        let finished = check(
            Some(CompletionVerdict::Achieved),
            CompletionCheckAction::Finished,
            0,
        );
        assert_eq!(notice(&finished), None);
        assert!(!is_blocked(&finished));

        let continued = check(
            Some(CompletionVerdict::InProgress),
            CompletionCheckAction::Continued,
            2,
        );
        assert_eq!(
            notice(&continued).unwrap(),
            "↻ Not finished yet: tests still fail. Continuing automatically (2/5)."
        );

        let exhausted = check(
            Some(CompletionVerdict::InProgress),
            CompletionCheckAction::BudgetExhausted,
            5,
        );
        assert!(
            notice(&exhausted)
                .unwrap()
                .contains("after 5 continuations")
        );

        let failed = check(None, CompletionCheckAction::ReviewFailed, 0);
        assert!(
            notice(&failed)
                .unwrap()
                .starts_with("Completion review unavailable")
        );

        let blocked = check(
            Some(CompletionVerdict::Blocked),
            CompletionCheckAction::Finished,
            0,
        );
        assert!(is_blocked(&blocked));
        assert_eq!(notice(&blocked), None);
    }

    #[test]
    fn a_question_blocks_only_when_no_detached_work_runs() {
        let turn: everruns::Turn = everruns_core::host::TurnResult {
            response: "Which file should I edit?".into(),
            iterations: 1,
            tool_calls_count: 1,
            success: true,
            error: None,
            stop_reason: everruns_contracts::turn::TurnStopReason::EndTurn,
            turn_id: everruns_contracts::typed_id::TurnId::new(),
        }
        .into();
        assert!(ended_blocked(&[], &turn, false));
        assert!(!ended_blocked(&[], &turn, true));
    }
}

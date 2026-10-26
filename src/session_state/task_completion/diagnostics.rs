//! Host completion diagnostics use the existing budget event protocol and session log.
use super::{CompletionController, CompletionDecision, CompletionState, bounded};
use crate::config::Settings;
use crate::runtime::RuntimeHandles;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use everruns_contracts::typed_id::TurnId;
use everruns_core::events::{BudgetEventData, EventData};
use everruns_core::{EventContext, EventEmitter, EventRequest};
use serde::Serialize;
use serde_json::json;

pub(crate) const METADATA_KEY: &str = "yolop.task_completion";
const BUDGET_ID: &str = "yolop.automatic_continuation";

#[derive(Serialize)]
pub(super) struct PendingReset {
    #[serde(rename = "reset_at")]
    pub ts: DateTime<Utc>,
    pub previous_turn_id: TurnId,
    #[serde(rename = "previous_used_turns")]
    pub used_turns: u64,
    #[serde(rename = "previous_used_tokens")]
    pub used_tokens: u64,
    #[serde(rename = "previous_elapsed_ms")]
    pub elapsed_ms: u64,
}

#[derive(Serialize)]
pub(super) struct Diagnostic {
    pub action: &'static str,
    state: &'static str,
    reason: String,
    used_turns: u64,
    used_tokens: u64,
    elapsed_ms: u64,
    max_turns: u64,
    max_tokens: u64,
    max_seconds: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_stop_reason: Option<everruns_core::host::TurnStopReason>,
    #[serde(skip_serializing_if = "Option::is_none")]
    exhausted_limit: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_continuation: Option<u64>,
    #[serde(flatten)]
    reset: Option<PendingReset>,
}

impl CompletionController {
    pub(super) fn elapsed_ms(&self) -> u64 {
        self.repair_started.map_or(0, |started| {
            u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
        })
    }

    pub(super) fn diagnostic(
        &self,
        settings: &Settings,
        used_turns: u64,
        state: &'static str,
        reason: &str,
    ) -> Diagnostic {
        Diagnostic {
            action: "stop",
            state,
            reason: bounded(reason, 2000),
            used_turns,
            used_tokens: self.tokens,
            elapsed_ms: self.elapsed_ms(),
            max_turns: settings.continuation_max_turns,
            max_tokens: settings.continuation_max_tokens,
            max_seconds: settings.continuation_max_seconds,
            turn_stop_reason: None,
            exhausted_limit: None,
            next_continuation: None,
            reset: None,
        }
    }

    pub(super) async fn record_reset(
        &mut self,
        handles: &RuntimeHandles,
        turn_id: TurnId,
        settings: &Settings,
    ) -> Result<()> {
        let Some(reset) = self.pending_reset.take() else {
            return Ok(());
        };
        let mut diagnostic = self.diagnostic(
            settings,
            0,
            "in_progress",
            "new human message reset the continuation budget",
        );
        diagnostic.action = "reset";
        diagnostic.reset = Some(reset);
        emit(handles, turn_id, diagnostic).await
    }

    pub(super) async fn record_decision(
        &self,
        handles: &RuntimeHandles,
        result: &everruns_core::host::TurnResult,
        settings: &Settings,
        used_turns: u64,
        reason: &str,
        decision: &CompletionDecision,
    ) -> Result<()> {
        let state = match decision.state {
            CompletionState::Achieved => "achieved",
            CompletionState::InProgress => "in_progress",
            CompletionState::Blocked => "blocked",
            CompletionState::Failed => "failed",
            CompletionState::WaitingOnBackground => "waiting_on_background",
        };
        let mut diagnostic = self.diagnostic(settings, used_turns, state, reason);
        diagnostic.turn_stop_reason = Some(result.stop_reason);
        diagnostic.action = if decision.notice.is_some() {
            "exhausted"
        } else if decision.followup.is_some() {
            "continue"
        } else {
            "stop"
        };
        if decision.notice.is_some() {
            diagnostic.exhausted_limit = Some(if used_turns >= settings.continuation_max_turns {
                "turns"
            } else if self.tokens >= settings.continuation_max_tokens {
                "tokens"
            } else {
                "time"
            });
        }
        if decision.followup.is_some() {
            diagnostic.next_continuation = Some(self.repairs);
        }
        emit(handles, result.turn_id, diagnostic).await
    }
}

pub(super) async fn emit(
    handles: &RuntimeHandles,
    turn_id: TurnId,
    diagnostic: Diagnostic,
) -> Result<()> {
    let currency = diagnostic.exhausted_limit.unwrap_or("turns");
    let (used, limit) = match currency {
        "tokens" => (diagnostic.used_tokens as f64, diagnostic.max_tokens as f64),
        "time" => (
            diagnostic.elapsed_ms as f64 / 1000.0,
            diagnostic.max_seconds as f64,
        ),
        _ => (diagnostic.used_turns as f64, diagnostic.max_turns as f64),
    };
    let data = BudgetEventData {
        budget_id: BUDGET_ID.to_string(),
        balance: (limit - used).max(0.0),
        limit,
        currency: if currency == "time" {
            "seconds"
        } else {
            currency
        }
        .to_string(),
        message: Some(diagnostic.reason.clone()),
        soft_limit: None,
    };
    // Paused means the host stopped spending this recovery budget, including
    // achieved work. `state` distinguishes completion, outside input, failure,
    // and background waiting; consumers must not infer task state from budget state.
    let data = match diagnostic.action {
        "continue" => EventData::BudgetWarning(data),
        "exhausted" => EventData::BudgetExhausted(data),
        "reset" => EventData::BudgetResumed(data),
        _ => EventData::BudgetPaused(data),
    };
    let context = EventContext {
        turn_id: Some(turn_id),
        parent_span_id: Some(turn_id.to_string()),
        ..EventContext::empty()
    };
    let request = EventRequest::new(handles.session_id, context, data)
        .with_metadata(json!({ METADATA_KEY: diagnostic }));
    handles
        .events
        .emit(request)
        .await
        .context("persist completion diagnostic")?;
    Ok(())
}

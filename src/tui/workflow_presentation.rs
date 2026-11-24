//! Product semantics for workflow views, independent of the terminal renderer.

use super::presentation::PresentationState;
use super::review::file_display;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct WorkflowPresentation {
    pub title: String,
    pub focus_hint: String,
    pub subtitle: String,
    pub query: String,
    pub query_label: String,
    pub rows: Vec<String>,
    pub selected: usize,
    pub detail: String,
    pub footer: String,
    pub loading: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum ExecutionPhase {
    #[default]
    Thinking,
    Running,
    Waiting,
    Compacting,
    Responding,
    Retrying,
    WaitingForCredits,
}

impl ExecutionPhase {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Thinking => "Thinking",
            Self::Running => "Running",
            Self::Waiting => "Waiting for client",
            Self::Compacting => "Compacting context",
            Self::Responding => "Writing response",
            Self::Retrying => "Retrying",
            Self::WaitingForCredits => "Waiting for credits",
        }
    }
}

pub(super) fn execution_summary(
    phase: ExecutionPhase,
    operation: &str,
    background: usize,
) -> String {
    let operation = operation.trim();
    let mut text = phase.label().to_string();
    if !operation.is_empty() && !operation.eq_ignore_ascii_case(phase.label()) {
        text.push_str(" · ");
        text.push_str(operation);
    }
    if background > 0 {
        text.push_str(&format!(" · {background} background"));
    }
    text
}

pub(super) fn usage_detail(state: &PresentationState) -> String {
    let occupancy = match (state.context_used_tokens, state.context_window_tokens) {
        (Some(used), Some(limit)) if limit > 0 => format!(
            "{used} / {limit} tokens ({}%)",
            u64::from(used) * 100 / u64::from(limit)
        ),
        (Some(used), _) => format!("{used} tokens; model capacity unknown"),
        _ => "unknown, awaiting provider usage".into(),
    };
    let total = state
        .session_tokens
        .map(|n| n.to_string())
        .unwrap_or_else(|| "unknown".into());
    let threshold = state
        .compaction_budget_percent
        .map(|n| format!("{n}% of model capacity"))
        .unwrap_or_else(|| "unknown".into());
    format!(
        "Context occupancy: {occupancy}\nLatest request input, not cumulative usage.\n\nSession tokens: {total}\nCumulative reported input and output for this process.\n\nProactive compaction threshold: {threshold}\nCompaction reduces the model's live context. Full recorded history remains available through session search and rewind.\n\nMonetary cost: unknown, not reported by this host\nSubscription allowance: unknown, not reported by this provider interface\n\nModel: {}\nProvider: {}",
        state.model_id, state.provider_name
    )
}

pub(super) fn approval_detail(request: &crate::sandbox_approval::ApprovalRequest) -> String {
    use crate::sandbox_approval::ApprovalScope;
    let (scope, consequence) = match request.scope {
        ApprovalScope::FullAccess => (
            "Full host access",
            "Runs outside sandbox containment with access to your machine.",
        ),
        ApprovalScope::Sandboxed => (
            "Active sandbox",
            "Runs inside the configured sandbox boundary.",
        ),
        ApprovalScope::Memory => ("Durable memory", "Changes memory retained across sessions."),
    };
    format!(
        "Action\n{}\n\nReason\n{}\n\nPermission scope: {scope}\n{consequence}\n\nAllow once: this request only.\nAllow for session: ALL later requests in this permission scope, until this session process exits.\nDeny: this request will not proceed.",
        file_display(&request.command),
        file_display(&request.reason)
    )
}

pub(super) fn recovery_detail(error: &str) -> String {
    format!(
        "{}\n\nContinue asks the agent to inspect current state and completed work before retrying. It does not blindly replay the failed command.\nYour composer draft is preserved.\nNo automatic retry is scheduled by this view.",
        file_display(error)
    )
}

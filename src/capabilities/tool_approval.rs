//! Hard tool-approval gate — the enforcement half of yolop's approval levels.
//!
//! Where [`approval`](super::approval) *asks the model* to pause for spoken
//! consent (prompt engineering), this capability *blocks the tool* until a host
//! actually approves it. It contributes a native [`PreToolUseHook`] that, for
//! tools the current [`ApprovalMode`] deems risky, calls out to a
//! [`ToolApprover`] (the ACP client's `session/request_permission`) and turns
//! the answer into a `Continue`/`Block` decision. The turn genuinely suspends
//! until the approver responds.
//!
//! Only hosts that can service an interactive prompt register it: the ACP
//! server wires a client-backed [`ToolApprover`]; the TUI and `--print` leave
//! it off and keep the soft-approval guidance alone.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use everruns_core::atoms::{PreToolUseDecision, PreToolUseHook};
use everruns_core::capabilities::{Capability, CapabilityStatus};
use everruns_core::tool_types::{ToolCall, ToolDefinition};
use everruns_core::traits::ToolContext;
use everruns_core::typed_id::SessionId;

use crate::config_service::ConfigService;
use crate::settings::ApprovalMode;

pub(crate) const TOOL_APPROVAL_CAPABILITY_ID: &str = "yolop_tool_approval";

/// A host that can interactively approve or reject a tool call. The ACP server
/// backs this with `session/request_permission`; other hosts do not register
/// the gate at all.
#[async_trait]
pub trait ToolApprover: Send + Sync {
    /// Ask the host to approve `tool_call`. Blocks the turn until it answers.
    async fn approve(
        &self,
        session_id: SessionId,
        tool_call: &ToolCall,
        tool_def: &ToolDefinition,
    ) -> ApprovalDecision;
}

/// A host's answer to an approval request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalDecision {
    /// Allow this call.
    Allow,
    /// Allow this call and stop asking for this tool this session.
    AllowAlways,
    /// Reject this call.
    Reject,
    /// Reject this call and keep rejecting this tool this session.
    RejectAlways,
    /// The turn was cancelled while the request was pending.
    Cancelled,
    /// The host could not be asked (unsupported or transport error). The gate
    /// falls back to allowing, so a client without a permission UI keeps working
    /// autonomously rather than deadlocking every mutating tool.
    Unavailable,
}

/// How risky a tool is, derived from the runtime's [`ToolHints`] annotations
/// rather than by guessing from names.
///
/// [`ToolHints`]: everruns_core::tool_types::ToolHints
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolRisk {
    /// Declares `readonly` — never gated.
    ReadOnly,
    /// Changes state but is not flagged destructive/outward (writes, edits, or
    /// an un-annotated tool: fail safe by treating unknown as mutating).
    Mutating,
    /// Declares `destructive` or `open_world` (deletes, force-push, publish,
    /// network side effects).
    Destructive,
}

fn classify(tool_def: &ToolDefinition) -> ToolRisk {
    let hints = tool_def.hints();
    if hints.readonly == Some(true) {
        ToolRisk::ReadOnly
    } else if hints.destructive == Some(true) || hints.open_world == Some(true) {
        ToolRisk::Destructive
    } else {
        ToolRisk::Mutating
    }
}

/// Whether a tool of the given risk needs approval at the given level. Mirrors
/// the soft-approval prose in [`super::approval`]: `off` never asks, `normal`
/// asks before destructive/outward actions, `protective` asks before any
/// state change.
fn requires_approval(mode: ApprovalMode, risk: ToolRisk) -> bool {
    match mode {
        ApprovalMode::Off => false,
        ApprovalMode::Normal => matches!(risk, ToolRisk::Destructive),
        ApprovalMode::Protective => !matches!(risk, ToolRisk::ReadOnly),
    }
}

/// Registers the [`ToolApprovalHook`]. Holds the hook so a single cache (of
/// "always" answers) is shared across the session's turns.
pub struct ToolApprovalCapability {
    hook: Arc<ToolApprovalHook>,
}

impl ToolApprovalCapability {
    pub fn new(approver: Arc<dyn ToolApprover>, config: Arc<dyn ConfigService>) -> Self {
        Self {
            hook: Arc::new(ToolApprovalHook {
                approver,
                config,
                remembered: Mutex::new(HashMap::new()),
            }),
        }
    }
}

#[async_trait]
impl Capability for ToolApprovalCapability {
    fn id(&self) -> &str {
        TOOL_APPROVAL_CAPABILITY_ID
    }
    fn name(&self) -> &str {
        "Tool Approval Gate"
    }
    fn description(&self) -> &str {
        "Blocks risky tools behind an interactive host approval, tuned by the approval level."
    }
    fn status(&self) -> CapabilityStatus {
        CapabilityStatus::Available
    }
    fn category(&self) -> Option<&str> {
        Some("Safety")
    }

    fn pre_tool_use_hooks(&self) -> Vec<Arc<dyn PreToolUseHook>> {
        vec![self.hook.clone()]
    }
}

struct ToolApprovalHook {
    approver: Arc<dyn ToolApprover>,
    /// Read live each call so a `session/set_mode` (or `/setup approval`) takes
    /// effect on the very next tool.
    config: Arc<dyn ConfigService>,
    /// "Allow always" / "reject always" answers, keyed by (session, tool name).
    /// `true` = remembered allow, `false` = remembered reject.
    remembered: Mutex<HashMap<(SessionId, String), bool>>,
}

impl ToolApprovalHook {
    fn block(tool_call: ToolCall, reason: &str) -> PreToolUseDecision {
        PreToolUseDecision::Block {
            reason: reason.to_string(),
            user_message: Some(format!("Denied `{}` — {reason}.", tool_call.name)),
            tool_call,
        }
    }
}

#[async_trait]
impl PreToolUseHook for ToolApprovalHook {
    async fn before_exec(
        &self,
        tool_call: ToolCall,
        tool_def: &ToolDefinition,
        context: &ToolContext,
    ) -> PreToolUseDecision {
        let mode = self.config.approval_mode();
        if !requires_approval(mode, classify(tool_def)) {
            return PreToolUseDecision::Continue(tool_call);
        }

        let key = (context.session_id, tool_call.name.clone());
        if let Some(&allowed) = self.remembered.lock().unwrap().get(&key) {
            return if allowed {
                PreToolUseDecision::Continue(tool_call)
            } else {
                Self::block(tool_call, "rejected earlier this session")
            };
        }

        match self
            .approver
            .approve(context.session_id, &tool_call, tool_def)
            .await
        {
            ApprovalDecision::Allow | ApprovalDecision::Unavailable => {
                PreToolUseDecision::Continue(tool_call)
            }
            ApprovalDecision::AllowAlways => {
                self.remembered.lock().unwrap().insert(key, true);
                PreToolUseDecision::Continue(tool_call)
            }
            ApprovalDecision::Reject => Self::block(tool_call, "rejected by user"),
            ApprovalDecision::RejectAlways => {
                self.remembered.lock().unwrap().insert(key, false);
                Self::block(tool_call, "rejected by user")
            }
            ApprovalDecision::Cancelled => Self::block(tool_call, "turn cancelled"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use everruns_core::tool_types::{BuiltinTool, ToolHints};
    use serde_json::json;

    fn tool_with(hints: ToolHints) -> ToolDefinition {
        ToolDefinition::Builtin(BuiltinTool {
            name: "t".to_string(),
            display_name: None,
            description: String::new(),
            parameters: json!({}),
            policy: Default::default(),
            category: None,
            deferrable: Default::default(),
            hints,
            full_parameters: None,
        })
    }

    #[test]
    fn classify_reads_hints() {
        assert_eq!(
            classify(&tool_with(ToolHints {
                readonly: Some(true),
                ..Default::default()
            })),
            ToolRisk::ReadOnly
        );
        assert_eq!(
            classify(&tool_with(ToolHints {
                destructive: Some(true),
                ..Default::default()
            })),
            ToolRisk::Destructive
        );
        assert_eq!(
            classify(&tool_with(ToolHints {
                open_world: Some(true),
                ..Default::default()
            })),
            ToolRisk::Destructive
        );
        // Un-annotated tools fail safe as mutating (gated in protective).
        assert_eq!(
            classify(&tool_with(ToolHints::default())),
            ToolRisk::Mutating
        );
        // `readonly` wins over `open_world`: a read-only network tool (e.g. web
        // search, which sets both) is never gated, not treated as outward.
        assert_eq!(
            classify(&tool_with(ToolHints {
                readonly: Some(true),
                open_world: Some(true),
                ..Default::default()
            })),
            ToolRisk::ReadOnly
        );
    }

    #[test]
    fn policy_matches_approval_semantics() {
        // Off never gates.
        for risk in [
            ToolRisk::ReadOnly,
            ToolRisk::Mutating,
            ToolRisk::Destructive,
        ] {
            assert!(!requires_approval(ApprovalMode::Off, risk));
        }
        // Normal gates destructive/outward only.
        assert!(!requires_approval(ApprovalMode::Normal, ToolRisk::ReadOnly));
        assert!(!requires_approval(ApprovalMode::Normal, ToolRisk::Mutating));
        assert!(requires_approval(
            ApprovalMode::Normal,
            ToolRisk::Destructive
        ));
        // Protective gates anything that changes state.
        assert!(!requires_approval(
            ApprovalMode::Protective,
            ToolRisk::ReadOnly
        ));
        assert!(requires_approval(
            ApprovalMode::Protective,
            ToolRisk::Mutating
        ));
        assert!(requires_approval(
            ApprovalMode::Protective,
            ToolRisk::Destructive
        ));
    }
}

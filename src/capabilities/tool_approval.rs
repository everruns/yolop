//! Live-config adapter for the upstream interactive tool-approval gate.
//!
//! `everruns-core` owns risk classification, remembered answers, and approval
//! decisions. Yolop adds one host concern: `approval_mode` is mutable central
//! configuration, so the upstream hook must receive a fresh config for every
//! call instead of capturing the session-start value.

use std::sync::Arc;

use async_trait::async_trait;
use everruns_builtins::tool_approval::ToolApprovalCapability as UpstreamToolApprovalCapability;
use everruns_core::ToolContext;
use everruns_core::tool_hooks::{PreToolUseDecision, PreToolUseHook};
use everruns_core::{Capability, CapabilityStatus};
use everruns_provider::{ToolCall, ToolDefinition};

use crate::config::service::ConfigService;

pub(crate) use everruns_builtins::TOOL_APPROVAL_CAPABILITY_ID;
pub(crate) use everruns_builtins::{ApprovalDecision, ToolApprover};

/// Delegates approval policy to everruns-core while resolving Yolop's live mode.
pub struct ToolApprovalCapability {
    upstream: Arc<UpstreamToolApprovalCapability>,
    config: Arc<dyn ConfigService>,
}

impl ToolApprovalCapability {
    pub fn new(approver: Arc<dyn ToolApprover>, config: Arc<dyn ConfigService>) -> Self {
        Self {
            upstream: Arc::new(UpstreamToolApprovalCapability::new(approver)),
            config,
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
        "Blocks risky tools behind an interactive host approval, tuned by the live approval mode."
    }

    fn status(&self) -> CapabilityStatus {
        CapabilityStatus::Available
    }

    fn category(&self) -> Option<&str> {
        Some("Safety")
    }

    fn is_guardrail(&self) -> bool {
        true
    }

    fn pre_tool_use_hooks(&self) -> Vec<Arc<dyn PreToolUseHook>> {
        vec![Arc::new(LiveApprovalHook {
            upstream: self.upstream.clone(),
            config: self.config.clone(),
        })]
    }
}

struct LiveApprovalHook {
    upstream: Arc<UpstreamToolApprovalCapability>,
    config: Arc<dyn ConfigService>,
}

#[async_trait]
impl PreToolUseHook for LiveApprovalHook {
    async fn before_exec(
        &self,
        tool_call: ToolCall,
        tool_def: &ToolDefinition,
        context: &ToolContext,
    ) -> PreToolUseDecision {
        let config = serde_json::json!({ "mode": self.config.approval_mode().as_str() });
        let hook = self
            .upstream
            .pre_tool_use_hooks_with_config(&config)
            .into_iter()
            .next()
            .expect("upstream tool approval capability always contributes one hook");
        hook.before_exec(tool_call, tool_def, context).await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use everruns_provider::typed_id::SessionId;
    use everruns_provider::{BuiltinTool, ToolHints};
    use serde_json::json;

    use super::*;
    use crate::config::{ApprovalMode, SettingsStore};

    struct RejectingApprover {
        asked: AtomicUsize,
    }

    #[async_trait]
    impl ToolApprover for RejectingApprover {
        async fn approve(
            &self,
            _session_id: SessionId,
            _tool_call: &ToolCall,
            _tool_def: &ToolDefinition,
        ) -> ApprovalDecision {
            self.asked.fetch_add(1, Ordering::Relaxed);
            ApprovalDecision::Reject
        }
    }

    fn destructive_tool() -> ToolDefinition {
        ToolDefinition::Builtin(BuiltinTool {
            name: "publish".to_string(),
            display_name: None,
            description: String::new(),
            parameters: json!({}),
            policy: Default::default(),
            category: None,
            deferrable: Default::default(),
            hints: ToolHints::default().with_destructive(true),
            full_parameters: None,
        })
    }

    fn call(id: &str) -> ToolCall {
        ToolCall {
            id: id.to_string(),
            name: "publish".to_string(),
            arguments: json!({}),
        }
    }

    #[tokio::test]
    async fn delegates_policy_to_upstream_and_reads_mode_live() {
        let temp = tempfile::tempdir().unwrap();
        let settings = Arc::new(SettingsStore::open(temp.path().join("settings.toml")));
        let approver = Arc::new(RejectingApprover {
            asked: AtomicUsize::new(0),
        });
        let capability = ToolApprovalCapability::new(approver.clone(), settings.clone());
        let hook = capability.pre_tool_use_hooks().pop().unwrap();
        let context = ToolContext::new(SessionId::new());

        assert!(matches!(
            hook.before_exec(call("call-1"), &destructive_tool(), &context)
                .await,
            PreToolUseDecision::Block { .. }
        ));
        assert_eq!(approver.asked.load(Ordering::Relaxed), 1);

        settings.set_approval_mode(ApprovalMode::Off).unwrap();
        assert!(matches!(
            hook.before_exec(call("call-2"), &destructive_tool(), &context)
                .await,
            PreToolUseDecision::Continue(_)
        ));
        assert_eq!(approver.asked.load(Ordering::Relaxed), 1);
    }
}

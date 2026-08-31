//! Live-config adapter for the upstream interactive tool-approval gate.
//!
//! `everruns-core` owns risk classification, remembered answers, and approval
//! decisions. Yolop adds one host concern: `approval_mode` is mutable central
//! configuration, so the upstream hook must receive a fresh config for every
//! call instead of capturing the session-start value.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use everruns_builtins::tool_approval::ToolApprovalCapability as UpstreamToolApprovalCapability;
use everruns_core::ToolContext;
use everruns_core::tool_hooks::{PreToolUseDecision, PreToolUseHook};
use everruns_core::{Capability, CapabilityStatus};
use everruns_provider::typed_id::SessionId;
use everruns_provider::{ToolCall, ToolDefinition};

use crate::config::ApprovalMode;
use crate::config::service::ConfigService;
use crate::exec::shell_policy::requires_destructive_approval;

pub(crate) use everruns_builtins::TOOL_APPROVAL_CAPABILITY_ID;
pub(crate) use everruns_builtins::{ApprovalDecision, ToolApprover};

/// Delegates approval policy to everruns-core while resolving Yolop's live mode.
pub struct ToolApprovalCapability {
    upstream: Arc<UpstreamToolApprovalCapability>,
    approver: Arc<dyn ToolApprover>,
    config: Arc<dyn ConfigService>,
    critical_remembered: Arc<Mutex<HashMap<(SessionId, String), bool>>>,
}

impl ToolApprovalCapability {
    pub fn new(approver: Arc<dyn ToolApprover>, config: Arc<dyn ConfigService>) -> Self {
        Self {
            upstream: Arc::new(UpstreamToolApprovalCapability::new(approver.clone())),
            approver,
            config,
            critical_remembered: Arc::new(Mutex::new(HashMap::new())),
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
            approver: self.approver.clone(),
            config: self.config.clone(),
            critical_remembered: self.critical_remembered.clone(),
        })]
    }
}

struct LiveApprovalHook {
    upstream: Arc<UpstreamToolApprovalCapability>,
    approver: Arc<dyn ToolApprover>,
    config: Arc<dyn ConfigService>,
    critical_remembered: Arc<Mutex<HashMap<(SessionId, String), bool>>>,
}

impl LiveApprovalHook {
    fn block(tool_call: ToolCall, reason: &str) -> PreToolUseDecision {
        PreToolUseDecision::Block {
            reason: reason.to_string(),
            user_message: Some(format!("Denied `{}`: {reason}.", tool_call.name)),
            tool_call,
        }
    }
}

#[async_trait]
impl PreToolUseHook for LiveApprovalHook {
    async fn before_exec(
        &self,
        tool_call: ToolCall,
        tool_def: &ToolDefinition,
        context: &ToolContext,
    ) -> PreToolUseDecision {
        let mode = self.config.approval_mode();
        let critical_command = if tool_call.name == "bash" {
            tool_call
                .arguments
                .get("command")
                .and_then(serde_json::Value::as_str)
                .filter(|command| requires_destructive_approval(command))
        } else {
            None
        };
        if mode != ApprovalMode::Off
            && let Some(command) = critical_command
        {
            let key = (context.session_id, command.to_string());
            if let Some(&allowed) = self.critical_remembered.lock().unwrap().get(&key) {
                return if allowed {
                    PreToolUseDecision::Continue(tool_call)
                } else {
                    Self::block(tool_call, "this exact command was rejected earlier")
                };
            }

            let mut effective_tool_def = tool_def.clone();
            match &mut effective_tool_def {
                ToolDefinition::Builtin(tool) => tool.hints.destructive = Some(true),
                ToolDefinition::ClientSide(tool) => tool.hints.destructive = Some(true),
            }
            return match self
                .approver
                .approve(context.session_id, &tool_call, &effective_tool_def)
                .await
            {
                ApprovalDecision::Allow => PreToolUseDecision::Continue(tool_call),
                ApprovalDecision::AllowAlways => {
                    self.critical_remembered.lock().unwrap().insert(key, true);
                    PreToolUseDecision::Continue(tool_call)
                }
                ApprovalDecision::Reject => Self::block(tool_call, "rejected by user"),
                ApprovalDecision::RejectAlways => {
                    self.critical_remembered.lock().unwrap().insert(key, false);
                    Self::block(tool_call, "rejected by user")
                }
                ApprovalDecision::Cancelled => Self::block(tool_call, "turn cancelled"),
                ApprovalDecision::Unavailable => Self::block(tool_call, "approval UI unavailable"),
            };
        }

        let config = serde_json::json!({ "mode": mode.as_str() });
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

    struct RememberingApprover {
        asked: AtomicUsize,
    }

    #[async_trait]
    impl ToolApprover for RememberingApprover {
        async fn approve(
            &self,
            _session_id: SessionId,
            _tool_call: &ToolCall,
            _tool_def: &ToolDefinition,
        ) -> ApprovalDecision {
            self.asked.fetch_add(1, Ordering::Relaxed);
            ApprovalDecision::AllowAlways
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

    fn bash_call(id: &str, command: &str) -> ToolCall {
        ToolCall {
            id: id.to_string(),
            name: "bash".to_string(),
            arguments: json!({ "command": command }),
        }
    }

    fn bash_tool() -> ToolDefinition {
        ToolDefinition::Builtin(BuiltinTool {
            name: "bash".to_string(),
            display_name: None,
            description: String::new(),
            parameters: json!({}),
            policy: Default::default(),
            category: None,
            deferrable: Default::default(),
            hints: ToolHints::default(),
            full_parameters: None,
        })
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

    #[tokio::test]
    async fn normal_mode_gates_process_control_and_nested_agent_commands() {
        let temp = tempfile::tempdir().unwrap();
        let settings = Arc::new(SettingsStore::open(temp.path().join("settings.toml")));
        let approver = Arc::new(RejectingApprover {
            asked: AtomicUsize::new(0),
        });
        let capability = ToolApprovalCapability::new(approver.clone(), settings);
        let hook = capability.pre_tool_use_hooks().pop().unwrap();
        let context = ToolContext::new(SessionId::new());

        for (index, command) in [
            "kill 1234",
            "pkill -f worker",
            "codex exec --full-auto 'finish the task'",
            "claude -p 'finish the task'",
        ]
        .into_iter()
        .enumerate()
        {
            assert!(matches!(
                hook.before_exec(
                    bash_call(&format!("call-{index}"), command),
                    &bash_tool(),
                    &context
                )
                .await,
                PreToolUseDecision::Block { .. }
            ));
        }
        assert_eq!(approver.asked.load(Ordering::Relaxed), 4);

        assert!(matches!(
            hook.before_exec(bash_call("safe", "cargo test"), &bash_tool(), &context)
                .await,
            PreToolUseDecision::Continue(_)
        ));
        assert_eq!(approver.asked.load(Ordering::Relaxed), 4);
    }

    #[tokio::test]
    async fn critical_approval_is_remembered_only_for_the_exact_command() {
        let temp = tempfile::tempdir().unwrap();
        let settings = Arc::new(SettingsStore::open(temp.path().join("settings.toml")));
        let approver = Arc::new(RememberingApprover {
            asked: AtomicUsize::new(0),
        });
        let capability = ToolApprovalCapability::new(approver.clone(), settings);
        let hook = capability.pre_tool_use_hooks().pop().unwrap();
        let context = ToolContext::new(SessionId::new());

        for (id, command) in [
            ("first", "kill 1234"),
            ("same", "kill 1234"),
            ("different", "kill 5678"),
        ] {
            assert!(matches!(
                hook.before_exec(bash_call(id, command), &bash_tool(), &context)
                    .await,
                PreToolUseDecision::Continue(_)
            ));
        }

        assert_eq!(approver.asked.load(Ordering::Relaxed), 2);
    }
}

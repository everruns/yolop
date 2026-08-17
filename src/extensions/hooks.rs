//! Hook adapters: turn an extension's manifest hook subscriptions into
//! runtime `PreToolUseHook`/`PostToolExecHook`s that fire `hook/fire` on the
//! extension's warm server. These run for *every* tool the agent calls, so
//! each is gated by a tool-name glob and bounded by a per-hook timeout and an
//! `on_error` policy. Precedent: `user_hooks` spawns a whole bash process per
//! event; an RPC to an already-warm process is strictly cheaper.

use super::manager::ExtensionProcess;
use super::package::{HookEvent, HookOnError, HookSubscription};
use async_trait::async_trait;
use everruns_core::ToolContext;
use everruns_core::tool_hooks::{PostToolExecHook, PostToolExecHookPriority};
use everruns_core::tool_hooks::{PreToolUseDecision, PreToolUseHook};
use everruns_provider::{ToolCall, ToolDefinition, ToolResult};
use std::sync::Arc;
use std::time::Duration;

/// Match a tool name against a manifest glob (`*` wildcards; `"*"` = all).
fn glob_match(pattern: &str, name: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    // Split on `*` and require the literal segments to appear in order,
    // anchored at both ends. No escaping — tool names are `[a-z_]`-ish.
    let parts: Vec<&str> = pattern.split('*').collect();
    let mut cursor = 0usize;
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        match name[cursor..].find(part) {
            Some(pos) => {
                let abs = cursor + pos;
                // First segment must anchor at the start (no leading `*`).
                if i == 0 && abs != 0 {
                    return false;
                }
                cursor = abs + part.len();
            }
            None => return false,
        }
    }
    // Last segment must anchor at the end (no trailing `*`).
    parts
        .last()
        .is_none_or(|last| last.is_empty() || name.ends_with(last))
}

pub struct ExtensionPreHook {
    pub ext_name: String,
    pub process: Arc<ExtensionProcess>,
    pub sub: HookSubscription,
}

#[async_trait]
impl PreToolUseHook for ExtensionPreHook {
    async fn before_exec(
        &self,
        tool_call: ToolCall,
        _tool_def: &ToolDefinition,
        _context: &ToolContext,
    ) -> PreToolUseDecision {
        if self.sub.event != HookEvent::PreToolUse
            || !glob_match(&self.sub.tool_name_glob, &tool_call.name)
        {
            return PreToolUseDecision::Continue(tool_call);
        }
        let fire = self
            .process
            .fire_hook("pre_tool_use", &tool_call.name, &tool_call.arguments);
        // Reduce the timeout outcome to either a decision or a failure detail.
        let result: Result<super::protocol::HookDecision, String> =
            match tokio::time::timeout(Duration::from_millis(self.sub.timeout_ms), fire).await {
                Ok(Ok(decision)) => Ok(decision),
                Ok(Err(err)) => Err(err.to_string()),
                Err(_) => Err("timed out".to_string()),
            };
        match result {
            Ok(decision) if decision.block => PreToolUseDecision::Block {
                reason: if decision.reason.is_empty() {
                    format!("blocked by extension `{}`", self.ext_name)
                } else {
                    decision.reason
                },
                user_message: decision.user_message,
                tool_call,
            },
            Ok(_) => PreToolUseDecision::Continue(tool_call),
            Err(detail) => match self.sub.on_error {
                HookOnError::Block => PreToolUseDecision::Block {
                    reason: format!(
                        "extension `{}` hook failed ({detail}); set to block on error",
                        self.ext_name
                    ),
                    user_message: None,
                    tool_call,
                },
                HookOnError::Warn => {
                    tracing::warn!(
                        target: "yolop::ext", ext = %self.ext_name,
                        "pre_tool_use hook failed ({detail}); allowing (`on_error = warn`)"
                    );
                    PreToolUseDecision::Continue(tool_call)
                }
            },
        }
    }
}

pub struct ExtensionPostHook {
    pub ext_name: String,
    pub process: Arc<ExtensionProcess>,
    pub sub: HookSubscription,
}

#[async_trait]
impl PostToolExecHook for ExtensionPostHook {
    fn priority(&self) -> PostToolExecHookPriority {
        PostToolExecHookPriority::Normal
    }

    async fn after_exec(
        &self,
        tool_call: &ToolCall,
        _tool_def: &ToolDefinition,
        _result: &mut ToolResult,
        _context: &ToolContext,
    ) {
        if self.sub.event != HookEvent::PostToolUse
            || !glob_match(&self.sub.tool_name_glob, &tool_call.name)
        {
            return;
        }
        // Observe-only in this phase: the decision (result rewriting) is a
        // later refinement; a post hook that errors is logged, never fatal.
        let fire = self
            .process
            .fire_hook("post_tool_use", &tool_call.name, &tool_call.arguments);
        if let Ok(Err(err)) =
            tokio::time::timeout(Duration::from_millis(self.sub.timeout_ms), fire).await
        {
            tracing::warn!(
                target: "yolop::ext", ext = %self.ext_name,
                "post_tool_use hook failed: {err}"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_matching() {
        assert!(glob_match("*", "anything"));
        assert!(glob_match("bash", "bash"));
        assert!(!glob_match("bash", "bashx"));
        assert!(glob_match("lsp_*", "lsp_rename"));
        assert!(!glob_match("lsp_*", "grep_files"));
        assert!(glob_match("*_file", "write_file"));
        assert!(glob_match("edit*file", "edit_file"));
        assert!(!glob_match("edit*file", "read_file"));
    }
}

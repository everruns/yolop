//! Approval as ordinary turn middleware.
//!
//! Everything that decides whether a call runs lives here: the hint lookup,
//! the prompt, and the denial message the model sees. `before_tool` blocks the
//! turn while it awaits the answer, so there is no approval state machine and
//! no `PendingApproval` turn phase — which is exactly the shape agentyk's
//! design notes predict.
//!
//! The classification comes from `ToolDefinition.metadata`, reachable on the
//! invocation, so a call is gated on what the tool declared rather than on its
//! name. A call whose tool the engine could not resolve, or whose definition
//! carries no hints, is gated: unclassified is not the same as safe.
//!
//! One limit is a finding rather than an oversight: the prompt reads stdin
//! directly, because the invocation carries no host channel to route a
//! question through. A UI-driven host has to smuggle its own channel in
//! through `ToolContext::extensions` or capture it in the middleware, as this
//! one does with its mode.

use std::io::{IsTerminal, Write};

use agentyk::{ToolCallDecision, ToolInvocation, TurnMiddleware};
use async_trait::async_trait;

use super::tools::is_readonly;

/// How the backend answers an approval request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalMode {
    /// Ask on stdin (interactive runs on a terminal).
    Prompt,
    /// Approve without asking — one-shot runs have nobody to ask.
    Auto,
}

pub struct ApprovalMiddleware {
    mode: ApprovalMode,
}

impl ApprovalMiddleware {
    pub fn new(mode: ApprovalMode) -> Self {
        Self { mode }
    }
}

#[async_trait]
impl TurnMiddleware for ApprovalMiddleware {
    fn name(&self) -> &str {
        "yolop-approval"
    }

    async fn before_tool(&self, invocation: &ToolInvocation<'_>) -> ToolCallDecision {
        if self.mode == ApprovalMode::Auto {
            return ToolCallDecision::Proceed;
        }
        // agentyk's own filesystem tools carry no hints, so the read-only ones
        // are named here; anything else must declare itself.
        let read_only = invocation.definition.is_some_and(is_readonly)
            || matches!(
                invocation.call.name.as_str(),
                "read_file" | "list_directory"
            );
        if read_only {
            return ToolCallDecision::Proceed;
        }
        let summary = summarize(&invocation.call.name, &invocation.call.arguments);
        match ask(&summary) {
            true => ToolCallDecision::Proceed,
            false => ToolCallDecision::Deny {
                reason: "the operator declined this call".to_string(),
            },
        }
    }
}

/// One line describing the call, for a human deciding in a hurry.
fn summarize(name: &str, arguments: &serde_json::Value) -> String {
    let detail = match name {
        "bash" => arguments["command"].as_str().map(str::to_string),
        "write_file" | "delete_file" | "edit_file" => {
            arguments["path"].as_str().map(str::to_string)
        }
        _ => None,
    };
    match detail {
        Some(detail) => format!("{name}: {detail}"),
        None => format!("{name}: {arguments}"),
    }
}

fn ask(summary: &str) -> bool {
    let stdin = std::io::stdin();
    if !stdin.is_terminal() {
        return false;
    }
    // Blocking reads inside async middleware are acceptable only because the
    // whole turn is stopped waiting for this answer anyway.
    print!("\napprove? {summary}\n[y/N] ");
    std::io::stdout().flush().ok();
    let mut answer = String::new();
    if stdin.read_line(&mut answer).is_err() {
        return false;
    }
    matches!(answer.trim(), "y" | "Y" | "yes")
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentyk::{SessionId, ToolCall, ToolContext, ToolDefinition, TurnId};
    use serde_json::json;

    fn call(name: &str, arguments: serde_json::Value) -> ToolCall {
        ToolCall {
            id: "1".into(),
            name: name.into(),
            arguments,
        }
    }

    #[tokio::test]
    async fn auto_mode_never_prompts() {
        let middleware = ApprovalMiddleware::new(ApprovalMode::Auto);
        let call = call("bash", json!({"command": "rm -rf /"}));
        let context = ToolContext::new(SessionId::new(), TurnId::new());
        let invocation = ToolInvocation::new(&call, None, &context);
        assert!(matches!(
            middleware.before_tool(&invocation).await,
            ToolCallDecision::Proceed
        ));
    }

    #[tokio::test]
    async fn a_read_only_hint_bypasses_the_gate() {
        let middleware = ApprovalMiddleware::new(ApprovalMode::Prompt);
        let call = call("grep_files", json!({"pattern": "fn main"}));
        let definition = ToolDefinition::new("grep_files", "", json!({}))
            .with_metadata(json!({"hints": {"readonly": true}}));
        let context = ToolContext::new(SessionId::new(), TurnId::new());
        let invocation = ToolInvocation::new(&call, Some(&definition), &context);
        assert!(matches!(
            middleware.before_tool(&invocation).await,
            ToolCallDecision::Proceed
        ));
    }

    #[tokio::test]
    async fn an_unclassified_call_without_a_terminal_is_denied() {
        // Tests do not run on a tty, so this exercises the fail-closed path.
        let middleware = ApprovalMiddleware::new(ApprovalMode::Prompt);
        let call = call("bash", json!({"command": "ls"}));
        let context = ToolContext::new(SessionId::new(), TurnId::new());
        let invocation = ToolInvocation::new(&call, None, &context);
        assert!(matches!(
            middleware.before_tool(&invocation).await,
            ToolCallDecision::Deny { .. }
        ));
    }

    #[test]
    fn summaries_name_the_thing_being_touched() {
        assert_eq!(
            summarize("bash", &json!({"command": "cargo test"})),
            "bash: cargo test"
        );
        assert_eq!(
            summarize("edit_file", &json!({"path": "src/main.rs"})),
            "edit_file: src/main.rs"
        );
    }
}

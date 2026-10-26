// Reveal-gated system-prompt contributions.
//
// `tool_search` hides the parameter schemas of deferrable tools until the model
// asks for them (knowledge/specs/tool-search.md). Capability prompt blocks had
// no equivalent: a capability's prose rode every turn even when its tools were
// deliberately cold, so we paid to explain how to call tools whose schemas were
// hidden. This module closes that gap.
//
// The rule, applied by callers rather than enforced here:
//
// - *Discovery* text, "this capability exists", "these memories are stored",
//   stays ungated. Gating it would be circular: the model cannot ask to reveal a
//   tool it has no reason to know about.
// - *How-to* text, argument shapes, call sequencing, where files live, is gated
//   on a reveal, because until the schema is loaded the model cannot act on it
//   anyway, and once it is loaded the tool description carries it.
//
// The revealed set comes from the engine: context assembly reads `tool_search`
// results back from the transcript into `SystemPromptContext::tool_disclosure`.
// It survives a resume and cannot drift from what the model was shown, which a
// process-local registry fed by a post-exec hook could not promise.
//
// A gated block must add its how-to as its own paragraph (separated by a blank
// line) and leave the discovery text byte-identical. The engine records a
// prompt change as a note when only new paragraphs appear, and keeps the cached
// prefix; a changed paragraph forces the whole prompt to be resent.

use everruns_contracts::runtime::capabilities::ToolDisclosure;

/// Whether any of `tools` has had its full schema loaded this session.
///
/// Callers pass the tools whose how-to their block explains; the block is worth
/// its tokens once at least one of them is callable with real arguments. A
/// revealed entry ending in `__` covers every tool with that prefix (a whole MCP
/// server).
pub(crate) fn any_revealed(disclosure: &ToolDisclosure, tools: &[&str]) -> bool {
    disclosure.revealed.iter().any(|revealed| {
        tools.iter().any(|tool| {
            revealed == tool || (revealed.ends_with("__") && tool.starts_with(revealed.as_str()))
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn disclosure(revealed: &[&str]) -> ToolDisclosure {
        ToolDisclosure {
            deferring: true,
            revealed: revealed.iter().map(|name| name.to_string()).collect(),
            failed: Default::default(),
            reveal_order: Default::default(),
        }
    }

    #[test]
    fn nothing_is_revealed_before_the_first_search() {
        assert!(!any_revealed(&ToolDisclosure::default(), &["get_config"]));
    }

    #[test]
    fn any_listed_tool_being_revealed_is_enough() {
        let revealed = disclosure(&["get_config", "set_config"]);
        assert!(any_revealed(&revealed, &["get_config"]));
        assert!(any_revealed(&revealed, &["recall", "set_config"]));
        assert!(!any_revealed(&revealed, &["remember"]));
    }

    #[test]
    fn a_server_prefix_reveals_every_tool_of_that_server() {
        let revealed = disclosure(&["mcp_github__"]);
        assert!(any_revealed(&revealed, &["mcp_github__create_issue"]));
        assert!(!any_revealed(&revealed, &["mcp_linear__save_issue"]));
        assert!(
            !any_revealed(&disclosure(&["mcp_github"]), &["mcp_github__create_issue"]),
            "only an entry ending in `__` is a prefix"
        );
    }
}

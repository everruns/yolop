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
// - *Discovery* text — "this capability exists", "these memories are stored" —
//   stays ungated. Gating it would be circular: the model cannot ask to reveal a
//   tool it has no reason to know about.
// - *How-to* text — argument shapes, call sequencing, where files live — is
//   gated on a reveal, because until the schema is loaded the model cannot act
//   on it anyway, and once it is loaded the tool description carries it.
//
// The revealed set is read from `tool_search`'s own structured `loaded` field
// rather than mirrored from upstream state, so it cannot drift from what the
// model was actually shown.

use async_trait::async_trait;
use everruns_builtins::TOOL_SEARCH_TOOL_NAME;
use everruns_core::ToolContext;
use everruns_core::tool_hooks::{PostToolExecHook, PostToolExecHookPriority};
use everruns_core::{Capability, CapabilityStatus};
use everruns_provider::typed_id::SessionId;
use everruns_provider::{ToolCall, ToolDefinition, ToolResult};
use serde_json::Value;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};

pub(crate) const TOOL_REVEAL_CAPABILITY_ID: &str = "yolop_tool_reveal";

/// Upper bound on tracked sessions. The registry is process-wide (one harness
/// serves every session), so without eviction a long-lived process would grow
/// without bound. Mirrors the bound upstream `tool_search` puts on its own
/// reveal registry.
const MAX_TRACKED_SESSIONS: usize = 256;

/// Session-keyed set of tool names revealed via `tool_search` this process.
#[derive(Default)]
pub(crate) struct RevealedTools {
    inner: Mutex<RevealState>,
}

#[derive(Default)]
struct RevealState {
    by_session: HashMap<SessionId, HashSet<String>>,
    /// Insertion order, for bounded eviction.
    order: VecDeque<SessionId>,
}

impl RevealedTools {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Record `names` as revealed for `session`, evicting the oldest sessions
    /// once the bound is crossed.
    ///
    /// In production the only writer is [`RevealTrackerHook`]; gated capabilities
    /// call this directly in their tests to stand in for a `tool_search` round.
    pub(crate) fn record(&self, session: SessionId, names: impl IntoIterator<Item = String>) {
        let mut state = self.lock();
        if !state.by_session.contains_key(&session) {
            state.order.push_back(session);
            while state.order.len() > MAX_TRACKED_SESSIONS {
                if let Some(evicted) = state.order.pop_front() {
                    state.by_session.remove(&evicted);
                }
            }
        }
        state.by_session.entry(session).or_default().extend(names);
    }

    /// Whether any of `tools` has been revealed for `session`.
    ///
    /// Callers pass the tools whose how-to their block explains; the block is
    /// worth its tokens once at least one of them is callable with real
    /// arguments.
    pub(crate) fn any_revealed(&self, session: SessionId, tools: &[&str]) -> bool {
        let state = self.lock();
        state
            .by_session
            .get(&session)
            .is_some_and(|revealed| tools.iter().any(|name| revealed.contains(*name)))
    }

    /// Recover from a poisoned mutex rather than panicking: a lost reveal costs
    /// a few tokens of extra prompt, which must never take down the agent loop.
    fn lock(&self) -> MutexGuard<'_, RevealState> {
        self.inner.lock().unwrap_or_else(|err| err.into_inner())
    }
}

/// Records `tool_search` results into a [`RevealedTools`] registry.
pub(crate) struct ToolRevealCapability {
    reveals: Arc<RevealedTools>,
}

impl ToolRevealCapability {
    pub(crate) fn new(reveals: Arc<RevealedTools>) -> Self {
        Self { reveals }
    }
}

#[async_trait]
impl Capability for ToolRevealCapability {
    fn id(&self) -> &str {
        TOOL_REVEAL_CAPABILITY_ID
    }

    fn name(&self) -> &str {
        "Tool Reveal Tracking"
    }

    fn description(&self) -> &str {
        "Tracks which deferred tool schemas `tool_search` has loaded, so capability \
         prompt blocks can wait until their tools are callable."
    }

    fn status(&self) -> CapabilityStatus {
        CapabilityStatus::Available
    }

    fn category(&self) -> Option<&str> {
        Some("Guardrails")
    }

    fn post_tool_exec_hooks(&self) -> Vec<Arc<dyn PostToolExecHook>> {
        vec![Arc::new(RevealTrackerHook {
            reveals: self.reveals.clone(),
        })]
    }
}

struct RevealTrackerHook {
    reveals: Arc<RevealedTools>,
}

#[async_trait]
impl PostToolExecHook for RevealTrackerHook {
    fn priority(&self) -> PostToolExecHookPriority {
        PostToolExecHookPriority::Normal
    }

    async fn after_exec(
        &self,
        tool_call: &ToolCall,
        _tool_def: &ToolDefinition,
        result: &mut ToolResult,
        context: &ToolContext,
    ) {
        if tool_call.name != TOOL_SEARCH_TOOL_NAME || result.error.is_some() {
            return;
        }
        let loaded = loaded_tool_names(result.result.as_ref());
        if !loaded.is_empty() {
            self.reveals.record(context.session_id, loaded);
        }
    }
}

/// Read `tool_search`'s structured `loaded` array. A miss is not an error: an
/// empty-match search returns `available_tools` instead, and nothing was
/// revealed in that case.
fn loaded_tool_names(result: Option<&Value>) -> Vec<String> {
    result
        .and_then(|value| value.get("loaded"))
        .and_then(Value::as_array)
        .map(|names| {
            names
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use everruns_provider::{BuiltinTool, DeferrablePolicy, ToolHints, ToolPolicy, ToolResult};
    use serde_json::json;

    fn tool_def(name: &str) -> ToolDefinition {
        ToolDefinition::Builtin(BuiltinTool {
            name: name.to_string(),
            display_name: None,
            description: String::new(),
            parameters: json!({ "type": "object", "properties": {} }),
            policy: ToolPolicy::Auto,
            category: None,
            deferrable: DeferrablePolicy::Automatic,
            hints: ToolHints::default(),
            full_parameters: None,
        })
    }

    fn call(name: &str) -> ToolCall {
        ToolCall {
            id: format!("call-{name}"),
            name: name.to_string(),
            arguments: json!({}),
        }
    }

    async fn run_hook(reveals: &Arc<RevealedTools>, session: SessionId, result: Value) {
        let hook = RevealTrackerHook {
            reveals: reveals.clone(),
        };
        let mut tool_result = ToolResult {
            tool_call_id: "call".to_string(),
            result: Some(result),
            images: None,
            error: None,
            connection_required: None,
            raw_output: None,
        };
        hook.after_exec(
            &call(TOOL_SEARCH_TOOL_NAME),
            &tool_def(TOOL_SEARCH_TOOL_NAME),
            &mut tool_result,
            &ToolContext::new(session),
        )
        .await;
    }

    #[tokio::test]
    async fn tool_search_results_reveal_the_tools_they_loaded() {
        let reveals = Arc::new(RevealedTools::new());
        let session = SessionId::new();

        assert!(
            !reveals.any_revealed(session, &["get_config"]),
            "nothing is revealed before the first tool_search"
        );

        run_hook(
            &reveals,
            session,
            json!({ "query": "config", "loaded": ["get_config", "set_config"] }),
        )
        .await;

        assert!(reveals.any_revealed(session, &["get_config"]));
        assert!(
            reveals.any_revealed(session, &["recall", "set_config"]),
            "any listed tool being revealed is enough"
        );
        assert!(!reveals.any_revealed(session, &["remember"]));
    }

    #[tokio::test]
    async fn reveals_do_not_leak_between_sessions() {
        let reveals = Arc::new(RevealedTools::new());
        let (first, second) = (SessionId::new(), SessionId::new());

        run_hook(&reveals, first, json!({ "loaded": ["get_config"] })).await;

        assert!(reveals.any_revealed(first, &["get_config"]));
        assert!(
            !reveals.any_revealed(second, &["get_config"]),
            "a reveal in one session must not unhide prompt text in another"
        );
    }

    #[tokio::test]
    async fn empty_match_searches_reveal_nothing() {
        let reveals = Arc::new(RevealedTools::new());
        let session = SessionId::new();

        // The no-match branch returns `available_tools`, not `loaded`.
        run_hook(
            &reveals,
            session,
            json!({ "tools": [], "available_tools": ["get_config"] }),
        )
        .await;

        assert!(!reveals.any_revealed(session, &["get_config"]));
    }

    #[test]
    fn tracked_sessions_are_bounded() {
        let reveals = RevealedTools::new();
        let first = SessionId::new();
        reveals.record(first, ["get_config".to_string()]);

        for _ in 0..MAX_TRACKED_SESSIONS {
            reveals.record(SessionId::new(), ["get_config".to_string()]);
        }

        assert!(
            !reveals.any_revealed(first, &["get_config"]),
            "the oldest session is evicted once the bound is crossed"
        );
        assert_eq!(reveals.lock().by_session.len(), MAX_TRACKED_SESSIONS);
    }
}

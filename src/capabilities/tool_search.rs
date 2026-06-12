// Vendored, stateful tool-search capability (provider-agnostic deferred tool
// loading).
//
// Background: everruns ships two deferral capabilities. `openai_tool_search`
// uses OpenAI's native Responses `tool_search` and currently fails with a
// `server_error` on the reasoning models that advertise it (EVE-521). The
// generic `everruns_core::capabilities::ToolSearchCapability` (renamed from
// `GenericToolSearchCapability` in 0.9.0) defers schemas client-side, but its
// `DeferSchemaHook` is *stateless*: it re-stubs every deferrable tool on every
// reason iteration. 0.9.0 added a `full_parameters` field so its `tool_search`
// can echo the real schema back *as text*, but that does not help structured
// callers (see below) — the registered schema the model sees stays the stub.
// Because OpenAI/Anthropic
// structured tool calling makes the model emit arguments against the tool's
// *registered* schema (the empty stub), the model calls every deferred tool
// with `{}` and can never pass parameters — even after `tool_search` returns
// the real schema as text. Verified live on gpt-5.4/gpt-5.5: 20+ iterations,
// every tool call empty. See EVE-521 for the full evidence.
//
// This vendored version fixes that with two changes:
//
//   1. Progressive disclosure (the real fix). A shared `RevealedTools` set is
//      threaded through the hook and the `tool_search` tool. When the model
//      calls `tool_search`, the matched tools are recorded as "revealed". The
//      reason atom re-assembles turn context (and re-runs this hook) on every
//      iteration, so on the *next* iteration the revealed tools are advertised
//      with their full, authoritative schemas — and the model can finally pass
//      arguments. Tool execution always uses the real tools, so only the
//      advertised schema ever changes.
//
//   2. A core-tool allowlist. The hot-path file/shell tools keep full schemas
//      always, so the agent is never crippled while the long tail (search,
//      memory, skills, history, todos) stays deferred until needed. This also
//      slashes the number of `tool_search` round-trips.
//
// Provider-agnostic: it only rewrites the standard `tools` array, so it works
// on OpenAI (gpt-5.4/5.5), Anthropic, and OpenAI-compatible backends such as
// OpenRouter (e.g. NVIDIA Nemotron) without any driver support.
//
// TODO(EVE-527): this whole module is a temporary vendor. As of 0.9.0 upstream
// has renamed `GenericToolSearchCapability` to
// `everruns_core::capabilities::ToolSearchCapability`, but it still defers with
// the stateless `DeferSchemaHook` above and has *not* shipped the
// progressive-disclosure fix — so it still regresses to empty tool calls on
// structured callers. Once upstream ships that fix (revealed-set + core
// allowlist, or equivalent), delete this file and register the upstream
// `ToolSearchCapability` in `runtime.rs` instead. Keep the `yolop_tool_search`
// id wiring until then so the harness selects this implementation. (EVE-521 is
// the related, separate native-`openai_tool_search` server_error.)

use async_trait::async_trait;
use everruns_core::capabilities::{Capability, CapabilityStatus, ToolDefinitionHook};
use everruns_core::tool_types::{BuiltinTool, DeferrablePolicy, ToolDefinition, ToolHints};
use everruns_core::tools::{Tool, ToolExecutionResult};
use everruns_core::traits::ToolContext;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::sync::{Arc, Mutex};

/// Capability id. Distinct from upstream `tool_search` / `openai_tool_search`
/// so the harness selects this vendored implementation unambiguously.
// TODO(EVE-527): drop the `yolop_` prefix and use the upstream `tool_search`
// id once `everruns_core::capabilities::ToolSearchCapability` ships the fix.
pub const TOOL_SEARCH_CAPABILITY_ID: &str = "yolop_tool_search";

/// Name of the tool the model calls to load deferred schemas.
pub const TOOL_SEARCH_TOOL_NAME: &str = "tool_search";

/// Default minimum total tool count before deferral kicks in. Below this the
/// full catalogue fits comfortably and deferral only adds round-trips.
pub const DEFAULT_TOOL_SEARCH_THRESHOLD: usize = 15;

/// Max tools returned (and revealed) by a single `tool_search` call.
const MAX_SEARCH_RESULTS: usize = 12;

/// Hot-path tools that always keep their full schemas, so the agent can read,
/// edit, search, run shell commands, and invoke TUI commands without a
/// `tool_search` round-trip. The long tail is deferred until the model asks for
/// it. Keep this list small — every entry is a tool the model pays full-schema
/// tokens for on every turn.
const ALWAYS_FULL: &[&str] = &[
    "read_file",
    "write_file",
    "edit_file",
    "list_directory",
    "grep_files",
    "bash",
    "run_yolop_command",
];

/// Names of tools the model has loaded via `tool_search` this session. Shared
/// (by `Arc`) between the capability, its schema hook, and its tool so a reveal
/// during tool execution is visible to the next context assembly.
//
// TODO(EVE-527): this revealed-set is the progressive-disclosure mechanism that
// upstream's `ToolSearchCapability` lacks. Once upstream adopts it, this type
// and its plumbing go away with the rest of the module.
type RevealedTools = Arc<Mutex<HashSet<String>>>;

/// Provider-agnostic deferred tool loading with progressive disclosure.
pub struct ToolSearchCapability {
    threshold: usize,
    revealed: RevealedTools,
}

impl ToolSearchCapability {
    pub fn new() -> Self {
        Self::with_threshold(DEFAULT_TOOL_SEARCH_THRESHOLD)
    }

    pub fn with_threshold(threshold: usize) -> Self {
        Self {
            threshold,
            revealed: Arc::new(Mutex::new(HashSet::new())),
        }
    }
}

impl Default for ToolSearchCapability {
    fn default() -> Self {
        Self::new()
    }
}

const SYSTEM_PROMPT: &str = "Some of your tools are loaded lazily to save context: you can see their \
names and descriptions, but their parameter schemas show as \"hidden\" until you load them. To use a \
hidden tool, first call `tool_search` with a short query (for example \"search the web\" or \"read \
memory\"); it returns the matching tools with their full JSON parameter schemas. On your next step \
the tool's real parameters become available — then call it with correct arguments. Core file and \
shell tools are always fully loaded and never need a search.";

#[async_trait]
impl Capability for ToolSearchCapability {
    fn id(&self) -> &str {
        TOOL_SEARCH_CAPABILITY_ID
    }

    fn name(&self) -> &str {
        "Tool Search"
    }

    fn description(&self) -> &str {
        "Provider-agnostic deferred tool loading. Hides long-tail tool parameter \
         schemas until the model loads them via the tool_search tool, reducing \
         token usage. Works on any model."
    }

    fn status(&self) -> CapabilityStatus {
        CapabilityStatus::Available
    }

    fn category(&self) -> Option<&str> {
        Some("Optimization")
    }

    fn system_prompt_addition(&self) -> Option<&str> {
        Some(SYSTEM_PROMPT)
    }

    fn tools(&self) -> Vec<Box<dyn Tool>> {
        vec![Box::new(ToolSearchTool {
            revealed: self.revealed.clone(),
        })]
    }

    fn tool_definition_hooks(&self) -> Vec<Arc<dyn ToolDefinitionHook>> {
        vec![Arc::new(DeferSchemaHook {
            threshold: self.threshold,
            revealed: self.revealed.clone(),
        })]
    }
}

// ============================================================================
// DeferSchemaHook — strips parameter schemas from deferrable, unrevealed tools
// ============================================================================

fn deferred_stub_schema() -> Value {
    json!({
        "type": "object",
        "description": "Parameters hidden to save context. Call tool_search to load the full schema before using this tool.",
    })
}

/// Returns true when a tool must always keep its full schema: the search tool
/// itself, the core allowlist, tools that opt out via `DeferrablePolicy::Never`,
/// and any tool already revealed via `tool_search` this session.
///
/// MCP tools are deliberately *not* exempt: with many configured servers (we've
/// seen setups with dozens) their full JSON schemas dominate the per-turn tool
/// payload, yet most go unused on any given turn. Deferring them keeps only
/// their name + description until the model loads the schema via `tool_search`,
/// exactly like the built-in long tail. Deferral only rewrites the *advertised*
/// schema; execution still routes through the real registry proxy, so stubbing
/// an MCP tool never breaks its call once revealed.
fn keep_full(tool: &ToolDefinition, revealed: &HashSet<String>) -> bool {
    let name = tool.name();
    name == TOOL_SEARCH_TOOL_NAME
        || ALWAYS_FULL.contains(&name)
        || matches!(tool.deferrable(), DeferrablePolicy::Never)
        || revealed.contains(name)
}

struct DeferSchemaHook {
    threshold: usize,
    revealed: RevealedTools,
}

impl ToolDefinitionHook for DeferSchemaHook {
    fn transform(&self, tools: Vec<ToolDefinition>) -> Vec<ToolDefinition> {
        // Defer only once the surface strictly exceeds the threshold; at or
        // below it the full catalogue fits comfortably. Matches the docs and
        // the `tool_surface_exceeds_tool_search_threshold` runtime test.
        if tools.len() <= self.threshold {
            return tools;
        }
        let revealed = self.revealed.lock().expect("revealed tools lock poisoned");
        tools
            .into_iter()
            .map(|tool| {
                if keep_full(&tool, &revealed) {
                    tool
                } else {
                    strip_parameters(tool)
                }
            })
            .collect()
    }

    // Mutually exclusive with the native (openai) tool_search request shaping.
    fn applies_with_native_tool_search(&self) -> bool {
        false
    }
}

/// Replace a tool's parameter schema with the deferred stub, keeping name,
/// description, policy, category, and hints intact.
fn strip_parameters(tool: ToolDefinition) -> ToolDefinition {
    match tool {
        ToolDefinition::Builtin(mut b) => {
            b.parameters = deferred_stub_schema();
            ToolDefinition::Builtin(b)
        }
        ToolDefinition::ClientSide(mut c) => {
            c.parameters = deferred_stub_schema();
            ToolDefinition::ClientSide(c)
        }
    }
}

// ============================================================================
// Tool: tool_search
// ============================================================================

/// Tool that returns full parameter schemas for tools matching a query and
/// records them as revealed so the schema hook stops stubbing them.
pub struct ToolSearchTool {
    revealed: RevealedTools,
}

impl ToolSearchTool {
    /// Rank `defs` against `query` by keyword overlap and return the best
    /// matches (with full schemas), capped at `MAX_SEARCH_RESULTS`. An empty
    /// query returns the first `MAX_SEARCH_RESULTS` tools in registry order so
    /// the model can browse. The search tool itself is always excluded.
    fn search(defs: &[ToolDefinition], query: &str) -> Vec<Value> {
        let terms: Vec<String> = query
            .split_whitespace()
            .map(|t| {
                t.trim_matches(|c: char| !c.is_alphanumeric())
                    .to_lowercase()
            })
            .filter(|t| !t.is_empty())
            .collect();

        let mut scored: Vec<(usize, &ToolDefinition)> = defs
            .iter()
            .filter(|d| d.name() != TOOL_SEARCH_TOOL_NAME)
            .filter_map(|d| {
                if terms.is_empty() {
                    return Some((0, d));
                }
                let haystack = format!("{} {}", d.name(), d.description()).to_lowercase();
                let score = terms.iter().filter(|t| haystack.contains(*t)).count();
                (score > 0).then_some((score, d))
            })
            .collect();

        // Stable sort by descending score; equal scores keep registry order.
        scored.sort_by_key(|entry| std::cmp::Reverse(entry.0));

        scored
            .into_iter()
            .take(MAX_SEARCH_RESULTS)
            .map(|(_, d)| {
                json!({
                    "name": d.name(),
                    "description": d.description(),
                    "parameters": d.parameters(),
                })
            })
            .collect()
    }
}

#[async_trait]
impl Tool for ToolSearchTool {
    fn name(&self) -> &str {
        TOOL_SEARCH_TOOL_NAME
    }

    fn display_name(&self) -> Option<&str> {
        Some("Tool Search")
    }

    fn description(&self) -> &str {
        "Search the available tools by keyword and load their full parameter \
         schemas. Returns matching tools with their names, descriptions, and JSON \
         parameter schemas. Call this before using any tool whose parameters show \
         as hidden."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Keywords describing the tool or capability you need (e.g. 'search the web', 'read memory', 'list skills')."
                }
            },
            "required": ["query"],
            "additionalProperties": false
        })
    }

    fn hints(&self) -> ToolHints {
        ToolHints::default()
            .with_readonly(true)
            .with_idempotent(true)
    }

    // Never defer the search tool's own schema.
    fn to_definition(&self) -> ToolDefinition {
        ToolDefinition::Builtin(BuiltinTool {
            name: self.name().to_string(),
            display_name: self.display_name().map(str::to_string),
            description: self.description().to_string(),
            parameters: self.parameters_schema(),
            policy: self.policy(),
            category: None,
            deferrable: DeferrablePolicy::Never,
            hints: self.hints(),
            full_parameters: None,
        })
    }

    fn requires_context(&self) -> bool {
        true
    }

    async fn execute(&self, _arguments: Value) -> ToolExecutionResult {
        ToolExecutionResult::tool_error(
            "tool_search requires tool execution context and cannot run standalone.",
        )
    }

    async fn execute_with_context(
        &self,
        arguments: Value,
        context: &ToolContext,
    ) -> ToolExecutionResult {
        let query = arguments
            .get("query")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();

        let Some(registry) = &context.tool_registry else {
            return ToolExecutionResult::tool_error(
                "Tool registry not available in this context. tool_search requires worker-side tool execution.",
            );
        };

        let defs = registry.tool_definitions();
        let matches = Self::search(&defs, query);

        if matches.is_empty() {
            // No keyword hits — surface the catalogue (names only) so the model
            // can refine its query instead of dead-ending.
            let names: Vec<&str> = defs
                .iter()
                .map(|d| d.name())
                .filter(|n| *n != TOOL_SEARCH_TOOL_NAME)
                .collect();
            return ToolExecutionResult::success(json!({
                "query": query,
                "tools": [],
                "message": "No tools matched the query. Try a different keyword.",
                "available_tools": names,
            }));
        }

        // Record the matched tools as revealed so the schema hook advertises
        // their full schemas on the next reason iteration. This is what lets
        // the model actually pass arguments to them.
        let revealed_now: Vec<String> = matches
            .iter()
            .filter_map(|t| t.get("name").and_then(Value::as_str).map(str::to_string))
            .collect();
        {
            let mut revealed = self.revealed.lock().expect("revealed tools lock poisoned");
            for name in &revealed_now {
                revealed.insert(name.clone());
            }
        }

        ToolExecutionResult::success(json!({
            "query": query,
            "tools": matches,
            "loaded": revealed_now,
            "message": "Full schemas loaded. You can now call these tools with correct arguments.",
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use everruns_core::tool_types::ToolPolicy;

    fn builtin(name: &str, deferrable: DeferrablePolicy) -> ToolDefinition {
        ToolDefinition::Builtin(BuiltinTool {
            name: name.to_string(),
            display_name: None,
            description: format!("{name} description"),
            parameters: json!({
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"]
            }),
            policy: ToolPolicy::default(),
            category: None,
            deferrable,
            hints: ToolHints::default(),
            full_parameters: None,
        })
    }

    fn is_stubbed(tool: &ToolDefinition) -> bool {
        tool.parameters()
            .get("properties")
            .and_then(Value::as_object)
            .is_none_or(|p| p.is_empty())
    }

    /// Build a tool set above the threshold: the always-full tools plus N
    /// long-tail tools so deferral activates.
    fn tool_set(extra: usize) -> Vec<ToolDefinition> {
        let mut tools: Vec<ToolDefinition> = ALWAYS_FULL
            .iter()
            .map(|n| builtin(n, DeferrablePolicy::default()))
            .collect();
        for i in 0..extra {
            tools.push(builtin(
                &format!("longtail_{i}"),
                DeferrablePolicy::default(),
            ));
        }
        tools
    }

    #[test]
    fn below_threshold_keeps_all_full_schemas() {
        let cap = ToolSearchCapability::new();
        let hook = &cap.tool_definition_hooks()[0];
        let extra = DEFAULT_TOOL_SEARCH_THRESHOLD - ALWAYS_FULL.len() - 3;
        let out = hook.transform(tool_set(extra));
        assert!(
            out.iter().all(|t| !is_stubbed(t)),
            "nothing should defer below threshold"
        );
    }

    #[test]
    fn deferral_activates_strictly_above_threshold() {
        let cap = ToolSearchCapability::with_threshold(15);
        let hook = &cap.tool_definition_hooks()[0];
        // Exactly at the threshold: nothing defers.
        let at_threshold_extra = DEFAULT_TOOL_SEARCH_THRESHOLD - ALWAYS_FULL.len();
        let at = hook.transform(tool_set(at_threshold_extra));
        assert_eq!(at.len(), DEFAULT_TOOL_SEARCH_THRESHOLD);
        assert!(
            at.iter().all(|t| !is_stubbed(t)),
            "at the threshold the full catalogue must fit; no deferral"
        );
        // One over: the long tail defers.
        let over = hook.transform(tool_set(at_threshold_extra + 1));
        assert!(
            over.iter().any(is_stubbed),
            "strictly above the threshold the long tail must defer"
        );
    }

    #[test]
    fn core_tools_keep_full_schemas_long_tail_is_deferred() {
        let cap = ToolSearchCapability::new();
        let hook = &cap.tool_definition_hooks()[0];
        let out = hook.transform(tool_set(12));
        for t in &out {
            if ALWAYS_FULL.contains(&t.name()) {
                assert!(
                    !is_stubbed(t),
                    "core tool {} must keep full schema",
                    t.name()
                );
            } else {
                assert!(
                    is_stubbed(t),
                    "long-tail tool {} must be deferred",
                    t.name()
                );
            }
        }
    }

    #[test]
    fn mcp_tools_defer_above_threshold_and_reveal_after_search() {
        let cap = ToolSearchCapability::new();
        let hook = &cap.tool_definition_hooks()[0];
        // Always-full/long-tail tools plus one MCP tool, above the threshold.
        let build = || {
            let mut tools = tool_set(12);
            tools.push(builtin("mcp_docs__search", DeferrablePolicy::default()));
            tools
        };

        // MCP tools defer like the rest of the long tail (no special exemption).
        let before = hook.transform(build());
        let mcp = before
            .iter()
            .find(|t| t.name() == "mcp_docs__search")
            .unwrap();
        assert!(is_stubbed(mcp), "MCP tool must defer above threshold");

        // Once revealed via tool_search it regains its full schema next pass.
        cap.revealed
            .lock()
            .unwrap()
            .insert("mcp_docs__search".to_string());
        let after = hook.transform(build());
        let mcp = after
            .iter()
            .find(|t| t.name() == "mcp_docs__search")
            .unwrap();
        assert!(
            !is_stubbed(mcp),
            "revealed MCP tool must regain full schema"
        );
    }

    #[test]
    fn revealing_a_tool_restores_its_full_schema_next_pass() {
        let cap = ToolSearchCapability::new();
        let hook = &cap.tool_definition_hooks()[0];

        // First pass: longtail_0 is deferred.
        let before = hook.transform(tool_set(12));
        let deferred = before.iter().find(|t| t.name() == "longtail_0").unwrap();
        assert!(
            is_stubbed(deferred),
            "precondition: longtail_0 starts deferred"
        );

        // Simulate tool_search revealing it (what execute_with_context does).
        cap.revealed
            .lock()
            .unwrap()
            .insert("longtail_0".to_string());

        // Next pass (same hook, re-run by the reason atom): full schema restored.
        let after = hook.transform(tool_set(12));
        let revealed = after.iter().find(|t| t.name() == "longtail_0").unwrap();
        assert!(
            !is_stubbed(revealed),
            "revealed tool must regain its full schema"
        );
        // Other long-tail tools stay deferred.
        let other = after.iter().find(|t| t.name() == "longtail_1").unwrap();
        assert!(
            is_stubbed(other),
            "unrevealed long-tail tools stay deferred"
        );
    }

    #[test]
    fn search_ranks_by_keyword_and_returns_full_schema() {
        let defs = tool_set(12);
        let results = ToolSearchTool::search(&defs, "grep");
        assert!(!results.is_empty());
        assert_eq!(results[0]["name"], "grep_files");
        // Returned schema is the real one, not the stub.
        assert!(results[0]["parameters"]["properties"]["path"].is_object());
    }

    #[test]
    fn search_excludes_itself_and_lists_on_empty_query() {
        let defs = tool_set(2);
        let results = ToolSearchTool::search(&defs, "");
        let names: Vec<&str> = results.iter().filter_map(|r| r["name"].as_str()).collect();
        assert!(!names.contains(&TOOL_SEARCH_TOOL_NAME));
        assert!(names.contains(&"read_file"));
    }
}

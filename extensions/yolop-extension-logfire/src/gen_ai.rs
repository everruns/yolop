//! Gen-AI semantic-convention attribute names, so exported spans agree with
//! the host's own OpenTelemetry and Braintrust backends instead of inventing a
//! private vocabulary a tracing UI cannot read.
//!
//! **Copied, not imported.** These names are defined upstream in
//! `everruns-core`'s `telemetry::gen_ai` module (see that crate's
//! `src/telemetry.rs`, and `everruns-host`'s `src/observability/otel.rs` for
//! how the in-process listener applies them). Depending on `everruns-core` for
//! a set of `&'static str` constants would pull ~29 transitive crates
//! (tokio, jsonschema, tree-sitter, …) into this small extension binary, so
//! the constants this exporter needs are duplicated here.
//!
//! The trade is explicit: a copy can drift from upstream. Re-check it against
//! `everruns-core::telemetry::gen_ai` when bumping the everruns line.
//!
//! Source of truth for the conventions themselves:
//! <https://opentelemetry.io/docs/specs/semconv/gen-ai/>

/// The operation being performed, e.g. `chat`, `execute_tool`.
pub const OPERATION_NAME: &str = "gen_ai.operation.name";
/// Provider identifier, e.g. `anthropic`.
pub const PROVIDER_NAME: &str = "gen_ai.provider.name";
/// Provider identifier under the older convention spelling. Emitted alongside
/// `PROVIDER_NAME` because UIs differ on which they read.
pub const SYSTEM: &str = "gen_ai.system";
/// Model requested.
pub const REQUEST_MODEL: &str = "gen_ai.request.model";
/// Model that actually served the request.
pub const RESPONSE_MODEL: &str = "gen_ai.response.model";
/// Why generation stopped.
pub const RESPONSE_FINISH_REASONS: &str = "gen_ai.response.finish_reasons";
/// Prompt tokens.
pub const USAGE_INPUT_TOKENS: &str = "gen_ai.usage.input_tokens";
/// Completion tokens.
pub const USAGE_OUTPUT_TOKENS: &str = "gen_ai.usage.output_tokens";
/// Tokens served from cache.
pub const USAGE_CACHE_READ_TOKENS: &str = "gen_ai.usage.cache_read_tokens";
/// Tokens written to cache (Anthropic).
pub const USAGE_CACHE_CREATION_TOKENS: &str = "gen_ai.usage.cache_creation_tokens";
/// Tool being executed.
pub const TOOL_NAME: &str = "gen_ai.tool.name";
/// Tool call identifier.
pub const TOOL_CALL_ID: &str = "gen_ai.tool.call.id";
/// Conversation or session identifier.
pub const CONVERSATION_ID: &str = "gen_ai.conversation.id";
/// Agent identifier.
pub const AGENT_ID: &str = "gen_ai.agent.id";

/// Operation names. The agentic-phase values (`reason`, `act`) are everruns
/// extensions to the spec, matching the upstream listener.
pub mod operation {
    pub const CHAT: &str = "chat";
    pub const EXECUTE_TOOL: &str = "execute_tool";
    pub const INVOKE_AGENT: &str = "invoke_agent";
    pub const REASON: &str = "reason";
    pub const ACT: &str = "act";
    pub const THINKING: &str = "thinking";
}

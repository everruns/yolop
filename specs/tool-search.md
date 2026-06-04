# `tool_search` — deferred tool loading

Status: v1 implemented.

## Why

Yolop ships ~17 always-on tools (file ops, bash, search, todos, skills,
memory, web fetch). Every turn sends every tool's full JSON schema to the
model upfront, which costs input tokens on each request and grows worse as
new tools (e.g. MCP servers) are added.

Deferred tool loading — OpenAI's Responses-API "tool search" — lets the model
fetch a tool's full schema on demand instead. Tools are grouped into
namespaces by capability category and exposed as name-only stubs plus a
`tool_search` activator; the model loads schemas only for the tools it
actually needs. This is a pure token optimization with no behavior change.

## What

Yolop enables the `openai_tool_search` capability
(`OpenAiToolSearchCapability`) from `everruns-core` in the coding harness for
both hosts (TUI and `--print`/ACP). It is wired in `src/runtime.rs`:
registered in the capability registry and listed in
`coding_harness_capabilities`.

Key properties (enforced by the runtime, not by yolop):

- **OpenAI-only.** The mechanism is a native OpenAI Responses-API feature.
  The runtime keeps it enabled only for models whose profile advertises
  `tool_search` support — currently the **GPT-5.4 family and GPT-5.5/5.5-pro**.
  Yolop's default model (`gpt-5.5`) qualifies.
- **Self-disabling no-op elsewhere.** For Anthropic, Google/Gemini,
  OpenRouter, Ollama, llmsim, and older GPT models, the runtime silently
  clears the config and sends full schemas as before. No error, no crash.
- **Threshold-gated.** Below `DEFAULT_TOOL_SEARCH_THRESHOLD` (15) tools, full
  schemas are still sent even when the capability is on. Yolop's surface
  (~17) sits above the threshold, so deferral is effective on supported
  models. A test guards that the surface stays above the threshold; if it
  ever drops below, set an explicit lower threshold via capability config.

## Non-goals

- No per-provider reimplementation. Anthropic has its own server-side tool
  search, but yolop does not wire it — non-OpenAI providers get full schemas.
- No tools of its own. The capability only reshapes the outgoing request; it
  adds nothing to yolop's tool surface.

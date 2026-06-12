# `tool_search` — deferred tool loading

Status: v1 implemented (vendored, provider-agnostic).

## Why

Yolop ships ~17 always-on tools (file ops, bash, search, todos, skills,
memory, web fetch). Sending every tool's full JSON schema on every turn costs
input tokens and scales badly as tools are added (e.g. MCP servers).

Deferred tool loading hides the parameter schemas of rarely-used tools until
the model asks for them, cutting per-turn token cost while keeping every tool
callable.

## What

The capability is **vendored** in `src/capabilities/tool_search.rs` (id
`yolop_tool_search`) rather than taken from `everruns-core`, because neither
upstream option works for yolop's models:

- `openai_tool_search` uses OpenAI's native Responses `tool_search`, which
  fails with a `server_error` on the reasoning models that advertise it
  (gpt-5.4 family; gpt-5.5 was gated off). See EVE-521.
- `GenericToolSearchCapability` defers schemas client-side but its hook is
  *stateless* — it re-stubs every deferrable tool on every iteration. Because
  structured tool calling makes the model emit arguments against the
  *registered* (stub) schema, the model calls every deferred tool with `{}`
  and can never pass parameters. Verified live: 20+ iterations, all empty.

The vendored version keeps the client-side approach (so it is
provider-agnostic) and adds the two things that make it actually work:

1. **Progressive disclosure.** A shared "revealed" set is threaded through the
   schema hook and the `tool_search` tool. When the model calls `tool_search`,
   the matched tools are recorded as revealed. The reason atom re-assembles
   turn context — and re-runs the hook — on every iteration, so on the *next*
   step the revealed tools are advertised with their full, authoritative
   schemas and the model can pass real arguments. Tool execution always uses
   the real tools; only the advertised schema changes.

2. **Core-tool allowlist.** The hot-path file/shell tools (`read_file`,
   `write_file`, `edit_file`, `list_directory`, `grep_files`, `bash`) always
   keep full schemas, so the agent is never crippled and common work needs no
   search. The long tail (search, web fetch, memory, skills, history, todos)
   is deferred until requested. **MCP server tools defer on the same footing**:
   with many configured servers their schemas are the largest and least-used
   part of the surface, so only their names and descriptions ride each turn
   until `tool_search` loads a schema. (Execution still routes through the real
   registry proxy, so a stubbed MCP tool call works once revealed.)

Deferral activates only once the total tool count crosses
`DEFAULT_TOOL_SEARCH_THRESHOLD` (15); below that, full schemas fit comfortably.

## Provider support

Works on every provider/model because it only rewrites the standard `tools`
array — no driver or native-feature dependency. Validated end-to-end (a
deferred web-search tool loaded via `tool_search` and called with correct
arguments) on:

- OpenAI `gpt-5.5` (default) and `gpt-5.4`
- Anthropic `claude-sonnet`
- NVIDIA Nemotron via OpenRouter

## Non-goals

- No native/server-side tool search. The native OpenAI path stays unused until
  EVE-521 is fixed upstream.
- The capability adds exactly one tool (`tool_search`); it does not otherwise
  change yolop's tool surface.

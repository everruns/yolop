# `tool_search` — deferred tool loading

Status: implemented via upstream `everruns-core` (provider-agnostic).

## Why

Yolop ships ~17 always-on tools (file ops, bash, search, todos, skills,
memory, web fetch). Sending every tool's full JSON schema on every turn costs
input tokens and scales badly as tools are added (e.g. MCP servers).

Deferred tool loading hides the parameter schemas of rarely-used tools until
the model asks for them, cutting per-turn token cost while keeping every tool
callable.

## What

Yolop registers the upstream `everruns_core::capabilities::ToolSearchCapability`
(id `tool_search`, generic/provider-agnostic) in `runtime.rs`. It does **not**
use the native `openai_tool_search` path, which fails with a `server_error` on
the reasoning models that advertise it (gpt-5.4 family; gpt-5.5 gated off — see
EVE-521).

Yolop previously shipped a *vendored* copy of this capability because the
upstream client-side mechanism was stateless: it re-stubbed every deferrable
tool each iteration, so structured tool callers emitted `{}` against the
registered stub schema and could never pass parameters. Both fixes landed
upstream in EVE-527 (everruns#2130, released in `everruns-core` 0.11.0), so the
vendor was deleted and yolop now consumes upstream directly:

1. **Progressive disclosure.** When the model calls `tool_search`, the matched
   tools are recorded as revealed for that session. Turn context is reassembled
   (and the schema hook re-runs) each iteration, so on the *next* step the
   revealed tools are advertised with their full, authoritative schemas on the
   *registered* definition and the model can pass real arguments. Tool execution
   always uses the real tools; only the advertised schema changes.

2. **Never-defer allowlist.** Yolop passes its hot-path tools to
   `ToolSearchCapability::new().with_never_defer([...])` so they always keep full
   schemas and common work needs no `tool_search` round-trip: the file/shell
   tools (`read_file`, `write_file`, `edit_file`, `list_directory`,
   `grep_files`, `bash`) plus `run_yolop_command` (the client-command dispatch
   tool, which requires a `command` argument and so must never be called against
   a stub). Yolop does not own those tool definitions (they come from
   `FileSystemCapability`, yolop's `bash` tool, and the `client_commands`
   capability), so it sets the policy by name rather than via each tool's
   `DeferrablePolicy`. The long tail (search, web fetch, memory, skills,
   history, todos) defers until requested. **MCP server tools defer on the same
   footing** — with many configured servers their schemas are the largest,
   least-used part of the surface, so only names and descriptions ride each turn
   until `tool_search` loads a schema (execution still routes through the real
   registry proxy, so a stubbed MCP tool call works once revealed).

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

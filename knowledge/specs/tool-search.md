---
type: Product Specification
title: `tool_search`, deferred tool loading
description: Defines the `tool_search`, deferred tool loading contract for Yolop.
---

# `tool_search`, deferred tool loading

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
the reasoning models that advertise it (gpt-5.4 family; gpt-5.5 gated off, see
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

2. **Static host-shaped eager profile.** Yolop passes first-turn repository
   discovery (`read_file`, `list_directory`, `grep_files`), bookkeeping
   (`write_todos`, `write_session_title`), and the mandatory progress-guard
   transition (`progress_checkpoint`) to
   `ToolSearchCapability::new().with_never_defer([...])`. Mutation, shell,
   background, release/control, skills, session history, web, and other
   specialized tools keep their names and descriptions visible but reveal their
   authoritative schemas through `tool_search` when the task calls for them.
   This is host/task shaped without a volatile classifier: the allowlist is
   stable for the session and provider-cache prefix. Opt-in LSP tools stay eager
   because enabling the host profile is itself an explicit task signal and the
   LSP adoption eval showed that stubbing those schemas drives adoption toward
   zero. Extension tools explicitly marked `never_defer` retain their manifest
   contract. Yolop does not own the built-in definitions, so it sets this policy
   by name rather than changing each tool's `DeferrablePolicy`.

   **MCP server tools defer on the same footing**: with many configured servers
   their schemas are the largest, least-used part of the surface, so only names
   and descriptions ride each turn until `tool_search` loads a schema (execution
   still routes through the real registry proxy, so a stubbed MCP tool call works
   once revealed).

3. **Compact deferred schemas.** `everruns-core` 0.17.21 reduced every deferred
   definition to the permissive JSON object stub `{type: object,
   additionalProperties: true}`. The single capability prompt owns the reveal
   instruction instead of repeating prose inside every tool schema. Full
   descriptions remain visible, and a reveal restores the same authoritative
   schema used for execution and structured tool calling.

Deferral activates only once the total tool count crosses
`DEFAULT_TOOL_SEARCH_THRESHOLD` (15); below that, full schemas fit comfortably.

## Provider support

Works on every provider/model because it only rewrites the standard `tools`
array, no driver or native-feature dependency. Validated end-to-end (a
deferred web-search tool loaded via `tool_search` and called with correct
arguments) on:

- OpenAI `gpt-5.5` (default) and `gpt-5.4`
- Anthropic `claude-sonnet`
- NVIDIA Nemotron via OpenRouter

## Reveal gating

Deferral hid tool *schemas* while capability prompt blocks kept explaining how
to call those same hidden tools every turn. `capabilities::tool_reveal` closes
that: a `PostToolExecHook` records `tool_search`'s structured `loaded` names
into a bounded, session-keyed registry, and gated capabilities check it before
contributing how-to prose. Discovery text stays ungated, see
[system prompt composition](system-prompt.md) for where the line falls.

The registry reads `tool_search`'s own result rather than mirroring upstream
state, so it cannot drift from what the model was shown. Gating is meaningless
without deferral, so `yolop_tool_reveal` is enabled alongside
`TOOL_SEARCH_CAPABILITY_ID`.

## Non-goals

- No native/server-side tool search. The native OpenAI path stays unused until
  EVE-521 is fixed upstream.
- The capability adds exactly one tool (`tool_search`); it does not otherwise
  change yolop's tool surface.

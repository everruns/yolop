---
type: Product Specification
title: `tool_search`, deferred tool loading
description: Defines the `tool_search`, deferred tool loading contract for Yolop.
---

# `tool_search`, deferred tool loading

Status: implemented via upstream `everruns-core` (provider-agnostic).

## Why

Yolop composes coding, shell, task, skill, memory, web and optional MCP tools. Sending every tool's full JSON schema on every turn costs
input tokens and scales badly as tools are added (e.g. MCP servers).

Deferred tool loading hides the parameter schemas of rarely-used tools until
the model asks for them, cutting per-turn token cost while keeping every tool
callable.

## What

Yolop registers upstream `ToolSearchCapability` in
[`runtime/mod.rs`](../../src/runtime/mod.rs). It rewrites provider-visible
schemas without changing authoritative definitions or tool execution routing.
This client-side mechanism works across providers and does not require native
server-side search support.

- **Progressive disclosure:** engine context assembly derives disclosure from
  the transcript. A searched tool's authoritative schema becomes available
  without a process-local reveal store, including after resume. Failed known
  calls also reveal the schema needed for recovery.
- **Eager tools:** `YOLOP_NEVER_DEFER_TOOLS` in the runtime owns the allowlist.
  Repository discovery and code navigation, shell and background execution,
  delegation, task bookkeeping, skill discovery/activation and mandatory
  progress checkpoints keep full schemas available immediately. Native editing
  keeps its trained-in shape visible when enabled. This prevents common tools
  and required transitions from spending a round trip correcting guessed fields.
- **Extensions and MCP:** extension `never_defer` manifest declarations are
  honored. Specialized tools, including MCP tools, otherwise defer on the same
  footing. A whole-server reveal is represented by the upstream `__` prefix.
- **Compact stubs:** a deferred definition exposes its name and description
  with a permissive object schema. The capability prompt explains discovery
  once. [Tool-call validation](tool-calling.md) still checks calls against the
  full authoritative schema before execution.

Upstream's `DEFAULT_TOOL_SEARCH_THRESHOLD` determines when deferral begins.
Yolop's default-harness test verifies that the composed tool count crosses it;
there is no separate host threshold or duplicate list of tool definitions.

## Reveal gating

Deferral hid tool *schemas* while capability prompt blocks kept explaining how
to call those same hidden tools every turn. Gated capabilities now check the
engine's `SystemPromptContext::tool_disclosure`, which context assembly reads
from the transcript's `tool_search` results, before contributing how-to prose.
Discovery text stays ungated, see [system prompt composition](system-prompt.md)
for where the line falls and why the how-to is a separate paragraph.

## Non-goals

- Native/server-side tool search is not enabled; provider-independent
  disclosure is the current host contract.
- The capability adds exactly one tool (`tool_search`); it does not otherwise
  change yolop's tool surface.

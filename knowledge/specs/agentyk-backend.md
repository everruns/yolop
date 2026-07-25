---
type: Architecture Specification
title: Agentyk execution backend
description: Defines the isolated, feature-gated second execution backend built on the agentyk library, what it covers, and what it deliberately does not.
---

# Agentyk execution backend

## Why

Yolop's execution story is built on `everruns-runtime`.
[agentyk](https://github.com/everruns/agentyk) is the value-first library the
everruns core is intended to be rebuilt on, and the open question is whether a
real coding agent — not an example — can stand on it today.

This backend exists to answer that question by building the thing, not by
reading the API. It is an **experiment with a stated purpose**: every gap it
hits is a finding, and the findings are the deliverable. Yolop's shipping
backend is unchanged.

## Shape

One CLI flag and one module, both off by default.

- `--engine agentyk` selects it; `--engine everruns` (default) is the shipping
  path. The flag is accepted only in a build with the `agentyk-backend`
  feature; without it the binary explains how to rebuild.
- `src/agentyk_backend/` is the whole implementation. Nothing outside it
  imports agentyk, and it imports only two things from the rest of yolop:
  containment (`exec::sandbox`) and provider resolution
  (`runtime::ProviderChoice`, `config::Settings`). Everything else it needs it
  builds against agentyk's seams.
- The branch happens in `main` **before** the everruns runtime is constructed.
  This is not a swappable component inside one runtime; it is a second
  execution story that owns its own loop, so an accidental dependency on
  everruns state cannot creep in.

Isolation is the point. A backend that reached into `runtime::` for
convenience would stop measuring what agentyk can do on its own.

## What it covers

| Concern | How |
| --- | --- |
| Model | yolop's `ProviderChoice` → `agentyk::ModelSpec` (`model.rs`) |
| Turn loop, history, replay | `agentyk::Session`, `JsonlEventLog` per run |
| Files | `agentyk::FileSystemCapability` over `WriteBlocklistFileSystem`(`RealDiskFileSystem`) |
| Shell | yolop's sandbox provider behind an agentyk `Tool` |
| `edit_file`, `grep_files` | yolop-side tools — agentyk ships neither |
| Instructions | an `agent_instructions` capability reading AGENTS.md/CLAUDE.md |
| Approval | `TurnMiddleware`, gating on the `hints` metadata hatch |
| Output | an `EventListener` rendering the event stream to stdout |
| Cancellation | `CancellationToken` bound to ctrl-c in the REPL |

Not covered, and not intended to be in this pass: the TUI, ACP, worktrees,
checkpoints, background tasks, MCP, skills, hooks, extensions, trajectory
export, session resume, compaction, and the rest of yolop's capability set.

## Findings

What the port actually hit, in the order it hurt. Fixing these is agentyk's
work, not yolop's; they are recorded here because this backend is the evidence.

1. **Provider coverage is two protocols wide.** agentyk ships `openai` (Chat
   Completions), `anthropic`, and `llmsim`. yolop serves eight providers.
   Google, Ollama, OpenRouter, and custom endpoints reach the OpenAI driver
   with a base URL, but Codex cannot be expressed at all: `ModelSpec` carries
   `api_key` and `base_url` and nothing else, so subscription auth (refresh
   token, account id, expiry) has no home. `ProviderMetadata` — or an
   equivalent hatch on `ModelSpec` — is the missing piece.
2. **No prompt caching.** The Anthropic driver never emits `cache_control`
   breakpoints and reads no `Message.metadata`. Every turn re-sends the whole
   transcript uncached, which is the difference between a viable and an
   unviable coding session on cost alone.
3. **A tool call cannot be cancelled.** `InProcessExecutor` checks the
   cancellation token between actions and between streaming chunks, but awaits
   `atoms::act` unraced; `ToolContext` carries no token. Ctrl-c during a long
   `bash` is therefore honored only after the command finishes. For a coding
   agent this is the single most user-visible gap.
4. **Tools cannot report progress.** `ToolOutput` is `{ content: String,
   is_error: bool }` and `ToolContext` has no event sink, so a tool cannot
   stream partial output, emit narration, or run in the background — everruns'
   `BackgroundEventSink` and narration phases have no counterpart. Rich tool
   results (images, structured payloads) are unrepresentable for the same
   reason.
5. **The filesystem capability is a starter set.** No `edit_file`, no
   `grep_files`, no `stat_file`, no offset/limit reads, no byte caps. Both
   tools written here are generic, not yolop-specific, and belong upstream.
6. **Tool batches run sequentially.** `TurnEngine` prepares the whole batch,
   but the in-process host dispatches it one call at a time. Parallel reads are
   table stakes for a coding agent.
7. **No mid-turn input.** `Session::run` takes `&mut self` for the duration of
   a turn and there is no way to append a message to history without running
   one, so steering ("actually, stop and do X") cannot be expressed at all.
8. **MCP is stdio-only and unauthenticated.** No HTTP/SSE transport and no auth
   provider seam, so yolop's remote MCP servers cannot be served. Capabilities
   still cannot contribute MCP servers (agentyk's own gap 13).
9. **Reasoning effort is unvalidated.** `ModelSpec::reasoning_effort` takes any
   string; there is no model-profile notion, so an effort a model rejects is
   discovered by the provider returning an error.

What worked without friction, and is worth recording as such: capabilities
composed by object (instructions, workspace tools) needed no ceremony;
middleware was the right shape for approval, and `ToolInvocation.definition`
made the metadata hints hatch reachable exactly where the decision is made;
the event stream was sufficient to build the whole terminal UI as a pure
observer; and `RealDiskFileSystem` plus the write blocklist gave a safe default
for free.

## Constraints

- The agentyk dependency is a git dependency on a branch while agentyk is
  pre-release — the published `0.1.0` predates middleware, the engine crate,
  and the filesystem capability. It moves to a version requirement when
  agentyk publishes one that carries them.
- The backend must not grow yolop-specific workarounds for the findings above.
  When a gap makes something impossible, the backend does without it and the
  gap is recorded here. Papering over a missing seam would destroy the
  measurement this backend exists to take.

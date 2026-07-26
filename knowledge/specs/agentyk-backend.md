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
| Files | `agentyk::FileSystemCapability` over `WriteBlocklistFileSystem`(`RealDiskFileSystem`) — read/write/edit/grep/stat/list/delete |
| Shell | yolop's sandbox provider behind an agentyk `Tool`, narrating progress as it runs |
| Instructions | an `agent_instructions` capability reading AGENTS.md/CLAUDE.md |
| MCP | yolop's configured servers (`yolop mcp add`, stdio and HTTP) as `McpCapability`, credentials from the environment per request |
| Model limits | yolop's everruns model profiles as an `agentyk::ModelCatalog`, so a bad `--reasoning-effort` fails at composition |
| Images | `-i/--image` attachments ride on the message that asks about them |
| Approval | `TurnMiddleware`, gating on the `hints` metadata hatch |
| Output | an `EventListener` rendering the event stream to stdout |
| Cancellation | `CancellationToken` bound to ctrl-c, racing both the tool call and the approval prompt |
| Steering | a line typed while a turn runs joins it via `Session::input()`; typed while idle, it starts the next one |

Not covered, and not intended to be in this pass: the TUI, ACP, worktrees,
checkpoints, background tasks, skills, hooks, extensions, trajectory export,
session resume, compaction, and the rest of yolop's capability set.

## Findings

What the port actually hit, in the order it hurt. Fixing these was agentyk's
work, not yolop's; they are recorded here because this backend is the evidence,
and all nine have since landed upstream (see
[`knowledge/yolop-adoption.md`](https://github.com/everruns/agentyk/blob/main/knowledge/yolop-adoption.md)
in that repository). The backend now consumes the fixes rather than working
around them: the hand-written `edit_file`/`grep_files` went upstream and were
deleted here, the shell reports progress and structured results through the
library's seams, and cancellation reaches into a running command.

1. **Provider coverage is two protocols wide.** *(Partly addressed:
   `ModelSpec::metadata` now exists, so a subscription provider is
   expressible; a Codex driver itself is still unwritten.)* agentyk ships `openai` (Chat
   Completions), `anthropic`, and `llmsim`. yolop serves eight providers.
   Google, Ollama, OpenRouter, and custom endpoints reach the OpenAI driver
   with a base URL, but Codex cannot be expressed at all: `ModelSpec` carries
   `api_key` and `base_url` and nothing else, so subscription auth (refresh
   token, account id, expiry) has no home. `ProviderMetadata` — or an
   equivalent hatch on `ModelSpec` — is the missing piece.
2. ~~**No prompt caching.**~~ *Fixed upstream:* the Anthropic driver places
   `cache_control` breakpoints by default, including the last two messages so
   caching stays incremental as the transcript grows.
3. ~~**A tool call cannot be cancelled.**~~ *Fixed upstream:* the executor
   races every call against the token and drops the loser, and `ToolContext`
   carries the token. Ctrl-c during a long `bash` now kills the child, because
   the command is spawned with `kill_on_drop`.
4. ~~**Tools cannot report progress.**~~ *Fixed upstream:*
   `ToolContext::report_progress` emits ephemeral `tool.progress` events, and
   `ToolOutput::metadata` carries structured results to the host. The shell
   tool uses both. Still open: results the *model* can look at (an image),
   and background/detached tools.
5. ~~**The filesystem capability is a starter set.**~~ *Fixed upstream:*
   `edit_file`, `grep_files`, `stat_file`, and line-window reads now ship with
   the library, and this backend deleted its copies. Byte caps on reads are
   still absent.
6. ~~**Tool batches run sequentially.**~~ *Fixed upstream:* the in-process
   host dispatches a prepared batch concurrently, recording results in batch
   order. `InProcessExecutor::sequential()` is the opt-out for hosts that need
   policy to bite inside a batch.
7. ~~**No mid-turn input.**~~ *Fixed upstream and consumed:* the REPL reads
   stdin on its own thread, so a line typed while the agent works becomes
   steering (`Session::input()`) and one typed while it is idle starts the
   next turn. The operator does not have to know which state it is in.
8. ~~**MCP is stdio-only and unauthenticated.**~~ *Fixed upstream and
   consumed:* the backend attaches yolop's *effective* MCP servers from the
   same store `yolop mcp add` writes, mapping both transports, with bearer
   tokens read from the environment per request. Capabilities contributing
   MCP servers (agentyk's gap 13) is still open.
9. ~~**Reasoning effort is unvalidated.**~~ *Fixed upstream and consumed:*
   `YolopModelCatalog` exposes everruns' model profiles to agentyk, so
   `--engine agentyk` rejects an unsupported effort at composition with the
   same authority the shipping backend has — and the profile's context window
   and output ceiling become the context budget for free.

10. ~~**A turn could not be opened with an image.**~~ Found while consuming
    the above: `Session::run` took `impl Into<String>`, so `-i/--image` had
    nowhere to go even though tool *results* could carry images and both
    drivers mapped them. *Fixed upstream* (`impl Into<Message>`) and consumed:
    attachments ride on the message that asks about them. The lesson
    generalizes — a capability at one end of a pipeline is unusable until
    every entry point admits it, and only an adopter notices.

What worked without friction, and is worth recording as such: capabilities
composed by object (instructions, workspace tools) needed no ceremony;
middleware was the right shape for approval, and `ToolInvocation.definition`
made the metadata hints hatch reachable exactly where the decision is made;
the event stream was sufficient to build the whole terminal UI as a pure
observer; and `RealDiskFileSystem` plus the write blocklist gave a safe default
for free.

## Live evidence

Offline proof is the default here — the whole point of `--provider llmsim` —
but a backend that has never made a real provider call has not been tested,
only compiled. What live runs against Anthropic, GitHub's hosted MCP server,
and a real image confirmed:

- the filesystem tools (`grep_files`, `stat_file`, `read_file`, `edit_file`)
  and the sandboxed shell, driving an actual bug fix end to end;
- tool progress reaching the transcript **while** a command runs, and two
  shell calls whose output interleaved — concurrent dispatch, observed;
- an image opening a turn (`-i`) and being described correctly;
- MCP over HTTP with a bearer token, calling a tool on GitHub's hosted server;
- the model catalog rejecting `--reasoning-effort nonsense` at composition,
  naming the supported values.

Two defects surfaced that no offline test would have:

1. **agentyk's HTTP drivers trusted only bundled roots**, so no provider was
   reachable through this environment's inspecting proxy. Fixed upstream.
2. **A failing MCP server took down the run.** One server missing its token
   401'd, and because a capability's `tools()` error aborts the turn, the
   session died with it. Fixed here: MCP capabilities are wrapped best-effort,
   so an unreachable server costs its tools and announces itself, not the
   session. The wrapper is the backend's own policy, not the library's —
   agentyk failing loudly is defensible, and silently losing tools is its own
   bug, which is why the loss is reported once per process.

## Constraints

- The agentyk dependency tracks the library's `main` branch while agentyk is
  pre-release — the published `0.1.0` predates middleware, the engine crate,
  and the filesystem capability. It moves to a version requirement when
  agentyk publishes one that carries them.
- The backend must not grow yolop-specific workarounds for the findings above.
  When a gap makes something impossible, the backend does without it and the
  gap is recorded here. Papering over a missing seam would destroy the
  measurement this backend exists to take.

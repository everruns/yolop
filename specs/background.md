# Background execution — background tasks and background agents

Status: scripted background tasks, background sub-agents, and user surfaces
(status-bar indicator + `/background` command) implemented. Proactive wake
remains a follow-up phase.

## Why

A coding session often spawns work that should not block the foreground turn:

- **Scripted waits** — kick off a long command and react when it finishes. The
  canonical case is *waiting for CI*: `gh pr checks <pr> --watch` (or a poll
  loop) runs for minutes, then the agent reads the result and continues.
- **Background sub-agents** — when a request fans out (analyse this subtree,
  draft an integration, review a diff) it is cheaper and faster to spin off
  focused sub-agents than to do everything inline in one context window.

Today yolop has exactly one agent loop per session and every unit of work is
*turn-scoped*: a turn is `runtime.run_turn`, and cancellation is a hard
`JoinHandle::abort()`. Nothing survives the end of a turn, and nothing survives
a process restart except the conversation itself (replayed from
`events.jsonl`). Background execution is the missing primitive.

The design goal is a **single generic background-execution core** that both
scripted tasks and background agents are *kinds* of — not two parallel
mechanisms. yolop already has the right substrate for durability: every session
owns a private folder under `<data_dir>/yolop/sessions/<session_id>/` (see
`specs/` neighbours and `src/session_log.rs`). Background state lives there, so
it is naturally per-session, owner-only, and restorable on `--session` resume.

## Core abstraction

A **background task** is a unit of work that runs detached from the foreground
turn. It has an id, a kind, a lifecycle status, captured output, and is
cancellable and observable. The registry owns them:

- `BackgroundRegistry` — an `Arc`-shared handle holding the live task table and
  persisting an index to `<session_dir>/background/index.json`. One registry per
  session, constructed during `runtime::build` and shared with the capability.
- `BackgroundRecord` — the serialized, restart-survivable description of a task:
  `id`, `kind`, `label`, optional `command`, `status`, `created`/`updated`,
  optional `exit_code`, `summary`, and the `log_file` holding full output.
- `BackgroundKind` — `script` in v1; `agent` is the Phase 2 extension and reuses
  the same record, index, status model, and surfaces.

### Status lifecycle

`running → {completed | failed | cancelled | timed_out}`, plus `interrupted`
which is assigned on restore (below). `completed`/`failed`/`cancelled`/
`timed_out`/`interrupted` are terminal.

### Surviving a restart

The index is the durable record. On `BackgroundRegistry::load`:

- terminal tasks are restored verbatim — a `completed`/`failed` task's
  `exit_code`, `summary`, and full `log_file` are still readable after a
  restart, so the agent can act on a result produced before the crash;
- any task still marked `running` is re-labelled **`interrupted`**. Its OS
  process was a child of the previous yolop and did not survive — yolop does not
  pretend otherwise (this mirrors Claude Code, where background bash is not
  resurrected on resume). The model sees the interrupted task and can re-launch
  it. The corrected index is re-persisted immediately.

This is the honest contract: *results* survive a restart; *in-flight OS
processes* do not. Background **agents** are different — an agent is itself a
`session_id` with its own `events.jsonl`, so even an interrupted agent's
transcript is durable and its child session id is recorded for resume.

## Surfaces

### Tools

The `background` capability contributes:

- `background_run` — start a scripted task. `command` (required), `label`
  (optional human tag). Returns the new task id and status. Non-blocking.
- `background_agent` — spin off a sub-agent. `task` (required, a complete
  standalone instruction — the sub-agent does not see the parent conversation),
  `label` (optional). Returns the new task id. Offered only when the session can
  spawn agents (see [Sub-agents](#sub-agents)).
- `background_list` — list every task with its status and one-line summary.
- `background_output` — read a task's captured output by `id` (tail-capped),
  with its status and exit code.
- `background_cancel` — cancel a running task by `id`.

### Sub-agents

A sub-agent (`background_agent`) is a real child yolop session built with
`runtime::build` (the pattern the ACP server already uses to run N concurrent
runtimes), reusing the parent's workspace, live provider, and settings. The
registry runs one turn on a detached task and records the final assistant
message as the result; the child's full transcript lives in its own session
folder (`background_output` reports the child session id for `--session` resume).

The capability holds an injected `AgentSpawner` only in top-level sessions;
child sessions are built with sub-agent spawning disabled, so the
`background_agent` tool is absent there. That bounds sub-agent depth at one
level — a sub-agent cannot recursively spawn its own sub-agents.

### System-prompt disclosure

Like `memory`, the capability injects a compact `<background_tasks>` block each
turn listing current tasks (most-recent first, capped) with status and summary.
This is how a turn-based agent *learns a task finished*: it started the task,
ended (or continued) its turn, and on a later turn the disclosure shows
`completed`/`failed` with a pointer to `background_output`. Restart-survivable
because it is rebuilt from the persisted index. When there are no tasks the
block is omitted entirely.

### Output and limits

Each task streams stdout+stderr into `<session_dir>/background/<id>.log`
(owner-only) as it runs, so `background_output` can show partial progress while
the task is still running. A per-task output cap (256 KiB) and a wall-clock
safety ceiling (30 min, → `timed_out`) keep a runaway background command from
filling the disk or living forever; both are generous for the CI-wait case. The
same cap and ceiling apply to sub-agents (a `timed_out` sub-agent abandons its
child turn).

### User surfaces

Background work is visible to the *user*, not just the model:

- The TUI status bar shows a compact `bg <running>▸/<total>` segment whenever
  the session has background tasks (hidden otherwise). The `App` reads the
  shared registry (exposed on `BuiltRuntime`) each frame via a cheap `counts()`.
- `/background` is a `System` command (contributed by the capability) that lists
  every task with its kind, status, exit code, summary, and — for sub-agents —
  the child session id. It works over the TUI and ACP uniformly because it
  returns a plain `CommandResult`.

## Phased plan

1. **Scripted background tasks (implemented).** The core registry, persistence +
   restore, the script tools, and system-prompt disclosure. Delivers the CI-wait
   use case end to end.
2. **Background sub-agents (implemented).** The `background_agent` tool builds a
   child session and drives one turn on a detached task; the parent reads the
   child's result via `background_output`. Each sub-agent is a real session
   folder, so its transcript is resumable. Depth is bounded at one level.
3. **User surfaces (implemented).** A compact `bg` count in the TUI status bar
   and a `/background` command listing all tasks (see
   [User surfaces](#user-surfaces)). A richer interactive view (per-task peek,
   in-place cancel, a dedicated panel à la Claude Code's agent view) remains a
   possible enhancement, but the status indicator + command cover the
   at-a-glance and detail needs.
4. **Phase 4 — proactive wake.** Optionally inject a turn when a watched task
   finishes, instead of waiting for the user's next prompt, for true
   fire-and-forget CI monitoring.

## Non-goals (for now)

- No resurrection of an interrupted OS process — only its captured output and
  final state survive a restart.
- No cross-session background work — tasks belong to the session that spawned
  them and live in that session's folder.
- No second persistence engine — the per-session folder and atomic-write
  pattern already used by `session_log.rs` / `memory` are reused as-is.
- No new approval gate — `background_run` runs the same unsandboxed shell as the
  `bash` tool and inherits the same standing guardrails.

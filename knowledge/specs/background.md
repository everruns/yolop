---
type: Product Specification
title: Background execution
description: Defines the background execution contract for Yolop.
---

# Background execution

Status: consolidated onto Everruns primitives. Detached background work runs
through Everruns `spawn_background` and is tracked in Everruns `session_tasks`
(backed locally by `everruns-local`). Yolop owns no competing task registry; it
adds only the wake delivery seam and the `/background` command.

## Why

A coding session often spawns work that should not block the foreground turn.
The canonical case is *waiting for CI*: `gh pr checks <pr> --watch` (or a poll
loop) runs for minutes, then the agent should read the result and continue.

Yolop has one agent loop per session and every unit of work is *turn-scoped*: a
turn is `runtime.run_turn`. Background execution is the primitive that lets work
outlive a turn and then call the agent back when it finishes.

The design goal is a **single generic background-execution core**, not a
Yolop-specific one. That owner is Everruns `session_tasks` + `spawn_background`:
a generic runtime task registry (task ids, kind, state, result paths, wake
policy, messages) plus a meta-tool that runs any background-capable built-in
tool detached. Yolop rides that model rather than maintaining a parallel system.

## How it works

### Launching

`spawn_background` (Everruns `background_execution` capability) wraps any tool
whose `ToolHints::supports_background` is `Some(true)`. Yolop's `bash` tool
declares it, so the model runs detached shell work with:

```
spawn_background { tool: "bash", args: { command: "gh pr checks 42 --watch" } }
```

The run executes on a detached task, streams to a session-file log
(`/.background/<run_id>/output.log` in the VFS, backed by
`<session_dir>/.background/<run_id>/output.log`), writes a `result.json`, and
mirrors its lifecycle onto a `background_tool` session task (`Running` →
`Succeeded` / `Failed` / `Canceled`). It never creates `.background` in the
workspace. `signal_on_completion` defaults to `true`.
Foreground shell calls retain the short interactive deadline; detached shell
runs use a separate 24-hour wall-clock limit so CI watches can outlive a turn
without becoming unbounded host processes.

For delayed or recurring work, `spawn_background` accepts a `schedule` instead
of starting the wrapped tool immediately. Everruns persists the monitor and its
payload in the local SQLite store. Yolop starts one `LocalScheduleRunner` per
live host session; when a schedule becomes due, the runner sends the stored
monitor prompt through the same upstream `HostRoutedRunner` used by background
completions.
The resulting turn starts the wrapped tool, and that tool's eventual completion
produces the ordinary second wake. The TUI and ACP session objects retain the
runner handle for their lifetimes, preventing polling from outliving its host.
Before each poll, `everruns-local::WakeRoutes` reports the live session routes
in this process; the local scheduler scopes claims to that set. This prevents
inactive sessions' overdue schedules from being claimed and rejected on every
poll. Delivery is still routed by `SessionId`, and a route that closes after the
claim rejects the wake so the occurrence remains durable and retryable.

### Steering (poll-proofing)

Detaching a wait only saves tokens if the model actually chooses it, so the
harness steers there at the three places poll loops start, on any repo:

- The `background` capability's system prompt says waits on external events
  (CI, reviews, deploys) must not consume turns: detach one blocking watch,
  end the turn, let the completion wake continue — or `wait_task` it in
  one-shot mode, where there is no wake.
- `progress_guard` classifies external-event and session-task probes
  (`gh pr checks`, `gh run …`, `get_task`, bare/leading `sleep`) as *waiting*.
  Four waiting signals in a bounded eight-observation window inject a warning
  that names `spawn_background` as the fix. Mutations and validations clear the
  window, so a legitimate one-off status check around real work stays quiet.
- The `bash` tool's 120s-timeout error points foreground watches at
  `spawn_background` instead of leaving the model to fall back to
  sleep-and-recheck turns.

### Sub-agents

The `subagents` capability exposes the unified `spawn_agent` tool for linked
child sessions. A child inherits the parent's harness, agent, model, workspace,
and safety boundary but gets its own message history and context window. The
local runner creates the child in the shared session store and drives its turns
through the same `InProcessRuntime`; per-child turn locks serialize initial
instructions and task-completion messages.

Background fan-out is bounded twice: Everruns permits five active background
runs per session, and Yolop configures at most 32 active descendants across a
linked tree (with the upstream depth-two and total-task limits still applying).
This supports a two-level `4 × 4` hierarchy — four coordinators with four
workers each, 20 sub-agents total — without weakening the per-session guard.
Users can tighten these limits through the `subagents` capability config.

Concurrent children share the same working tree. Delegation prompts must assign
non-overlapping files or read-only analysis; the coordinator owns edits to
shared manifests, lockfiles, migrations, and final integration. Background
sub-agents remain parent obligations: the parent uses the session-task tools to
wait for every branch before it reports completion.

### Inspecting and controlling

The Everruns `session_tasks` capability exposes the model-facing tools:
`list_tasks`, `get_task` (returns state, summary, and `result_path` — read it
with the file tools), `cancel_task`, `message_task`, and `wait_task`. Yolop adds
one host-facing surface: the `/background` command (and the `Ctrl+B` TUI
activity rail / status-bar count) lists the session's task tree. Subagent-shaped
task records link their child sessions, so descendants are nested and each
branch rolls up the child sessions' token usage and best-effort USD cost. The
rail groups those agents separately from every other session task, including
detached commands and scheduled monitors, and is shared by both renderers. Arrow
keys select task rows, Page Up/Down moves through overflow, and `x` requests
cooperative cancellation. Status counts distinguish executing tools from
scheduled monitors, and the rail labels armed monitors as waiting rather than
running commands. Monitors still use `cancel_task`, because cancellation must
also disarm their durable schedule.

Scheduled monitors remain owned obligations after creation. Before reporting
that their parent work is complete, the agent cancels monitors whose purpose is
satisfied, superseded, or no longer relevant. Yolop wraps `cancel_task` so a
monitor is reported canceled only after its linked schedule is disabled; the
result distinguishes terminal `disarmed` cancellation from cooperative
`cancellation_pending` work that is still winding down.

### Proactive wake (the callback)

On completion `spawn_background`'s sink calls
`platform_store.send_message(session_id, <completion message>)`. Without a
platform store that call is a silent no-op — which is why finished background
work previously never reached the agent.

Due schedules enter through the same host boundary: `everruns-local` calls
`LocalSessionRunner::send_message(session_id, <stored monitor prompt>)`. The
single channel and host turn lock serialize schedule wakes, foreground prompts,
and background completions.

Yolop installs a platform store to close that gap (`crate::background_wake`):

- `runtime.rs` wires a `LocalPlatformStore` backed by
  `everruns-local::HostRoutedRunner` via `LocalBackends::with_platform_runner`.
  The upstream route registry hands live-host messages to Yolop's enrichment
  bridge and then `BuiltRuntime::background_wake`, so the host can stream the
  turn normally. Its inner Yolop runner resolves authenticated task handoffs and
  runs child sub-agent turns synchronously when no live host owns the session.
- The runner's `routable_session_ids` exposes only currently registered wake
  routes, which bounds local schedule claims to sessions this host process can
  actually wake.
- The **TUI** drains the channel from its idle event loop
  (`App::maybe_wake_from_background_channel`) and starts a streamed turn.
- The **ACP** server, whose request/response loop only runs turns while a client
  prompt is in flight, drains the channel from a push-based per-session task
  (`spawn_background_wake_drain`) that takes the same `turn_lock` as client
  prompts so a wake turn never overlaps one, and joins on connection teardown.
- Both frame the completion message as an `[automatic]` prompt
  (`frame_wake_prompt`): explicitly not a user message, pointing the model at the
  run's result before it continues. The host resolves the matching durable task
  snapshot and attaches provenance metadata that ordinary prompt text cannot
  forge.
- A completion turn receives a bounded model-view handoff instead of replaying
  the parent transcript prefix. It contains the active ask and goal, task
  identity and requested scope, terminal state, concise execution/validation
  summary, and result/log/artifact references. The suffix created during the
  wake turn remains intact across later reason/act iterations. Task summaries
  and specs are explicitly untrusted execution data, never new instructions.
- The lossless parent history, task record, `result.json`, and `output.log`
  remain durable and queryable through `query_history`, task tools, and session
  file reads. If task resolution, summary validation, or handoff decoding fails,
  the provider view falls back to ordinary full history rather than guessing.
- Automatic prompts never replace the tracked user ask or reset its completion
  budget. A tool-using turn with active detached work is classified
  waiting-on-background; the eventual wake resumes that same ask.
- Both coalesce completions already queued at the same idle boundary into one
  automatic prompt in notification order. All resolved task snapshots share
  one bounded handoff; a mixed or unusually large burst falls back safely and
  the durable task registry remains authoritative.
- Opt-out: the `proactive_wake` setting (on by default) suppresses the auto-turn
  and surfaces a one-line notice instead.
- `--print` is one-shot, so it does not auto-wake.

## Durability and restart

Session tasks and their `result.json` / `output.log` artifacts live in
`everruns-local` (SQLite) and the session file store, so results survive a
restart. In-flight OS processes do not: a run whose worker died is not resumed
unless an orphan reaper re-attaches it (Yolop runs none, so non-reattachable
runs simply stop). The wake is a live, in-process signal — a completion that
happened while Yolop was down is observed on the next `/background` / `get_task`,
not replayed as a wake.

Schedules are different: their trigger state is durable. The local runner uses
atomic, leased SQLite claims, advances recurring schedules only after delivery,
and disables successful one-shot schedules. A due schedule that could not fire
while Yolop was down is claimed after the session is loaded again; stale claims
from an interrupted runner become retryable after the upstream claim timeout.

## Safety

`spawn_background{bash}` runs through the same sandbox provider as the `bash`
tool, so it inherits both containment and approval policy; there is no separate
background approval gate. The implemented [`sandboxing`](./sandboxing.md)
boundary routes foreground, direct, and background shell execution through one
environment so detached work cannot bypass isolation. Concurrency is bounded
by Everruns' per-worker / per-session background-run limits.

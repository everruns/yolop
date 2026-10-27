---
type: Product Specification
title: Background execution
description: Defines the background execution contract for Yolop.
---

# Background execution

Status: consolidated onto Everruns primitives. Detached background work runs
through Everruns `spawn_background` and is tracked in Everruns `session_tasks`
(backed by native Engine SQLite hosting). Yolop owns no competing task registry; it
adds only the wake delivery boundary and the `/background` command.

## Why

A coding session often spawns work that should not block the foreground turn.
The canonical case is *waiting for CI*: `gh pr checks <pr> --watch` (or a poll
loop) runs for minutes, then the agent should read the result and continue.

Yolop has one agent loop per session and every unit of work is *turn-scoped*: a
turn is executed by its registered Engine session. Background execution is the primitive that lets work
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
of starting immediately. Native hosting persists the monitor and payload and
owns its leased schedule runner. The process's retained native supervisor
executes admitted monitor inputs. The resulting turn starts the wrapped tool;
its completion follows the ordinary task handoff path. Frontend wake channels
observe this work and do not authorize schedule execution.

### Steering (poll-proofing)

Detaching a wait only saves tokens if the model actually chooses it, so the
harness steers there at the three places poll loops start, on any repo:

- The `background` capability's system prompt says waits on external events
  (CI, reviews, deploys) must not consume turns: detach one blocking watch,
  end the turn, and let the completion wake continue. Print mode also
  waits for completion, without model polling; `wait_task` remains available.
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
native platform service creates another registered Engine session. Native
admission and epochs serialize initial instructions and subsequent messages.

Background fan-out is bounded twice: Everruns permits five active background
runs per session, and Yolop configures at most 32 active descendants across a
linked tree (with the upstream depth-two and total-task limits still applying).
This supports a two-level `4 × 4` hierarchy, four coordinators with four
workers each, 20 sub-agents total, without weakening the per-session guard.
Users can tighten these limits through the `subagents` capability config.

Concurrent children share the same working tree. Delegation prompts must assign
non-overlapping files or read-only analysis; the coordinator owns edits to
shared manifests, lockfiles, migrations, and final integration. Background
sub-agents remain parent obligations: the parent uses the session-task tools to
wait for every branch before it reports completion.

### Inspecting and controlling

The Everruns `session_tasks` capability exposes the model-facing tools:
`list_tasks`, `get_task` (returns state, summary, and `result_path`, read it
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
platform store that call is a silent no-op, which is why finished background
work previously never reached the agent.

Yolop's platform adapter authenticates completion text against the durable task
registry and builds a bounded handoff with task ID, attempt, state, summary and
artifact references. Task content remains untrusted. The adapter calls Engine
`send_with_id` with a stable identity derived from the task's terminal occurrence.
A repeated signal cannot create another admission. Artifact paths resolve to
physical display paths so both file tools and shell reads can open them.

Engine classifies a fast completion as steering or a later turn. TUI, print and
ACP consume observation notifications for that accepted input, correlate streams
with its assignment and wait for its outcome. They do not resubmit it. An input
steered into a foreground turn produces a handled notice rather than another
model turn. A terminal task already inspected through task tools needs only a
notice. Completion review waits for owed native admissions and task handoffs.

The notice names the task and result even if no model text follows. ACP serializes
presentation with client prompts and reconciles terminal tasks on each later
user request. Startup and lifecycle reads recover pending admission observation
when a lossy event listener misses it. Ordinary user text cannot forge host
handoff provenance. Automatic inputs retain the parent's conversation context
and use the shared [completion controller](task-completion.md).

The `proactive_wake` opt-out prevents automatic task admission and sends a notice.
The next user turn reconciles unreported terminal work. Print mode waits for
pending execution and accepted completion inputs before reporting success, with
a ten-minute host wait bound. Cancellation, disabled proactive wake or exhausted
waiting reports an explicit error. A notification batch cannot coalesce distinct
already-admitted inputs into a replacement execution command.

## Durability and restart

Native authority events, task records and private artifacts survive restart.
The retained supervisor rediscovers admitted runnable work. OS subprocesses
whose owner died are not blindly relaunched; Framework task recovery settles
non-reattachable work. Terminal task inspection and ACP reconciliation recover
results whose completion notification was not admitted before process loss.

Schedules retain leased occurrence state, advance recurring schedules after
successful delivery and disable delivered one-shot schedules. The framework
uses a stable occurrence admission identity across delivery and acknowledgement
retries, including the correction tracked by
[EVE-1309](https://linear.app/everruns/issue/EVE-1309).

Native foreground subagent waits use committed turn outcomes and settle their
tasks directly, including the correction tracked by
[EVE-1310](https://linear.app/everruns/issue/EVE-1310). Bare idle does not imply
success. Yolop adds no local outcome remapping.

## Safety

`spawn_background{bash}` runs through the same sandbox provider as the `bash`
tool, so it inherits both containment and approval policy; there is no separate
background approval gate. The implemented [`sandboxing`](./sandboxing.md)
boundary routes foreground, direct, and background shell execution through one
environment so detached work cannot bypass isolation. Concurrency is bounded
by Everruns' per-worker / per-session background-run limits.

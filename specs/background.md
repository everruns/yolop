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
(`/.background/<run_id>/output.log`), writes a `result.json`, and mirrors its
lifecycle onto a `background_tool` session task (`Running` → `Succeeded` /
`Failed` / `Canceled`). `signal_on_completion` defaults to `true`.

### Steering (poll-proofing)

Detaching a wait only saves tokens if the model actually chooses it, so the
harness steers there at the three places poll loops start, on any repo:

- The `background` capability's system prompt says waits on external events
  (CI, reviews, deploys) must not consume turns: detach one blocking watch,
  end the turn, let the completion wake continue — or `wait_task` it in
  one-shot mode, where there is no wake.
- `progress_guard` classifies external-event probes (`gh pr checks`,
  `gh run …`, bare/leading `sleep`) as *waiting* and, on consecutive repeats,
  injects a warning that names `spawn_background` as the fix.
- The `bash` tool's 120s-timeout error points foreground watches at
  `spawn_background` instead of leaving the model to fall back to
  sleep-and-recheck turns.

Sub-agents are intentionally **not enabled**. Everruns 0.17.7 can run linked
sub-agents in the background and spawn detached peer sessions, but both require a
local session runner that can create and drive child sessions. Yolop's runner is
deliberately narrower: it only delivers completion wakes for background tools.
Registering the upstream sub-agent capability without a real child-session host
would advertise tools that fail at execution time. `spawn_background` therefore
remains Yolop's detached-work primitive.

### Inspecting and controlling

The Everruns `session_tasks` capability exposes the model-facing tools:
`list_tasks`, `get_task` (returns state, summary, and `result_path` — read it
with the file tools), `cancel_task`, `message_task`, and `wait_task`. Yolop adds
one host-facing surface: the `/background` command (and the `Ctrl+B` TUI panel /
status-bar count) lists the session's tasks via
`session_tasks_view::render_task_list`.

### Proactive wake (the callback)

On completion `spawn_background`'s sink calls
`platform_store.send_message(session_id, <completion message>)`. Without a
platform store that call is a silent no-op — which is why finished background
work previously never reached the agent.

Yolop installs a platform store to close that gap (`crate::background_wake`):

- `runtime.rs` wires a `LocalPlatformStore` backed by a `WakeRunner` via
  `LocalBackends::with_platform_runner`. The runner's `send_message` does **not**
  run a turn synchronously (the `LocalSessionRunner` contract's synchronous mode
  only fits child sub-agent sessions; running a turn there would re-enter the
  session from a detached task and bypass the host's streaming turn loop).
  Instead it hands the completion message to the host over an unbounded channel
  (`BuiltRuntime::background_wake`).
- The **TUI** drains the channel from its idle event loop
  (`App::maybe_wake_from_background_channel`) and starts a streamed turn.
- The **ACP** server, whose request/response loop only runs turns while a client
  prompt is in flight, drains the channel from a push-based per-session task
  (`spawn_background_wake_drain`) that takes the same `turn_lock` as client
  prompts so a wake turn never overlaps one, and joins on connection teardown.
- Both frame the completion message as an `[automatic]` prompt
  (`frame_wake_prompt`): explicitly not a user message, pointing the model at the
  run's result before it continues.
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

## Safety

`spawn_background{bash}` runs the same unsandboxed shell as the `bash` tool, so
it inherits that tool's approval policy; there is no separate background approval
gate. Concurrency is bounded by Everruns' per-worker / per-session background-run
limits.

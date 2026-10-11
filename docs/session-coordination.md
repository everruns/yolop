# Session coordination

A coordinator session hands independent pieces of work to threads and follows
them to completion. The model is the everruns `coordination` capability, the
same one Everruns Platform Chat runs. Yolop's part is where a thread runs: each
thread is its own headless Yolop process, so threads keep working while the
coordinator is closed, and you can open any of them like an ordinary session.

## Start a coordinator

Enable the capability in a named profile:

```toml
instructions = "Triage incoming requests. Hand each independent piece of repository work to a thread, then follow it through to completion."

[[capabilities]]
ref = "coordination"
```

Then start a session with it, in a terminal or an editor:

```bash
yolop -C /path/to/repo --profile coordinator
yolop --acp --profile coordinator
```

For Zed or Paseo, run `yolop into zed` or `yolop into paseo`, then add
`"--profile", "coordinator"` to the generated `args`.

## Threads and assignments

The coordinator gets these tools:

- `start_thread` starts a thread with a brief.
- `message_thread` sends it a follow-up.
- `list_threads` and `get_thread` show each thread's state, checklist and
  recent messages.
- `resolve_thread` closes a thread whose work is done.

Each brief is an assignment the coordinator owns. While a thread works on one,
it has its own tools:

- `update_checklist` keeps its progress steps current.
- `complete_assignment` reports the result with a summary and artifacts.
- `ask_decision` asks a question with up to four options and a recommended one.
- `report_to_coordinator` and `redirect_to_coordinator` send it a note.

Every change to an assignment wakes the coordinator with an automatic update,
and the coordinator reads the thread before acting on it. A thread turn that
ends without completing its assignment is flagged as needing attention, unless
the thread is only waiting on background work it started.

A thread runs the coordinator's executable with its profile, provider, model,
and sessions directory. In a Git repository each thread works in its own
worktree on its own branch, so threads never edit the same checkout; the
thread names its branch when it completes, and the coordinator reviews and
brings in the change. A profile with `worktrees = "off"` keeps threads in the
coordinator's checkout.

## Watch the board

In the terminal, a coordinator starts with its activity rail open as the threads
board, and a `THREADS` section fills in as threads start. `Ctrl+B` focuses or
closes it. Each thread shows its current assignment, where it
stands (working, needs you, ready for review, closed) and its checklist
progress, such as `2/5 steps`. Select a thread and press `Enter` to open it;
quitting the thread brings you back to the coordinator.

## Decisions

A thread that hits a real choice calls `ask_decision`, and its coordinator
asks you. In the terminal that is a question with its options. In an editor
that supports ACP form elicitation it is a form with the options to pick from;
an editor without it gets the question in the conversation.

A thread that needs to cross the sandbox (network access, a write outside its
worktree) asks through its coordinator too: the coordinator's terminal shows
the usual approval prompt, naming the thread. An approval for the session
covers the thread's session only. A coordinator in an editor or in `--print`
mode cannot show that prompt, so it denies, as it does for its own commands.

Threads belong to the coordinator that started them. Several coordinators can
run at once, and none of them sees or steers another's threads.

## Restarts

Threads, assignments, checklists and pending messages live in the local
sessions database, so coordination survives the usual interruptions:

- **The coordinator closes or restarts.** Threads keep working. Resume the
  coordinator with `yolop --session <id> --profile coordinator`; updates it
  missed are delivered then.
- **A thread's process stops.** A thread with a message waiting is started
  again by its coordinator. A thread that stopped in the middle of an
  assignment is restarted with a note to check where its work stands. Commands
  the stopped process was running (builds, tests, servers, including ones it
  started in the background with `&`) are stopped first, so they do not race
  its successor in the same worktree.
- **A thread is idle.** Its process exits after ten minutes with nothing to do
  and starts again when the coordinator messages it.
- **A woken turn fails** (provider error, quota, stalled stream). A failed
  thread turn fails its assignment, and the coordinator decides what to do. A
  plain message is offered again after a backoff, three attempts in all.

## Open a thread

`yolop sessions live` lists the sessions running now and, for threads, their
coordinator. Open one with `yolop --session <id>`. A thread's background host
finishes the turn it is running and hands the session over; messages for the
thread are then delivered to the session you opened.

## Message any session

Any session can message any other, coordinated or not. Inside a session, or
from a shell:

```bash
yolop sessions live
yolop sessions send <session-id> "Rebase on main before you push."
```

A live session receives the message on its next idle moment. A message for a
session that is not running waits until that session next starts. A message
sent from a session tells the receiver who sent it and how to answer.

## Delivery and safety

Presence uses short SQLite leases, so a crashed process drops out without a
central daemon. Messages are acknowledged only when the turn they woke
finishes, so a process that dies first leaves them for its next run. Automatic
coordination wakes are not disabled by the background-only `proactive_wake`
preference.

Coordination does not widen authority. A thread has the coordinator's profile,
sandbox and approval policy. A headless thread cannot show approval prompts,
so a sandbox escalation is put to its coordinator, as described under
Decisions. A request nobody answers is denied after thirty minutes, or as soon
as you open the thread, where you can retry it. Everything stays in one user-owned sessions database,
with no network listener.

On Linux, running sessions start threads and control calls through
`/proc/<pid>/exe`, so upgrading Yolop under them keeps them on the binary they
started with until they restart.

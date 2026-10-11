---
type: Product Specification
title: Session coordination
description: Defines coordinator threads hosted as separate Yolop processes, and messaging between sessions.
---

# Session coordination

Status: implemented on the everruns `coordination` capability. Yolop hosts each
thread as its own headless process and carries messages between sessions
through a durable local inbox.

## Ownership

The coordination model is everruns', not Yolop's: threads, assignments, the
coordinator and worker tools, wake text, and the rule that settles a thread
turn all come from `everruns_capabilities::capabilities::coordination`, the
capability Platform Chat runs. A profile enables it with
`ref = "coordination"`. Yolop must not grow a parallel dispatch protocol; a
missing seam is an upstream change.

Yolop owns three things:

- where a thread runs (a headless `yolop --serve` process per thread);
- the transport between processes (presence leases and the inbox);
- `yolop sessions live` and `yolop sessions send`, the generic messaging
  surface any session or shell can use.

Implementation: `src/capabilities/session_coordination.rs`, routing in
`WakeRunner` (`src/runtime/background_wake.rs`), the serve loop in
`src/main.rs`.

## Threads out of process

Upstream starts and drives a thread only through the session's sub-agent
delegate (`create_session_with_options`, `send_message`, `get_messages`,
`get_session_status`). Yolop's `WakeRunner` is that delegate. For a session
that is a thread, it routes `send_message` to the thread's inbox, reads
messages from the thread's session log, and reports status from presence.

A session becomes a thread the first time its coordinator messages it: the
coordinator has an `assignment` task linking the child. The relationship is
recorded in `yolop_threads`, because the local session store keeps
`parent_session_id` in memory only. The thread host sets its own
`parent_session_id` from that record, which is what makes upstream inject the
worker tools.

A thread is launched with the coordinator's executable (`/proc/<pid>/exe` on
Linux), workspace, and an allowlist of inherited flags (profile, provider,
model, reasoning effort, config, data and session directories, environment
context), in its own process group.

In a Git repository a thread session resolves the default `auto` worktree mode
to `always`, because threads run side by side on one repository; an explicit
`off` is honored. The thread's environment context names its coordinator and,
with a worktree, its path and branch, and tells it to name the branch in
`complete_assignment` so the coordinator can review and bring it in.

## Messages and wakes

Every cross-process message is an inbox row, acknowledged only when the turn
it woke finishes:

- a brief or relay for a thread (raw text);
- an assignment update for its coordinator, produced by a
  `TaskTransitionObserver` on the coordinator's task registry using upstream's
  `wake_text_for`, framed as automatic and not a request from the person;
- a plain message from `yolop sessions send`, framed with its sender and how
  to answer.

A host delivers one message per idle moment as a wake with host-only delivery
metadata, so user and editor input cannot forge it. A host acknowledges only
deliveries it made. A message claimed by a host that dies first is offered to
the next incarnation of that session. A failed turn on a plain message is
retried after a growing backoff, three attempts in all. A failed thread turn is
not retried: it fails the assignment through `settle_thread_turn`, and the
coordinator decides.

The coordinator's own process never wakes itself through the inbox for its
own transitions.

## Settling thread turns

Only turns an inbox delivery started are settled, with upstream's
`settle_thread_turn`. A turn the person starts in an opened thread is theirs.
A turn that ends while the thread's own background work is pending is not
"ended without completing"; the completion wake continues it. Without that
exception, a thread waiting on a long build bounced its coordinator.

## Supervision and restarts

Every host heartbeats a presence lease (two seconds, fifteen-second lease) and
marks itself offline on drop. A coordinator's host supervises its threads:

- a thread with pending messages and no live host is started, at most once
  per thirty-second backoff;
- a thread with no live host whose latest assignment is still running and not
  waiting on input is restarted with a note to check what survived.

Before restarting anything, supervision stops the commands a dead headless
host left running. Each `--serve` host records the process group of every bash
command it starts, with the leader's start time, in a per-process ledger under
`<sessions>/.thread-procs/`, and forgets it when the command ends. A ledger
whose host process is gone (pid absent or start time different) has each
group whose leader still matches its recorded start time killed, then is
deleted. Start times guard against signalling a reused pid. Interactive hosts
keep no ledger: a crashed TUI's processes are the person's to manage, while a
thread is restarted automatically and its leftovers would race the successor.
A group whose leader already exited cannot be verified and is left alone.

A headless host exits after ten idle minutes unless background work is
pending. A coordinator that restarts keeps its threads, assignments, and
missed updates, all in the private WAL-mode `everruns-local` database under
the sessions directory.

## Opening a thread

Only one process may write a session log. A person opening a session held by a
headless host (`yolop --session <id>`) sets a release request on the host's
presence row. The host stops taking deliveries, finishes its current turn, and
exits; the opener waits for the lease to end (up to two minutes) and holds off
the coordinator's restart of that thread meanwhile. An interactive host never
yields.

## Board and decisions

The TUI activity rail turns a coordinator's assignment tasks into a `THREADS`
section, one row per thread (its latest assignment) in the board's words, with
checklist progress. Assignment rows never descend into the thread's session,
which lives in another process. A thread's `ask_decision` puts its assignment
in needs-input; the coordinator then asks the person with `ask_user`, which
the TUI shows as a question and ACP as a form elicitation where the editor
supports it (see [ACP](acp.md)).

## Isolation

Upstream scopes every coordinator tool by parent session, so several
coordinators can run against one sessions database without seeing or steering
each other's threads. Supervision only touches threads recorded under the
supervising coordinator.

## Safety constraints

- Coordination never widens authority: a thread has the coordinator's profile,
  sandbox, and approval policy.
- A headless host has no prompt surface, so sandbox escalations are denied as
  in `--print`.
- Message text is bounded (32 KiB) before storage or prompt construction.
- Everything is local to one user-owned database; there is no network
  listener.
- Background `proactive_wake` controls background-task convenience only;
  coordination wakes always wake an idle target.

## Known gaps

- Background work started with `&` inside a command that then returned has no
  verifiable group leader, so a dead host's leftovers of that shape survive.
- A headless thread denies sandbox escalations that need an approval prompt;
  opening the thread is the way to approve one.
- Selecting a thread on the board does not open it; `yolop --session <id>`
  does.

## Verification bar

Tests cover thread recognition from an assignment, supervision restarts with
and without pending work, the restart note, inbox acknowledgement and
redelivery, plain-message retry, settling only this host's deliveries, the
"ended without completing" and failed-turn outcomes, the background-work
exception, the coordinator not waking itself, yield-on-open, the
`sessions live` and `sessions send` CLI, stopping a dead host's command groups
(and never a live host's or a reused pid's), a thread's own worktree, the
`THREADS` board, and a decision answered through an ACP form end to end.

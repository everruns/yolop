---
type: Product Specification
title: Session coordination
description: Defines local coordinator and worker session orchestration.
---

# Session coordination

Status: first local transport implemented. Live opt-in workers can receive
durable assignments and return explicit completion to a coordinator. Worker
process supervision is not yet owned by Yolop.

## Ownership

`session_coordination` is one capability boundary. It owns role policy,
presence, worker selection, typed control and CLI actions, inbox prompts,
interpretation, and completion. The host supplies the existing session-task
registry, local SQLite handle, workspace identity, and generic wake channel.

There is no second task system. Each dispatch is a `session_dispatch` task
owned by the coordinator's ordinary `SessionTaskRegistry`. The coordination
tables add routing state only.

## Roles and activation

The capability config is strict:

- `role` is `coordinator`, `worker`, or `both`.
- `accept_work` is a boolean.

The default harness enables `worker` with `accept_work = false`. This makes the
surface available without silently enrolling interactive sessions. A named
profile is the intended way to define a standing coordinator or worker.

The capability contributes no model tools. Coordinator and worker agents use
the ordinary foreground Bash tool to invoke `yolop coordination ...`, matching
extension administration and the shared attached-control contract. The
contributed CLI covers `list`, `status`, `dispatch`, `complete`, `accept`, and
`drain`; `/coordination` parses the same action grammar. Multiword payload
fields consume unquoted words until the next option, preserving the control
plane's conservative direct-invocation grammar.

The attached host derives session identity and role. `dispatch` requires a
coordinator or combined role, `complete` requires a worker or combined role and
an active assignment, and availability changes operate only on the attached
host. Detached execution is read-only and may only list presence. Attached
lists are project-scoped; detached lists are an operator view across projects.

## Identity and selection

A worker row is keyed by `SessionId` and leased to a random host incarnation.
The project identity is the canonical Git common directory, so linked
worktrees share a pool. Non-Git workspaces use their canonical root. Only
same-project, live, opted-in, idle sessions are eligible, and a coordinator
cannot select itself.

Selection is deterministic after heartbeat ordering and reserves the worker
with a compare-and-set update. An explicit target goes through the same checks.
One worker therefore cannot accept two concurrent assignments even when two
coordinators race.

## Protocol

Dispatch performs these state changes:

1. Create a queued `session_dispatch` task under the coordinator.
2. Reserve one eligible worker.
3. Persist the assignment route and inbox message in one SQLite transaction.
4. Mark the task running with its worker ID.

The worker host polls its durable inbox and injects a typed automatic wake. It
does not interrupt an active turn. The message carries the assignment ID,
coordinator ID, title, and requested work. Ordinary user text cannot create
this host-only wake provenance.

The worker must invoke `yolop coordination complete` with a terminal status,
summary, validation evidence, and bounded artifact references. The typed
control action settles the coordinator's task, releases the worker, and
persists a completion inbox message. The coordinator receives that message
through the same generic wake channel and can inspect the authoritative task.

An unfinished assignment is redelivered across host incarnations. A message
records the host incarnation that claimed it, so the same host does not receive
it twice and a restarted worker can continue. Once the assignment is terminal,
its worker message is no longer eligible. Completion wakes are delivered once;
after that, the coordinator's terminal task is the durable recovery source.
Completion actions must therefore be explicit and terminal transitions must
remain idempotent or fail visibly.

## Lifecycle

Hosts heartbeat every two seconds and leases expire after fifteen seconds.
Graceful drop marks the host offline immediately. Presence and inbox state use
the private WAL-mode `everruns-local` database under the sessions directory.

The capability does not launch worker processes in this version. Starting a
worker requires lifecycle ownership for process identity, profile selection,
logs, restarts, worktree cleanup, and orphan recovery. Until that owner exists,
`yolop coordination dispatch` fails when no eligible live worker is available
instead of creating an untracked process.

## Safety constraints

- Coordination never expands the worker's filesystem, sandbox, approval, or
  capability authority.
- Workers must opt in, and draining prevents new reservations without
  interrupting an active assignment.
- Request, summary, validation, and artifact sizes are bounded before durable
  storage or prompt construction.
- Presence is local to one user-owned sessions database. There is no network
  listener or session-ID based remote mutation surface.
- Background `proactive_wake` controls background-task convenience only.
  Authenticated assignments and completions always wake an idle target.

## Verification bar

Tests cover strict config parsing, absence of model tools, role authorization,
same-project atomic reservation, CLI decoding into typed control actions, real
task registry transitions, assignment and completion inbox delivery, and
restart redelivery. Runtime tests must also prove automatic wake framing. The
contributed CLI receives a binary smoke test.

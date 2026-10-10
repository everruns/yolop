---
type: Product Specification
title: Session coordination
description: Defines local coordinator and worker session orchestration.
---

# Session coordination

Status: first local transport implemented. Live opt-in workers can receive
durable assignments and return explicit completion to a coordinator. Worker
processes are started by the operator from launch plans the capability
generates (see Lifecycle); the capability routes to those live workers,
records parent and child session IDs on every assignment, and lets the owning
coordinator manage running work with cancel.

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
the ordinary Bash tool to invoke `yolop coordination ...`, matching
extension administration and the shared attached-control contract. The
contributed CLI covers `list`, `status`, `dispatch`, `complete`, `cancel`, `accept`,
`drain`, and `spawn-workers`; `/coordination` parses the same action grammar.
The `yolop_spawn` command routes a spawn request to the right primitive: mode
`agent` delegates to the platform `spawn_agent` for an in-process child, mode
`session` plans separate worker sessions (own model, provider, worktree). Multiword payload
fields consume unquoted words until the next option. Ordinary shell quoting and
composition are also supported by the attached endpoint.

The attached host derives session identity and role. `dispatch` requires a
coordinator or combined role, `complete` requires a worker or combined role and
an active assignment, `cancel` requires a coordinator or combined role and
ownership of the running assignment, and availability changes operate only on
the attached host. Detached execution is side-effect free and may list presence
or plan worker launches with `spawn-workers`. Attached
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
coordinators race. Each assignment records its owner as the parent session and
its worker as the child session, so roots can list and manage their own
children through status views and cancel.

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

Every inbox message (assignment, completion, cancellation) stays pending until
the turn it woke finishes. The wake input carries the message ID in host-only
metadata; the host follows that input to its turn and acknowledges the message
on completion or a user cancel. A message records the host incarnation that
claimed it, so one host never receives it twice while a restarted host (the
same session resumed with `--session`) receives whatever was claimed but never
acknowledged. A failed turn hands the message back after a growing backoff,
for three attempts in all; an assignment still failing then fails back to its
coordinator with the last error, and a completion is acknowledged as given up,
leaving the coordinator's terminal task as the record. Once an assignment is
terminal, its worker message is no longer eligible.

Settling is a compare-and-set on the running assignment, so exactly one of
completion, cancel, or orphan failure wins and notifies the coordinator. The
loser fails visibly: a late `complete` is refused rather than overwriting the
coordinator's task.

The completion carries the worker's registered workspace root, because workers
normally run in their own worktree and the coordinator has to review and bring
their change over.

## Lifecycle

Hosts heartbeat every two seconds and leases expire after fifteen seconds.
Graceful drop marks the host offline immediately. The host is owned by the
runtime handles, which every frontend (TUI, ACP, print) keeps for the session
lifetime; owning it anywhere a frontend destructures away leaves the session
offline from its first second.

A coordinator host fails its running assignments whose worker has held no lease
for two minutes (`worker_offline`), longer than the lease so a worker resumed
with `--session` keeps its assignment. The coordinator wakes with the reason
and the resume command. Presence and inbox state use
the private WAL-mode `everruns-local` database under the sessions directory.

The capability plans worker launches but does not supervise processes.
`yolop coordination spawn-workers` generates one launch command per worker
with the coordinator's chosen profile, provider, and model (up to 10 per
call); starting those processes stays with the operator, so presence only ever
reports workers that actually heartbeat. `yolop coordination dispatch` still
fails when no eligible live worker is available instead of creating an
untracked process. Full supervision with owned restarts, logs, and worktree
cleanup waits for a durable worker lifecycle.

Sessions re-execute their own binary for the `yolop` shim, the sandbox worker,
and attached control children. On Linux that goes through `/proc/<pid>/exe`, so
an upgrade that replaces the file under running sessions leaves them on the
image they started with, protocol included, until restarted.

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
- Only message IDs the host itself delivered are settled from turn outcomes;
  user and editor input cannot set the delivery metadata.

## Model guidance

Sessions with an opted-in role get a short prompt block; ordinary drained
sessions get none. A coordinator learns the live idle and busy worker counts,
to prefer `dispatch` over in-process `spawn_agent` children that share its
checkout, to brief workers with repository-relative paths, and to verify and
bring in work from the worker's workspace. Without it, models defaulted to
in-process children even with workers idle.

## Verification bar

Tests cover strict config parsing, absence of model tools, role authorization,
same-project atomic reservation, CLI decoding into typed control actions, real
task registry transitions, assignment and completion inbox delivery,
acknowledgement and restart redelivery, failed-turn retry and hand-back, orphan
failure, refusal of a late completion, delivery correlation that ignores
foreign IDs, and presence surviving the way frontends destructure the runtime. Runtime tests must also prove automatic wake framing. The
contributed CLI receives a binary smoke test.

# Session coordination

Yolop can coordinate work across independently running local sessions. The
`session_coordination` capability provides worker discovery, durable dispatch,
and an explicit completion path back to the coordinator.

The feature is intentionally local and opt-in. A worker must be running, share
the same sessions directory and Git repository, and advertise that it accepts
work. Git worktrees from the same repository are treated as one project.

## Configure roles

Use named profiles to give sessions a stable role. A coordinator profile can
contain:

```toml
instructions = "Triage incoming requests. Dispatch actionable repository work to an available local worker, then follow the durable task through completion."

[[capabilities]]
ref = "session_coordination"
role = "coordinator"
accept_work = false
```

A worker profile can opt in at startup:

```toml
[[capabilities]]
ref = "session_coordination"
role = "worker"
accept_work = true

worktrees = "always"
```

Start the sessions against the same repository and sessions directory:

```bash
yolop -C /path/to/repo --profile worker
yolop -C /path/to/repo --profile coordinator
```

Every ordinary session has the capability in worker mode but starts drained,
so it cannot receive work until it opts in.

## Operate the pool

Inside a running session, `/coordination list`, `/coordination status`,
`/coordination accept`, and `/coordination drain` inspect or change local
availability. The contributed CLI has the same grammar:

```bash
yolop coordination list
yolop coordination list --json
```

When invoked by a running Yolop session through foreground Bash, status and
availability changes use the attached control channel. A detached shell may
list live sessions, but it cannot mutate one by guessing a session ID.

The coordinator receives `list_workers` and `dispatch_work`. Dispatch reserves
one idle worker atomically and creates a `session_dispatch` task in the
coordinator's existing task registry. The worker receives an authenticated
automatic prompt and must call `complete_assignment`. Completion settles the
task and wakes the coordinator with the durable result.

If no eligible worker is live, dispatch fails visibly. This version does not
launch a new operating-system process. Process supervision and pool sizing are
kept outside the capability until Yolop has a durable worker lifecycle that can
own restarts, logs, and cleanup.

## Delivery and safety

Presence uses short SQLite leases, so crashed workers disappear without a
central daemon. Assignment and completion messages use a durable SQLite inbox.
Restarting the target session redelivers unfinished messages to the new host.
Automatic coordination wakes are not disabled by the background-only
`proactive_wake` preference.

Coordination does not widen authority. A worker keeps its own workspace,
sandbox, approval policy, profile instructions, and capability set. The
coordinator can target only a live, opted-in session in the same local project.

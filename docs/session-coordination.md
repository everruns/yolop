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

A worker profile can opt in at startup. Top-level keys such as `worktrees` go
above the first `[[capabilities]]` table; TOML would otherwise read them as
part of that capability:

```toml
worktrees = "always"

[[capabilities]]
ref = "session_coordination"
role = "worker"
accept_work = true
```

Start the sessions against the same repository and sessions directory:

```bash
yolop -C /path/to/repo --profile worker
yolop -C /path/to/repo --profile coordinator
```

The same profiles work in an editor over ACP: `yolop --acp --profile
coordinator`. For Zed or Paseo, run `yolop into zed` or `yolop into paseo`, then
add `"--profile", "coordinator"` to the generated `args`.

Every ordinary session has the capability in worker mode but starts drained,
so it cannot receive work until it opts in.

A coordinator profile's session is told how many workers are live and to
prefer dispatching to them over in-process `spawn_agent` children, which share
its checkout. Workers run in their own worktrees, so a completion names the
worker's workspace; the coordinator reviews the change there and brings it
into its own checkout.

## Operate the pool

Coordination administration is CLI-first and contributes no model tools. Inside
a running session, invoke `yolop coordination ...` through Bash. A local
session endpoint carries raw argv to the host, whose exact Yolop executable
parses and dispatches the typed request to the live capability.

The CLI covers discovery, worker planning, dispatch, completion, cancellation, and worker availability:

```bash
yolop coordination list
yolop coordination list --json
yolop coordination status
yolop coordination dispatch --title Inspect parser --request Inspect parser behavior and add tests
yolop coordination complete --status succeeded --summary Parser fixed --validation cargo test passed --artifact https://example.test/pr/1
yolop coordination cancel --task-id <task-id> --reason Superseded by newer plan
yolop coordination accept
yolop coordination drain
yolop coordination spawn-workers --count 3 --profile review-worker
```

### Start workers for a review round

A coordinator that needs more workers plans them with `spawn-workers`, which
prints one `yolop` launch command per worker with the requested profile,
provider, and model (up to 10 per call; run it again for more). Start one
worker per launch command (one terminal each) in the project, wait until
`yolop coordination status` shows them as live workers, then dispatch one
review per worker. `spawn-workers` needs no role and works outside a session;
dispatching still requires the coordinator role.

The `yolop_spawn` command routes a spawn request to the right primitive: mode
`agent` delegates to `spawn_agent` for an in-process child (same session, same
model), mode `session` plans separate Yolop worker sessions with their own
model, provider, and worktree, dispatching directly when a target worker is
named:

```text
yolop_spawn --mode session --count 2 --profile review-worker --task <work>
yolop_spawn --mode agent --task <work> --title <short title>
```

Multiword `--title`, `--request`, `--summary`, and `--validation` values consume
words until the next option, though ordinary shell quoting also works.
Run `yolop coordination <operation> --help` for the canonical grammar. The
built-in `/coordination` command accepts the same operation syntax.

An attached `list` is scoped to the current Git project. A CLI outside a
session may list all live sessions and plan worker launches with
`spawn-workers`, but every other operation requires a running session
and derives its identity and role from that attachment. A caller cannot mutate
another session by supplying a session ID.

`dispatch` requires a coordinator or combined role. It reserves one idle worker
atomically and creates a `session_dispatch` task in the coordinator's existing
task registry. The worker receives an authenticated automatic prompt and must
invoke `yolop coordination complete` from its own attached session. Completion
settles the task and wakes the coordinator with the durable result. Every dispatch
records parent (coordinator) and child (worker) session IDs in the success
payload and status views. When no worker is available, dispatch fails instead
of creating an untracked process: plan workers with
`yolop coordination spawn-workers` and start them (or spawn one with the
`spawn_agent` tool for an in-process child), then retry dispatch. The owning coordinator can
cancel its running assignment with `yolop coordination cancel`, which fails
the coordinator task, releases the worker for new work, and delivers a one-shot
cancellation inbox message to the worker.

If no eligible worker is live, dispatch fails visibly. The spawner generates
launch commands with the coordinator's chosen profile, provider, and model;
starting the worker processes themselves stays with the operator, so tracking
stays honest about which workers are actually live. Full process supervision
with owned restarts, logs, and cleanup waits for a durable worker lifecycle.

## Restarts and failures

Every assignment, completion, and cancellation stays in a durable inbox until
the turn it woke finishes. That makes the pool survive the usual interruptions:

- **A session restarts.** Resume it with its id, `yolop --session <id> --profile
  worker` (find ids with `yolop sessions`). Anything it had not finished, an
  assignment it was working on or a completion its coordinator had not yet
  processed, is delivered again. A brand-new session gets a new id and cannot
  pick up another session's work.
- **A woken turn fails** (provider error, quota, stalled stream). The message is
  offered again after a backoff, three attempts in all. An assignment that still
  cannot be processed fails back to its coordinator with the last error.
- **A worker disappears** (closed, crashed, or not resumed). Once it has been gone
  for two minutes its coordinator fails the assignment with `worker_offline`
  and wakes with the reason. Partial work stays in the worker's workspace. A
  worker that returns later cannot complete the failed assignment.
- **Yolop is upgraded while sessions run.** On Linux, running sessions keep
  using the binary they started with for their `yolop` shim, sandbox, and
  control calls, so `yolop coordination` keeps working until you restart them.
  On macOS the old binary must still exist at its original path.

Cancelling an assignment releases the worker and tells it to stop. It does not
kill processes the worker started in the background, so long-running commands
may need cleaning up.

## Delivery and safety

Presence uses short SQLite leases, so crashed workers disappear without a
central daemon. Assignment and completion messages use a durable SQLite inbox,
acknowledged when the woken turn finishes.
Automatic coordination wakes are not disabled by the background-only
`proactive_wake` preference.

Coordination does not widen authority. A worker keeps its own workspace,
sandbox, approval policy, profile instructions, and capability set. The
coordinator can target only a live, opted-in session in the same local project.

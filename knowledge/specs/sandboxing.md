---
type: Product Specification
title: Sandboxed execution
description: Defines the sandboxed execution contract for Yolop.
---

# Sandboxed execution

Status: modes × approvals implemented for arbitrary shell execution on macOS and Linux.
Windows runs the shell unsandboxed (no native containment yet) and warns.

## Purpose and boundary

Yolop composes a hard shell approval policy with the sandbox boundary. Soft
approval remains separate prompt guidance for critical actions. The sandbox
limits what arbitrary child processes can do even when the model is
jailbroken, confused, or simply wrong.

Every shell entry point uses one shared `SandboxProvider` boundary:

- the model-facing `bash` tool;
- `spawn_background` executions of that tool;
- the `/shell` command; and
- the TUI `!shell` shortcut.

Structured file tools remain in Yolop's trusted host broker. They are rooted at
the active `WorkspaceHost` and retain the existing protected-path checks. System,
global, workspace, and extension skills also keep their existing read-only
mounts. Model-provider credentials stay in the trusted parent process.

Broad structured grep must not fail solely because aggregate workspace text
exceeds a small input budget. Workspace grep streams one bounded text file at a
time while preserving the regex, path-filter, match-pagination, and response
bounds; gitignored files and oversized individual files remain excluded.

This is intentionally a provider boundary rather than OS-specific policy logic in
the tool. A provider receives the canonical active workspace and script and
returns the process to launch. Bashkit, agentOS, Daytona, Monty, or another
backend can implement the same boundary without changing tool schemas.

## Modes × approvals

The implicit default is `sandbox_mode = "danger-full-access"` plus
`approval_policy = "on-request"`. Shell commands run directly on the host and
are surfaced as `UNSAFE HOST`. The provider is selected once when a runtime is
built; each command resolves the current workspace again, so worktree
activation is reflected on
the next command.

`read-only` removes both the workspace and shared `/tmp` grants while retaining
the private Yolop temporary directory. This prevents a workspace below `/tmp`
from inheriting a broader writable ancestor.
`danger-full-access` runs directly on the host and is surfaced as `UNSAFE HOST`.

Approval policies are independent: `untrusted` gates commands outside a
conservative read-only allowlist; `on-failure` gates a full-access retry after a
likely sandbox denial; `on-request` gates explicit `require_escalated` calls;
and `never` refuses escalation without prompting. The TUI owns the shell gate:
users can deny, approve one command, or approve the displayed sandbox scope for
the rest of the session. A sandbox-only session grant does not grant later
`danger-full-access` requests. Print and ACP shell escalation requests fail
closed; ACP's separate general tool-permission gate is unchanged.

On macOS and Linux, native execution is fail closed: if the required OS
primitive is unavailable, the command returns a sandbox-setup error and is not
retried on the host. Windows has no native provider yet and is the documented
exception, see [Windows](#windows) below.

### macOS

The native provider launches `/usr/bin/sandbox-exec` with a Seatbelt profile:

- processes and host reads are allowed for compiler/SDK compatibility;
- writes are allowed only below the active workspace, `/tmp`, and Yolop
  sandbox temp;
- `.git` below the active workspace is read-only; and
- all network access is denied.

Host reads are deliberately allowed in this first version. Therefore it does
not yet prevent a command from reading unrelated host files. It prevents writes
and network exfiltration, and the limitation is part of the documented threat
model rather than an implied guarantee.

### Linux

The native provider re-executes Yolop as a small policy worker. The worker uses
Landlock to grant host reads plus writes only below the active workspace,
`/tmp`, and private temp, then installs a seccomp-BPF filter that denies internet
and packet socket creation before replacing itself with bash. Restrictions are
inherited by descendants. A kernel that cannot enforce Landlock is an error,
never an unsandboxed fallback.

The policy requires full Landlock ABI v3 enforcement (Linux 6.2 or a backport)
so direct truncate operations cannot bypass the write boundary.

Landlock path rules are additive: a writable workspace grant cannot subtract a
writable `.git` subtree. Linux therefore permits Git metadata writes that live
inside the active workspace. Linked-worktree metadata outside that boundary
remains read-only. This is weaker than the macOS policy and is communicated as
a known limit.

### Windows

There is no native sandbox on Windows yet, so the platform is fail *open*, not
fail closed: every mode, including `workspace-write`, runs the
shell with full host access. The shell is PowerShell (`powershell.exe -NoProfile
-NonInteractive -Command`) rather than bash, since it ships in-box on every
supported Windows. Because nothing is contained, the startup warning fires for
every mode on Windows, and the `bash` tool advertises PowerShell so the model
emits the right syntax. A native provider built on
Windows primitives (restricted tokens, ACLs, a firewall-isolated user) is the
path to fail-closed parity and is tracked as future work.

## Sandbox opt-in

The `--sandbox` CLI flag applies `workspace-write` to one process, including
its print, TUI, or ACP runtime, without modifying persistent configuration.
Users may persist the same mode with:

```toml
sandbox_mode = "workspace-write"
```

The same change is available through
`set_config key=sandbox_mode value=workspace-write`. Enabling applies on the
next run. Yolop communicates the risk of the unsandboxed default in three
places:

- `set_config` returns an explicit `DANGER` / `UNSAFE HOST` message;
- startup writes the warning to stderr and the TUI transcript; and
- the status bar appends `UNSAFE HOST` to its persistent approval indicator.

Clearing the setting restores `danger-full-access`. An explicit
`workspace-write` entry remains visible in the file.

## Provider contract and future composition

The implemented host boundary is deliberately small:

```rust
trait SandboxProvider: Send + Sync {
    fn mode(&self) -> SandboxMode;
    fn command(&self, cwd: &Path, script: &str) -> Result<tokio::process::Command>;
}
```

The registry contains read-only, workspace-write, and full-access providers. A
provider owns policy compilation and launch but not output collection,
timeouts, cancellation, or background event streaming; those remain in the
shared `BashTool` executor. That separation guarantees every entry point keeps
the existing lifecycle semantics.

Results identify the selected provider mode. When native execution fails with
an OS denial signature, the result also carries a `likely` sandbox-denial
classification. Presentation renders stderr with a red marker and explains the
likely containment denial. The classification remains probabilistic because
Linux does not distinguish Landlock `EACCES` from ordinary permission errors.

The next protocol revision may expand this into a session-oriented interface
for providers with virtual filesystems, snapshots, or remote lifecycle:

```rust
#[async_trait]
trait SandboxSession: Send + Sync {
    async fn execute(&self, request: ExecRequest, events: Arc<dyn ExecEventSink>)
        -> Result<ExecOutcome>;
    async fn snapshot(&self) -> Result<Option<OpaqueSnapshot>>;
    async fn shutdown(&self) -> Result<()>;
}
```

That extension must preserve strict event ordering (zero or more stream events,
then exactly one terminal result), opaque provider state, fail-closed startup,
and a provider-neutral policy description. It must not let model input choose a
host executable or silently widen mounts/network.

## Containment providers

- **Workspace write** is the opt-in kernel-enforced local provider.
- **Bashkit** is a containment provider in its own right: it interprets a bash
  subset against a virtual filesystem and does not spawn arbitrary OS
  processes. It would advertise different capabilities from native execution,
  not masquerade as an equivalent shell.
- **agentOS** and **Daytona** can provide VM/remote containment and lifecycle.
- **Monty** is a restricted Python interpreter provider, not a general shell.
- **Logfire Sandboxes** can be a remotely observed Python execution provider.

Provider capabilities must be explicit so policy can require a kernel/VM
boundary, virtual-runtime boundary, network denial, snapshots, or a full system
shell without guessing from the provider name.

### Monty protocol

Monty's protobuf protocol is a useful precedent for restricted interpreters. It
strictly alternates execution with typed host callbacks and represents
continuations as opaque snapshots. A future Yolop adapter should map that state
machine into the common executor rather than adopt Python-specific messages as
the universal sandbox protocol. Monty has had sandbox-escape security work, so
its interpreter boundary must not be described as equivalent to kernel/VM
isolation.

### Logfire Sandboxes

Logfire's sandbox API combines Python execution with managed lifecycle and
observability. It is a promising provider for Python-heavy tasks. Enforcement
must remain independent of telemetry: Logfire/OpenTelemetry spans may record
provider, execution, denial, and resource metadata, but code, tool arguments,
filesystem contents, credentials, and callback values are omitted by default.

## Worktrees and Git

The active workspace is resolved immediately before each spawn. A worktree
switch therefore changes the write boundary without rebuilding tool schemas or
skill configuration. On macOS, the worktree's `.git` indirection remains
readable but is not writable from arbitrary shell commands. Linux Landlock
cannot subtract that path from a writable parent, so metadata paths below the
workspace remain writable there; linked metadata outside it remains protected.
The boundary still tracks the newly active worktree for every command.
Shared `/tmp` is an overlapping writable root: when managed worktrees are
stored below it, sibling worktrees are also writable by path. The active-root
resolution guarantee still covers worktrees stored elsewhere, including the
home-directory fallback, but `/tmp` must not be represented as session
isolation.

## Validation

The automated suite exercises the real `BashTool` launch path and asserts:

- the full-access default and sparse settings serialization;
- explicit sandbox opt-in persistence and danger messaging;
- writes inside the workspace succeed;
- writes in shared `/tmp` succeed;
- writes outside the workspace and `/tmp` fail;
- network connections fail;
- the active worktree is resolved per command;
- foreground and background execution retain the shared executor;
- runtime construction still resolves system skills and automatic settings;
- the real `grep_files` tool searches a workspace larger than the upstream
  aggregate scan cap while retaining its path filter and response envelope;
- an `AGENTS.md`-loaded real-binary TUI session cannot write outside its
  workspace through `!shell`;
- denial output carries provider metadata and an explicit user-facing
  containment explanation.

Platform CI runs the black-box contract suite on both macOS and Linux. Release
validation also includes the normal llmsim packaged-binary smoke and a live
provider smoke.

## Known limitations

- macOS host reads are allowed in the first provider for toolchain
  compatibility;
- Linux host reads and workspace `.git` writes are allowed in the first
  provider for toolchain and worktree compatibility;
- shared `/tmp` is writable for development-tool compatibility and permits
  filesystem communication with other host processes;
- managed worktrees below `/tmp` are not mutually write-isolated by the native
  provider;
- native policy has fixed writable roots and no custom mounts or network
  allowlist yet;
- there is no typed Git mutation broker;
- structured file tools and trusted Git worktree/checkpoint operations use
  their own host-side boundaries;
- hooks use Bashkit virtual execution rather than the native shell provider;
- LSP, MCP, and extension server processes are configured control-plane
  processes and are not automatically moved into this shell sandbox; and
- resource controls are the existing wall-clock/output limits, not yet cgroup
  or VM quotas.

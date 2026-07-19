# Sandboxed execution

Status: implemented for arbitrary shell execution on macOS and Linux.

## Purpose and boundary

Yolop treats soft approval and sandboxing as separate controls. Approval asks
whether an action is intended. The sandbox limits what arbitrary child
processes can do even when the model is jailbroken, confused, or simply wrong.

Every shell entry point uses one shared `SandboxProvider` boundary:

- the model-facing `bash` tool;
- `spawn_background` executions of that tool;
- the `/shell` command; and
- the TUI `!shell` shortcut.

Structured file tools remain in Yolop's trusted host broker. They are rooted at
the active `WorkspaceHost` and retain the existing protected-path checks. System,
global, workspace, and extension skills also keep their existing read-only
mounts. Model-provider credentials stay in the trusted parent process.

This is intentionally a provider seam rather than OS-specific policy logic in
the tool. A provider receives the canonical active workspace and script and
returns the process to launch. Bashkit, agentOS, Daytona, Monty, or another
backend can implement the same seam without changing tool schemas.

## Default policy

`sandbox = "native"` is the implicit default. It provides a live writable
workspace, a private temporary directory, no network, and restrictions inherited
by descendant processes. The provider is selected once when a runtime is built;
each command resolves the current workspace again, so worktree activation is
reflected on the next command.

Native execution is fail closed. If the required OS primitive is unavailable,
the command returns a sandbox-setup error and is not retried on the host.

### macOS

The native provider launches `/usr/bin/sandbox-exec` with a Seatbelt profile:

- processes and host reads are allowed for compiler/SDK compatibility;
- writes are allowed only below the active workspace and Yolop sandbox temp;
- `.git` below the active workspace is read-only; and
- all network access is denied.

Host reads are deliberately allowed in this first version. Therefore it does
not yet prevent a command from reading unrelated host files. It prevents writes
and network exfiltration, and the limitation is part of the documented threat
model rather than an implied guarantee.

### Linux

The native provider re-executes Yolop as a small policy worker. The worker uses
Landlock to grant host reads plus writes only below the active workspace and
private temp, then installs a seccomp-BPF filter that denies internet and packet
socket creation before replacing itself with bash. Restrictions are inherited
by descendants. A kernel that cannot enforce Landlock is an error, never an
unsandboxed fallback.

The policy requires full Landlock ABI v3 enforcement (Linux 6.2 or a backport)
so direct truncate operations cannot bypass the write boundary.

Landlock path rules are additive: a writable workspace grant cannot subtract a
writable `.git` subtree. Linux therefore permits Git metadata writes that live
inside the active workspace. Linked-worktree metadata outside that boundary
remains read-only. This is weaker than the macOS policy and is communicated as
a known limit.

## Unsafe opt-out

Users already running Yolop inside a trusted VM/container may set:

```toml
sandbox = "off"
```

The same change is available through `set_config key=sandbox value=off`.
Disabling applies on the next run. Yolop communicates the risk in three places:

- `set_config` returns an explicit `DANGER` / `UNSAFE HOST` message;
- startup writes the warning to stderr and the TUI transcript; and
- the status bar appends `UNSAFE HOST` to its persistent approval indicator.

Clearing the setting restores `native`. Yolop never writes the safe default to
the file, so an `off` entry is conspicuous during review.

## Provider contract and future composition

The implemented host seam is deliberately small:

```rust
trait SandboxProvider: Send + Sync {
    fn command(&self, cwd: &Path, script: &str) -> Result<tokio::process::Command>;
}
```

The current registry contains `native` and the explicit `off` provider. A
provider owns policy compilation and launch but not output collection,
timeouts, cancellation, or background event streaming; those remain in the
shared `BashTool` executor. That separation guarantees every entry point keeps
the existing lifecycle semantics.

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

- **Native** is the default kernel-enforced local provider.
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

## Validation

The automated suite exercises the real `BashTool` launch path and asserts:

- the safe default and sparse settings serialization;
- explicit unsafe opt-out persistence and danger messaging;
- writes inside the workspace succeed;
- writes outside it fail;
- network connections fail;
- the active worktree is resolved per command;
- foreground and background execution retain the shared executor; and
- runtime construction still resolves system skills and automatic settings.

Platform CI runs the black-box contract suite on both macOS and Linux. Release
validation also includes the normal llmsim packaged-binary smoke and a live
provider smoke.

## Known limitations

- macOS host reads are allowed in the first provider for toolchain
  compatibility;
- Linux host reads and workspace `.git` writes are allowed in the first
  provider for toolchain and worktree compatibility;
- native policy is fixed to workspace-write/network-deny (no custom mounts or
  network allowlist yet);
- there is no typed Git mutation broker;
- MCP/extension server processes are user-configured control-plane processes
  and are not automatically moved into this shell sandbox; and
- resource controls are the existing wall-clock/output limits, not yet cgroup
  or VM quotas.

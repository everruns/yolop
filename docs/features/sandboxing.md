# Shell sandboxing

Yolop runs shell commands inside native operating-system containment by
default. The sandbox limits the damage an incorrect or compromised model can
cause even after a command has been selected for execution.

Sandboxing and [soft approval](./approvals.md) solve different problems.
Approval asks whether an action is intended; sandboxing limits what the command
can reach.

## What is contained

The same policy covers every Yolop shell entry point:

- the model-facing `bash` tool;
- background shell jobs;
- the `/shell` command; and
- the TUI `!shell` shortcut.

With the default `native` mode:

- the active workspace and a private temporary directory are writable;
- writes outside those locations are denied;
- internet and packet-network sockets are denied;
- restrictions are inherited by child processes; and
- provider credentials and local agent socket paths are removed from the shell
  environment.

Host files remain readable so compilers, SDKs, package caches, and installed
skills continue to work. Structured file tools use Yolop's separate rooted
workspace checks rather than the shell sandbox.

## Platform support

| Platform | Enforcement | Requirements |
|---|---|---|
| macOS | Seatbelt | `/usr/bin/sandbox-exec`, included with supported macOS versions |
| Linux | Landlock filesystem rules and seccomp-BPF network filtering | Full Landlock ABI v3 support: Linux 6.2 or a vendor backport |
| Other platforms | Native mode fails closed | Run Yolop inside trusted containment before disabling the sandbox |

If the required operating-system primitive is unavailable, Yolop returns a
sandbox setup error. It never silently retries the command on the host.

## Worktrees and Git

Yolop resolves the active workspace immediately before each command. Switching
to another managed worktree changes the writable boundary for the next command
without restarting the session.

On macOS, `.git` below the active workspace is read-only to arbitrary shell
commands. On Linux, Landlock cannot subtract an in-workspace `.git` directory
from the writable workspace grant, so that metadata is writable. Metadata for
a linked worktree that lives outside the active workspace remains protected.

## Disabling the sandbox

Disable native containment only when Yolop already runs inside a trusted VM,
container, or remote sandbox:

```toml
sandbox = "off"
```

Add the setting to Yolop's `settings.toml`, or ask Yolop to set the `sandbox`
configuration key to `off`. The change applies on the next run.

> **Danger:** `sandbox = "off"` gives shell commands unrestricted access to
> host files, processes, credentials present in the environment, and the
> network. A jailbroken or confused model can damage the host.

Yolop marks this state as `UNSAFE HOST` in configuration output, startup
warnings, the TUI transcript, and the status bar. Remove the setting or set it
back to `native` to restore containment.

## Troubleshooting

### Native sandbox unavailable on Linux

Check the kernel version and whether the distribution enables Landlock. Yolop
requires full ABI v3 enforcement because earlier versions do not mediate every
filesystem operation needed by the write boundary.

### A build cannot download dependencies

Native mode intentionally denies network access. Populate dependency caches
before starting Yolop, use structured integrations outside the shell boundary,
or run Yolop inside separate trusted containment before choosing `off`.

### A command cannot write a file

Confirm the target is below the currently active workspace. Absolute paths,
symlink targets, and linked-worktree metadata outside that boundary remain
read-only.

## Current limitations

- Host reads are allowed for toolchain compatibility.
- There is no per-command mount policy or network allowlist.
- Linux permits `.git` writes when the metadata is physically inside the
  writable workspace.
- User-configured MCP and extension server processes are control-plane
  processes and are not automatically moved into the shell sandbox.
- Wall-clock and output limits apply, but native mode does not yet provide
  cgroup or VM resource quotas.

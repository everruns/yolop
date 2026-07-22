# Approvals

Yolop has two independent approval layers. **Hard shell approval** controls
when a command may execute or cross the sandbox boundary. **Soft approval** is
spoken consent in chat for actions that require judgement beyond shell access,
including destructive or outward-facing structured tools.

## Hard shell approval policies

`approval_policy` composes with any `sandbox_mode`:

| Policy | Behavior |
|---|---|
| `untrusted` | Ask before a command outside Yolop's conservative read-only command set. |
| `on-failure` | Run sandboxed first; if the sandbox likely denies it, ask before retrying with `danger-full-access`. |
| `on-request` | Run sandboxed by default; ask only when the agent explicitly requests `danger-full-access`. This is the default. |
| `never` | Never prompt and never grant a full-access escalation. |

The TUI renders shell approvals with the command and reason. Press `y` to
approve once, `a` to approve requests with the displayed sandbox scope for the
rest of the session, or `n`/Escape to deny. Session approval is not persisted to
later yolop sessions, and sandbox-only approval never grants `danger-full-access`.
One-shot print and ACP sessions do not service this shell-escalation gate, so
those requests fail closed. ACP's separate general tool-permission gate remains
available to compatible editor clients. The default **Auto** preset is
`workspace-write × on-request`.

Soft approval is guidance to the model, not a hard permission gate. yolop is
prompted to batch safe work, pause before critical actions, ask one short
approval question, record the approval, and then continue. There is no separate
approval modal or button.

## Soft approval levels

The `approval_mode` setting controls how cautious yolop should be:

| Level        | Behavior |
|--------------|----------|
| `protective` | Ask before any state-changing action, including writes, commits, pushes, installs, or non-read-only shell commands. |
| `normal`     | Ask only before destructive, irreversible, or outward-facing actions. This is the default. |
| `off`        | Do not ask. yolop runs fully autonomously. |

The current level is shown in the status bar as `approval <level>`.

## Changing the level

Use the explicit setup command:

```bash
/setup approval protective
/setup approval normal
/setup approval off
```

Bare `/setup approval` reports the current level.

You can also ask in ordinary language:

```text
be more careful
stop asking me
yolo mode
```

yolop writes the setting to `settings.toml`, so it persists across sessions.

## How consent works

When soft approval is active, yolop should keep reading, planning, and doing
safe local work without interruption. When it reaches a critical action, it
stops first and explains what it wants to do, why it matters, and what is at
risk.

Approve in plain language:

```text
yes
approved
go ahead
do it
```

One approval covers the action yolop described. It does not automatically cover
unrelated later actions unless you explicitly grant a category exemption, such
as "you don't need to ask for local commits."

## Audit trail

After you approve, yolop calls the internal `record_approval` tool with a short
description of the approved action. That tool call is written into the
session's durable `events.jsonl` log with the rest of the session events.

The audit entry is intended to answer what was approved and when. Do not put
secrets in an approval message; command details and approval text can be
recorded in the session log.

## Limits

Soft approval is not a hard gate. It asks the model to pause at the right time,
but it does not mechanically block a tool call. For deterministic enforcement,
use the shell approval policy or hooks. Soft approval is for judgement and
workflow; hard approvals, hooks, and sandboxing provide enforcement. See
[Shell sandboxing](./sandboxing.md) for the kernel-enforced boundary.

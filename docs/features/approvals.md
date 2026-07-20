# Approvals

yolop is autonomous by default. It does not stop for a yes/no prompt before
each tool call, file edit, or shell command. Instead, yolop uses **soft
approval**: spoken consent in the chat for the small set of actions that are
destructive, irreversible, or outward-facing.

Soft approval is guidance to the model, not a hard permission gate. yolop is
prompted to batch safe work, pause before critical actions, ask one short
approval question, record the approval, and then continue. There is no separate
approval modal or button.

## Approval levels

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

Soft approval is not a sandbox. It asks the model to pause at the right time,
but it does not mechanically block a tool call. For deterministic enforcement,
use hooks, which can block tool calls before they run. Soft approval is for
judgement and workflow; hooks are for guarantees. See
[Shell sandboxing](./sandboxing.md) for the kernel-enforced boundary.

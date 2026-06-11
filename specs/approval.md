# `approval` — soft approval

Status: v1 implemented.

## Why

A coding agent runs tools that touch the real host: it writes files, deletes
things, commits, pushes, installs packages. Users want a say before the
risky ones — but a hard, per-call yes/no gate is miserable. It interrupts safe
work, can't be reasoned about, and trains users to reflexively approve.

Soft approval takes the opposite tack. It is **prompt-engineering, not a
permission gate**: yolop is told, in its system prompt, to batch safe work and
to pause for spoken consent only at the genuinely critical moments. The model
decides what is critical; the user approves in plain language ("yes",
"approved"); the granted approval is logged for audit. There is no separate
approval UI to wire up — consent lives in the conversation.

## What

### Levels

A single central setting, `approval_mode`, picks how cautious yolop is:

| Level        | yolop pauses before…                                            |
|--------------|-----------------------------------------------------------------|
| `protective` | any state-changing action (writes, commits, pushes, installs, non-read-only `bash`) |
| `normal`     | only clearly destructive/irreversible or outward-facing actions (default) |
| `off`        | nothing — yolop acts autonomously, no soft-approval prompt at all |

The level is **central configuration**: it lives in `settings.toml`
(`approval_mode = "protective"`) next to the other yolop settings, so it is
cross-session and global, not per-project. `normal` is the default and is
omitted from the file to keep it sparse.

### System-prompt injection

The `yolop_approval` capability reads the level **live each turn** and
contributes a `<soft_approval>` block to the system prompt:

- `off` contributes nothing.
- `normal` / `protective` inject the threshold for that level plus the
  operating rules: plan first and **batch** the safe steps without pausing
  (read-only inspection never needs approval); stop before a critical action,
  briefly justify it (the "proof") and ask one short question; treat an
  affirmative chat reply as the approval; record it; then proceed. A single
  approval covers the action described, not unrelated later ones, and a
  user-granted category exemption ("you don't need to ask for commits") is
  honored without re-asking.

Reading the level live means `/setup approval` and `set_approval_mode` take
effect on the very next turn without a restart.

### Audit

Approvals are auditable through the existing per-session event log. The
`record_approval` tool is called right after the user consents; because every
tool call already persists a `tool.completed` line to the session's
`events.jsonl`, the approval — its description, optional detail, and a
timestamp — lands in that durable, owner-only log for free. No separate audit
store is introduced.

### Configuration surfaces

The paranoia level can be changed three ways, all writing the same setting:

- **Status bar** — the current level is always shown in the session status
  line (`approval <level>`), so it is visible at a glance.
- **`/setup approval <protective|normal|off>`** — the explicit command form,
  alongside the other `/setup` knobs; bare `/setup approval` reports the
  current level.
- **Natural language** — because users address yolop directly ("be more
  careful", "stop asking me", "yolo mode"), the `set_approval_mode` tool lets
  the model switch the level in response, the same way the `your` layer
  handles other "configure yolop itself" requests.

## Non-goals

- **Not a hard sandbox.** Soft approval cannot *prevent* a tool call; it asks
  the model to. Hard enforcement belongs to hooks (see `specs/hooks.md`), which
  can block a tool deterministically. The two compose: hooks for guarantees,
  soft approval for judgement.
- **No bespoke approval UI.** Consent is spoken in chat; there is deliberately
  no modal or button.

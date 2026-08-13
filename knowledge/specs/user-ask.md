---
type: Product Specification
title: User ask — request tracking and turn-end validation
description: Defines the user ask — request tracking and turn-end validation contract for Yolop.
---

# User ask — request tracking and turn-end validation

Status: experimental and opt-in through the `yolop_user_ask` capability.

## Why

Users state what they want in natural language and often refine or change direction
over several turns. The agent needs a durable record of the current request,
independent of `/goal` completion loops, so it can stay aligned and the host can
judge whether the latest turn actually addressed what was asked.

`/goal` remains the explicit user-authored completion condition. When enabled,
user ask provides a completion safety net: one tracked request per session,
revision history on pivots, and selective bounded continuation when a turn stops
without finishing.

## Behavior

| Surface | Effect |
| --- | --- |
| User message (TUI / `--print` / ACP) | Host records the message as the active user ask (superseding the previous ask in revision history). |
| `set_user_ask` tool | Agent records or replaces the tracked ask (for refinement or when the user pivots in conversation). |
| `clear_user_ask` tool | Clears the tracked ask when the user abandons the request. |
| `/ask <text>` | Set or replace the tracked ask manually. |
| `/ask` | Show status: current ask, evaluated turns, last outcome, revision history. |
| `/ask clear` | Clear the tracked ask (`stop`, `off`, `reset`, `none`, `cancel` are aliases). |
| `/clear` | Also clears the tracked user ask (new conversation). |

After each agent turn while a user ask is active, every host applies the same
cheap deterministic gate. A successful no-tool final is achieved without an
evaluator call. Tool/commentary activity with no final is in progress and
continues automatically. Active detached work is waiting-on-background and does
not consume turns; its wake continues the same actual ask. Provider errors,
refusals, and cancellation become failed or blocked and never retry blindly.

A tool-using turn that produced a candidate final is semantically ambiguous, so
only then the host calls the tool-less evaluator through `CommandHost::completion`.
It returns `achieved`, `blocked`, `failed`, `waiting_on_background`, or
`in_progress`. In-progress work continues from a compact automatic prompt.

Automatic work is capped per user ask at six agent turns, 64,000 provider-reported
tokens, and ten minutes. Exhaustion leaves the ask active for an explicit user
resume. A fresh user message resets the budget; automatic wakes do not. Achieved,
blocked, and failed deactivate the ask. Waiting remains active for its wake.
Provider/runtime failures are classified as failed before the continuation budget
is charged, so a stall or transport error never surfaces as "budget exhausted".

## Configuration

The capability is experimental and off by default. Enable it in `settings.toml`
with:

```toml
[[capabilities]]
ref = "yolop_user_ask"
enabled = true
```

The ordinary capability override path takes effect on the next run. The
capability remains registered while disabled, so it is discoverable through
`get_config key=capabilities` and can be enabled conversationally with
`set_config`.

## System prompt

An inactive tracker contributes no system-prompt text. While active, it contributes
only the current compact ask and last evaluation; revision history stays available
through `/ask` instead of bloating each prompt. Tool schemas remain discoverable
through the ordinary tool-search path.

## Hosts

| Host | Support |
| --- | --- |
| Interactive TUI | Streams each bounded continuation, surfaces state, and shows `? ask (N)`. |
| `--print` | Runs bounded continuations synchronously; waiting work remains one-shot and requires resume/wake in a live host. |
| ACP | Streams the same continuation loop; prompt resolves only at a terminal/waiting/budget state. |

## Persistence

Active asks are stored in `<session_dir>/user_ask.json` so a resumed session
(`--session`) restores the actual ask and pivot history. Host continuation budgets
restart on process resume.

## Independence from `/goal`

- Separate store, capability id, tools, and evaluator.
- Either capability can be enabled or disabled in `[[capabilities]]` without
  affecting the other.
- `/goal` uses the user's explicit completion condition; opt-in user ask
  continuation uses the latest conversational ask and a smaller fixed budget.

## Ownership

- `UserAskCapability` and `UserAskStore` live in yolop (`src/capabilities/user_ask.rs`,
  `src/session_state/user_ask.rs`).
- The deterministic gate and three-axis continuation budget live in
  `everruns-core::turn_completion`. Yolop's `src/session_state/task_completion.rs`
  adapts runtime turn results and owns continuation tagging, prompts, and ask
  projection; hosts own streaming.
- Evaluator calls use upstream `CommandHost::completion` (everruns-core).

## Related

- [`goal.md`](./goal.md) — autonomous completion loops.
- [`conversational-control.md`](./conversational-control.md) — `set_user_ask` /
  `clear_user_ask` tools.
- [`commands.md`](./commands.md) — `/ask` in the command registry.

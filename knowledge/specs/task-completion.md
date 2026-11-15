---
type: Architecture Specification
title: Task completion and bounded recovery
description: How yolop uses the engine completion check across ACP, TUI, and print.
---

# Task completion and bounded recovery

A model finishing a generation is not proof that the user's work is done.
Yolop builds its agent with the everruns engine completion check set to
`Always`: every finishing turn is reviewed on the agent's own model, watched or
not, and a turn that stopped on unfinished work ("running the tests now")
gets an automatic continuation inside the same turn. The engine owns the
review prompt, the verdicts (achieved, in progress, blocked), the budget, and
the durable record; yolop's hosts only read the outcome. There is no yolop
review loop, controller, or second budget.

The conversation owns task context. User followups steer the ongoing work;
continuations carry the engine's `turn_continuation` origin and do not
replace the original request. Hosts hide them from replayed transcripts and
show the check's notice instead.

What is settled without a review is the engine's call: cancellation and
provider failure end the turn, a turn whose tools left detached work running
waits for it, and a plain answer with no tool work is accepted (everruns
releases after 0.52 also review tool-free promises under `Always`). An answer
that ends on a question is blocked on the person.

Continuations default to five per turn, 256,000 tokens (cache reads excluded),
and ten minutes without tool work: a continuation that runs tools restarts
the window, so long tasks that keep working are bounded by turns and tokens.
The schema-backed settings `continuation_max_turns`, `continuation_max_tokens`,
and `continuation_max_seconds` accept positive integers and support named
profile overrides. They become the agent definition's continuation limits
when a session starts; a change applies to the next session. When the budget
runs out the turn finishes with a notice naming how many continuations ran;
any next message from the person starts a turn with a fresh budget.

Each check is one `turn.completion_checked` event in the session log: verdict,
action (finished, continued, budget exhausted, review failed), short reason,
and continuation usage. The TUI and ACP render a notice for continued,
exhausted, and failed checks as they stream; a blocked turn sets terminal and
herdr status. `search_sessions` finds checks other than plain finishes as
diagnostics, and trace extensions receive the event like any other: the
bundled Logfire exporter emits it as a point span under its turn. See
[extensions](extensions.md).

Print waits for background completion, bounded by ten minutes from its first
wait, and prints only what the agent wrote after the last continuation. It
returns a failure exit status when the budget ran out or the review found the
work blocked on outside input; an answer that merely ends on a question still
succeeds. Unavailable reviews are reported on stderr; they never claim that
unfinished work succeeded.

## Out of credits

A provider refusal for lack of credits is the engine's too. When the balance
still covers a useful answer the request is retried once with fewer output
tokens; otherwise the turn fails and the engine schedules a check every
minute for up to an hour, continuing the turn when credits are back
(`turn.credit_shortfall` events, rendered as notices). Yolop keeps only the
readable failure block (token counts, key and credits links). Local schedule
fires reach the session through everruns' local schedule runner, which admits
them without a `Schedule` origin; yolop's hosting adds it to the admission
whose id is the runner's occurrence id, so the engine recognizes its own
checks. Drop that once everruns writes the origin itself.

See [background execution](background.md), [tool output](tool-output.md), and
[progress guard](progress-guard.md).

## Background completion evidence

Print mode retains its ten-minute deadline. A genuine timeout reports bounded blocking
task IDs, kind, state, attempt, owner, heartbeat age and cancellation intent, without
dumping task specs. Unknown or live ownership remains an actionable blocker rather than
being guessed dead.

Print mode also hands off non-monitor attempts that became terminal during the
foreground turn, even if they finished before the pending-work check or requested
no wake signal. A terminal snapshot the foreground already read counts as
observed; terminal history from earlier turns does not trigger another turn.

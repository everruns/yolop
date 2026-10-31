---
type: Architecture Specification
title: Task completion and bounded recovery
description: Defines the shared turn-end policy for ACP, TUI, and print.
---

# Task completion and bounded recovery

ACP, TUI, and print use one host controller. A model finishing a generation is
not proof that the user's work is done. The controller distinguishes achieved,
recoverable work, background waiting, cancellation, provider failure, and a
blocker requiring outside input. A failing command or test is recoverable
evidence rather than a blocker by itself.

The conversation owns task context. User followups steer the ongoing work;
automatic messages carry host provenance and do not replace its original scope.
There is no separate ask file, goal command, or model-specific promise guard.
Resumed sessions retain their conversation, without restoring a parallel task
tracker. Queued steering takes precedence over completion review. The TUI runs reviews
as cancellable background work so input stays responsive. A new human submission
supersedes a TUI review and dispatches host commands normally. ACP keeps cancellation
armed through the review and all repair turns.

Cancellation and provider failure stop automatic repairs. Queued or running detached
execution means waiting, including a tool-free status response. Persistent monitors
and tasks awaiting outside input proceed to review of the current request, rather
than forcing a wait for future events. Other candidate final
answers receive a tool-free review by the session model, even when no tools ran.
The review sees the original request, recent steering, transcript outcomes, and
candidate final. It must respect authorized scope and completed analysis work;
it must not invent implementation or shipping requirements. Review errors or
invalid responses stop recovery with a visible notice rather than retry blindly.

Recovery defaults to five automatic turns, 256,000 budget-weighted tokens,
and ten minutes from the first repair. Cache reads repeat the same prefix
every turn and bill far below fresh input, so they count tenfold discounted
toward the token budget. The schema-backed settings
`continuation_max_turns`, `continuation_max_tokens`, and
`continuation_max_seconds` accept positive integers and support named profile
overrides. The controller reads effective settings after each review, so an
attached `yolop config set` changes the current recovery budget without resetting
usage. Raising one limit leaves the other two in force. The original turn is
not charged against that budget. The controller checks completion before reporting exhaustion, so a
completed repair is accepted even if it used the remaining budget. Exhaustion
names the reached limit and asks the user to approve resuming with a fresh
budget. Approval is conversational, following the
existing approval convention. A new human message resets recovery and can also
redirect or cancel the task. Background execution retains its own task limits.

Print waits for background completion, bounded by ten minutes from its first
wait, and emits the accepted or terminal response. Interactive hosts stream candidate
responses as they arrive and continue unfinished work in the same session.
Limits and unavailable reviews are reported explicitly; they never claim that
unfinished work succeeded. Print returns a failure exit status for exhausted
recovery or required outside input. The same limits apply to all models.

Completion decisions use the existing budget event protocol in the private
session event log. Metadata records the decision, short reason, turn stop
reason, consumed turns/tokens/time, effective limits, and the exhausted limit.
A continuation is a budget warning, a terminal or waiting decision pauses the
recovery budget, and exhaustion has its own event. Human resets are recorded
when the next turn is observed, with the original reset timestamp and previous
usage. Review failures are also recorded. Native turn outcomes and provider
recovery events remain durable, so stopping and retrying can be investigated
after restart. These records do not become model conversation messages.

Trace extensions receive the same metadata. The bundled Logfire exporter emits
budget point spans with completion attributes and preserves correlation to the
most recently completed turn. Exporting remains opt-in and does not block the
agent loop. See [extensions](extensions.md).

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

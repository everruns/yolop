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

Cancellation and provider failure stop automatic repairs. Any active detached
task means waiting, including a tool-free status response. Other candidate final
answers receive a tool-free review by the session model, even when no tools ran.
The review sees the original request, recent steering, transcript outcomes, and
candidate final. It must respect authorized scope and completed analysis work;
it must not invent implementation or shipping requirements. Review errors or
invalid responses stop recovery with a visible notice rather than retry blindly.

Recovery allows up to three automatic turns, 256,000 provider-reported tokens,
and ten minutes from the first repair. The original turn is not charged against
that budget. The controller checks completion before reporting exhaustion, so a
completed repair is accepted even if it used the remaining budget. A new human
message resets recovery. Background execution retains its own task limits.

Print waits for background completion, bounded by ten minutes from its first
wait, and emits the accepted or terminal response. Interactive hosts stream candidate
responses as they arrive and continue unfinished work in the same session.
Limits and unavailable reviews are reported explicitly; they never claim that
unfinished work succeeded. Print returns a failure exit status for exhausted
recovery or required outside input. The same limits apply to all models.

See [background execution](background.md), [tool output](tool-output.md), and
[progress guard](progress-guard.md).

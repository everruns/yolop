---
type: Product Specification
title: Mid-turn steering
description: Defines the mid-turn steering contract for Yolop.
---

# Mid-turn steering

## Why

Long tool runs should not lock the interactive composer. A user must be able to
add context or redirect the agent without first cancelling useful work.

## TUI behavior

- The composer remains editable while an agent or shell turn is active.
- `Enter` submits the draft as a steering message. The message appears in the
  transcript immediately and the composer clears for another message.
- Submitted messages, expanded paste attachments, and images enter a FIFO
  admission queue. The busy separator shows messages awaiting assignment.
- Once the foreground input has an authoritative turn assignment, the oldest
  queued message is admitted durably through Engine. The active turn consumes
  it at its next safe reasoning boundary, after current tool calls finish.
  The TUI reports receipt only after Engine assigns it to that turn. Delivery
  never injects text into an in-flight provider call or interrupts a useful tool.
- Startup and standalone shell commands retain the queue until an agent turn
  can accept it. Each message keeps one admission identity and the exact input
  used on its first attempt. Uncertain replies reuse them. If the original turn
  finishes during admission, the TUI observes the accepted follow-up instead
  of submitting the message again.
- Engine owns the ordering of admitted steering, completion review and automatic
  continuation. The TUI does not add a second execution loop.
- Pressing `Esc` twice cancels the active turn and stops further steering into
  it. Inputs not yet admitted remain queued for later delivery. Inputs already
  assigned to the cancelled turn remain in its durable history and are not
  automatically replayed as new work.
- While busy, slash- and bang-prefixed drafts are steering messages, not
  immediate host commands, except the read-only workflow views (`/diff`,
  `/timeline`, `/sessions`, `/attachments`, `/context`, `/actions`, `/find`,
  `/recover`), plus their local attachment and comparison controls. These open
  local inspection without overlapping execution.
  Restoration and session switching still refuse active foreground work.

Only inputs awaiting admission are process-local. An accepted input belongs to
the session's durable engine authority, including if the TUI exits before the
active command finishes. Failed admission leaves the message queued and reports
the error without claiming receipt. A changed retry error is reported again;
identical consecutive errors are suppressed.

## Hosts

This behavior belongs to the interactive TUI. `--print` has no mid-turn input
surface, and ACP clients own their input and cancellation UX.

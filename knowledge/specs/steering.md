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
- Submitted messages, expanded paste attachments, and images are retained in a
  FIFO queue. The busy separator shows the queue length.
- The embedded runtime accepts one foreground input at a time. Yolop therefore
  delivers the oldest queued message immediately after the active turn settles,
  then delivers later messages one turn at a time in submission order. This is
  the local-runtime equivalent of Everruns consuming queued user messages at a
  safe iteration boundary; messages are never injected into an in-flight model
  call or tool execution.
- Queued steering takes priority over `/goal` continuation and user-ask
  evaluation. Those post-turn actions run only after the steering queue drains,
  so a new instruction is never evaluated against the preceding response.
- Pressing `Esc` twice cancels only the active turn. Queued messages remain and
  begin delivery when cancellation settles.
- While busy, slash- and bang-prefixed drafts are steering messages, not
  immediate host commands. This prevents commands or shell processes from
  overlapping the active turn.

The queue is session-local and process-local, matching the embedded runtime's
turn lifecycle. Normal session persistence begins when each queued message is
delivered to the runtime.

## Hosts

This behavior belongs to the interactive TUI. `--print` has no mid-turn input
surface, and ACP clients own their input and cancellation UX.

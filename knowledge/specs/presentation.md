---
type: Product Specification
title: Presentation Model Specification
description: Defines the presentation model specification contract for Yolop.
---

# Presentation Model Specification

## Abstract

Yolop has terminal and non-terminal hosts, but the user-visible agent output is
one product surface. Transcript entries, live activity, stream previews, and
status-bar values must be represented by a terminal-independent presentation
model before any TUI, `--print`, or editor-specific rendering.

The presentation model is the testable contract for "what the user sees." It
must not depend on ratatui, crossterm, ANSI color, terminal buffers, or a real
terminal.

## Required Model

The model must expose structured values for:

1. **Transcript output**: user, assistant, narration, tool, tool-detail, diff,
   and system entries with stable labels and text. Tool completion wording lives
   here, including success/failure markers and summaries.
2. **Live activity**: current turn status such as thinking, running tools,
   waiting for client results, cancelled, or failed.
3. **Stream previews**: assistant and tool delta previews while a turn is
   active.
4. **Session status**: every value shown in the status bar, including provider
   and model, active configuration profile, approval mode, goal state, background-task counts, token counts,
   current session, worktree state, and busy/idle state when present.
5. **Startup empty state**: immediate workspace readiness plus optional
   repository name, branch, worktree cleanliness, and latest commit context.
   Fullscreen repository inspection runs asynchronously and may enrich the
   empty state after the composer is already interactive; it must never extend
   time to first input. Inline mode does not inspect or show repository pulse
   data, because changing the height of its pinned footer would visibly reflow
   the scrollback-adjacent surface.

The startup empty state is not transcript history. It appears only while there
are no transcript messages, disappears on the first real message, and returns
after `/clear`. Its initial workspace and help text require no repository
inspection. In fullscreen mode, Git metadata replaces that minimal state when
the background result arrives; a non-repository workspace simply keeps the
initial state. Inline mode keeps the minimal state stable for the whole empty
session. A danger-full-access warning remains visible in the empty state in
addition to the compact safety status.

The TUI may still own layout, colors, wrapping, scrollback anchoring, input
widgets, overlays, and terminal-specific affordances. It must not be the only
place where a user-visible transcript label, tool wording, or status-bar value
is assembled.

### Interactive Renderer Default

Interactive sessions use the fullscreen alternate-screen renderer by default.
The application owns transcript scrolling, overlays, and viewport composition in
this mode. `--inline` selects the scrollback-native renderer when terminal
history is preferred: a composer pinned to the terminal's last rows with
finalized transcript entries published above it as ordinary scrollback, which
the terminal keeps, selectable, scrollable, and still there after the session
exits. A published entry is never repainted, so anything that changes during a
turn belongs in the pinned rows, and an entry the pinned rows still show must
not also be published, a transcript entry appears once, never twice.

Neither interactive renderer permits tracing output to inherit stderr: an
asynchronous diagnostic would bypass the layout and overwrite owned terminal
rows. `RUST_LOG` output is written without ANSI styling to owner-only rotating
files under the platform data directory's `yolop/logs/` folder; command,
`--print`, and ACP modes continue to write tracing output to stderr. At most
five interactive trace files are retained, capped at 4 MiB each.

`--compact-work` selects an alternate fullscreen transcript projection. Each
live turn owns one mutable work summary instead of appending narration and tool
rows. The summary carries current activity, elapsed time, top-level action
count, and its terminal success, failure, or cancellation state. `Ctrl+O`
expands or collapses the latest turn's retained detail rows inline beneath the
summary; the final assistant response remains an ordinary transcript entry
after it. Compact projection never changes the canonical session event log,
trajectory export, or non-TUI hosts. It conflicts with `--inline`, because a
split-footer renderer cannot collapse details after the terminal has accepted
them as immutable native scrollback. While compact work owns the live activity
row, the composer separator does not repeat that activity a second time. Its
retained detail projection shares the transcript's bounded line budget.

Transcript links use OSC 8 targets and leave activation to the terminal
emulator, including its platform-native modifier (`Ctrl` on Linux, `Command` on
macOS). The fullscreen host must not also launch the URL from the reported mouse
event. While mouse capture is active, link hover may set the terminal pointer
through OSC 22, and must restore the default pointer when the session ends.

Fullscreen mouse text selection is application-owned (mouse capture replaces the
terminal's native drag-select). A left-drag across the transcript highlights and
copies via OSC 52 on release. Bare modifier key events, which arrive when the
session enables `REPORT_ALL_KEYS_AS_ESCAPE_CODES`, must not dismiss that
highlight; with an active selection, `Ctrl+C` re-arms the OSC 52 copy instead of
interrupting. Ordinary typing still clears the selection.

### Fullscreen Status Drawer

The fullscreen host projects expanded session status as a responsive drawer.
At wide terminal widths the Runtime, Session, and Workspace sections render as
three columns; narrower widths reflow them to two columns. Compact mode remains
one row. Values must fit their assigned column without escaping the frame.

Status fields may carry typed host actions. Model and reasoning-effort fields
open their existing selectors, background state opens its panel, and the
explicit expand/collapse field changes layout. Empty status space has no action.

The interactive TUI exposes a bounded, turn-scoped agent status contribution.
The agent may update or clear that value while working; the host clears it when
the turn finishes. Host-owned runtime, safety, session, and workspace values
remain authoritative and cannot be replaced by the agent contribution.

When a named configuration profile is active, the safety status identifies it
before the soft approval, sandbox, and hard approval-policy values. This keeps
the selected execution bundle visible in compact and expanded layouts without
conflating it with the provider or model name.

### Activity Rail

The interactive TUI projects session tasks into a flat right-hand activity rail.
Linked sub-agents form the `AGENTS` section; ordinary background commands and
scheduled monitors remain visible in `BACKGROUND`, even while agents exist.
Monitors are described as waiting rather than executing commands. Each task has
a semantic state marker, name, right-aligned state, and branch usage when space
permits.

The rail opens once, when the first sub-agent appears, and stays passive so the
composer retains focus. `Ctrl+B` focuses a passive rail and closes a focused
one; a user-closed automatic rail must not repeatedly reopen during the same
session. The body has a persisted scroll viewport and scrollbar. Passive mode
follows newly appended work; focused mode supports task selection, paging, and
cooperative cancellation without selecting section headers.

Wide terminals dock the rail beside the transcript. A passive rail hides below
the responsive breakpoint, while a focused rail becomes a right-side drawer
over the full-width conversation. A focused rail must always have a visible
rectangle, including on degenerate terminal sizes.

The root transcript is session-scoped. Both live delivery and catch-up routing
must discard child-session transcript and title events; child progress belongs
in the agent sidebar. Catch-up may still advance its event-store cursor past
foreign events so they are not reconsidered indefinitely.

`--print` and ACP may project the model differently, but shared semantics should
come from the same presentation model rather than parallel ad hoc formatting.

## Test Coverage Contract

Any change that affects user-visible agent output or current status must add or
update automated tests against the terminal-independent presentation model.
Terminal-buffer tests are still useful for layout regressions, but they are not
sufficient proof for transcript wording, status values, or live activity text.

Required coverage examples:

- Tool transcript lines must be asserted as visible output, for example
  ``tool › ✓ Bash `git status --short` exit=0``, not only as internal tool
  result JSON or a ratatui `Line`.
- Regressions for fallback wording such as `Ran Bash` / `Ran Read File` must be
  covered with presenter-level fixtures built from runtime tool-completion
  events.
- Status-bar segments must be asserted through the presentation model whenever a
  feature adds or changes a visible status value.
- Compact work summaries must be asserted through the presentation model for
  active, completed, failed, cancelled, expanded, and collapsed states. A real
  fullscreen PTY test must prove `Ctrl+O` reveals and hides retained details.
- Startup empty-state wording must be asserted through the presentation model;
  TUI tests cover its responsive wrapping and disappearance when the transcript
  begins. Repository inspection tests must prove the worker can remain blocked
  without delaying construction of the interactive TUI.
- TUI tests should focus on terminal-only concerns: geometry, wrapping, colors,
  scrollback, overlays, key handling, and input behavior.

## Ownership Boundary

- Runtime events, messages, and tool results are source data.
- The presentation model owns user-visible semantics.
- Hosts own projection: terminal layout for the TUI, plain text for `--print`,
  and protocol objects for ACP.

## Related

- [`knowledge/specs/commands.md`](./commands.md), terminal-side commands and host effects.
- [`knowledge/specs/shipping.md`](./shipping.md), required validation before merge.
- [`knowledge/specs/maintenance.md`](./maintenance.md), drift checks across user-facing
  surfaces.

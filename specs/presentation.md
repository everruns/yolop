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

1. **Transcript output** — user, assistant, narration, tool, tool-detail, diff,
   and system entries with stable labels and text. Tool completion wording lives
   here, including success/failure markers and summaries.
2. **Live activity** — current turn status such as thinking, running tools,
   waiting for client results, cancelled, or failed.
3. **Stream previews** — assistant and tool delta previews while a turn is
   active.
4. **Session status** — every value shown in the status bar, including provider
   and model, approval mode, goal state, background-task counts, token counts,
   current session, worktree state, and busy/idle state when present.

The TUI may still own layout, colors, wrapping, scrollback anchoring, input
widgets, overlays, and terminal-specific affordances. It must not be the only
place where a user-visible transcript label, tool wording, or status-bar value
is assembled.

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
- TUI tests should focus on terminal-only concerns: geometry, wrapping, colors,
  scrollback, overlays, key handling, and input behavior.

## Ownership Boundary

- Runtime events, messages, and tool results are source data.
- The presentation model owns user-visible semantics.
- Hosts own projection: terminal layout for the TUI, plain text for `--print`,
  and protocol objects for ACP.

## Related

- [`specs/commands.md`](./commands.md) — terminal-side commands and host effects.
- [`specs/shipping.md`](./shipping.md) — required validation before merge.
- [`specs/maintenance.md`](./maintenance.md) — drift checks across user-facing
  surfaces.

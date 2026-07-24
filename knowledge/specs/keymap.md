---
type: Product Specification
title: tuika keymap
description: Defines the tuika keymap engine and how Yolop dispatches key bindings through it.
---

# tuika keymap

## Why

Yolop's key handling grew as a hand-rolled cascade of `match key.code` blocks in
the TUI event loop. Global chord shortcuts (Ctrl+R reverse search, Ctrl+C
interrupt, Ctrl+D quit, Ctrl+B background panel, Ctrl+V paste) were scattered
inline alongside modal handlers, so *what a key does* was neither declared in one
place nor discoverable for help surfaces. Adding a shortcut meant editing an
imperative branch and remembering its ordering relative to every modal guard.

tuika is Yolop's own terminal-UI toolkit, and key routing is a toolkit concern —
the same one [OpenTUI's keymap](https://opentui.com/docs/keymap/overview/) solves
for the browser and terminal. tuika should own a declarative binding engine the
way it owns layout, overlays, and focus, so any host (Yolop included) declares
its shortcuts once and dispatches through a single seam.

## What

A host-agnostic keymap engine in tuika (`tuika::keymap`) that resolves declared
key bindings to named command values, and Yolop's adoption of it for its
app-global shortcuts.

- **Chords and sequences.** A `Chord` is one key press plus modifiers, parsed
  from a string (`ctrl+r`, `alt+shift+tab`, `?`, `space`, literal `ctrl++`). A
  `KeySequence` is one or more chords typed in order, written space-separated
  (`g g`, `ctrl+x s`), so multi-stroke bindings are first-class.
- **Layers.** Bindings are grouped into named, prioritized `Layer`s. A layer may
  be *gated* on runtime data (`when("mode", "panel")`) so it is active only in a
  given application mode; an ungated layer is always active. When two active
  layers bind the same sequence, the higher priority wins.
- **Dispatch.** `Keymap::dispatch` takes a translated `Key` and returns
  `Command(c)`, `Pending` (the strokes so far are a live prefix of a longer
  binding), or `Unmatched` (the host should handle the key itself).
- **Query.** `Keymap::hints` lists the currently-active bindings — key label,
  optional help text, command — for a help overlay or `KeyHints` row.

Yolop builds one `Keymap<GlobalAction>` of always-active chord shortcuts and
routes them through `dispatch` at the top of the TUI key handler, replacing the
former inline `match`. The engine's layer/mode/sequence features are available
for the remaining modal handlers (setup, background panel, `ui/ask`) to adopt.

## Design

### Built on translated events, not crossterm

The engine consumes tuika's own `Key` (the terminal-independent event the host
already translates crossterm into), never a crossterm type. That keeps the
engine free of terminal I/O, so it is unit-testable without a PTY — the same
boundary that lets the rest of tuika's widget layer be tested against synthetic
events.

### Modifier normalization mirrors how terminals report input

Matching is on a normalized `Chord`, so a binding matches the event a terminal
actually delivers:

- For a character key, Shift is folded into the character itself (`?` arrives as
  `Char('?')`, not `Shift`+`Char('/')`), so the `shift` flag is dropped for
  characters and kept meaningful only for non-character keys (`Shift`+`Enter`).
- `Shift`+`Tab` folds to the distinct `BackTab` key that terminals report.

Both the string parser and `Chord::from_key` apply the same normalization, so a
parsed binding and a live event agree.

### Sequence resolution: exact wins, dead-ends retry

`dispatch` accumulates strokes into a pending buffer. On each key it resolves the
pending buffer against the active layers: an exact match fires immediately and
clears the buffer, even when the buffer is also the prefix of a longer binding
(so a bound `g` fires without waiting to see whether `g g` follows — overlapping
bindings are authored with that in mind). A live prefix with no exact match
returns `Pending`. A buffer that matches nothing is dropped, and the final stroke
is retried on its own so it can begin a fresh sequence. Changing runtime data
clears the pending buffer, since the bindings that could complete it may no
longer be active.

### Bindings are static, so parse errors panic by construction

`Layer::bind` panics on a malformed key spec, because bindings are authored as
static literals — a bad spec is a programmer error caught on first run, like a
malformed regex literal. `Chord::parse` / `Layer::try_bind` return a `Result` for
the rare dynamic case (config-sourced bindings).

### Yolop preserves dispatch ordering

Yolop's global layer is ungated and dispatched ahead of every modal guard, which
reproduces the exact precedence the inline `match` had: the global chords fire in
any mode — mid-turn, during setup, or with an overlay open. A key that does not
translate to an event (a release), or that no binding matches, yields no action
and falls through to the composer and modal handlers unchanged.

## Non-goals

- No key *decoding* in the engine — the host translates crossterm into tuika
  `Key` events first, exactly as the rest of tuika consumes input.
- No sequence-timeout clock in the engine. Pending state is cleared explicitly
  (a completed or dead-ended sequence, or a mode change); a host that wants a
  timeout drives `Keymap::reset` from its own tick.
- No user-facing rebinding/config format yet. The engine supports dynamically
  sourced bindings (`try_bind`), but Yolop's bindings are code-defined.

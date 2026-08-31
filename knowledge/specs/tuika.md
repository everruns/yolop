---
type: Architecture Specification
title: Tuika dependency
description: Defines Yolop's relationship to the tuika terminal-UI toolkit and how the host wires into its boundaries.
---

# Tuika dependency

## Why

Yolop's full-screen renderer needs layout, overlays, focus, a component set, and
a streaming markdown renderer. Those are toolkit concerns, not agent concerns.
They began life inside this repository, grew a clean host-agnostic boundary, and
now live in [`everruns/tuika`](https://github.com/everruns/tuika) as published
crates.

Keeping them out of this workspace is the point, not an accident of packaging.
A toolkit that ships from the host's repository accumulates host-shaped
shortcuts: a component that reads a yolop config value, a layout special case
for the transcript, an escape sequence gated on a `YOLOP_*` env var. The
repository split makes each of those a visible cross-repository change rather
than a one-line convenience.

## What

Yolop depends on four crates from crates.io:

- `tuika`, the toolkit. Version-pinned like any other dependency, with no path
  override.
- `tuika-codeformatters`, the tree-sitter `Highlighter` implementation. Kept
  separate upstream so tuika core stays grammar-free.
- `tuika-mermaid`, the `MarkdownBlockRenderer` that turns ` ```mermaid ` fences
  into Unicode cell diagrams. Kept separate upstream so tuika core takes on no
  diagram engine.
- `tuika-html`, the safe block-HTML renderer. It maps supported semantic HTML
  to styled terminal text and ignores unsafe elements and attributes.

## Screen modes

Both renderers are tuika screen modes rather than two hand-rolled hosts:
`ScreenMode::Alternate` for the fullscreen default, and
`ScreenMode::SplitFooter` for `--inline`. The mode decides the viewport, and the
toolkit owns the split-footer mechanics that yolop used to open-code, pinning
the footer to the terminal's last rows across resizes (`pin_footer`), publishing
a block above it (`publish_block`), and handing its rows back at exit
(`close_footer`), so the shell prompt resumes below the transcript.

What stays here is what yolop *publishes*: which transcript lines are final
enough to leave the frame, and the fact that a published block is never
repainted, so anything live, the composer, the status bar, the busy
indicator, belongs in the footer.

A line is shown in exactly one place. The footer paints what has not been
published yet, and the flush holds back exactly the rows those lines cover, a
row-level cut, splitting an entry when one straddles the edge, so the region is
neither doubled nor left half empty. The retained tail is published as it is
pushed out, and in full at exit: the footer's rows are handed back there, so an
unpublished line would be erased rather than left with the session.

## Where the boundary falls

tuika owns *presentation*; yolop owns *acquisition and meaning*. Concretely:

| Concern | tuika provides | Yolop supplies |
| --- | --- | --- |
| Code highlighting | `CodeBlock` framing, gutter, wrapping | the `Highlighter` (`tuika-codeformatters`) |
| Markdown images | block reservation and protocol emission | an `ImageResolver` that decodes bytes to RGBA |
| Mermaid fences | the `FencedBlockRenderer` boundary | the renderer (`tuika-mermaid`) and telling the model the TUI paints them ([system prompt](./system-prompt.md)) |
| Key bindings | the keymap engine (chords, sequences, gated layers, dispatch) | the binding table and what each command *means* |
| Input routing | `Router` stages, and the `FocusRegistry` ownership they read | which modal state owns input, and in what order |
| Links | OSC 8 encoding and the `LinkPolicy` sanitizer | which schemes are allowed, and the transcript's link runs |
| Progress | the OSC 9;4 encoder | when a turn is running |
| Live data | reading shared state at render time | producing it and requesting redraws |

Yolop's keymap adoption is the clearest case of the split: tuika resolves a
translated key to a named command, and yolop decides that the resulting command
interrupts a turn, opens the activity rail, or starts a reverse search. The
engine's precedence is yolop's choice too, the global layer is ungated and runs
at the router's `Pre` stage, ahead of every surface, which reproduces the
precedence the former inline `match` had: global chords fire in any mode,
mid-turn, during setup, or with an overlay open. A key that no binding matches
falls through to whichever surface owns input.

Routing is the same split one layer down. tuika owns *how* an event reaches a
surface, and yolop owns *who* the surfaces are: one ordered table names them
(reverse search, sandbox approval, background panel, extension ask, a running
turn's Esc claim, setup, composer) and every event kind is delivered against it.
Two properties are load-bearing and neither is tuika's to enforce:

- **Ownership is derived once, not per event kind.** A key and a paste consult
  the same table. When they did not, only the key chain knew about the sandbox
  approval prompt, so pasted text landed in the composer behind an open dialog.
  A new surface is added in one place or it is wrong everywhere.
- **Reverse search outranks the global chords.** It owns the keyboard outright,
  Ctrl+C included, which cancels the search rather than arming exit. `Pre` is
  the router's first stage and no surface can outrank it, so this exception is
  written into the `Pre` handler rather than declared.

A running turn is a *partial* claim rather than a modal: it takes Esc and
nothing else, so every other key reaches the surface underneath. That is why
ownership is resolved against the event rather than once per frame.

Link activation is the other case worth stating, because it is a *negative*
requirement: OSC 8 targets are activated by the terminal emulator, using its
platform-native modifier. Yolop must not also open the URL from the mouse event
it receives, or a modifier-click opens the browser twice. See
[`presentation.md`](./presentation.md).

## Constraints

- **No path dependency, no git dependency, no fork.** If a change is needed in
  the toolkit, it lands and releases upstream first, then the version is bumped
  here. A git or path dependency is not a shortcut but a dead end: `cargo
  publish` refuses a dependency without a version requirement, so it would make
  yolop unreleasable for as long as it stayed.
- **A yolop-only feature does not belong upstream.** If a proposed tuika change
  only makes sense for yolop, the right shape is a new boundary (a trait, a state
  type, a callback) that yolop fills in.
- **The companion crates move with tuika.** `tuika-codeformatters`,
  `tuika-mermaid`, and `tuika-html` pin a compatible `tuika` range, so bumping
  one usually means bumping the full set.
- **Mermaid fences stay diagrams at every transcript width.** `tuika-mermaid`
  lays content out at its natural size and ignores the offered width. Yolop
  preserves that diagram output rather than replacing wide charts with source;
  viewport clipping is a presentation concern for tuika.
- **`ratatui` stays aligned.** Yolop and tuika must resolve to one shared
  `ratatui-core`, since the interoperability boundary is a raw `Buffer` from it. A
  `ratatui` major bump is a coordinated change across both repositories.

## Testing boundary

`tests/tuika_pty.rs` stays here because it drives the **yolop binary** under a
pseudo-terminal: it proves yolop's renderer emits the right alternate-screen,
progress, hyperlink, and truecolor protocol through its hidden `tuika-gallery`
demo. tuika has its own equivalent over its `gallery` example, covering the
toolkit in isolation. Neither replaces the other: a regression can live in
either the toolkit or in how yolop composes it.

The nightly cross-terminal workflow here likewise drives `yolop tuika-gallery`,
because what it checks is how emulators paint *yolop's* output.

## Non-goals

- No vendoring or git dependency on tuika.
- No yolop-specific fork of the toolkit.
- No duplication of tuika's own design rationale in this bundle: the toolkit's
  specs live in its repository.

## Related

- [`presentation.md`](./presentation.md)
- [`release.md`](./release.md)

---
type: Architecture Specification
title: Tuika dependency
description: Defines Yolop's relationship to the tuika terminal-UI toolkit and how the host wires into its seams.
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

Yolop depends on three crates from crates.io:

- `tuika` — the toolkit. Version-pinned like any other dependency, with no path
  override.
- `tuika-codeformatters` — the tree-sitter `Highlighter` implementation. Kept
  separate upstream so tuika core stays grammar-free.
- `tuika-mermaid` — the `FencedBlockRenderer` that turns ` ```mermaid ` fences
  into Unicode cell diagrams. Kept separate upstream so tuika core takes on no
  diagram engine.

## Where the boundary falls

tuika owns *presentation*; yolop owns *acquisition and meaning*. Concretely:

| Concern | tuika provides | Yolop supplies |
| --- | --- | --- |
| Code highlighting | `CodeBlock` framing, gutter, wrapping | the `Highlighter` (`tuika-codeformatters`) |
| Markdown images | block reservation and protocol emission | an `ImageResolver` that decodes bytes to RGBA |
| Mermaid fences | the `FencedBlockRenderer` seam | the renderer (`tuika-mermaid`), the transcript's width guard, and telling the model the TUI paints them ([system prompt](./system-prompt.md)) |
| Key bindings | the keymap engine (chords, sequences, gated layers, dispatch) | the binding table and what each command *means* |
| Links | OSC 8 encoding and the `LinkPolicy` sanitizer | which schemes are allowed, and the transcript's link runs |
| Progress | the OSC 9;4 encoder | when a turn is running |
| Live data | reading shared state at render time | producing it and requesting redraws |

Yolop's keymap adoption is the clearest case of the split: tuika resolves a
translated key to a named command, and yolop decides that the resulting command
interrupts a turn, opens the background panel, or starts a reverse search. The
engine's precedence is yolop's choice too — the global layer is ungated and
dispatched ahead of every modal guard, which reproduces the precedence the
former inline `match` had: global chords fire in any mode, mid-turn, during
setup, or with an overlay open. A key that no binding matches falls through to
the composer and modal handlers unchanged.

Link activation is the other case worth stating, because it is a *negative*
requirement: OSC 8 targets are activated by the terminal emulator, using its
platform-native modifier. Yolop must not also open the URL from the mouse event
it receives, or a modifier-click opens the browser twice. See
[`presentation.md`](./presentation.md).

## Constraints

- **No path dependency, no fork.** If a change is needed in the toolkit, it
  lands and releases upstream first, then the version is bumped here.
- **A yolop-only feature does not belong upstream.** If a proposed tuika change
  only makes sense for yolop, the right shape is a new seam (a trait, a state
  type, a callback) that yolop fills in.
- **The companion crates move with tuika.** `tuika-codeformatters` and
  `tuika-mermaid` pin a compatible `tuika` range, so bumping one usually means
  bumping all three.
- **A fenced block that does not fit is not painted.** A companion renderer may
  lay content out at its natural size and ignore the offered width;
  `tuika-mermaid` does. The transcript falls back to the themed code block
  rather than paint a diagram the viewport will clip, so the source stays
  readable at any terminal width.
- **`ratatui` stays aligned.** Yolop and tuika must resolve to one shared
  `ratatui-core`, since the interoperability seam is a raw `Buffer` from it. A
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

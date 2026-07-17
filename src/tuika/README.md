# tuika

A small retained-tree terminal UI toolkit. `tuika` provides the layout,
overlay, focus, and component primitives that `ratatui` leaves to you, while
letting `ratatui` keep ownership of the cell buffer and its diff against the
terminal.

It is self-contained — it depends only on `ratatui`, `crossterm`, `textwrap`,
and `unicode-width`, and knows nothing about yolop — and is staged for
extraction into its own crate. Today yolop drives it from
`src/app/fullscreen.rs` (the `--fullscreen` renderer) and `src/main.rs` (the
`tuika-gallery` demo).

## Model

- **Views** (`view::View`) are rebuilt from application state every frame. This
  is cheap because `ratatui` diffs the resulting cell buffer, so there is no
  reconciler.
- **State** that must survive across frames — scroll offset, selection index,
  focus — lives in host-persisted `*State` structs (the `StatefulWidget`
  idiom), not in the view tree.
- **Layout** is a flexbox subset (`layout`): `Dimension` (`Auto`/`Fixed`/
  `Percent`/`Flex`), `Align`, `Justify`, `Direction`, over a direction-agnostic
  axis so rows and columns share one solver.
- **Overlays** (`overlay`) anchor a view over the base tree; the **host**
  (`host`) owns the alternate screen, translates crossterm input, and
  composites the frame.
- **Motion** (`anim`, `components::{Spinner, ProgressBar, Loader}`,
  `native::TerminalProgress`) animates from a host-supplied frame counter and
  can drive the terminal's own OSC 9;4 progress indicator.

## Components

| Component | Purpose |
| --- | --- |
| `Text` / `Paragraph` | Styled lines / word-wrapped text |
| `Flex` | Flexbox container (the composition primitive) |
| `Boxed` | Border + padding + title, focus-aware |
| `Spacer` | Flexible filler |
| `Scroll` (+ `ScrollState`) | Vertical scroll viewport + scrollbar |
| `SelectList` (+ `SelectState`) | Selectable list |
| `StatusBar` | One-row left/right status segments |
| `Spinner` | Frame-cycled activity glyph |
| `ProgressBar` | Determinate (sub-cell) / indeterminate bar |
| `Loader` | Spinner + message + hint row |

## Example

```rust
use crate::tuika::{self, Boxed, Flex, ProgressBar, Spinner, Text, element};

let theme = tuika::Theme::default();
let root = Flex::column()
    .gap(1)
    .fixed(1, element(Spinner::new(frame)))
    .fixed(1, element(ProgressBar::determinate(0.6).percent(true)))
    .grow(1, element(Text::raw("body")));

// In a `terminal.draw(|f| ...)` closure:
tuika::paint(f.buffer_mut(), f.area(), &theme, root.as_ref(), &[]);
```

Run the live demo: `cargo run -- tuika-gallery` (press `q` to quit).

## Native terminal progress

`native::TerminalProgress` emits the OSC 9;4 sequence, which drives the
terminal's own progress indicator — a bar across the top of the window in
Ghostty, the taskbar in Windows Terminal / ConEmu, and similar in
WezTerm / Konsole / mintty. It is out-of-band (no cursor movement, no cells),
so it works in both the inline and full-screen renderers; terminals that don't
understand it ignore the sequence. yolop shows it (indeterminate) while a turn
runs and clears it when idle.

## Extending

Add a component by implementing `view::View` in a new module under
`components/`. There is no registration step — containers accept any boxed
`View`.

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

## Declarative DSL (`view!`)

`view!` is optional sugar over the builders — it expands to the exact same
`Flex`/`Boxed`/`element(...)` calls, so there is no runtime cost and nothing new
in the model. It just makes nested layout read top-down:

```rust
let root = crate::view! {
    col(gap = 1, padding = tuika::Padding::all(1)) {
        boxed(title = " body ") { text("hello") }
        grow(1) { spacer() }
        node(status_bar)          // any expression that is `impl View`
    }
};
```

Grammar (each keyword consumes exactly one node):

- `col(attrs) { … }` / `row(attrs) { … }` — flex containers. Attrs (all
  optional): `gap`, `padding`, `align`, `justify`, `background`.
- `boxed(attrs) { child }` — bordered container. Attrs: `title`, `border`,
  `padding`, `background`.
- `text(expr)`, `spacer()` — leaves.
- `grow(n) { node }` / `fixed(n) { node }` — set a child's main-axis size
  (default auto).
- **`node(expr)`** — splice any `impl View`. This is the escape hatch, and how
  a component **from another crate** participates in the DSL:

  ```rust
  use other_crate::Sparkline;
  crate::view! { col { node(Sparkline::new(&data)) } };
  ```

First-class `Sparkline { … }` syntax for external components (with their own
constructors and named attrs) needs a proc-macro; that lands when `tuika`
becomes its own crate. Until then, `node(...)` covers every third-party
component. The `tuika-gallery` demo is built entirely with `view!`.

## Native terminal progress

`native::TerminalProgress` emits the OSC 9;4 sequence, which drives the
terminal's own progress indicator — a bar across the top of the window in
Ghostty, the taskbar in Windows Terminal / ConEmu, and similar in
WezTerm / Konsole / mintty. It is out-of-band (no cursor movement, no cells),
so it works in both the inline and full-screen renderers; terminals that don't
understand it ignore the sequence. yolop shows it (indeterminate) while a turn
runs and clears it when idle.

## Testing

Layout and rendering are tested hermetically by rendering into an in-memory
ratatui `Buffer` and reading cells back — no real terminal:

- **Unit tests** (`src/tuika/tests.rs`) — layout math, component rendering,
  interactive state (scroll/select/focus), compositor, easing, OSC encoder,
  and palette (every themed cell pinned to its `Theme` slot).
- **Property tests** (`src/tuika/proptests.rs`, `proptest`) — solver and overlay
  invariants for *any* input (children stay in bounds, flex fills exactly).
- **Golden snapshots** (`src/tuika/snapshots.rs`) — whole screens diffed against
  checked-in glyph grids; refresh with `UPDATE_SNAPSHOTS=1`.
- **Resize / degenerate sizes** — a size sweep from `0×0` up asserts no panic
  and no out-of-clip writes.
- **PTY smoke** (`tests/tuika_pty.rs`) — drives the real binary under a
  pseudo-terminal and asserts the terminal-facing protocol: alternate-screen
  enter/leave, OSC 9;4 progress, resize survival, clean exit.

### Manual terminal matrix

The automated tests cover the *protocol* a terminal receives; they cannot
verify how a specific emulator actually paints it. This is a **checklist to run
before a release**, not a record of verified results — tick a box only after
confirming it yourself. Run `cargo run -- tuika-gallery` in each terminal and
check alt-screen enter/exit, Braille/wide glyphs, truecolor, and mouse-wheel
scroll:

- [ ] Ghostty
- [ ] iTerm2
- [ ] WezTerm
- [ ] Kitty
- [ ] Windows Terminal
- [ ] Konsole
- [ ] tmux (truecolor needs `Tc`/`RGB` in `terminal-overrides`)

**Native OSC 9;4 progress** support is a fixed property of each terminal (not
something to re-verify per release). Terminals that render it: **Ghostty**
(bar at top of window), **Windows Terminal** and **ConEmu** (taskbar),
**WezTerm**, **Konsole**, **mintty**. Others (e.g. **iTerm2**, **Kitty**)
silently ignore the unknown OSC, so emitting it is safe everywhere — the
in-terminal UI is unaffected. See the OSC 9;4 references linked from the
motion-module PR.

## Extending

Add a component by implementing `view::View` in a new module under
`components/`. There is no registration step — containers accept any boxed
`View`.

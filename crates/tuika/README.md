# tuika

A small composable terminal UI toolkit. `tuika` provides the layout,
overlay, focus, and component primitives that `ratatui` leaves to you, while
letting `ratatui` keep ownership of the cell buffer and its diff against the
terminal.

It is a published, self-contained crate that depends only on `ratatui`,
`crossterm`, `textwrap`, `unicode-segmentation`, and `unicode-width`. It knows
nothing about yolop; yolop is one production consumer.

## Model

- **Views** (`view::View`) are rebuilt from application state every frame. This
  is cheap because `ratatui` diffs the resulting cell buffer, so there is no
  reconciler.
- **State** that must survive across frames — scroll offset, selection index,
  focus — lives in host-persisted `*State` structs (the `StatefulWidget`
  idiom), not in the view tree.
- **Live data** (`Live` / `LiveView`) is shared application state read at render
  time. Updates request a redraw from the runner; Tuika does not spawn data
  sources or reconcile a retained widget tree.
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
| `Text` / `Paragraph` | Styled lines / word-wrapped plain text |
| `Wrap` | Word-wraps pre-styled lines, preserving per-span styles |
| `Flex` | Flexbox container (the composition primitive) |
| `Responsive` / `Constrained` | Breakpoint selection and min/max measurement |
| `Boxed` | Border + padding + title, focus-aware |
| `Spacer` | Flexible filler |
| `Scroll` (+ `ScrollState`) | Vertical scroll viewport + scrollbar |
| `SelectList` (+ `SelectState`) | Selectable list |
| `StatusBar` | One-row left/right status segments |
| `Tabs` / `KeyHints` | Host-state tab navigation and command hints |
| `Spinner` | Frame-cycled activity glyph |
| `ProgressBar` | Determinate (sub-cell) / indeterminate bar |
| `Loader` | Spinner + message + hint row |

## Example

```rust
use tuika::{Flex, ProgressBar, Spinner, Text, Theme, element, paint};

let theme = Theme::default();
let root = Flex::column()
    .gap(1)
    .fixed(1, element(Spinner::new(frame)))
    .fixed(1, element(ProgressBar::determinate(0.6).percent(true)))
    .grow(1, element(Text::raw("body")));

// In a `terminal.draw(|f| ...)` closure:
paint(f.buffer_mut(), f.area(), &theme, root.as_ref(), &[]);
```

### Runnable examples

Each enters the alternate screen; press `q` (or `esc`) to quit.

| Example    | Command                                   | Shows                                              |
| ---------- | ----------------------------------------- | -------------------------------------------------- |
| `gallery`  | `cargo run -p tuika --example gallery`    | motion components + native OSC 9;4 progress        |
| `select`   | `cargo run -p tuika --example select`     | `SelectState` + `SelectList` (stateful-widget idiom) |
| `overlay`  | `cargo run -p tuika --example overlay`    | `OverlaySpec` centered dialog + input routing      |
| `ratatui_dashboard` | `cargo run -p tuika --example ratatui_dashboard` | mixed Ratatui widgets + responsive live data |
| `mouse`     | `cargo run -p tuika --example mouse`      | drag-to-select + highlight + OSC 52 copy, clickable buttons |

(Embedded in yolop, the gallery is also reachable as `yolop tuika-gallery`.)

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
  use other_crate::CustomView;
  crate::view! { col { node(CustomView::new(&data)) } };
  ```

`node(...)` accepts any type that already implements Tuika's `View`; it does
not make a Ratatui `Widget` implement `View`. Use `RatatuiView` for Ratatui
widgets. The `tuika-gallery` demo is built entirely with `view!`.

## Ratatui interoperability

Tuika deliberately does not duplicate Ratatui's widget catalog. Wrap existing
widgets in `RatatuiView`; they render into an isolated buffer and only the
assigned clip is composited into the frame:

```rust
use ratatui::widgets::{Sparkline, Widget};
use tuika::{RatatuiView, Size};

let values = vec![1, 4, 2, 8];
let chart = RatatuiView::sized(Size::new(20, 4), move |area, buffer| {
    Sparkline::default().data(&values).render(area, buffer);
});
```

The closure form supports widgets that borrow captured data. Stateful widgets
can capture host-owned synchronized state and call `StatefulWidget::render`
inside the same closure. `Surface::render_ratatui` is the lower-level escape
hatch for custom views that need several widgets. Neither API exposes the
frame's mutable buffer.

## Responsive and live views

`Responsive` chooses complete compact/wide view trees from the current width;
this supports row-to-column reflow and intentionally omitted secondary
content. `Constrained` supplies min/max intrinsic measurements to flex layout.

`Live<T>` is shared application data with a narrow read/update API. `LiveView`
derives a fresh view from its current value each frame. Connect it to
`Runner::redraw_handle()` when background producers should invalidate the
screen. Producers retain ownership of their threads, tasks, retries, and
lifecycle.

## Terminal lifecycle and runner

`TerminalSession` is the complete RAII guard: it owns raw mode, alternate
screen, mouse capture, and cursor visibility, including rollback after partial
initialization. It preserves raw mode when the caller had already enabled it.
`AltScreen` remains available for hosts that intentionally own raw mode and
cursor visibility themselves.

`Runner` is an optional synchronous event loop for dashboards and small tools.
It owns `TerminalSession`, frame scheduling, Crossterm event translation, and
data-driven redraw checks. Async applications can keep their existing loop and
call `paint` directly.

## Native terminal progress

`native::TerminalProgress` emits the OSC 9;4 sequence, which drives the
terminal's own progress indicator — a bar across the top of the window in
Ghostty, the taskbar in Windows Terminal / ConEmu, and similar in
WezTerm / Konsole / mintty. It is out-of-band (no cursor movement, no cells),
so it works in both the inline and full-screen renderers; terminals that don't
understand it ignore the sequence. yolop shows it (indeterminate) while a turn
runs and clears it when idle.

## Mouse, selection, and clipboard

Enabling mouse capture (which `AltScreen` / `TerminalSession` do) means the
terminal stops doing its own click-drag text selection and hands every drag to
the app instead. The `mouse` module rebuilds those affordances over the grid
you already rendered:

- **Text selection.** `SelectionState` turns a left-button `Down → Drag → Up`
  gesture into a `SelectionRange` (a plain click selects nothing; a new press
  clears the old selection). `selected_text(buffer, area, range)` reads the text
  back out of the rendered `ratatui::Buffer` — linear/stream selection like a
  terminal's own, wide glyphs intact — and `highlight(buffer, area, range,
  style)` paints it in.
- **Clicks and regions.** `HitMap<T>` maps screen rects to values (a button, a
  link, a row); the last-pushed match wins, so children/overlays registered
  after their parents take precedence. `ClickTracker` turns a same-cell
  `Down`/`Up` into a `Click` and lets an intervening drag cancel it.
- **Clipboard.** `clipboard::write_clipboard(out, text)` copies via **OSC 52**
  (`clipboard::osc52` is the pure encoder) — no platform clipboard library,
  works over SSH. Same tmux caveat as OSC 8: needs `allow-passthrough on`.

The enriched event model carries what selection and clicks need: `MouseKind` is
`Down/Up/Drag(MouseButton)`, `Moved`, and `ScrollUp/Down/Left/Right`, and every
`Mouse` reports `shift/ctrl/alt`. **Shift-drag** is deliberately left to the
terminal — most emulators use it to bypass app mouse capture for a native
selection — so a host should act on `plain()` left-drags.

**Touch** arrives as mouse events: terminal emulators translate a tap to a
`Down`+`Up` and a swipe to scroll or a drag, so touch flows through this same
path — there is no separate touch event to handle.

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

Downstream crates can use `tuika::testing::{render, render_sizes, grid}` for
buffer assertions, resize sweeps, and stable glyph snapshots without a real
terminal or `TestBackend` setup.

## Compatibility

- Minimum supported Rust version: **1.88**, declared as `rust-version` and
  checked in CI.
- Tuika 0.x follows Cargo semver: minor releases may make deliberate breaking
  API changes; patch releases do not.
- Ratatui and Crossterm are part of Tuika's public interoperability surface.
  Tuika tracks compatible minor lines deliberately; applications should use
  matching versions so Cargo can deduplicate them.

### Manual terminal matrix

The automated tests cover the *protocol* a terminal receives; they cannot
verify how a specific emulator actually paints it. This is a **checklist to run
before a release**, not a record of verified results — tick a box only after
confirming it yourself. Run `cargo run -- tuika-gallery` in each terminal and
check alt-screen enter/exit, Braille/wide glyphs, truecolor, mouse-wheel
scroll, and — with `YOLOP_HYPERLINKS=1` — that the footer URL is a clickable
OSC 8 link:

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

**OSC 8 hyperlinks** ([`HyperlinkBackend`](src/hyperlink.rs)) wrap `http(s)`
URL runs so a supporting terminal makes them clickable: **Ghostty**, **iTerm2**,
**WezTerm**, **Kitty**, **Konsole**, recent **GNOME Terminal / VTE**. Others
ignore the escape and render the URL as plain (usually still auto-linkified)
text, so emitting it is safe everywhere. Unlike OSC 9;4, this one *is* worth
re-checking, because it writes styled spans straight to the terminal: confirm
the link is clickable **and** that surrounding text, colors, and wrapping are
undamaged. In yolop it is opt-in (`YOLOP_HYPERLINKS=1`), default-off until this
matrix is walked — that is what the checkbox above verifies.

## Extending

Add a component by implementing `view::View` in a new module under
`components/`. There is no registration step — containers accept any boxed
`View`.

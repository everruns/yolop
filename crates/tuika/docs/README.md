# tuika component gallery

A visual catalog of tuika's components. Each entry has a name, a one-line
description, and an animated demo. The same GIFs are embedded inline in the API
docs on [docs.rs](https://docs.rs/tuika), so a reader browsing `struct Spinner`
sees it move.

> The demos are generated, not hand-drawn — see
> [Regenerating the demos](#regenerating-the-demos) at the bottom.

## Motion

Animated from a host-supplied frame counter (see the [`anim`](https://docs.rs/tuika/latest/tuika/anim/index.html) module).

### `Spinner`

A frame-cycled activity glyph — `Braille` (smooth default), `Line` (ASCII
fallback), or `Dots`. [API](https://docs.rs/tuika/latest/tuika/struct.Spinner.html)

![Spinner](demos/spinner.gif)

```rust
use tuika::{Spinner, SpinnerStyle};
Spinner::new(frame).style(SpinnerStyle::Braille);
```

### `ProgressBar`

A single-row bar: determinate (sub-cell eighth-block fill, optional `NN%`) or an
indeterminate marquee driven by the frame counter.
[API](https://docs.rs/tuika/latest/tuika/struct.ProgressBar.html)

![ProgressBar](demos/progress_bar.gif)

```rust
use tuika::ProgressBar;
ProgressBar::determinate(0.6).percent(true);
ProgressBar::indeterminate(frame);
```

### `Loader`

A spinner, a message, and an optional trailing hint on one row.
[API](https://docs.rs/tuika/latest/tuika/struct.Loader.html)

![Loader](demos/loader.gif)

```rust
use tuika::Loader;
Loader::new(frame, "compiling crate…").hint("esc to cancel");
```

## Text

### `Text`

A block of pre-styled [`Line`](https://docs.rs/ratatui)s drawn top-down and
clipped. `Paragraph` word-wraps plain text in one style; `Wrap` word-wraps
pre-styled lines while preserving per-span styles.
[API](https://docs.rs/tuika/latest/tuika/struct.Text.html)

![Text](demos/text.gif)

```rust
use tuika::{Text, Paragraph};
Text::raw("hello");
Paragraph::new("word-wrapped prose", style);
```

### `Rule`

A one-row horizontal separator: optional leading title, then a fill glyph out to
the width. [API](https://docs.rs/tuika/latest/tuika/struct.Rule.html)

![Rule](demos/rule.gif)

```rust
use tuika::Rule;
Rule::new().title(" Section ");
```

## Layout

### `Flex`

The flexbox container and composition primitive — `grow(n)` children share
leftover space by weight, `fixed(n)` reserve exact size, with `gap` and
`padding`. [API](https://docs.rs/tuika/latest/tuika/struct.Flex.html)

![Flex](demos/flex.gif)

```rust
use tuika::{Flex, element};
Flex::row().gap(1)
    .grow(1, element(left))
    .fixed(12, element(right));
```

### `Boxed`

A border + padding + title wrapping one child; the border color is focus-aware.
[API](https://docs.rs/tuika/latest/tuika/struct.Boxed.html)

![Boxed](demos/boxed.gif)

```rust
use tuika::{Boxed, BorderStyle, element};
Boxed::new(element(child)).title(" title ").border(BorderStyle::Rounded);
```

### `StatusBar`

One row with left- and right-anchored segment groups.
[API](https://docs.rs/tuika/latest/tuika/struct.StatusBar.html)

![StatusBar](demos/status_bar.gif)

```rust
use tuika::StatusBar;
StatusBar::new().left(left_spans).right(right_spans);
```

## Interactive

Each pairs a rendered view with a host-persisted `*State` (the
`StatefulWidget` idiom): the state owns cursor/offset/selection and handles
events, the view borrows it for a frame.

### `Scroll` + `ScrollState`

A windowed view over long content with a scrollbar; `ScrollState` handles
paging, wheel scroll, and stick-to-bottom.
[API](https://docs.rs/tuika/latest/tuika/struct.Scroll.html)

![Scroll](demos/scroll.gif)

```rust
use tuika::{Scroll, ScrollState};
let mut state = ScrollState::new();      // held by the host across frames
state.handle(&event, content_h, viewport_h);
Scroll::new(lines, &state);
```

### `SelectList` + `SelectState`

A selectable list; `SelectState` navigates with the arrow keys (wrapping),
confirms on Enter, cancels on Esc.
[API](https://docs.rs/tuika/latest/tuika/struct.SelectList.html)

![SelectList](demos/select.gif)

```rust
use tuika::{SelectList, SelectState};
let mut state = SelectState::new();
state.handle(&event, items.len());
SelectList::new(items, &state);
```

### `Tabs` + `TabsState`

A one-line tab strip; `TabsState` handles left/right and tab navigation.
[API](https://docs.rs/tuika/latest/tuika/struct.Tabs.html)

![Tabs](demos/tabs.gif)

```rust
use tuika::{Tabs, TabsState};
let mut state = TabsState::default();
state.handle(&event, labels.len());
Tabs::new(labels, &state);
```

### `TextInput` + `TextInputState`

A multi-line edit model: buffer, cursor, editing, and soft-wrap. `TextInput`
renders a snapshot; the host places the terminal cursor from
`TextInputState::cursor_screen`.
[API](https://docs.rs/tuika/latest/tuika/struct.TextInput.html)

![TextInput](demos/textinput.gif)

```rust
use tuika::{TextInput, TextInputState};
let mut state = TextInputState::from_text("");
state.handle(&event);
TextInput::new(&state);
```

## Also in the toolkit

Structural helpers that shape other components rather than paint content, so they
have no standalone demo: **`Spacer`** (flexible filler), **`Responsive`**
(pick a compact/wide subtree at a width breakpoint), **`Constrained`**
(min/max intrinsic size), **`Wrap`** (style-preserving word wrap), and
**`KeyHints`** (a row of `key → action` labels). See the
[crate docs](https://docs.rs/tuika) and the runnable
[`examples/`](../examples/).

## Regenerating the demos

Each demo is one scene in [`examples/demo.rs`](../examples/demo.rs) — a pure
function of a frame counter — recorded by the matching tape in
[`tapes/`](tapes/) with [VHS](https://github.com/charmbracelet/vhs).

```bash
# One component:
cargo run -p tuika --example demo -- spinner        # interactive (q/esc quits)
cargo run -p tuika --example demo -- spinner --dump  # print one frame as text
cargo run -p tuika --example demo -- list            # list every scene

# All GIFs (needs vhs + ttyd + ffmpeg on PATH):
crates/tuika/docs/generate.sh                        # or: generate.sh spinner tabs
```

### Adding a component demo

1. Add a `scene_*` function and a `DEMOS` entry in `examples/demo.rs`.
2. Confirm it renders: `cargo run -p tuika --example demo -- <name> --dump`.
3. Add `docs/tapes/<name>.tape` (copy an existing one; set the `Height`).
4. Record it: `crates/tuika/docs/generate.sh <name>`.
5. Reference `demos/<name>.gif` here and inline on the component's `struct` doc
   (via the `raw.githubusercontent.com/.../main/...` URL, so docs.rs resolves it).

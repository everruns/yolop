# tuika component gallery

A visual catalog of tuika's components — each with a name, a one-line
description, and an animated demo.

## Motion

Animated from a host-supplied frame counter (see the [`anim`](https://docs.rs/tuika/latest/tuika/anim/index.html) module).

### `Spinner`

A frame-cycled activity glyph — `Braille` (smooth default), `Line` (ASCII
fallback), or `Dots`. [API](https://docs.rs/tuika/latest/tuika/struct.Spinner.html)

<img src="demos/spinner.gif" width="880" alt="Spinner demo">

```rust
use tuika::{Spinner, SpinnerStyle, view};
view! {
    row(gap = 1) {
        node(Spinner::new(frame).style(SpinnerStyle::Braille))
        text("working…")
    }
}
```

### `ProgressBar`

A single-row bar: determinate (sub-cell eighth-block fill, optional `NN%`) or an
indeterminate marquee driven by the frame counter.
[API](https://docs.rs/tuika/latest/tuika/struct.ProgressBar.html)

<img src="demos/progress_bar.gif" width="880" alt="ProgressBar demo">

```rust
use tuika::{ProgressBar, view};
view! {
    col(gap = 1) {
        node(ProgressBar::determinate(0.6).percent(true))
        node(ProgressBar::indeterminate(frame))
    }
}
```

### `Loader`

A spinner, a message, and an optional trailing hint on one row.
[API](https://docs.rs/tuika/latest/tuika/struct.Loader.html)

<img src="demos/loader.gif" width="880" alt="Loader demo">

```rust
use tuika::{Loader, view};
view! {
    node(Loader::new(frame, "compiling crate…").hint("esc to cancel"))
}
```

## Text

### `Text`

A block of pre-styled [`Line`](https://docs.rs/ratatui)s drawn top-down and
clipped. `Paragraph` word-wraps plain text in one style; `Wrap` word-wraps
pre-styled lines while preserving per-span styles.
[API](https://docs.rs/tuika/latest/tuika/struct.Text.html)

<img src="demos/text.gif" width="880" alt="Text demo">

```rust
use tuika::{Paragraph, view};
view! {
    col(gap = 1) {
        text("a plain line")
        node(Paragraph::new("word-wrapped prose", style))
    }
}
```

### `Rule`

A one-row horizontal separator: optional leading title, then a fill glyph out to
the width. [API](https://docs.rs/tuika/latest/tuika/struct.Rule.html)

<img src="demos/rule.gif" width="880" alt="Rule demo">

```rust
use tuika::{Rule, view};
view! {
    node(Rule::new().title(" Section "))
}
```

## Layout

### `Flex`

The flexbox container and composition primitive — `grow(n)` children share
leftover space by weight, `fixed(n)` reserve exact size, with `gap` and
`padding`. It *is* the `view!` DSL's `row`/`col`.
[API](https://docs.rs/tuika/latest/tuika/struct.Flex.html)

<img src="demos/flex.gif" width="880" alt="Flex demo">

```rust
use tuika::view;
view! {
    row(gap = 1) {
        grow(1) { node(left) }
        fixed(12) { node(right) }
    }
}
```

### `Boxed`

A border + padding + title wrapping one child; the border color is focus-aware.
[API](https://docs.rs/tuika/latest/tuika/struct.Boxed.html)

<img src="demos/boxed.gif" width="880" alt="Boxed demo">

```rust
use tuika::{BorderStyle, view};
view! {
    boxed(title = " title ", border = BorderStyle::Rounded) {
        node(child)
    }
}
```

### `StatusBar`

One row with left- and right-anchored segment groups.
[API](https://docs.rs/tuika/latest/tuika/struct.StatusBar.html)

<img src="demos/status_bar.gif" width="880" alt="StatusBar demo">

```rust
use tuika::{StatusBar, view};
view! {
    node(StatusBar::new().left(left_spans).right(right_spans))
}
```

## Interactive

Each pairs a rendered view with a host-persisted `*State` (the
`StatefulWidget` idiom): the state owns cursor/offset/selection and handles
events, the view borrows it for a frame.

### `Scroll` + `ScrollState`

A windowed view over long content with a scrollbar; `ScrollState` handles
paging, wheel scroll, and stick-to-bottom.
[API](https://docs.rs/tuika/latest/tuika/struct.Scroll.html)

<img src="demos/scroll.gif" width="880" alt="Scroll demo">

```rust
use tuika::{Scroll, ScrollState, view};
let mut state = ScrollState::new();          // held by the host across frames
state.handle(&event, content_h, viewport_h);
view! { node(Scroll::new(lines, &state)) }
```

### `SelectList` + `SelectState`

A selectable list; `SelectState` navigates with the arrow keys (wrapping),
confirms on Enter, cancels on Esc.
[API](https://docs.rs/tuika/latest/tuika/struct.SelectList.html)

<img src="demos/select.gif" width="880" alt="SelectList demo">

```rust
use tuika::{SelectList, SelectState, view};
let mut state = SelectState::new();
state.handle(&event, items.len());
view! { node(SelectList::new(items, &state)) }
```

### `Tabs` + `TabsState`

A one-line tab strip; `TabsState` handles left/right and tab navigation.
[API](https://docs.rs/tuika/latest/tuika/struct.Tabs.html)

<img src="demos/tabs.gif" width="880" alt="Tabs demo">

```rust
use tuika::{Tabs, TabsState, view};
let mut state = TabsState::default();
state.handle(&event, labels.len());
view! { node(Tabs::new(labels, &state)) }
```

### `TextInput` + `TextInputState`

A multi-line edit model: buffer, cursor, editing, and soft-wrap. `TextInput`
renders a snapshot; the host places the terminal cursor from
`TextInputState::cursor_screen`.
[API](https://docs.rs/tuika/latest/tuika/struct.TextInput.html)

<img src="demos/textinput.gif" width="880" alt="TextInput demo">

```rust
use tuika::{TextInput, TextInputState, view};
let state = TextInputState::from_text("");
view! {
    boxed(title = " commit message ") {
        node(TextInput::new(&state))
    }
}
```

## See also

- [API documentation](https://docs.rs/tuika) — the complete component reference,
  including helpers without a standalone demo (`Spacer`, `Responsive`,
  `Constrained`, `Wrap`, `KeyHints`).
- [Runnable examples](../examples/) — enter the alternate screen; quit with `q`/`esc`.
- [README](../README.md) — the model behind the toolkit.

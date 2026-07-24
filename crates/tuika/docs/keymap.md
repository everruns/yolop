# tuika keymap

A host-agnostic binding engine: declare an application's key bindings once, as
data, and resolve incoming key presses to named commands. It sits beside the
[component gallery](components.md) and [terminal features](features.md) as a
third kind of tuika primitive — not a widget and not an escape sequence, but the
routing layer between input and behavior.

It follows the **register → dispatch → query** shape of
[OpenTUI's keymap](https://opentui.com/docs/keymap/overview/), adapted to
idiomatic Rust and to tuika's own [`Key`] events. Because it consumes the
already-translated `Key` — never a terminal type — the whole engine runs and
unit-tests without a PTY.
[API](https://docs.rs/tuika/latest/tuika/keymap/index.html)

## The model

Four types, smallest to largest:

- **`Chord`** — one key press plus its modifiers (`ctrl+r`, `alt+shift+tab`,
  `?`, `space`, the literal `ctrl++`). Parse it from a string with
  `Chord::parse`, or build it from a live event with `Chord::from_key`.
- **`KeySequence`** — one *or more* chords typed in order, written
  space-separated (`g g`, `ctrl+x s`). A single chord is a one-element sequence,
  so every binding is a sequence underneath.
- **`Layer`** — a named, prioritized group of bindings. A layer may be *gated*
  on runtime data (`when("mode", "search")`) so it is active only in a given
  application mode; an ungated layer is always active.
- **`Keymap<C>`** — the engine. It owns the layers, a small runtime-data store
  used to gate them, and the pending multi-stroke state. `C` is your command
  type — usually an enum.

## Defining bindings

Bindings are built declaratively and resolve to a command value of your choosing:

```rust
use tuika::keymap::{Keymap, Layer};

#[derive(Clone, Debug, PartialEq)]
enum Action { Search, Quit, Top, Close }

let mut keymap = Keymap::new()
    .layer(
        Layer::new("global")
            .bind_labeled("ctrl+r", "search", Action::Search)
            .bind("ctrl+d", Action::Quit)
            // A two-stroke sequence, vim-style.
            .bind("g g", Action::Top),
    );
```

`bind` (and `bind_labeled`, which also carries a help string) panics on a
malformed spec, because bindings are authored as static literals — a bad spec is
a programmer error caught on first run, like a malformed regex literal. For
config-sourced specs, use `Chord::parse` / `Layer::try_bind`, which return a
`Result`.

## Dispatching

Feed each translated `Key` to `dispatch`. It returns one of three outcomes:

```rust
use tuika::event::{Key, KeyCode};
use tuika::keymap::Dispatch;

let ctrl_r = Key { code: KeyCode::Char('r'), ctrl: true, alt: false, shift: false };
match keymap.dispatch(ctrl_r) {
    Dispatch::Command(action) => { /* run it */ }
    Dispatch::Pending         => { /* a multi-stroke sequence is mid-flight */ }
    Dispatch::Unmatched       => { /* no binding — handle the key yourself */ }
}
```

- **Single-stroke** bindings resolve immediately.
- **Multi-stroke** sequences accumulate: the first `g` of `g g` returns
  `Pending`, the second returns `Command(Top)`.
- An **exact** match wins immediately even when it is also the prefix of a
  longer binding — a bound `g` fires without waiting to see whether `g g`
  follows, so author overlapping bindings with that in mind.
- A sequence that **dead-ends** is dropped, and its final stroke is retried on
  its own so it can begin a fresh sequence. `Keymap::reset` abandons a pending
  sequence explicitly (e.g. on a focus change or a timeout tick — the engine
  keeps no clock of its own).

A key that no active binding matches returns `Unmatched`, so the host stays in
control of everything the keymap does not claim (typing into a text field,
say).

### Modifier normalization

Matching is on a normalized chord, so a binding matches the event a terminal
actually delivers. For a character key, Shift is folded into the character
itself (`?` arrives as `Char('?')`, not `Shift`+`Char('/')`), so the `shift`
flag is dropped for characters and kept meaningful only for non-character keys
(`Shift`+`Enter`). `Shift`+`Tab` folds to the distinct `BackTab` key terminals
report. Both the string parser and `Chord::from_key` apply the same rules, so a
parsed binding and a live event agree.

## Modes: layers gated on runtime data

`set_data` drives which layers are active, mirroring an application's modes. A
layer's `when` clauses must all match the current data for it to be active;
changing data recomputes activation and clears any now-impossible pending
sequence.

```rust
let mut keymap = Keymap::new()
    .layer(Layer::new("global").bind("ctrl+d", Action::Quit))
    .layer(
        Layer::new("panel")
            .when("mode", "panel")
            .bind("q", Action::Close),
    );

// `q` does nothing until the app enters panel mode…
keymap.set_data("mode", "panel");
// …now it closes the panel, while ctrl+d still quits in every mode.
```

When two active layers bind the same sequence, the higher `priority` wins, so an
overlay layer can shadow a global binding while it is up.

## Querying for help

`hints()` lists the currently-active bindings — key label, optional help text,
and command — ready to feed a [`KeyHints`](components.md) row or a help overlay.
Because activation is data-driven, the hints always reflect the current mode:

```rust
for hint in keymap.hints() {
    println!("{:<8} {}", hint.keys, hint.label.as_deref().unwrap_or(""));
    // e.g.  "Ctrl+R   search"
}
```

## Testing

The engine never touches the terminal, so behavior is tested by feeding
synthetic `Key`s and asserting the `Dispatch` outcome — no raw mode, no PTY:

```rust
let mut keymap = Keymap::new().layer(Layer::new("nav").bind("g g", Action::Top));
let g = Key::new(KeyCode::Char('g'));
assert_eq!(keymap.dispatch(g), Dispatch::Pending);
assert_eq!(keymap.dispatch(g), Dispatch::Command(Action::Top));
```

[`Key`]: https://docs.rs/tuika/latest/tuika/event/struct.Key.html

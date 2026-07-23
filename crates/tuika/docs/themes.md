# tuika themes

Every tuika component styles itself from a [`Theme`](https://docs.rs/tuika/latest/tuika/struct.Theme.html)
passed through the render context — no component hard-codes a color. Swapping
the theme handed to [`paint`](https://docs.rs/tuika/latest/tuika/fn.paint.html)
restyles the whole tree at once.

A `Theme` is a plain `Copy` struct of colors, so a palette is just data. tuika
bundles a few standard palettes as full `const Theme` structures in the
[`themes`](https://docs.rs/tuika/latest/tuika/themes/index.html) module. Reach
one directly, by constructor, or by name:

```rust
use tuika::themes;

let a = themes::GRUVBOX_DARK;            // the struct
let b = tuika::Theme::gruvbox_dark();    // named constructor
let c = tuika::theme_by_name("gruvbox-dark").unwrap(); // config / --theme
assert_eq!(a, b);
assert_eq!(a, c);
```

Iterate [`themes::PRESETS`](https://docs.rs/tuika/latest/tuika/themes/constant.PRESETS.html)
to build a picker or validate a `--theme` value — each entry carries a stable
`name`, a human `label`, and the `theme` itself.

Each demo below is the **same** animated scene (the component gallery from
[`examples/screenshot.rs`](../examples/screenshot.rs)) driven through that one
palette — an honest side-by-side of what a theme does to real components,
recorded the same way as the [component gallery](components.md):

```text
crates/tuika/scripts/gen-tuika-theme-demos.sh          # all themes
crates/tuika/scripts/gen-tuika-theme-demos.sh dracula  # just one
```

## `solarized-dark`

Ethan Schoonover's precision palette on the dark base tones.
[struct](https://docs.rs/tuika/latest/tuika/themes/constant.SOLARIZED_DARK.html)

<img src="themes/theme-solarized-dark.gif" width="880" alt="Solarized Dark theme">

## `solarized-light`

The same accents on Solarized's light base tones — the light half of the pair.
[struct](https://docs.rs/tuika/latest/tuika/themes/constant.SOLARIZED_LIGHT.html)

<img src="themes/theme-solarized-light.gif" width="880" alt="Solarized Light theme">

## `gruvbox-dark`

morhetz's warm, retro-tinted dark palette (medium contrast).
[struct](https://docs.rs/tuika/latest/tuika/themes/constant.GRUVBOX_DARK.html)

<img src="themes/theme-gruvbox-dark.gif" width="880" alt="Gruvbox Dark theme">

## `light`

A neutral, near-monochrome palette for light terminals — the understated light
default, not a brand.
[struct](https://docs.rs/tuika/latest/tuika/themes/constant.LIGHT.html)

<img src="themes/theme-light.gif" width="880" alt="Light theme">

## `dracula`

The widely-ported dark theme — purple/pink accents on a slate background.
[struct](https://docs.rs/tuika/latest/tuika/themes/constant.DRACULA.html)

<img src="themes/theme-dracula.gif" width="880" alt="Dracula theme">

## Rolling your own

The bundled palettes are a starting point, not a ceiling. Construct a `Theme`
literal (or clone a preset and tweak a few slots) and every component follows —
that is exactly how yolop builds its own look on top of tuika.

```rust
use tuika::{Theme, themes};

let mine = Theme {
    accent: ratatui::style::Color::Rgb(45, 91, 158),
    ..themes::GRUVBOX_DARK
};
```

## See also

- [`themes` API](https://docs.rs/tuika/latest/tuika/themes/index.html) — the
  presets, the `NamedTheme` catalog, and `by_name`.
- [component gallery](components.md) — the components these palettes style.
- [README](../README.md) — the model behind the toolkit.

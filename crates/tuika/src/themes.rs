//! Bundled standard themes.
//!
//! [`Theme`](crate::Theme) is a plain `Copy` struct of colors, so a palette is
//! just data — these presets are written out as full `const Theme` structures
//! rather than hidden behind a builder. Each is also reachable by a stable
//! `name` through [`by_name`] and the [`PRESETS`] catalog, so a host can offer a
//! `--theme` flag or a picker without hard-coding the list.
//!
//! ```
//! use tuika::themes;
//!
//! let t = themes::SOLARIZED_DARK;                    // the struct directly
//! let same = themes::by_name("solarized-dark").unwrap(); // …or by name
//! assert_eq!(t, same);
//! ```
//!
//! The palettes below use each project's published colors. tuika maps them onto
//! its own slots (`background`, `surface`, `accent`, the [`CodeTheme`] syntax
//! roles); it does not reproduce every upstream UI element.

use ratatui_core::style::Color;

use crate::style::{CodeTheme, Theme};

/// A bundled [`Theme`] paired with the identifiers a host uses to expose it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NamedTheme {
    /// Stable, lowercase, kebab-case id for config and `--theme` (`"gruvbox-dark"`).
    pub name: &'static str,
    /// Human-facing label for menus and pickers (`"Gruvbox Dark"`).
    pub label: &'static str,
    /// The palette itself.
    pub theme: Theme,
}

/// Every bundled theme, in menu order. Iterate this to build a picker or to
/// validate a `--theme` value.
pub const PRESETS: &[NamedTheme] = &[
    NamedTheme {
        name: "solarized-dark",
        label: "Solarized Dark",
        theme: SOLARIZED_DARK,
    },
    NamedTheme {
        name: "solarized-light",
        label: "Solarized Light",
        theme: SOLARIZED_LIGHT,
    },
    NamedTheme {
        name: "gruvbox-dark",
        label: "Gruvbox Dark",
        theme: GRUVBOX_DARK,
    },
    NamedTheme {
        name: "light",
        label: "Light",
        theme: LIGHT,
    },
    NamedTheme {
        name: "dracula",
        label: "Dracula",
        theme: DRACULA,
    },
];

/// Look a bundled theme up by its [`NamedTheme::name`], case-insensitively.
/// Returns `None` for an unknown id; pair it with [`PRESETS`] to list the valid
/// choices in an error message.
pub fn by_name(name: &str) -> Option<Theme> {
    PRESETS
        .iter()
        .find(|p| p.name.eq_ignore_ascii_case(name))
        .map(|p| p.theme)
}

// --- Solarized (Ethan Schoonover) -----------------------------------------
// base03 #002b36, base02 #073642, base01 #586e75, base00 #657b83,
// base0 #839496, base1 #93a1a1, base2 #eee8d5, base3 #fdf6e3,
// yellow #b58900, orange #cb4b16, red #dc322f, magenta #d33682,
// blue #268bd2, cyan #2aa198, green #859900.

/// Solarized Dark — Ethan Schoonover's precision palette on the dark base tones.
pub const SOLARIZED_DARK: Theme = Theme {
    background: Color::Rgb(0, 43, 54),    // base03
    surface: Color::Rgb(7, 54, 66),       // base02
    text: Color::Rgb(131, 148, 150),      // base0
    muted: Color::Rgb(88, 110, 117),      // base01
    dim: Color::Rgb(58, 82, 92),          // between base01 and base02
    accent: Color::Rgb(38, 139, 210),     // blue
    accent_alt: Color::Rgb(42, 161, 152), // cyan
    border: Color::Rgb(58, 82, 92),
    border_focused: Color::Rgb(38, 139, 210),
    selection_bg: Color::Rgb(7, 54, 66),     // base02
    selection_fg: Color::Rgb(147, 161, 161), // base1
    code: CodeTheme {
        heading: Color::Rgb(147, 161, 161),     // base1
        link: Color::Rgb(38, 139, 210),         // blue
        background: Color::Rgb(7, 54, 66),      // base02
        text: Color::Rgb(131, 148, 150),        // base0
        label: Color::Rgb(88, 110, 117),        // base01
        keyword: Color::Rgb(133, 153, 0),       // green
        function: Color::Rgb(38, 139, 210),     // blue
        type_name: Color::Rgb(181, 137, 0),     // yellow
        constant: Color::Rgb(211, 54, 130),     // magenta
        string: Color::Rgb(42, 161, 152),       // cyan
        comment: Color::Rgb(88, 110, 117),      // base01
        punctuation: Color::Rgb(101, 123, 131), // base00
    },
};

/// Solarized Light — the same accents on the light base tones.
pub const SOLARIZED_LIGHT: Theme = Theme {
    background: Color::Rgb(253, 246, 227), // base3
    surface: Color::Rgb(238, 232, 213),    // base2
    text: Color::Rgb(101, 123, 131),       // base00
    muted: Color::Rgb(147, 161, 161),      // base1
    dim: Color::Rgb(203, 198, 182),        // faint over the light base
    accent: Color::Rgb(38, 139, 210),      // blue
    accent_alt: Color::Rgb(42, 161, 152),  // cyan
    border: Color::Rgb(147, 161, 161),     // base1
    border_focused: Color::Rgb(38, 139, 210),
    selection_bg: Color::Rgb(238, 232, 213), // base2
    selection_fg: Color::Rgb(88, 110, 117),  // base01
    code: CodeTheme {
        heading: Color::Rgb(88, 110, 117),      // base01
        link: Color::Rgb(38, 139, 210),         // blue
        background: Color::Rgb(238, 232, 213),  // base2
        text: Color::Rgb(101, 123, 131),        // base00
        label: Color::Rgb(147, 161, 161),       // base1
        keyword: Color::Rgb(133, 153, 0),       // green
        function: Color::Rgb(38, 139, 210),     // blue
        type_name: Color::Rgb(181, 137, 0),     // yellow
        constant: Color::Rgb(211, 54, 130),     // magenta
        string: Color::Rgb(42, 161, 152),       // cyan
        comment: Color::Rgb(147, 161, 161),     // base1
        punctuation: Color::Rgb(101, 123, 131), // base00
    },
};

// --- Gruvbox (morhetz), dark medium ---------------------------------------
// bg0 #282828, bg1 #3c3836, bg2 #504945, bg3 #665c54, fg0 #fbf1c7,
// fg1 #ebdbb2, fg4 #a89984, gray #928374, bright red #fb4934,
// bright green #b8bb26, bright yellow #fabd2f, bright purple #d3869b,
// bright aqua #8ec07c, bright orange #fe8019, neutral blue #83a598.

/// Gruvbox Dark (medium) — morhetz's warm retro palette.
pub const GRUVBOX_DARK: Theme = Theme {
    background: Color::Rgb(40, 40, 40),    // bg0
    surface: Color::Rgb(60, 56, 54),       // bg1
    text: Color::Rgb(235, 219, 178),       // fg1
    muted: Color::Rgb(168, 153, 132),      // fg4
    dim: Color::Rgb(102, 92, 84),          // bg3
    accent: Color::Rgb(254, 128, 25),      // bright orange
    accent_alt: Color::Rgb(142, 192, 124), // bright aqua
    border: Color::Rgb(102, 92, 84),       // bg3
    border_focused: Color::Rgb(254, 128, 25),
    selection_bg: Color::Rgb(80, 73, 69),    // bg2
    selection_fg: Color::Rgb(251, 241, 199), // fg0
    code: CodeTheme {
        heading: Color::Rgb(235, 219, 178),     // fg1
        link: Color::Rgb(131, 165, 152),        // neutral blue
        background: Color::Rgb(29, 32, 33),     // bg0_hard
        text: Color::Rgb(235, 219, 178),        // fg1
        label: Color::Rgb(168, 153, 132),       // fg4
        keyword: Color::Rgb(251, 73, 52),       // bright red
        function: Color::Rgb(142, 192, 124),    // bright aqua
        type_name: Color::Rgb(250, 189, 47),    // bright yellow
        constant: Color::Rgb(211, 134, 155),    // bright purple
        string: Color::Rgb(184, 187, 38),       // bright green
        comment: Color::Rgb(146, 131, 116),     // gray
        punctuation: Color::Rgb(168, 153, 132), // fg4
    },
};

// --- Plain Light -----------------------------------------------------------
// A neutral, near-monochrome light theme for terminals with a light background.
// Deliberately understated so it reads as "the light default", not a brand.

/// Light — a neutral, near-monochrome palette for light terminals.
pub const LIGHT: Theme = Theme {
    background: Color::Rgb(250, 250, 250),
    surface: Color::Rgb(240, 240, 240),
    text: Color::Rgb(30, 30, 30),
    muted: Color::Rgb(110, 110, 110),
    dim: Color::Rgb(190, 190, 190),
    accent: Color::Rgb(30, 102, 204),    // a calm blue
    accent_alt: Color::Rgb(0, 135, 120), // teal
    border: Color::Rgb(200, 200, 200),
    border_focused: Color::Rgb(30, 102, 204),
    selection_bg: Color::Rgb(210, 226, 250),
    selection_fg: Color::Rgb(20, 40, 70),
    code: CodeTheme {
        heading: Color::Rgb(20, 20, 20),
        link: Color::Rgb(30, 102, 204),
        background: Color::Rgb(240, 240, 240),
        text: Color::Rgb(40, 40, 40),
        label: Color::Rgb(110, 110, 110),
        keyword: Color::Rgb(170, 13, 145),  // magenta
        function: Color::Rgb(30, 102, 204), // blue
        type_name: Color::Rgb(0, 92, 197),
        constant: Color::Rgb(155, 76, 0), // brown/orange
        string: Color::Rgb(24, 128, 56),  // green
        comment: Color::Rgb(120, 130, 120),
        punctuation: Color::Rgb(90, 90, 90),
    },
};

// --- Dracula (Zeno Rocha et al.) ------------------------------------------
// bg #282a36, current-line #44475a, fg #f8f8f2, comment #6272a4,
// cyan #8be9fd, green #50fa7b, orange #ffb86c, pink #ff79c6,
// purple #bd93f9, red #ff5555, yellow #f1fa8c.

/// Dracula — the widely-ported dark theme.
pub const DRACULA: Theme = Theme {
    background: Color::Rgb(40, 42, 54),
    surface: Color::Rgb(68, 71, 90), // current line
    text: Color::Rgb(248, 248, 242),
    muted: Color::Rgb(98, 114, 164), // comment
    dim: Color::Rgb(73, 76, 98),
    accent: Color::Rgb(189, 147, 249),     // purple
    accent_alt: Color::Rgb(255, 121, 198), // pink
    border: Color::Rgb(98, 114, 164),
    border_focused: Color::Rgb(189, 147, 249),
    selection_bg: Color::Rgb(68, 71, 90),
    selection_fg: Color::Rgb(248, 248, 242),
    code: CodeTheme {
        heading: Color::Rgb(248, 248, 242),
        link: Color::Rgb(139, 233, 253), // cyan
        background: Color::Rgb(33, 34, 44),
        text: Color::Rgb(248, 248, 242),
        label: Color::Rgb(98, 114, 164),
        keyword: Color::Rgb(255, 121, 198),   // pink
        function: Color::Rgb(80, 250, 123),   // green
        type_name: Color::Rgb(139, 233, 253), // cyan
        constant: Color::Rgb(189, 147, 249),  // purple
        string: Color::Rgb(241, 250, 140),    // yellow
        comment: Color::Rgb(98, 114, 164),
        punctuation: Color::Rgb(248, 248, 242),
    },
};

impl Theme {
    /// The [`SOLARIZED_DARK`] preset.
    pub const fn solarized_dark() -> Theme {
        SOLARIZED_DARK
    }

    /// The [`SOLARIZED_LIGHT`] preset.
    pub const fn solarized_light() -> Theme {
        SOLARIZED_LIGHT
    }

    /// The [`GRUVBOX_DARK`] preset.
    pub const fn gruvbox_dark() -> Theme {
        GRUVBOX_DARK
    }

    /// The [`LIGHT`] preset.
    pub const fn light() -> Theme {
        LIGHT
    }

    /// The [`DRACULA`] preset.
    pub const fn dracula() -> Theme {
        DRACULA
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn by_name_matches_presets_case_insensitively() {
        for p in PRESETS {
            assert_eq!(by_name(p.name), Some(p.theme));
            assert_eq!(by_name(&p.name.to_uppercase()), Some(p.theme));
        }
        assert_eq!(by_name("no-such-theme"), None);
    }

    #[test]
    fn constructors_match_catalog() {
        assert_eq!(Theme::solarized_dark(), by_name("solarized-dark").unwrap());
        assert_eq!(
            Theme::solarized_light(),
            by_name("solarized-light").unwrap()
        );
        assert_eq!(Theme::gruvbox_dark(), by_name("gruvbox-dark").unwrap());
        assert_eq!(Theme::light(), by_name("light").unwrap());
        assert_eq!(Theme::dracula(), by_name("dracula").unwrap());
    }

    #[test]
    fn preset_names_are_unique_and_kebab_case() {
        let mut seen = std::collections::HashSet::new();
        for p in PRESETS {
            assert!(seen.insert(p.name), "duplicate preset name {}", p.name);
            assert!(
                p.name
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
                "preset name {} is not kebab-case",
                p.name
            );
            assert!(!p.label.is_empty());
        }
    }
}

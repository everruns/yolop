//! Terminal-respecting color theme for the TUI.
//!
//! Yolop runs inside the user's terminal, which already carries a chosen color
//! scheme. So rather than baking in absolute RGB values (which silently assume
//! a dark background and look broken on a light terminal), the palette is built
//! from the terminal's own colors:
//!
//! * **Accents** (`accent_blue`, `accent_gold`, the diff colors, …) are ANSI
//!   named colors. The terminal remaps these to whatever the user configured,
//!   so yolop inherits their theme for free and adapts to light/dark.
//! * **Primary text** is [`Color::Reset`] — the terminal's default foreground.
//!   Maximum contrast on both light and dark backgrounds, by construction.
//! * **Grays and panel/code backgrounds** are the only light/dark-sensitive
//!   part. They use the xterm-256 grayscale ramp (consistent across terminals)
//!   and flip direction by mode so a code block is a *subtle* shade off the
//!   background instead of an absolute near-black fill.
//!
//! The resolved theme is a process global so existing render code can read it
//! without threading a `&Theme` through every function. It is set once at TUI
//! startup; tests that never initialize it see the dark default.

use std::sync::OnceLock;

use ratatui::style::Color;

/// Semantic colors used across the TUI. Field names mirror the historical
/// constants so call sites read the same.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Theme {
    pub accent_blue: Color,
    pub accent_gold: Color,
    pub text_primary: Color,
    pub text_muted: Color,
    pub text_dim: Color,
    pub diff_add: Color,
    pub diff_delete: Color,
    pub diff_meta: Color,
    pub code_bg: Color,
    pub panel_bg: Color,
    /// Bright teal accent (e.g. interactive setup highlights).
    pub accent_cyan: Color,
    /// Warm warning accent (e.g. destructive hints).
    pub accent_warn: Color,
}

impl Theme {
    /// Build the palette for a dark or light terminal background.
    ///
    /// Accents are terminal-palette colors regardless of mode; only the grays
    /// and backgrounds depend on `dark`.
    pub fn for_terminal(dark: bool) -> Self {
        // Foreground accents inherit the user's 16-color palette.
        let accents = ThemeAccents {
            accent_blue: Color::Blue,
            accent_gold: Color::Yellow,
            diff_add: Color::Green,
            diff_delete: Color::Red,
            diff_meta: Color::Cyan,
            accent_cyan: Color::Cyan,
            accent_warn: Color::LightRed,
        };
        // Grayscale ramp indices (xterm-256): higher = lighter. Code is the
        // darkest inset; panel sits one step toward the text for separation.
        let (text_muted, text_dim, code_bg, panel_bg) = if dark {
            (
                Color::Indexed(245),
                Color::Indexed(240),
                Color::Indexed(235),
                Color::Indexed(237),
            )
        } else {
            (
                Color::Indexed(241),
                Color::Indexed(247),
                Color::Indexed(253),
                Color::Indexed(255),
            )
        };
        Self {
            accent_blue: accents.accent_blue,
            accent_gold: accents.accent_gold,
            // Default terminal foreground: correct on light and dark alike.
            text_primary: Color::Reset,
            text_muted,
            text_dim,
            diff_add: accents.diff_add,
            diff_delete: accents.diff_delete,
            diff_meta: accents.diff_meta,
            code_bg,
            panel_bg,
            accent_cyan: accents.accent_cyan,
            accent_warn: accents.accent_warn,
        }
    }
}

struct ThemeAccents {
    accent_blue: Color,
    accent_gold: Color,
    diff_add: Color,
    diff_delete: Color,
    diff_meta: Color,
    accent_cyan: Color,
    accent_warn: Color,
}

static THEME: OnceLock<Theme> = OnceLock::new();

/// Install the active theme. Idempotent: the first call wins, later calls (and
/// concurrent test setup) are ignored so the global stays stable.
pub fn init_theme(theme: Theme) {
    let _ = THEME.set(theme);
}

/// The active theme. Defaults to the dark palette when uninitialized, which is
/// what unit tests rendering snippets without a TUI see.
pub fn theme() -> &'static Theme {
    THEME.get_or_init(|| Theme::for_terminal(true))
}

/// Best-effort detection of a dark terminal background.
///
/// Reads `COLORFGBG` (set by many terminals as `fg;bg` or `fg;default;bg`,
/// where the trailing field is the background color index). Indices 7 and 15
/// are the light-gray/white backgrounds; everything else — and an absent or
/// unparseable value — is treated as dark, matching the historical default.
pub fn detect_dark_background() -> bool {
    let Ok(value) = std::env::var("COLORFGBG") else {
        return true;
    };
    let bg = value
        .rsplit(';')
        .next()
        .and_then(|s| s.trim().parse::<u8>().ok());
    // 7 (light gray) and 15 (white) are the light backgrounds; treat anything
    // else — including an unparseable trailing field — as dark.
    !matches!(bg, Some(7) | Some(15))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primary_text_uses_terminal_default() {
        // Reset means "the terminal's own foreground", so it is correct on both
        // light and dark backgrounds without per-mode tuning.
        assert_eq!(Theme::for_terminal(true).text_primary, Color::Reset);
        assert_eq!(Theme::for_terminal(false).text_primary, Color::Reset);
    }

    #[test]
    fn accents_are_palette_colors_in_both_modes() {
        let dark = Theme::for_terminal(true);
        let light = Theme::for_terminal(false);
        // Accents come from the terminal palette and do not depend on mode.
        assert_eq!(dark.accent_blue, Color::Blue);
        assert_eq!(dark.accent_blue, light.accent_blue);
        assert_eq!(dark.diff_add, light.diff_add);
    }

    #[test]
    fn backgrounds_flip_for_light_terminals() {
        // The concrete light-terminal bug: a dark fill must not be reused. The
        // light palette's code background is a light ramp index, not a dark one.
        let dark = Theme::for_terminal(true);
        let light = Theme::for_terminal(false);
        assert_ne!(dark.code_bg, light.code_bg);
        assert_ne!(dark.panel_bg, light.panel_bg);
        assert_eq!(light.code_bg, Color::Indexed(253));
        assert_eq!(dark.code_bg, Color::Indexed(235));
    }

    #[test]
    fn colorfgbg_light_background_detected() {
        // Guard against env bleed from a real terminal running the tests.
        temp_env_colorfgbg(Some("0;15"), || assert!(!detect_dark_background()));
        temp_env_colorfgbg(Some("15;0"), || assert!(detect_dark_background()));
        temp_env_colorfgbg(Some("0;default;15"), || assert!(!detect_dark_background()));
        temp_env_colorfgbg(None, || assert!(detect_dark_background()));
        temp_env_colorfgbg(Some("garbage"), || assert!(detect_dark_background()));
    }

    fn temp_env_colorfgbg(value: Option<&str>, f: impl FnOnce()) {
        let prev = std::env::var("COLORFGBG").ok();
        match value {
            Some(v) => unsafe { std::env::set_var("COLORFGBG", v) },
            None => unsafe { std::env::remove_var("COLORFGBG") },
        }
        f();
        match prev {
            Some(v) => unsafe { std::env::set_var("COLORFGBG", v) },
            None => unsafe { std::env::remove_var("COLORFGBG") },
        }
    }
}

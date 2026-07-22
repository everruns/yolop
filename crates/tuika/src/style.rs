//! Styling and theming.
//!
//! `tuika` reuses ratatui's [`Style`], [`Color`], and [`Modifier`] rather than
//! reinventing cell attributes, but components never hard-code colors: they
//! pull from a [`Theme`] passed through the render context. Swapping the theme
//! restyles every component at once, and downstream libraries can ship their
//! own palette without touching component code.

use ratatui::style::{Color, Modifier, Style};

/// Line-drawing style for bordered components.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BorderStyle {
    #[default]
    Rounded,
    Plain,
    Thick,
    None,
}

impl BorderStyle {
    /// (top_left, top_right, bottom_left, bottom_right, horizontal, vertical)
    pub fn glyphs(self) -> BorderGlyphs {
        match self {
            BorderStyle::Rounded => BorderGlyphs::new('╭', '╮', '╰', '╯', '─', '│'),
            BorderStyle::Plain => BorderGlyphs::new('┌', '┐', '└', '┘', '─', '│'),
            BorderStyle::Thick => BorderGlyphs::new('┏', '┓', '┗', '┛', '━', '┃'),
            BorderStyle::None => BorderGlyphs::new(' ', ' ', ' ', ' ', ' ', ' '),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BorderGlyphs {
    pub top_left: char,
    pub top_right: char,
    pub bottom_left: char,
    pub bottom_right: char,
    pub horizontal: char,
    pub vertical: char,
}

impl BorderGlyphs {
    const fn new(
        top_left: char,
        top_right: char,
        bottom_left: char,
        bottom_right: char,
        horizontal: char,
        vertical: char,
    ) -> Self {
        Self {
            top_left,
            top_right,
            bottom_left,
            bottom_right,
            horizontal,
            vertical,
        }
    }
}

/// A named palette every component styles itself from.
///
/// The default mirrors yolop's existing inline palette so the full-screen
/// renderer looks like the same product. A different host can construct its
/// own `Theme` and the whole component tree follows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Theme {
    pub background: Color,
    pub surface: Color,
    pub text: Color,
    pub muted: Color,
    pub dim: Color,
    pub accent: Color,
    pub accent_alt: Color,
    pub border: Color,
    pub border_focused: Color,
    pub selection_bg: Color,
    pub selection_fg: Color,
    /// Markdown and code-block role colors, consumed by [`Markdown`] and
    /// [`CodeBlock`] (and any [`Highlighter`] the host plugs in).
    ///
    /// [`Markdown`]: crate::components::Markdown
    /// [`CodeBlock`]: crate::components::CodeBlock
    /// [`Highlighter`]: crate::highlight::Highlighter
    pub code: CodeTheme,
}

/// The palette [`Markdown`](crate::components::Markdown) and
/// [`CodeBlock`](crate::components::CodeBlock) style themselves from.
///
/// It carries two related things: a few markdown-prose roles (`heading`,
/// `link`, the inline/`block` code background) and the syntax roles a
/// [`Highlighter`](crate::highlight::Highlighter) maps token classes onto so
/// highlighted code follows the host theme instead of a hard-coded palette.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CodeTheme {
    /// Heading text (`# …`).
    pub heading: Color,
    /// Link text and bare URLs.
    pub link: Color,
    /// Background behind inline code and fenced blocks.
    pub background: Color,
    /// Default code text: unclassified tokens and the plain fallback.
    pub text: Color,
    /// Language-tag label above a fenced block.
    pub label: Color,
    // Syntax roles a highlighter maps token classes onto.
    pub keyword: Color,
    pub function: Color,
    pub type_name: Color,
    pub constant: Color,
    pub string: Color,
    pub comment: Color,
    pub punctuation: Color,
}

impl Default for Theme {
    fn default() -> Self {
        // tuika's own toolkit identity — a warm red-on-dark palette. This is a
        // neutral default for any app built on tuika; it is deliberately NOT any
        // one host's brand. A host with its own look builds its own `Theme` (see
        // yolop's `fullscreen::yolop_theme`) rather than relying on this.
        Theme {
            background: Color::Rgb(20, 18, 20),
            surface: Color::Rgb(34, 28, 30),
            text: Color::Rgb(235, 230, 230),
            muted: Color::Rgb(150, 140, 142),
            dim: Color::Rgb(90, 74, 78),
            accent: Color::Rgb(200, 60, 70),
            accent_alt: Color::Rgb(230, 140, 90),
            border: Color::Rgb(90, 74, 78),
            border_focused: Color::Rgb(200, 60, 70),
            selection_bg: Color::Rgb(120, 30, 40),
            selection_fg: Color::Rgb(240, 235, 235),
            code: CodeTheme::default(),
        }
    }
}

impl Default for CodeTheme {
    fn default() -> Self {
        // A warm, low-contrast syntax palette that sits alongside tuika's
        // default red-on-dark identity. Hosts with their own look build their
        // own `CodeTheme` (see yolop's `yolop_theme`).
        CodeTheme {
            heading: Color::Rgb(235, 230, 230),
            link: Color::Rgb(120, 160, 210),
            background: Color::Rgb(28, 24, 26),
            text: Color::Rgb(210, 205, 205),
            label: Color::Rgb(150, 140, 142),
            keyword: Color::Rgb(210, 120, 90),
            function: Color::Rgb(120, 160, 210),
            type_name: Color::Rgb(126, 170, 176),
            constant: Color::Rgb(200, 160, 120),
            string: Color::Rgb(150, 180, 140),
            comment: Color::Rgb(120, 110, 112),
            punctuation: Color::Rgb(150, 140, 142),
        }
    }
}

impl Theme {
    pub fn text_style(&self) -> Style {
        Style::default().fg(self.text)
    }

    pub fn muted_style(&self) -> Style {
        Style::default().fg(self.muted)
    }

    pub fn accent_style(&self) -> Style {
        Style::default()
            .fg(self.accent)
            .add_modifier(Modifier::BOLD)
    }

    pub fn border_color(&self, focused: bool) -> Color {
        if focused {
            self.border_focused
        } else {
            self.border
        }
    }

    pub fn selection_style(&self) -> Style {
        Style::default()
            .bg(self.selection_bg)
            .fg(self.selection_fg)
            .add_modifier(Modifier::BOLD)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::rainbow_theme;
    use ratatui::style::Modifier;

    #[test]
    fn theme_helper_styles_map_to_slots() {
        let t = rainbow_theme();
        assert_eq!(t.text_style().fg, Some(t.text));
        assert_eq!(t.muted_style().fg, Some(t.muted));
        assert_eq!(t.accent_style().fg, Some(t.accent));
        assert!(t.accent_style().add_modifier.contains(Modifier::BOLD));
        assert_eq!(t.border_color(false), t.border);
        assert_eq!(t.border_color(true), t.border_focused);
        let sel = t.selection_style();
        assert_eq!(sel.bg, Some(t.selection_bg));
        assert_eq!(sel.fg, Some(t.selection_fg));
        assert!(sel.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn default_theme_is_the_toolkit_identity_not_a_host_brand() {
        use ratatui::style::Color;
        // tuika's own look is warm red-on-dark. It is deliberately a neutral toolkit
        // identity, not any host's brand — a host with its own palette builds its own
        // `Theme` (e.g. yolop's `fullscreen::yolop_theme`) instead of inheriting this.
        let t = Theme::default();
        assert_eq!(t.accent, Color::Rgb(200, 60, 70));
        assert_eq!(t.selection_bg, Color::Rgb(120, 30, 40));
        // Not yolop's accent blue.
        assert_ne!(t.accent, Color::Rgb(45, 91, 158));
    }
}

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
}

impl Default for Theme {
    fn default() -> Self {
        // Kept in lockstep with the palette in `crate::app` so inline and
        // full-screen share one look.
        Theme {
            background: Color::Rgb(18, 18, 22),
            surface: Color::Rgb(28, 28, 34),
            text: Color::Rgb(230, 230, 232),
            muted: Color::Rgb(140, 140, 145),
            dim: Color::Rgb(72, 72, 78),
            accent: Color::Rgb(45, 91, 158),
            accent_alt: Color::Rgb(126, 94, 19),
            border: Color::Rgb(72, 72, 78),
            border_focused: Color::Rgb(45, 91, 158),
            selection_bg: Color::Rgb(45, 91, 158),
            selection_fg: Color::Rgb(230, 230, 232),
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

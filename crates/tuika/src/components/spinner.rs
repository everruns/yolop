//! Spinner — a frame-cycled activity indicator.
//!
//! Stateless: the host passes its frame counter and the spinner picks the glyph
//! for that frame. Pair with a steady tick (yolop advances `busy_frame` every
//! loop iteration while a turn runs) for smooth motion.

use ratatui::layout::Rect;
use ratatui::style::Style;

use crate::geometry::Size;
use crate::surface::Surface;
use crate::view::{RenderCtx, View};

/// A named set of spinner frames.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SpinnerStyle {
    /// Braille dots — the smooth default.
    #[default]
    Braille,
    /// ASCII line spinner (`| / - \`), for terminals without Braille.
    Line,
    /// Bouncing dots.
    Dots,
}

impl SpinnerStyle {
    /// The ordered glyph set the host cycles through for this style.
    pub fn frames(self) -> &'static [&'static str] {
        match self {
            SpinnerStyle::Braille => &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"],
            SpinnerStyle::Line => &["|", "/", "-", "\\"],
            SpinnerStyle::Dots => &["⠂", "⠒", "⠐", "⠰", "⠠", "⠤", "⠄", "⠆"],
        }
    }
}

/// An animated spinner glyph.
///
/// ![spinner demo](https://raw.githubusercontent.com/everruns/yolop/main/crates/tuika/docs/demos/spinner.gif)
pub struct Spinner {
    style: SpinnerStyle,
    frame: u64,
    color: Option<ratatui::style::Color>,
}

impl Spinner {
    /// A spinner showing the glyph for `frame`.
    pub fn new(frame: u64) -> Self {
        Self {
            style: SpinnerStyle::default(),
            frame,
            color: None,
        }
    }

    /// Set the glyph set.
    pub fn style(mut self, style: SpinnerStyle) -> Self {
        self.style = style;
        self
    }

    /// Override the glyph color (defaults to the theme accent).
    pub fn color(mut self, color: ratatui::style::Color) -> Self {
        self.color = Some(color);
        self
    }

    /// The glyph this spinner draws for its current frame.
    pub fn glyph(&self) -> &'static str {
        let frames = self.style.frames();
        frames[(self.frame as usize) % frames.len()]
    }
}

impl View for Spinner {
    fn measure(&self, _available: Size) -> Size {
        Size::new(1, 1)
    }

    fn render(&self, area: Rect, surface: &mut Surface, ctx: &RenderCtx) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let color = self.color.unwrap_or(ctx.theme.accent);
        surface.set_string(area.x, area.y, self.glyph(), Style::default().fg(color));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Surface;
    use crate::test_support::{buffer, rainbow_theme};
    use crate::view::{RenderCtx, View};

    #[test]
    fn spinner_cycles_frames() {
        let frames = SpinnerStyle::Braille.frames();
        assert_eq!(Spinner::new(0).glyph(), frames[0]);
        assert_eq!(Spinner::new(1).glyph(), frames[1]);
        // Wraps at the end of the frame set.
        assert_eq!(Spinner::new(frames.len() as u64).glyph(), frames[0]);
    }

    #[test]
    fn spinner_default_color_is_theme_accent() {
        let t = rainbow_theme();
        let ctx = RenderCtx::new(&t);
        let mut buf = buffer(3, 1);
        let area = buf.area;
        let mut surface = Surface::new(&mut buf, area);
        Spinner::new(0).render(area, &mut surface, &ctx);
        assert_eq!(buf[(0, 0)].fg, t.accent);
    }
}

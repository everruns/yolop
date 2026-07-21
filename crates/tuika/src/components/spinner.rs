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

    pub fn style(mut self, style: SpinnerStyle) -> Self {
        self.style = style;
        self
    }

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

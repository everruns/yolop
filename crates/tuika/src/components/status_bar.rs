//! Status bar — a single-row strip of left/right aligned segments over a
//! surface-colored background. Used for the full-screen renderer's model /
//! mode / token line, mirroring yolop's inline status chrome.

use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::Span;

use crate::geometry::Size;
use crate::surface::Surface;
use crate::view::{RenderCtx, View};

/// A one-line status bar with left- and right-anchored segment groups.
///
/// ![status_bar demo](https://raw.githubusercontent.com/everruns/yolop/main/crates/tuika/docs/demos/status_bar.gif)
pub struct StatusBar {
    left: Vec<Span<'static>>,
    right: Vec<Span<'static>>,
    background: Option<Style>,
}

impl StatusBar {
    pub fn new() -> Self {
        Self {
            left: Vec::new(),
            right: Vec::new(),
            background: None,
        }
    }

    pub fn left(mut self, spans: Vec<Span<'static>>) -> Self {
        self.left = spans;
        self
    }

    pub fn right(mut self, spans: Vec<Span<'static>>) -> Self {
        self.right = spans;
        self
    }

    pub fn background(mut self, style: Style) -> Self {
        self.background = Some(style);
        self
    }
}

impl Default for StatusBar {
    fn default() -> Self {
        Self::new()
    }
}

fn spans_width(spans: &[Span]) -> u16 {
    spans
        .iter()
        .map(|s| crate::width::str_cols(s.content.as_ref()))
        .fold(0, u16::saturating_add)
}

impl View for StatusBar {
    fn measure(&self, available: Size) -> Size {
        Size::new(available.width, 1)
    }

    fn render(&self, area: Rect, surface: &mut Surface, ctx: &RenderCtx) {
        if area.height == 0 {
            return;
        }
        let bg = self
            .background
            .unwrap_or_else(|| Style::default().bg(ctx.theme.surface));
        let row = Rect::new(area.x, area.y, area.width, 1);
        {
            let mut fill = surface.child(row);
            fill.fill(bg);
        }
        // Left group from the left edge.
        let mut x = area.x;
        for span in &self.left {
            if x >= area.right() {
                break;
            }
            x = surface.set_string(x, area.y, span.content.as_ref(), span.style.patch(bg));
        }
        // Right group anchored to the right edge, if it fits past the left.
        let right_w = spans_width(&self.right);
        let mut rx = area.right().saturating_sub(right_w);
        if rx > x {
            for span in &self.right {
                if rx >= area.right() {
                    break;
                }
                rx = surface.set_string(rx, area.y, span.content.as_ref(), span.style.patch(bg));
            }
        }
    }
}

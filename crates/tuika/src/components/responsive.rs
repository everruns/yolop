//! Width-driven view selection for responsive terminal layouts.

use ratatui::layout::Rect;

use crate::{Element, RenderCtx, Size, Surface, View};

/// Select between compact and wide layouts at a column breakpoint.
///
/// Each branch is a normal view tree, so an application can switch from a row
/// to a column, omit secondary content, or choose a shorter status component
/// without embedding breakpoint logic in individual leaves.
pub struct Responsive {
    breakpoint: u16,
    compact: Element,
    wide: Element,
}

impl Responsive {
    /// Use `compact` below `breakpoint` columns and `wide` otherwise.
    pub fn new(breakpoint: u16, compact: Element, wide: Element) -> Self {
        Self {
            breakpoint,
            compact,
            wide,
        }
    }

    fn select(&self, width: u16) -> &Element {
        if width < self.breakpoint {
            &self.compact
        } else {
            &self.wide
        }
    }
}

impl View for Responsive {
    fn measure(&self, available: Size) -> Size {
        self.select(available.width).measure(available)
    }

    fn render(&self, area: Rect, surface: &mut Surface, ctx: &RenderCtx) {
        self.select(area.width).render(area, surface, ctx);
    }
}

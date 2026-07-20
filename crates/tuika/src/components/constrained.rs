//! Intrinsic min/max sizing for responsive flex children.

use ratatui::layout::Rect;

use crate::{Element, RenderCtx, Size, Surface, View};

/// Clamp a child's measured size without changing its rendering contract.
pub struct Constrained {
    child: Element,
    min: Size,
    max: Size,
}

impl Constrained {
    /// Wrap `child` with unconstrained defaults.
    pub fn new(child: Element) -> Self {
        Self {
            child,
            min: Size::ZERO,
            max: Size::new(u16::MAX, u16::MAX),
        }
    }

    /// Set the minimum intrinsic width and height.
    pub fn min_size(mut self, width: u16, height: u16) -> Self {
        self.min = Size::new(width, height);
        self
    }

    /// Set the maximum intrinsic width and height.
    pub fn max_size(mut self, width: u16, height: u16) -> Self {
        self.max = Size::new(width, height);
        self
    }
}

impl View for Constrained {
    fn measure(&self, available: Size) -> Size {
        let measured = self.child.measure(available);
        Size::new(
            measured
                .width
                .max(self.min.width)
                .min(self.max.width)
                .min(available.width),
            measured
                .height
                .max(self.min.height)
                .min(self.max.height)
                .min(available.height),
        )
    }

    fn render(&self, area: Rect, surface: &mut Surface, ctx: &RenderCtx) {
        self.child.render(area, surface, ctx);
    }
}

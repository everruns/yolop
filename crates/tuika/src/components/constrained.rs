//! Intrinsic min/max sizing for responsive flex children.

use ratatui_core::layout::Rect;

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Size;
    use crate::components::Text;
    use crate::view::{View, element};

    #[test]
    fn constrained_clamps_intrinsic_measurement() {
        let constrained = Constrained::new(element(Text::raw("long content")))
            .min_size(4, 1)
            .max_size(6, 2);
        assert_eq!(constrained.measure(Size::new(20, 10)), Size::new(6, 1));
        assert_eq!(constrained.measure(Size::new(3, 1)), Size::new(3, 1));
    }
}

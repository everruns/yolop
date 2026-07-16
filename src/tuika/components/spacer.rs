//! Spacer — empty, flexible filler. Paints nothing; useful as a
//! `Dimension::Flex` child to push siblings apart, or fixed to insert a gap.

use ratatui::layout::Rect;

use crate::tuika::geometry::Size;
use crate::tuika::surface::Surface;
use crate::tuika::view::{RenderCtx, View};

pub struct Spacer;

impl View for Spacer {
    fn measure(&self, _available: Size) -> Size {
        Size::ZERO
    }

    fn render(&self, _area: Rect, _surface: &mut Surface, _ctx: &RenderCtx) {}
}

//! Safe interoperability with ratatui widgets.
//!
//! [`RatatuiView`] participates in Tuika layout while rendering through
//! [`Surface::render_ratatui`](crate::Surface::render_ratatui), so arbitrary
//! widget code never receives the frame's underlying buffer.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use crate::{RenderCtx, Size, Surface, View};

/// A Tuika view backed by a ratatui rendering closure.
///
/// The closure form is intentional: many ratatui widgets borrow their data.
/// Captured owned data can be borrowed while constructing the widget inside
/// the closure, without requiring a self-referential wrapper.
pub struct RatatuiView<F> {
    measure: Measure,
    render: F,
}

#[derive(Clone, Copy, Debug)]
enum Measure {
    Fill,
    Fixed(Size),
}

impl<F> RatatuiView<F>
where
    F: Fn(Rect, &mut Buffer),
{
    /// Create a view that requests all space offered by its parent.
    ///
    /// ```
    /// use ratatui::widgets::{Block, Widget};
    /// use tuika::RatatuiView;
    ///
    /// let view = RatatuiView::fill(|area, buffer| {
    ///     Block::bordered().title(" ratatui ").render(area, buffer);
    /// });
    /// ```
    pub fn fill(render: F) -> Self {
        Self {
            measure: Measure::Fill,
            render,
        }
    }

    /// Create a view with a fixed intrinsic size for `Dimension::Auto` layout.
    pub fn sized(size: Size, render: F) -> Self {
        Self {
            measure: Measure::Fixed(size),
            render,
        }
    }
}

impl<F> View for RatatuiView<F>
where
    F: Fn(Rect, &mut Buffer),
{
    fn measure(&self, available: Size) -> Size {
        match self.measure {
            Measure::Fill => available,
            Measure::Fixed(size) => Size::new(
                size.width.min(available.width),
                size.height.min(available.height),
            ),
        }
    }

    fn render(&self, area: Rect, surface: &mut Surface, _ctx: &RenderCtx) {
        surface.render_ratatui(area, &self.render);
    }
}

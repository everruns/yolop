//! Flex container — the composition primitive.
//!
//! Holds a [`LayoutStyle`] and a list of children, each tagged with a
//! [`Dimension`] for its main-axis sizing. Delegates rect assignment to the
//! [`solve`](crate::layout::solve) flexbox solver, then renders each
//! child into its rect through a clipped child surface. Nesting `Flex`es is how
//! every screen is built.

use ratatui_core::layout::Rect;
use ratatui_core::style::Style;

use crate::geometry::Size;
use crate::layout::{Dimension, Item, LayoutStyle, solve};
use crate::surface::Surface;
use crate::view::{Element, RenderCtx, View};

struct Child {
    view: Element,
    dimension: Dimension,
}

/// A flexbox container of child views.
///
/// # Example
///
/// ```
/// use tuika::{Flex, Text, Theme, element};
/// use tuika::testing::{grid, render};
///
/// // A row of two fixed-width cells, laid left to right.
/// let view = Flex::row()
///     .fixed(3, element(Text::raw("abc")))
///     .fixed(3, element(Text::raw("xyz")));
///
/// let buffer = render(&view, 6, 1, &Theme::default());
/// assert_eq!(grid(&buffer), "abcxyz");
/// ```
///
/// ![flex demo](https://raw.githubusercontent.com/everruns/yolop/main/crates/tuika/docs/demos/flex.gif)
pub struct Flex {
    style: LayoutStyle,
    children: Vec<Child>,
    background: Option<Style>,
}

impl Flex {
    /// An empty flex container with the given layout style.
    pub fn new(style: LayoutStyle) -> Self {
        Self {
            style,
            children: Vec::new(),
            background: None,
        }
    }

    /// An empty container laying children left-to-right along the horizontal axis.
    pub fn row() -> Self {
        Self::new(LayoutStyle::row())
    }

    /// An empty container stacking children top-to-bottom along the vertical axis.
    pub fn column() -> Self {
        Self::new(LayoutStyle::column())
    }

    /// Fill the container area with `style` before drawing children.
    pub fn background(mut self, style: Style) -> Self {
        self.background = Some(style);
        self
    }

    /// Set the gap between children (passthrough to the layout style).
    pub fn gap(mut self, gap: u16) -> Self {
        self.style.gap = gap;
        self
    }

    /// Set padding inside the container (passthrough to the layout style).
    pub fn padding(mut self, padding: crate::geometry::Padding) -> Self {
        self.style.padding = padding;
        self
    }

    /// Set cross-axis alignment (passthrough to the layout style).
    pub fn align(mut self, align: crate::layout::Align) -> Self {
        self.style.align_items = align;
        self
    }

    /// Set main-axis distribution (passthrough to the layout style).
    pub fn justify(mut self, justify: crate::layout::Justify) -> Self {
        self.style.justify = justify;
        self
    }

    /// Add a child with an explicit main-axis dimension.
    pub fn child(mut self, dimension: Dimension, view: Element) -> Self {
        self.children.push(Child { view, dimension });
        self
    }

    /// Add an auto-sized child (sizes to its measured content).
    pub fn auto(self, view: Element) -> Self {
        self.child(Dimension::Auto, view)
    }

    /// Add a flexible child that grows to share leftover space.
    pub fn grow(self, weight: u16, view: Element) -> Self {
        self.child(Dimension::Flex(weight), view)
    }

    /// Add a fixed-size child.
    pub fn fixed(self, cells: u16, view: Element) -> Self {
        self.child(Dimension::Fixed(cells), view)
    }

    fn items(&self, available: Size) -> Vec<Item> {
        self.children
            .iter()
            .map(|c| Item::new(c.dimension, c.view.measure(available)))
            .collect()
    }

    /// Resolve the child rects this container would assign inside `area`,
    /// without painting anything.
    ///
    /// This is the same measure-then-[`solve`] pass `render` runs, exposed so a
    /// host can compute layout ahead of (or instead of) a render — to clamp a
    /// scroll offset to a pane's real height, hit-test a click against child
    /// rects, or decide what fits before drawing. The returned `Vec` has one
    /// rect per child, in insertion order.
    ///
    /// ```
    /// use tuika::{Flex, Text, element};
    /// use ratatui_core::layout::Rect;
    ///
    /// let flex = Flex::row()
    ///     .fixed(4, element(Text::raw("abcd")))
    ///     .grow(1, element(Text::raw("rest")));
    /// let rects = flex.solve(Rect::new(0, 0, 10, 1));
    /// assert_eq!(rects.len(), 2);
    /// assert_eq!(rects[0], Rect::new(0, 0, 4, 1));
    /// assert_eq!(rects[1], Rect::new(4, 0, 6, 1)); // grows into the leftover
    /// ```
    pub fn solve(&self, area: Rect) -> Vec<Rect> {
        solve(area, &self.style, &self.items(Size::from(area)))
    }
}

impl View for Flex {
    fn measure(&self, available: Size) -> Size {
        // Sum children on the main axis, max on the cross axis, plus gaps and
        // padding. Used when this Flex is itself an Auto child.
        let axis = self.style.direction.axis();
        let inner = self
            .style
            .padding
            .inner(Rect::new(0, 0, available.width, available.height));
        let inner_avail = Size::from(inner);
        let mut main_total: u16 = 0;
        let mut cross_max: u16 = 0;
        for (i, c) in self.children.iter().enumerate() {
            let sz = c.view.measure(inner_avail);
            main_total = main_total.saturating_add(axis.main(sz));
            if i > 0 {
                main_total = main_total.saturating_add(self.style.gap);
            }
            cross_max = cross_max.max(axis.cross(sz));
        }
        let content = axis.size(main_total, cross_max);
        Size::new(
            content
                .width
                .saturating_add(self.style.padding.horizontal()),
            content.height.saturating_add(self.style.padding.vertical()),
        )
    }

    fn render(&self, area: Rect, surface: &mut Surface, ctx: &RenderCtx) {
        if let Some(bg) = self.background {
            let mut fill = surface.child(area);
            fill.fill(bg);
        }
        let rects = self.solve(area);
        for (child, rect) in self.children.iter().zip(rects) {
            let mut child_surface = surface.child(rect);
            child.view.render(rect, &mut child_surface, ctx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::Text;
    use crate::probe::RectProbe;
    use crate::style::Theme;
    use crate::view::element;

    #[test]
    fn solve_matches_the_rects_render_paints_into() {
        // Probe every child, render, then compare the painted rects to what the
        // paint-free `solve` returns for the same area — they must be identical.
        let area = Rect::new(0, 0, 20, 6);
        let probes: Vec<RectProbe> = (0..3).map(|_| RectProbe::new()).collect();
        let flex = Flex::column()
            .fixed(1, probes[0].wrap(element(Text::raw("header"))))
            .grow(1, probes[1].wrap(element(Text::raw("body"))))
            .fixed(2, probes[2].wrap(element(Text::raw("footer"))));

        let precomputed = flex.solve(area);
        let _ = crate::testing::render(&flex, area.width, area.height, &Theme::default());

        let painted: Vec<Rect> = probes.iter().map(|p| p.rect()).collect();
        assert_eq!(
            precomputed, painted,
            "solve() must return exactly the rects render() paints into"
        );
        // And they are the expected column layout: 1-row header, grown body, 2-row footer.
        assert_eq!(precomputed[0], Rect::new(0, 0, 20, 1));
        assert_eq!(precomputed[1], Rect::new(0, 1, 20, 3));
        assert_eq!(precomputed[2], Rect::new(0, 4, 20, 2));
    }

    #[test]
    fn solve_of_empty_container_is_empty() {
        let flex = Flex::row();
        assert!(flex.solve(Rect::new(0, 0, 10, 3)).is_empty());
    }

    #[test]
    fn crate_root_reexports_solver_primitives() {
        // `solve` and `Item` are reachable from the crate root, not just
        // `tuika::layout` — build a layout by hand without a Flex.
        use crate::{Dimension, Item, LayoutStyle, Size, solve};
        let items = [
            Item::new(Dimension::Fixed(3), Size::new(3, 1)),
            Item::new(Dimension::Flex(1), Size::new(0, 1)),
        ];
        let rects = solve(Rect::new(0, 0, 10, 1), &LayoutStyle::row(), &items);
        assert_eq!(rects[0], Rect::new(0, 0, 3, 1));
        assert_eq!(rects[1], Rect::new(3, 0, 7, 1));
    }
}

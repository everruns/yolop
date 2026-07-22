//! Flex container — the composition primitive.
//!
//! Holds a [`LayoutStyle`] and a list of children, each tagged with a
//! [`Dimension`] for its main-axis sizing. Delegates rect assignment to the
//! [`solve`](crate::layout::solve) flexbox solver, then renders each
//! child into its rect through a clipped child surface. Nesting `Flex`es is how
//! every screen is built.

use ratatui::layout::Rect;
use ratatui::style::Style;

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
        let items = self.items(Size::from(area));
        let rects = solve(area, &self.style, &items);
        for (child, rect) in self.children.iter().zip(rects) {
            let mut child_surface = surface.child(rect);
            child.view.render(rect, &mut child_surface, ctx);
        }
    }
}

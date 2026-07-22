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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::Text;
    use crate::style::Theme;
    use crate::test_support::row;
    use crate::view::element;

    #[test]
    fn responsive_selects_layout_from_render_width() {
        let view = Responsive::new(
            10,
            element(Text::raw("compact")),
            element(Text::raw("wide")),
        );
        let theme = Theme::default();
        let compact = crate::testing::render(&view, 8, 1, &theme);
        let wide = crate::testing::render(&view, 12, 1, &theme);
        assert_eq!(row(&compact, 0), "compact");
        assert_eq!(row(&wide, 0), "wide");
    }
}

//! Bordered box — a single-child container with border, padding, background,
//! and an optional title. Focus-aware: its border recolors when the render
//! context reports focus. This is `tuika`'s framing primitive (Pi's
//! `DynamicBorder` / `Box`).

use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::Line;

use crate::geometry::{Padding, Size};
use crate::style::BorderStyle;
use crate::surface::Surface;
use crate::view::{Element, RenderCtx, View};

use super::text::line_width;

/// A bordered, padded container wrapping one child.
///
/// ![boxed demo](https://raw.githubusercontent.com/everruns/yolop/main/crates/tuika/docs/demos/boxed.gif)
pub struct Boxed {
    child: Element,
    border: BorderStyle,
    padding: Padding,
    title: Option<Line<'static>>,
    background: Option<Style>,
}

impl Boxed {
    /// Wrap `child` with default rounded border and symmetric horizontal padding.
    pub fn new(child: Element) -> Self {
        Self {
            child,
            border: BorderStyle::Rounded,
            padding: Padding::symmetric(1, 0),
            title: None,
            background: None,
        }
    }

    /// Set the border style (`BorderStyle::None` removes the border).
    pub fn border(mut self, border: BorderStyle) -> Self {
        self.border = border;
        self
    }

    /// Set the padding between the border and the child.
    pub fn padding(mut self, padding: Padding) -> Self {
        self.padding = padding;
        self
    }

    /// Set a title drawn on the top border, clipped between the corners.
    pub fn title(mut self, title: impl Into<Line<'static>>) -> Self {
        self.title = Some(title.into());
        self
    }

    /// Fill the box interior with `style` before drawing the child.
    pub fn background(mut self, style: Style) -> Self {
        self.background = Some(style);
        self
    }

    fn has_border(&self) -> bool {
        !matches!(self.border, BorderStyle::None)
    }

    /// Interior rect available to the child after border + padding.
    fn inner(&self, area: Rect) -> Rect {
        let bordered = if self.has_border() {
            Rect {
                x: area.x.saturating_add(1),
                y: area.y.saturating_add(1),
                width: area.width.saturating_sub(2),
                height: area.height.saturating_sub(2),
            }
        } else {
            area
        };
        self.padding.inner(bordered)
    }

    fn chrome(&self) -> Size {
        let border = if self.has_border() { 2 } else { 0 };
        Size::new(
            self.padding.horizontal().saturating_add(border),
            self.padding.vertical().saturating_add(border),
        )
    }
}

impl View for Boxed {
    fn measure(&self, available: Size) -> Size {
        let chrome = self.chrome();
        let inner_avail = Size::new(
            available.width.saturating_sub(chrome.width),
            available.height.saturating_sub(chrome.height),
        );
        let content = self.child.measure(inner_avail);
        Size::new(
            content.width.saturating_add(chrome.width),
            content.height.saturating_add(chrome.height),
        )
    }

    fn render(&self, area: Rect, surface: &mut Surface, ctx: &RenderCtx) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        if let Some(bg) = self.background {
            let mut fill = surface.child(area);
            fill.fill(bg);
        }
        if self.has_border() {
            let border_style = Style::default().fg(ctx.theme.border_color(ctx.focused));
            surface.draw_border(area, self.border.glyphs(), border_style);
            if let Some(title) = &self.title {
                // Title sits on the top border, one cell in, clipped to fit
                // between the corners.
                let max = area.width.saturating_sub(4);
                let mut x = area.x.saturating_add(2);
                if line_width(title) <= max {
                    for span in &title.spans {
                        x = surface.set_string(x, area.y, span.content.as_ref(), span.style);
                    }
                }
            }
        }
        let inner = self.inner(area);
        let mut inner_surface = surface.child(inner);
        self.child.render(inner, &mut inner_surface, ctx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Surface;
    use crate::components::Text;
    use crate::style::Theme;
    use crate::test_support::{buffer, rainbow_theme};
    use crate::view::{RenderCtx, View, element};
    use ratatui::style::Color;

    #[test]
    fn boxed_border_closed_across_sizes() {
        let theme = Theme::default();
        let ctx = RenderCtx::new(&theme);
        for w in 2..=12u16 {
            for h in 2..=8u16 {
                let mut buf = buffer(w, h);
                let area = buf.area;
                let mut surface = Surface::new(&mut buf, area);
                Boxed::new(element(Text::raw("x"))).render(area, &mut surface, &ctx);
                assert_eq!(buf[(0, 0)].symbol(), "╭", "top-left at {w}x{h}");
                assert_eq!(buf[(w - 1, 0)].symbol(), "╮", "top-right at {w}x{h}");
                assert_eq!(buf[(0, h - 1)].symbol(), "╰", "bottom-left at {w}x{h}");
                assert_eq!(buf[(w - 1, h - 1)].symbol(), "╯", "bottom-right at {w}x{h}");
            }
        }
    }

    #[test]
    fn boxed_border_follows_theme_focus_color() {
        let t = rainbow_theme();

        let make = |focused: bool| {
            let mut buf = buffer(8, 3);
            let area = buf.area;
            let ctx = RenderCtx::new(&t).with_focus(focused);
            let boxed = Boxed::new(element(Text::raw("x")));
            let mut surface = Surface::new(&mut buf, area);
            boxed.render(area, &mut surface, &ctx);
            buf[(0, 0)].fg // the '╭' corner
        };

        assert_eq!(make(false), t.border, "unfocused border uses theme.border");
        assert_eq!(
            make(true),
            t.border_focused,
            "focused border uses theme.border_focused"
        );
    }

    #[test]
    fn swapping_theme_restyles_the_same_tree() {
        let tree = || Boxed::new(element(Text::raw("x")));
        let render_border = |theme: &Theme| {
            let mut buf = buffer(8, 3);
            let area = buf.area;
            let ctx = RenderCtx::new(theme);
            let mut surface = Surface::new(&mut buf, area);
            tree().render(area, &mut surface, &ctx);
            buf[(0, 0)].fg
        };

        let a = Theme {
            border: Color::Indexed(21),
            ..rainbow_theme()
        };
        let b = Theme {
            border: Color::Indexed(99),
            ..rainbow_theme()
        };
        assert_eq!(render_border(&a), Color::Indexed(21));
        assert_eq!(render_border(&b), Color::Indexed(99));
        assert_ne!(
            render_border(&a),
            render_border(&b),
            "theme swap must restyle"
        );
    }
}

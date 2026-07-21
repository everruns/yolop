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
    pub fn new(child: Element) -> Self {
        Self {
            child,
            border: BorderStyle::Rounded,
            padding: Padding::symmetric(1, 0),
            title: None,
            background: None,
        }
    }

    pub fn border(mut self, border: BorderStyle) -> Self {
        self.border = border;
        self
    }

    pub fn padding(mut self, padding: Padding) -> Self {
        self.padding = padding;
        self
    }

    pub fn title(mut self, title: impl Into<Line<'static>>) -> Self {
        self.title = Some(title.into());
        self
    }

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

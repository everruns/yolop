//! Bordered box — a single-child container with border, padding, background,
//! and an optional title. Focus-aware: its border recolors when the render
//! context reports focus. This is `tuika`'s framing primitive (Pi's
//! `DynamicBorder` / `Box`).

use ratatui::layout::{Alignment, Rect};
use ratatui::style::Style;
use ratatui::text::Line;

use crate::geometry::{Padding, Size};
use crate::style::BorderStyle;
use crate::surface::Surface;
use crate::view::{Element, RenderCtx, View};

use super::text::{aligned_x, line_width};

/// A bordered, padded container wrapping one child.
///
/// # Example
///
/// ```
/// use tuika::{Boxed, Text, Theme, element};
/// use tuika::testing::{grid, render};
///
/// // Default rounded border with symmetric horizontal padding.
/// let view = Boxed::new(element(Text::raw("hi")));
///
/// let buffer = render(&view, 6, 3, &Theme::default());
/// assert_eq!(
///     grid(&buffer),
///     "╭────╮\n\
///      │ hi │\n\
///      ╰────╯",
/// );
/// ```
///
/// A secondary title can ride the bottom border — a position counter, footer
/// legend, or hint — and defaults to flush-right:
///
/// ```
/// use tuika::{Boxed, Text, Theme, element};
/// use tuika::testing::{grid, render};
///
/// let view = Boxed::new(element(Text::raw("hi")))
///     .title(" files ")
///     .title_bottom(" 1/3 ");
///
/// let buffer = render(&view, 12, 3, &Theme::default());
/// assert_eq!(
///     grid(&buffer),
///     "╭─ files ──╮\n\
///      │ hi       │\n\
///      ╰──── 1/3 ─╯",
/// );
/// ```
///
/// ![boxed demo](https://raw.githubusercontent.com/everruns/yolop/main/crates/tuika/docs/demos/boxed.gif)
pub struct Boxed {
    child: Element,
    border: BorderStyle,
    padding: Padding,
    title: Option<Line<'static>>,
    title_bottom: Option<Line<'static>>,
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
            title_bottom: None,
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
    ///
    /// The title's horizontal placement honors its [`Line::alignment`]; an unset
    /// alignment (the default) is flush-left, matching a plain string title.
    pub fn title(mut self, title: impl Into<Line<'static>>) -> Self {
        self.title = Some(title.into());
        self
    }

    /// Set a secondary title drawn on the bottom border, clipped between the
    /// corners — the slot list/table TUIs use for a `1 of 3` position counter, a
    /// footer legend, or a keybinding hint.
    ///
    /// Placement honors the title's [`Line::alignment`]. An unset alignment
    /// defaults to flush-**right** here (unlike the top title's flush-left),
    /// since a bottom-right counter is the common case; pass a `.centered()` or
    /// `.left_aligned()` [`Line`] to override.
    pub fn title_bottom(mut self, title: impl Into<Line<'static>>) -> Self {
        self.title_bottom = Some(title.into());
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
                draw_title(surface, area, area.y, title, Alignment::Left);
            }
            if let Some(title) = &self.title_bottom {
                let bottom = area.bottom().saturating_sub(1);
                draw_title(surface, area, bottom, title, Alignment::Right);
            }
        }
        let inner = self.inner(area);
        let mut inner_surface = surface.child(inner);
        self.child.render(inner, &mut inner_surface, ctx);
    }
}

/// Draw a border `title` on row `y`, inset one cell from each corner and clipped
/// to fit between them. Horizontal placement honors the line's alignment,
/// falling back to `default_align` when it is unset. A title too wide for the
/// span between the corners is dropped rather than overrunning a corner.
fn draw_title(
    surface: &mut Surface,
    area: Rect,
    y: u16,
    title: &Line<'static>,
    default_align: Alignment,
) {
    let max = area.width.saturating_sub(4);
    if line_width(title) > max {
        return;
    }
    // The title span runs from one cell past the left corner to one cell before
    // the right corner; align the content within that inset region.
    let region = Rect {
        x: area.x.saturating_add(2),
        y,
        width: max,
        height: 1,
    };
    let align = title.alignment.unwrap_or(default_align);
    let mut x = aligned_x(align, line_width(title), region);
    for span in &title.spans {
        x = surface.set_string(x, y, span.content.as_ref(), span.style);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Surface;
    use crate::components::Text;
    use crate::style::Theme;
    use crate::test_support::{buffer, rainbow_theme, row};
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
    fn bottom_title_defaults_flush_right() {
        let theme = Theme::default();
        let ctx = RenderCtx::new(&theme);
        let mut buf = buffer(10, 3);
        let area = buf.area;
        let mut surface = Surface::new(&mut buf, area);
        // Plain string title, no explicit alignment -> flush-right on the
        // bottom border, one cell inside the right corner.
        Boxed::new(element(Text::raw("x")))
            .title_bottom(" 1/3 ")
            .render(area, &mut surface, &ctx);
        // Width 10: corners at 0 and 9, inset span [2..8) is 6 cols, " 1/3 " is
        // 5 cols -> right-aligned start at column 3.
        assert_eq!(row(&buf, 2), "╰── 1/3 ─╯");
    }

    #[test]
    fn bottom_title_honors_explicit_alignment() {
        let theme = Theme::default();
        let ctx = RenderCtx::new(&theme);
        let mut buf = buffer(10, 3);
        let area = buf.area;
        let mut surface = Surface::new(&mut buf, area);
        Boxed::new(element(Text::raw("x")))
            .title_bottom(Line::from("ab").left_aligned())
            .render(area, &mut surface, &ctx);
        // Left-aligned -> one cell in from the left corner.
        assert_eq!(row(&buf, 2), "╰─ab─────╯");
    }

    #[test]
    fn top_title_honors_center_alignment() {
        let theme = Theme::default();
        let ctx = RenderCtx::new(&theme);
        let mut buf = buffer(10, 3);
        let area = buf.area;
        let mut surface = Surface::new(&mut buf, area);
        Boxed::new(element(Text::raw("x")))
            .title(Line::from("ab").centered())
            .render(area, &mut surface, &ctx);
        // Inset span [2..8) width 6, content 2 -> slack 4, centered start col
        // 2 + 4/2 = 4.
        assert_eq!(row(&buf, 0), "╭───ab───╮");
    }

    #[test]
    fn oversized_bottom_title_is_dropped() {
        let theme = Theme::default();
        let ctx = RenderCtx::new(&theme);
        let mut buf = buffer(6, 3);
        let area = buf.area;
        let mut surface = Surface::new(&mut buf, area);
        Boxed::new(element(Text::raw("x")))
            .title_bottom("a title far too wide")
            .render(area, &mut surface, &ctx);
        // Doesn't fit between the corners -> border stays intact, no overrun.
        assert_eq!(row(&buf, 2), "╰────╯");
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

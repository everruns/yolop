//! Horizontal rule / section separator: an optional title followed by a run of
//! a fill glyph out to the available width — `─── title ───────────`.
//!
//! The title is any styled [`Line`], so a caller can put a spinner, hint, or
//! colored label in it; the fill takes the rule's own glyph and style. This is
//! the reusable form of the blue/gold separators a chat UI draws above and
//! below its composer.

use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};

use super::line_width;
use crate::geometry::Size;
use crate::surface::Surface;
use crate::view::{RenderCtx, View};

/// A one-row horizontal rule with an optional leading title.
///
/// ![rule demo](https://raw.githubusercontent.com/everruns/yolop/main/crates/tuika/docs/demos/rule.gif)
pub struct Rule {
    title: Line<'static>,
    glyph: char,
    style: Style,
}

impl Rule {
    /// A full-width rule drawn with `─` and no title.
    pub fn new() -> Self {
        Self {
            title: Line::default(),
            glyph: '─',
            style: Style::default(),
        }
    }

    /// A styled label rendered before the fill (its own spans keep their style).
    pub fn title(mut self, title: impl Into<Line<'static>>) -> Self {
        self.title = title.into();
        self
    }

    /// The fill glyph (default `─`).
    pub fn glyph(mut self, glyph: char) -> Self {
        self.glyph = glyph;
        self
    }

    /// The style of the fill run.
    pub fn style(mut self, style: Style) -> Self {
        self.style = style;
        self
    }

    /// The composed line: title spans, then the fill glyph out to `width`.
    fn line(&self, width: u16) -> Line<'static> {
        let mut spans = self.title.spans.clone();
        let fill = width.saturating_sub(line_width(&self.title));
        if fill > 0 {
            spans.push(Span::styled(
                self.glyph.to_string().repeat(fill as usize),
                self.style,
            ));
        }
        Line::from(spans)
    }
}

impl Default for Rule {
    fn default() -> Self {
        Self::new()
    }
}

impl View for Rule {
    fn measure(&self, available: Size) -> Size {
        Size::new(available.width, 1)
    }

    fn render(&self, area: Rect, surface: &mut Surface, _ctx: &RenderCtx) {
        if area.height == 0 {
            return;
        }
        let mut x = area.x;
        for span in &self.line(area.width).spans {
            if x >= area.right() {
                break;
            }
            x = surface.set_string(x, area.y, span.content.as_ref(), span.style);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::style::Theme;
    use crate::test_support::row;
    use ratatui::style::{Color, Style};
    use ratatui::text::{Line, Span};

    #[test]
    fn rule_renders_title_then_fills_to_width() {
        let theme = Theme::default();
        let rule = Rule::new()
            .title(Line::from(Span::raw("─ hi ")))
            .style(Style::default().fg(Color::Blue));
        let buf = crate::testing::render(&rule, 10, 1, &theme);
        assert_eq!(row(&buf, 0), "─ hi ─────");
        // The fill run carries the rule's style.
        assert_eq!(buf[(9, 0)].symbol(), "─");
        assert_eq!(buf[(9, 0)].fg, Color::Blue);
    }
}

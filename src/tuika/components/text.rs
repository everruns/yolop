//! Text and paragraph components.
//!
//! [`Text`] draws pre-styled ratatui [`Line`]s faithfully (mixed styles per
//! line preserved), clipping to its area. [`Paragraph`] takes a plain string
//! plus one style and word-wraps it to the available width. Callers that need
//! wrapped *and* multi-styled text should wrap upstream and feed [`Text`].

use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::Line;
use unicode_width::UnicodeWidthStr;

use crate::tuika::geometry::Size;
use crate::tuika::surface::Surface;
use crate::tuika::view::{RenderCtx, View};

/// Display width of a styled line (sum of span widths).
pub fn line_width(line: &Line) -> u16 {
    line.spans
        .iter()
        .map(|s| UnicodeWidthStr::width(s.content.as_ref()) as u16)
        .fold(0, u16::saturating_add)
}

/// A block of pre-styled lines, drawn top-down and clipped.
pub struct Text {
    lines: Vec<Line<'static>>,
}

impl Text {
    pub fn new(lines: Vec<Line<'static>>) -> Self {
        Self { lines }
    }

    /// A single unstyled line from a string.
    pub fn raw(text: impl Into<String>) -> Self {
        Self::new(vec![Line::from(text.into())])
    }
}

impl View for Text {
    fn measure(&self, _available: Size) -> Size {
        let width = self.lines.iter().map(line_width).max().unwrap_or(0);
        Size::new(width, self.lines.len() as u16)
    }

    fn render(&self, area: Rect, surface: &mut Surface, _ctx: &RenderCtx) {
        for (row, line) in self.lines.iter().enumerate() {
            let y = area.y.saturating_add(row as u16);
            if y >= area.bottom() {
                break;
            }
            let mut x = area.x;
            for span in &line.spans {
                if x >= area.right() {
                    break;
                }
                x = surface.set_string(x, y, span.content.as_ref(), span.style);
            }
        }
    }
}

/// Plain text word-wrapped to the render width in one style.
pub struct Paragraph {
    text: String,
    style: Style,
}

impl Paragraph {
    pub fn new(text: impl Into<String>, style: Style) -> Self {
        Self {
            text: text.into(),
            style,
        }
    }

    fn wrap(&self, width: u16) -> Vec<String> {
        if width == 0 {
            return Vec::new();
        }
        self.text
            .split('\n')
            .flat_map(|para| {
                let wrapped = textwrap::wrap(para, width as usize);
                if wrapped.is_empty() {
                    vec![String::new()]
                } else {
                    wrapped.into_iter().map(|c| c.into_owned()).collect()
                }
            })
            .collect()
    }
}

impl View for Paragraph {
    fn measure(&self, available: Size) -> Size {
        let lines = self.wrap(available.width);
        let width = lines
            .iter()
            .map(|l| UnicodeWidthStr::width(l.as_str()) as u16)
            .max()
            .unwrap_or(0);
        Size::new(width, lines.len() as u16)
    }

    fn render(&self, area: Rect, surface: &mut Surface, _ctx: &RenderCtx) {
        for (row, line) in self.wrap(area.width).into_iter().enumerate() {
            let y = area.y.saturating_add(row as u16);
            if y >= area.bottom() {
                break;
            }
            surface.set_string(area.x, y, &line, self.style);
        }
    }
}

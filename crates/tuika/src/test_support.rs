//! Shared helpers for the crate's unit tests.
//!
//! Compiled only under `#[cfg(test)]`, so nothing here ships in the library.
//! Per-module `mod tests` blocks own the assertions; the utilities they all
//! reach for — building an in-memory buffer, reading a row back, an
//! identifiable "rainbow" theme, rendering an element to text — live here so
//! they are defined once.

use ratatui_core::buffer::Buffer;
use ratatui_core::layout::Rect;
use ratatui_core::style::Color;

use crate::Surface;
use crate::style::Theme;
use crate::view::{Element, RenderCtx, View};

/// Read a buffer row into a trimmed string for assertions.
pub fn row(buffer: &Buffer, y: u16) -> String {
    let area = buffer.area;
    let mut s = String::new();
    for x in area.x..area.right() {
        s.push_str(buffer[(x, y)].symbol());
    }
    s.trim_end().to_string()
}

/// An empty, zero-origin buffer of the given size.
pub fn buffer(width: u16, height: u16) -> Buffer {
    Buffer::empty(Rect::new(0, 0, width, height))
}

/// A theme whose slots are all distinct, identifiable indexed colors, so a
/// rendered cell's fg/bg pins down exactly which slot a component read.
pub fn rainbow_theme() -> Theme {
    Theme {
        background: Color::Indexed(1),
        surface: Color::Indexed(2),
        text: Color::Indexed(3),
        muted: Color::Indexed(4),
        dim: Color::Indexed(5),
        accent: Color::Indexed(6),
        accent_alt: Color::Indexed(7),
        border: Color::Indexed(8),
        border_focused: Color::Indexed(9),
        selection_bg: Color::Indexed(10),
        selection_fg: Color::Indexed(11),
        code: crate::style::CodeTheme {
            heading: Color::Indexed(12),
            link: Color::Indexed(13),
            background: Color::Indexed(14),
            text: Color::Indexed(15),
            label: Color::Indexed(16),
            keyword: Color::Indexed(17),
            function: Color::Indexed(18),
            type_name: Color::Indexed(19),
            constant: Color::Indexed(20),
            string: Color::Indexed(21),
            comment: Color::Indexed(22),
            punctuation: Color::Indexed(23),
        },
    }
}

/// Render an [`Element`] into a fresh buffer and return its rows as strings.
pub fn render_el(el: &Element, w: u16, h: u16) -> Vec<String> {
    let theme = Theme::default();
    let mut buf = buffer(w, h);
    let area = buf.area;
    let ctx = RenderCtx::new(&theme);
    let mut surface = Surface::new(&mut buf, area);
    el.render(area, &mut surface, &ctx);
    (0..h).map(|y| row(&buf, y)).collect()
}

/// Render a `&dyn View` into a fresh buffer and return its rows as strings.
pub fn render_view_rows(view: &dyn View, w: u16, h: u16) -> Vec<String> {
    let theme = Theme::default();
    let mut buf = buffer(w, h);
    let area = buf.area;
    let ctx = RenderCtx::new(&theme);
    let mut surface = Surface::new(&mut buf, area);
    view.render(area, &mut surface, &ctx);
    (0..h).map(|y| row(&buf, y)).collect()
}

//! Clipped drawing surface over a ratatui [`Buffer`].
//!
//! Components paint through a [`Surface`], which owns a mutable borrow of the
//! frame buffer plus a clip [`Rect`]. Every write is intersected with the clip
//! region, so a child can never scribble outside the area the layout gave it —
//! this is what makes scroll viewports and overlays safe to compose. ratatui
//! still owns the shadow-buffer diff against the terminal, so `tuika` pays
//! nothing extra for dirty-cell tracking.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use unicode_width::UnicodeWidthChar;

use super::style::BorderGlyphs;

/// A clipped view into the frame buffer for one component.
pub struct Surface<'a> {
    buffer: &'a mut Buffer,
    clip: Rect,
}

impl<'a> Surface<'a> {
    /// Wrap `buffer`, clipping all writes to the intersection of `clip` and the
    /// buffer's own area.
    pub fn new(buffer: &'a mut Buffer, clip: Rect) -> Self {
        let clip = clip.intersection(buffer.area);
        Self { buffer, clip }
    }

    /// The region this surface may draw into.
    pub fn area(&self) -> Rect {
        self.clip
    }

    /// Re-borrow as a child surface clipped to `area` (further intersected with
    /// the current clip). Used when a container hands a sub-rect to a child.
    pub fn child(&mut self, area: Rect) -> Surface<'_> {
        let clip = area.intersection(self.clip);
        Surface {
            buffer: self.buffer,
            clip,
        }
    }

    /// Render one or more ratatui widgets without giving them unrestricted
    /// access to the frame buffer.
    ///
    /// The callback receives a temporary [`Buffer`] covering `area`. Existing
    /// cells are copied into it first, so widgets that only patch styles retain
    /// the surrounding background. After the callback returns, only cells in
    /// this surface's clip are composited back. A widget that writes outside
    /// its assigned area therefore cannot overwrite a sibling or an overlay.
    ///
    /// # Example
    ///
    /// ```
    /// use ratatui::widgets::{Sparkline, Widget};
    /// use tuika::Surface;
    ///
    /// # fn draw(surface: &mut Surface<'_>) {
    /// let values = [1, 4, 2, 8];
    /// let area = surface.area();
    /// surface.render_ratatui(area, |area, buffer| {
    ///     Sparkline::default().data(&values).render(area, buffer);
    /// });
    /// # }
    /// ```
    pub fn render_ratatui(&mut self, area: Rect, render: impl FnOnce(Rect, &mut Buffer)) {
        let render_area = area.intersection(self.buffer.area);
        if render_area.is_empty() {
            return;
        }

        // A custom View can pass any Rect. Restrict allocation to the actual
        // frame while retaining the full assigned area for normal clipped
        // composition inside that frame.
        let mut scratch = Buffer::empty(render_area);
        for y in render_area.y..render_area.bottom() {
            for x in render_area.x..render_area.right() {
                scratch[(x, y)] = self.buffer[(x, y)].clone();
            }
        }

        render(render_area, &mut scratch);

        let destination = render_area.intersection(self.clip);
        for y in destination.y..destination.bottom() {
            for x in destination.x..destination.right() {
                if let Some(cell) = scratch.cell((x, y)) {
                    self.buffer[(x, y)] = cell.clone();
                }
            }
        }
    }

    fn contains(&self, x: u16, y: u16) -> bool {
        x >= self.clip.x && x < self.clip.right() && y >= self.clip.y && y < self.clip.bottom()
    }

    /// Paint every cell in the clip region with `style` (and a space glyph).
    pub fn fill(&mut self, style: Style) {
        for y in self.clip.y..self.clip.bottom() {
            for x in self.clip.x..self.clip.right() {
                let cell = &mut self.buffer[(x, y)];
                cell.set_char(' ');
                cell.set_style(style);
            }
        }
    }

    /// Set a single cell's symbol and style if it is inside the clip.
    pub fn set(&mut self, x: u16, y: u16, ch: char, style: Style) {
        if self.contains(x, y) {
            let cell = &mut self.buffer[(x, y)];
            cell.set_char(ch);
            cell.set_style(style);
        }
    }

    /// Draw `text` starting at (`x`, `y`), advancing by each glyph's display
    /// width and stopping at the clip's right edge. Returns the exclusive end
    /// column actually written.
    pub fn set_string(&mut self, x: u16, y: u16, text: &str, style: Style) -> u16 {
        let mut col = x;
        for ch in text.chars() {
            let w = UnicodeWidthChar::width(ch).unwrap_or(0) as u16;
            if w == 0 {
                continue;
            }
            if col >= self.clip.right() {
                break;
            }
            self.set(col, y, ch, style);
            // Blank the trailing cell of a wide glyph so stale content behind a
            // double-width char never shows through.
            if w == 2 && col + 1 < self.clip.right() {
                self.set(col + 1, y, ' ', style);
            }
            col = col.saturating_add(w);
        }
        col
    }

    /// Draw a rectangular border with `glyphs` around `area`, styled `style`.
    pub fn draw_border(&mut self, area: Rect, glyphs: BorderGlyphs, style: Style) {
        if area.width < 2 || area.height < 2 {
            return;
        }
        let right = area.right() - 1;
        let bottom = area.bottom() - 1;
        for x in area.x..area.right() {
            self.set(x, area.y, glyphs.horizontal, style);
            self.set(x, bottom, glyphs.horizontal, style);
        }
        for y in area.y..area.bottom() {
            self.set(area.x, y, glyphs.vertical, style);
            self.set(right, y, glyphs.vertical, style);
        }
        self.set(area.x, area.y, glyphs.top_left, style);
        self.set(right, area.y, glyphs.top_right, style);
        self.set(area.x, bottom, glyphs.bottom_left, style);
        self.set(right, bottom, glyphs.bottom_right, style);
    }
}

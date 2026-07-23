//! Clipped drawing surface over a ratatui [`Buffer`].
//!
//! Components paint through a [`Surface`], which owns a mutable borrow of the
//! frame buffer plus a clip [`Rect`]. Every write is intersected with the clip
//! region, so a child can never scribble outside the area the layout gave it —
//! this is what makes scroll viewports and overlays safe to compose. ratatui
//! still owns the shadow-buffer diff against the terminal, so `tuika` pays
//! nothing extra for dirty-cell tracking.

use ratatui_core::buffer::Buffer;
use ratatui_core::layout::Rect;
use ratatui_core::style::Style;
use unicode_segmentation::UnicodeSegmentation;

use super::style::BorderGlyphs;
use super::width::grapheme_cols;

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

    /// Set a cell to a whole grapheme cluster (may be several scalars, e.g. an
    /// emoji ZWJ sequence) and style, if it is inside the clip.
    fn set_cluster(&mut self, x: u16, y: u16, cluster: &str, style: Style) {
        if self.contains(x, y) {
            let cell = &mut self.buffer[(x, y)];
            cell.set_symbol(cluster);
            cell.set_style(style);
        }
    }

    /// Draw `text` starting at (`x`, `y`), advancing by each grapheme cluster's
    /// display width and stopping at the clip's right edge. Returns the
    /// exclusive end column actually written.
    ///
    /// Iteration is per grapheme cluster, not per `char`, so a multi-scalar
    /// emoji (ZWJ family, flag, skin-tone, or a VS16-promoted glyph) lands in a
    /// single cell with the correct width instead of being scattered across
    /// several — see [`crate::width`].
    pub fn set_string(&mut self, x: u16, y: u16, text: &str, style: Style) -> u16 {
        let mut skip = 0;
        self.set_string_skip(x, y, text, style, &mut skip)
    }

    /// Like [`set_string`](Self::set_string) but first skips `*skip` display
    /// columns of `text`, decrementing `*skip` by the columns consumed; returns
    /// the exclusive end column written. Draw a whole line span-by-span with one
    /// shared `skip` to honor a horizontal pan offset — each span resumes where
    /// the previous left off. A wide glyph straddling the skip boundary is
    /// dropped (its full width consumed from `skip`) rather than shown half.
    ///
    /// [`set_string`](Self::set_string) is this with `skip = 0`; both go through
    /// the same grapheme walk, so panning adds no second cluster-iteration site
    /// to the crate's paint hot path (which under whole-program optimization
    /// would de-inline the common flush-left path and regress it).
    pub fn set_string_skip(
        &mut self,
        x: u16,
        y: u16,
        text: &str,
        style: Style,
        skip: &mut u16,
    ) -> u16 {
        let mut col = x;
        for cluster in text.graphemes(true) {
            let w = grapheme_cols(cluster);
            if w == 0 {
                continue;
            }
            if *skip > 0 {
                *skip = skip.saturating_sub(w);
                continue;
            }
            if col >= self.clip.right() {
                break;
            }
            self.set_cluster(col, y, cluster, style);
            // Blank the trailing cell of a wide glyph so stale content behind a
            // double-width cluster never shows through.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::{Boxed, Text};
    use crate::style::Theme;
    use crate::test_support::{buffer, row};
    use crate::view::{RenderCtx, View, element};
    use ratatui_core::buffer::Buffer;
    use ratatui_core::layout::Rect;
    use ratatui_core::style::Style;

    #[test]
    fn ratatui_rendering_preserves_clip_even_if_buffer_expands() {
        let mut buf = Buffer::filled(Rect::new(0, 0, 12, 3), ratatui_core::buffer::Cell::new("#"));
        let clip = Rect::new(4, 1, 4, 1);
        {
            let mut surface = Surface::new(&mut buf, clip);
            surface.render_ratatui(Rect::new(2, 0, 8, 3), |_area, scratch| {
                let expanded =
                    Buffer::filled(Rect::new(0, 0, 12, 3), ratatui_core::buffer::Cell::new("x"));
                scratch.merge(&expanded);
            });
        }

        for y in 0..3 {
            for x in 0..12 {
                let expected = if y == 1 && (4..8).contains(&x) {
                    "x"
                } else {
                    "#"
                };
                assert_eq!(buf[(x, y)].symbol(), expected, "cell ({x}, {y})");
            }
        }
    }

    #[test]
    fn ratatui_rendering_starts_with_existing_cells() {
        use ratatui_core::style::Color;

        let mut cell = ratatui_core::buffer::Cell::new(".");
        cell.set_bg(Color::Blue);
        let mut buf = Buffer::filled(Rect::new(0, 0, 4, 1), cell);
        let area = buf.area;
        {
            let mut surface = Surface::new(&mut buf, area);
            surface.render_ratatui(area, |_area, scratch| {
                scratch[(1, 0)].set_char('x');
            });
        }
        assert_eq!(buf[(1, 0)].symbol(), "x");
        assert_eq!(buf[(1, 0)].bg, Color::Blue);
    }

    #[test]
    fn ratatui_rendering_limits_scratch_to_the_frame() {
        let mut buf = buffer(3, 2);
        let area = buf.area;
        let mut surface = Surface::new(&mut buf, area);
        surface.render_ratatui(Rect::new(0, 0, u16::MAX, u16::MAX), |actual, _| {
            assert_eq!(actual, Rect::new(0, 0, 3, 2));
        });
    }

    #[test]
    fn ratatui_rendering_tolerates_a_callback_that_resizes_its_buffer() {
        let mut buf = Buffer::filled(Rect::new(0, 0, 4, 1), ratatui_core::buffer::Cell::new("#"));
        let area = buf.area;
        let mut surface = Surface::new(&mut buf, area);
        surface.render_ratatui(area, |_area, scratch| {
            scratch.resize(Rect::new(0, 0, 1, 1));
            scratch[(0, 0)].set_char('x');
        });
        assert_eq!(row(&buf, 0), "x###");
    }

    #[test]
    fn set_string_places_multi_scalar_emoji_in_one_wide_cell() {
        // A ZWJ family (multiple scalars) is a single grapheme: it must occupy one
        // cell and advance the cursor by 2, not scatter one component per cell.
        const FAMILY: &str = "👨\u{200D}👩\u{200D}👧";
        let mut buf = Buffer::filled(Rect::new(0, 0, 6, 1), ratatui_core::buffer::Cell::new("."));
        let end = {
            let mut surface = Surface::new(&mut buf, Rect::new(0, 0, 6, 1));
            surface.set_string(0, 0, &format!("{FAMILY}x"), Style::default())
        };
        assert_eq!(
            buf[(0, 0)].symbol(),
            FAMILY,
            "whole cluster lands in one cell"
        );
        assert_eq!(buf[(1, 0)].symbol(), " ", "trailing half of the wide glyph");
        assert_eq!(buf[(2, 0)].symbol(), "x", "next glyph starts at column 2");
        assert_eq!(end, 3, "cursor advanced 2 (emoji) + 1 (x)");
    }

    #[test]
    fn set_string_skip_pans_and_carries_across_calls() {
        // "abcdef" drawn with a running skip of 2: "ab" dropped, "cdef" drawn
        // from x=0. A second call keeps consuming from the same skip.
        let mut buf = Buffer::filled(Rect::new(0, 0, 6, 1), ratatui_core::buffer::Cell::new("."));
        let mut surface = Surface::new(&mut buf, Rect::new(0, 0, 6, 1));
        let mut skip = 2u16;
        let end = surface.set_string_skip(0, 0, "abcdef", Style::default(), &mut skip);
        assert_eq!(skip, 0, "skip fully consumed by the drawn span");
        assert_eq!(end, 4, "drew 4 cols (cdef) starting at x=0");
        assert_eq!(buf[(0, 0)].symbol(), "c");
        assert_eq!(buf[(3, 0)].symbol(), "f");

        // With skip still set, a whole short span is consumed without drawing and
        // the skip carries to the next call.
        let mut buf2 = Buffer::filled(Rect::new(0, 0, 4, 1), ratatui_core::buffer::Cell::new("."));
        let mut s2 = Surface::new(&mut buf2, Rect::new(0, 0, 4, 1));
        let mut skip = 3u16;
        let x = s2.set_string_skip(0, 0, "ab", Style::default(), &mut skip); // fully skipped
        assert_eq!(skip, 1, "1 column of skip remains after the 2-col span");
        assert_eq!(x, 0, "nothing drawn, cursor unmoved");
        let x = s2.set_string_skip(x, 0, "cd", Style::default(), &mut skip); // skip 'c', draw 'd'
        assert_eq!(skip, 0);
        assert_eq!(buf2[(0, 0)].symbol(), "d", "resumes after the carried skip");
        assert_eq!(x, 1);
    }

    #[test]
    fn set_string_skip_zero_matches_set_string() {
        // The flush-left fast path: skip == 0 must be identical to set_string.
        let render = |use_skip: bool| {
            let mut buf =
                Buffer::filled(Rect::new(0, 0, 8, 1), ratatui_core::buffer::Cell::new("."));
            let end = {
                let mut s = Surface::new(&mut buf, Rect::new(0, 0, 8, 1));
                if use_skip {
                    let mut skip = 0u16;
                    s.set_string_skip(0, 0, "hi 世界", Style::default(), &mut skip)
                } else {
                    s.set_string(0, 0, "hi 世界", Style::default())
                }
            };
            (buf, end)
        };
        let (a, ea) = render(false);
        let (b, eb) = render(true);
        assert_eq!(ea, eb);
        assert_eq!(a.content, b.content, "skip=0 diverged from set_string");
    }

    #[test]
    fn surface_never_writes_outside_its_clip() {
        let theme = Theme::default();
        let ctx = RenderCtx::new(&theme);

        let mut buf = buffer(16, 8);
        for y in 0..8u16 {
            for x in 0..16u16 {
                buf[(x, y)].set_char('#');
            }
        }
        // Clip is a small window, but we hand the component a much larger area so
        // it *tries* to draw past the clip.
        let clip = Rect::new(3, 1, 6, 3);
        {
            let mut surface = Surface::new(&mut buf, clip);
            Boxed::new(element(Text::raw(
                "content far wider and taller than the clip window",
            )))
            .render(Rect::new(3, 1, 40, 20), &mut surface, &ctx);
        }
        for y in 0..8u16 {
            for x in 0..16u16 {
                let inside = x >= clip.x && x < clip.right() && y >= clip.y && y < clip.bottom();
                if !inside {
                    assert_eq!(buf[(x, y)].symbol(), "#", "clip leaked at ({x},{y})");
                }
            }
        }
    }
}

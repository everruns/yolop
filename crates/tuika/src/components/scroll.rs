//! Vertical scroll viewport with a scrollbar.
//!
//! This is the primitive that replaces native terminal scrollback in the
//! full-screen renderer: content taller than the viewport is windowed by a
//! persisted [`ScrollState`], and the state handles wheel/paging events. The
//! offset is measured in content rows from the top; a "stick to bottom" flag
//! keeps a live transcript pinned to the newest line until the user scrolls up.
//!
//! Beyond the built-in wheel/paging [`handle`](ScrollState::handle), the offset
//! is **host-drivable**: an app that owns its scroll position in its own model
//! mirrors it into the view with [`set_offset`](ScrollState::set_offset), the
//! vertical peer of [`SelectState::select`](crate::SelectState::select).

use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::Line;

use crate::event::{Event, EventFlow, KeyCode, MouseKind};
use crate::geometry::Size;
use crate::surface::Surface;
use crate::view::{RenderCtx, View};

/// Persisted scroll position for one scroll region.
///
/// Dimensions are `usize` on purpose: a transcript can wrap to far more than
/// `u16::MAX` rows in a long session. Measuring the offset and content height in
/// `u16` let a 65,536-row transcript wrap to ~0, which collapsed
/// stick-to-bottom's `max_offset` to 0 and silently snapped the view to the top.
#[derive(Clone, Copy, Debug, Default)]
pub struct ScrollState {
    /// Top visible content row.
    offset: usize,
    /// When true, `clamp` snaps to the bottom on content growth so new
    /// transcript output stays visible.
    stick_to_bottom: bool,
}

impl ScrollState {
    /// A fresh state at the top with bottom-stick armed.
    pub fn new() -> Self {
        Self {
            offset: 0,
            stick_to_bottom: true,
        }
    }

    /// Top visible content row (0-based).
    pub fn offset(&self) -> usize {
        self.offset
    }

    /// Set the top visible content row explicitly, detaching bottom-stick.
    ///
    /// The vertical counterpart to [`SelectState::select`](crate::SelectState::select):
    /// an event-loop app that already owns a scroll position in its own model
    /// mirrors it into the view each frame with this, instead of only nudging
    /// via [`handle`](Self::handle). A following [`clamp`](Self::clamp) still
    /// bounds it to the content, so an out-of-range value snaps into range
    /// rather than scrolling past the end.
    pub fn set_offset(&mut self, offset: usize) {
        self.offset = offset;
        self.stick_to_bottom = false;
    }

    /// Whether the view is pinned to the newest content.
    pub fn is_stuck_to_bottom(&self) -> bool {
        self.stick_to_bottom
    }

    fn max_offset(content_h: usize, viewport_h: usize) -> usize {
        content_h.saturating_sub(viewport_h)
    }

    /// Reconcile the offset with current content/viewport dimensions, honoring
    /// the stick-to-bottom flag. Call once per frame before rendering.
    pub fn clamp(&mut self, content_h: usize, viewport_h: usize) {
        let max = Self::max_offset(content_h, viewport_h);
        if self.stick_to_bottom {
            self.offset = max;
        } else {
            self.offset = self.offset.min(max);
        }
    }

    fn scroll_up(&mut self, lines: usize) {
        self.offset = self.offset.saturating_sub(lines);
        self.stick_to_bottom = false;
    }

    fn scroll_down(&mut self, lines: usize, content_h: usize, viewport_h: usize) {
        let max = Self::max_offset(content_h, viewport_h);
        self.offset = self.offset.saturating_add(lines).min(max);
        // Re-arm bottom-stick once the user scrolls back to the end.
        self.stick_to_bottom = self.offset >= max;
    }

    /// Jump to the newest content and re-enable bottom-stick.
    pub fn jump_to_bottom(&mut self, content_h: usize, viewport_h: usize) {
        self.offset = Self::max_offset(content_h, viewport_h);
        self.stick_to_bottom = true;
    }

    /// Jump to the top.
    pub fn jump_to_top(&mut self) {
        self.offset = 0;
        self.stick_to_bottom = false;
    }

    /// Handle a scroll/paging event against the given dimensions.
    pub fn handle(&mut self, event: &Event, content_h: usize, viewport_h: usize) -> EventFlow {
        let page = viewport_h.saturating_sub(1).max(1);
        match event {
            Event::Mouse(m) => match m.kind {
                MouseKind::ScrollUp => {
                    self.scroll_up(3);
                    EventFlow::Consumed
                }
                MouseKind::ScrollDown => {
                    self.scroll_down(3, content_h, viewport_h);
                    EventFlow::Consumed
                }
                _ => EventFlow::Ignored,
            },
            Event::Key(k) if k.plain() => match k.code {
                KeyCode::PageUp => {
                    self.scroll_up(page);
                    EventFlow::Consumed
                }
                KeyCode::PageDown => {
                    self.scroll_down(page, content_h, viewport_h);
                    EventFlow::Consumed
                }
                KeyCode::Home => {
                    self.jump_to_top();
                    EventFlow::Consumed
                }
                KeyCode::End => {
                    self.jump_to_bottom(content_h, viewport_h);
                    EventFlow::Consumed
                }
                _ => EventFlow::Ignored,
            },
            _ => EventFlow::Ignored,
        }
    }
}

/// A windowed view of content, showing the slice at `offset` and a scrollbar
/// when content overflows.
///
/// Two constructors trade off who holds the rows. [`new`](Scroll::new) owns the
/// *whole* content and paints the visible slice out of it — simplest, and right
/// for short lists. [`windowed`](Scroll::windowed) is handed *only* the visible
/// slice plus the true content height; for very long content that turns a frame
/// from O(content) (clone every row in, drop it out) into O(viewport). Both draw
/// identically; only the ownership differs.
///
/// ![scroll demo](https://raw.githubusercontent.com/everruns/yolop/main/crates/tuika/docs/demos/scroll.gif)
pub struct Scroll {
    /// The rows this view holds: the whole content in [`new`](Scroll::new), or
    /// just `content[window_start..]` in [`windowed`](Scroll::windowed).
    lines: Vec<Line<'static>>,
    /// Absolute content-row index of `lines[0]`. Zero for `new`; `offset` for
    /// `windowed`, so `render` maps a content row to a `lines` index the same
    /// way in both modes.
    window_start: usize,
    /// Total content height in rows, even when `lines` holds only a window.
    content_height: usize,
    /// Top visible content row.
    offset: usize,
    scrollbar: bool,
}

impl Scroll {
    /// Build a viewport over the whole `lines`, painting the slice at `state`'s
    /// offset. The view owns every row; for content far taller than the viewport
    /// prefer [`windowed`](Scroll::windowed).
    pub fn new(lines: Vec<Line<'static>>, state: &ScrollState) -> Self {
        Self {
            content_height: lines.len(),
            lines,
            window_start: 0,
            offset: state.offset(),
            scrollbar: true,
        }
    }

    /// Build a viewport that already holds only the visible window —
    /// `content[offset .. offset + viewport_height]`, where `offset` is
    /// `state`'s offset — instead of the whole content. `content_height` is the
    /// full row count, so the scrollbar and [`measure`](View::measure) still
    /// reflect the entire content.
    ///
    /// This is the O(viewport) path for very long content: the caller slices its
    /// own cache once per frame rather than handing over — and dropping — every
    /// row. The window may be shorter than the viewport near the bottom;
    /// `render` simply stops at its end.
    pub fn windowed(
        window: Vec<Line<'static>>,
        content_height: usize,
        state: &ScrollState,
    ) -> Self {
        let offset = state.offset();
        Self {
            lines: window,
            window_start: offset,
            content_height,
            offset,
            scrollbar: true,
        }
    }

    /// Toggle the scrollbar (shown by default when content overflows).
    pub fn scrollbar(mut self, show: bool) -> Self {
        self.scrollbar = show;
        self
    }

    /// Total content height in rows (one per line), even in windowed mode.
    pub fn content_height(&self) -> usize {
        self.content_height
    }
}

impl View for Scroll {
    fn measure(&self, available: Size) -> Size {
        // `Size` is a terminal-cell extent (`u16`); a transcript can be taller
        // than that. Saturate — the intrinsic hint only matters when the scroll
        // is not a flex `grow` child, and a viewport is never `u16::MAX` tall.
        let intrinsic_h = self.content_height().min(u16::MAX as usize) as u16;
        Size::new(available.width, intrinsic_h)
    }

    fn render(&self, area: Rect, surface: &mut Surface, ctx: &RenderCtx) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let content_h = self.content_height();
        let overflow = content_h > area.height as usize;
        let text_width = if overflow && self.scrollbar {
            area.width.saturating_sub(1)
        } else {
            area.width
        };

        for row in 0..area.height {
            // Map the content row (offset + row) to an index into `lines`, which
            // begins at `window_start` (0 in full mode, `offset` when windowed).
            let Some(idx) = (self.offset + row as usize).checked_sub(self.window_start) else {
                break;
            };
            let Some(line) = self.lines.get(idx) else {
                break;
            };
            let y = area.y + row;
            let mut clip = surface.child(Rect::new(area.x, y, text_width, 1));
            let mut x = area.x;
            for span in &line.spans {
                if x >= area.x + text_width {
                    break;
                }
                x = clip.set_string(x, y, span.content.as_ref(), span.style);
            }
        }

        if overflow && self.scrollbar && text_width < area.width {
            self.draw_scrollbar(area, content_h, surface, ctx);
        }
    }
}

impl Scroll {
    fn draw_scrollbar(&self, area: Rect, content_h: usize, surface: &mut Surface, ctx: &RenderCtx) {
        let track_x = area.right() - 1;
        let track_h = area.height;
        let track_h_u = track_h as usize;
        let content_h = content_h.max(1);
        let max_offset = content_h.saturating_sub(area.height as usize).max(1);
        // Thumb size proportional to the visible fraction, at least one cell. All
        // math is `usize` so a > u16::MAX-row transcript can't wrap the ratios;
        // the final screen positions are bounded by `track_h` and cast back down.
        let thumb_h = ((track_h_u * track_h_u) / content_h).max(1).min(track_h_u) as u16;
        let travel = track_h.saturating_sub(thumb_h);
        let thumb_y = area.y + ((self.offset * travel as usize) / max_offset) as u16;
        let track_style = Style::default().fg(ctx.theme.dim);
        let thumb_style = Style::default().fg(ctx.theme.muted);
        for row in 0..track_h {
            let y = area.y + row;
            let within = y >= thumb_y && y < thumb_y.saturating_add(thumb_h);
            let (glyph, style) = if within {
                ('█', thumb_style)
            } else {
                ('│', track_style)
            };
            surface.set(track_x, y, glyph, style);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Surface;
    use crate::event::{Event, EventFlow, Key, KeyCode, Mouse, MouseKind};
    use crate::style::Theme;
    use crate::test_support::{buffer, rainbow_theme, row};
    use crate::view::{RenderCtx, View};
    use ratatui::style::Color;
    use ratatui::text::Line;

    #[test]
    fn scroll_sticks_to_bottom_until_scrolled_up() {
        let mut s = ScrollState::new();
        // content 100 rows, viewport 10 => bottom offset 90.
        s.clamp(100, 10);
        assert_eq!(s.offset(), 90);
        assert!(s.is_stuck_to_bottom());

        // Wheel up unsticks and moves up by 3.
        let up = Event::Mouse(Mouse::at(MouseKind::ScrollUp, 0, 0));
        assert_eq!(s.handle(&up, 100, 10), EventFlow::Consumed);
        assert!(!s.is_stuck_to_bottom());
        assert_eq!(s.offset(), 87);

        // Growing content no longer drags the view down while unstuck.
        s.clamp(200, 10);
        assert_eq!(s.offset(), 87);
    }

    #[test]
    fn scroll_offset_survives_content_taller_than_u16() {
        // Regression: a transcript taller than u16::MAX must not wrap the
        // content height back near 0 and collapse stick-to-bottom to the top.
        let content_h = u16::MAX as usize + 5_000; // 70,535 rows
        let viewport_h = 40;
        let mut s = ScrollState::new();
        s.clamp(content_h, viewport_h);
        assert_eq!(s.offset(), content_h - viewport_h);
        assert!(s.is_stuck_to_bottom());

        // Paging up from the bottom detaches and moves by a page, still far from
        // the top rather than snapping there.
        let up = Event::Key(Key::new(KeyCode::PageUp));
        assert_eq!(s.handle(&up, content_h, viewport_h), EventFlow::Consumed);
        assert!(!s.is_stuck_to_bottom());
        assert_eq!(s.offset(), content_h - viewport_h - (viewport_h - 1));
    }

    #[test]
    fn set_offset_positions_view_and_detaches_stick() {
        let mut s = ScrollState::new();
        s.clamp(100, 10);
        assert!(s.is_stuck_to_bottom(), "starts stuck to bottom");
        // Host mirrors its own scroll row in; stick detaches and clamp honors it.
        s.set_offset(40);
        assert!(!s.is_stuck_to_bottom());
        s.clamp(100, 10);
        assert_eq!(s.offset(), 40);
        // An out-of-range value snaps into range on the next clamp rather than
        // scrolling past the end.
        s.set_offset(500);
        s.clamp(100, 10);
        assert_eq!(s.offset(), 90, "clamped to content height - viewport");
    }

    #[test]
    fn scroll_end_key_rearms_bottom_stick() {
        let mut s = ScrollState::new();
        s.clamp(100, 10);
        s.jump_to_top();
        assert_eq!(s.offset(), 0);
        assert!(!s.is_stuck_to_bottom());
        let end = Event::Key(Key::new(KeyCode::End));
        assert_eq!(s.handle(&end, 100, 10), EventFlow::Consumed);
        assert_eq!(s.offset(), 90);
        assert!(s.is_stuck_to_bottom());
    }

    #[test]
    fn scroll_view_windows_content_and_draws_scrollbar() {
        let lines: Vec<Line<'static>> = (0..20).map(|i| Line::from(format!("line{i}"))).collect();
        let mut state = ScrollState::new();
        state.clamp(20, 5); // stuck to bottom => offset 15
        let scroll = Scroll::new(lines, &state);
        let mut buf = buffer(10, 5);
        let theme = Theme::default();
        let ctx = RenderCtx::new(&theme);
        let area = buf.area;
        let mut surface = Surface::new(&mut buf, area);
        scroll.render(area, &mut surface, &ctx);
        // Bottom-stuck: shows the last five lines (15..20).
        assert!(row(&buf, 0).starts_with("line15"));
        assert!(row(&buf, 4).starts_with("line19"));
        // Scrollbar drawn in the last column somewhere.
        let has_bar = (0..5).any(|y| {
            let c = buf[(9, y)].symbol().to_string();
            c == "█" || c == "│"
        });
        assert!(has_bar, "expected a scrollbar in the right column");
    }

    #[test]
    fn scroll_windowed_matches_full_render() {
        // `windowed` holds only the visible slice but must paint byte-for-byte
        // identically to `new` (which owns the whole content), scrollbar and all.
        let content: Vec<Line<'static>> =
            (0..1000).map(|i| Line::from(format!("line{i}"))).collect();
        let (width, height) = (12u16, 6u16);
        let theme = Theme::default();
        let ctx = RenderCtx::new(&theme);

        // A mid-content offset (not top, not bottom) exercises the window origin.
        let mut state = ScrollState::new();
        state.clamp(content.len(), height as usize);
        state.jump_to_top();
        let end = Event::Key(Key::new(KeyCode::PageDown));
        state.handle(&end, content.len(), height as usize); // one page down
        let offset = state.offset();
        assert!(offset > 0 && offset < content.len() - height as usize);

        let render = |scroll: Scroll| {
            let mut buf = buffer(width, height);
            let area = buf.area;
            let mut surface = Surface::new(&mut buf, area);
            scroll.render(area, &mut surface, &ctx);
            buf
        };

        let full = render(Scroll::new(content.clone(), &state));
        // The window is exactly what `windowed`'s caller would slice.
        let window = content[offset..offset + height as usize].to_vec();
        let windowed = render(Scroll::windowed(window, content.len(), &state));

        assert_eq!(
            full.content, windowed.content,
            "windowed render diverged from the full render"
        );
    }

    #[test]
    fn scrollbar_thumb_and_track_use_theme() {
        let t = rainbow_theme();
        let lines: Vec<Line<'static>> = (0..30).map(|i| Line::from(format!("l{i}"))).collect();
        let mut state = ScrollState::new();
        state.clamp(30, 5);
        let scroll = Scroll::new(lines, &state);
        let mut buf = buffer(10, 5);
        let area = buf.area;
        let ctx = RenderCtx::new(&t);
        let mut surface = Surface::new(&mut buf, area);
        scroll.render(area, &mut surface, &ctx);
        let col = 9; // scrollbar column
        let fgs: Vec<Color> = (0..5).map(|y| buf[(col, y)].fg).collect();
        assert!(fgs.contains(&t.muted), "thumb uses theme.muted: {fgs:?}");
        assert!(fgs.contains(&t.dim), "track uses theme.dim: {fgs:?}");
    }
}

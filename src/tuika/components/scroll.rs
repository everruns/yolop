//! Vertical scroll viewport with a scrollbar.
//!
//! This is the primitive that replaces native terminal scrollback in the
//! full-screen renderer: content taller than the viewport is windowed by a
//! persisted [`ScrollState`], and the state handles wheel/paging events. The
//! offset is measured in content rows from the top; a "stick to bottom" flag
//! keeps a live transcript pinned to the newest line until the user scrolls up.

use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::Line;

use crate::tuika::event::{Event, EventFlow, KeyCode, MouseKind};
use crate::tuika::geometry::Size;
use crate::tuika::surface::Surface;
use crate::tuika::view::{RenderCtx, View};

/// Persisted scroll position for one scroll region.
#[derive(Clone, Copy, Debug, Default)]
pub struct ScrollState {
    /// Top visible content row.
    offset: u16,
    /// When true, `clamp` snaps to the bottom on content growth so new
    /// transcript output stays visible.
    stick_to_bottom: bool,
}

impl ScrollState {
    pub fn new() -> Self {
        Self {
            offset: 0,
            stick_to_bottom: true,
        }
    }

    pub fn offset(&self) -> u16 {
        self.offset
    }

    pub fn is_stuck_to_bottom(&self) -> bool {
        self.stick_to_bottom
    }

    fn max_offset(content_h: u16, viewport_h: u16) -> u16 {
        content_h.saturating_sub(viewport_h)
    }

    /// Reconcile the offset with current content/viewport dimensions, honoring
    /// the stick-to-bottom flag. Call once per frame before rendering.
    pub fn clamp(&mut self, content_h: u16, viewport_h: u16) {
        let max = Self::max_offset(content_h, viewport_h);
        if self.stick_to_bottom {
            self.offset = max;
        } else {
            self.offset = self.offset.min(max);
        }
    }

    fn scroll_up(&mut self, lines: u16) {
        self.offset = self.offset.saturating_sub(lines);
        self.stick_to_bottom = false;
    }

    fn scroll_down(&mut self, lines: u16, content_h: u16, viewport_h: u16) {
        let max = Self::max_offset(content_h, viewport_h);
        self.offset = self.offset.saturating_add(lines).min(max);
        // Re-arm bottom-stick once the user scrolls back to the end.
        self.stick_to_bottom = self.offset >= max;
    }

    /// Jump to the newest content and re-enable bottom-stick.
    pub fn jump_to_bottom(&mut self, content_h: u16, viewport_h: u16) {
        self.offset = Self::max_offset(content_h, viewport_h);
        self.stick_to_bottom = true;
    }

    /// Jump to the top.
    pub fn jump_to_top(&mut self) {
        self.offset = 0;
        self.stick_to_bottom = false;
    }

    /// Handle a scroll/paging event against the given dimensions.
    pub fn handle(&mut self, event: &Event, content_h: u16, viewport_h: u16) -> EventFlow {
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

/// A windowed view of `lines`, showing the slice at `offset` and a scrollbar
/// when content overflows.
pub struct Scroll {
    lines: Vec<Line<'static>>,
    offset: u16,
    scrollbar: bool,
}

impl Scroll {
    pub fn new(lines: Vec<Line<'static>>, state: &ScrollState) -> Self {
        Self {
            lines,
            offset: state.offset(),
            scrollbar: true,
        }
    }

    pub fn scrollbar(mut self, show: bool) -> Self {
        self.scrollbar = show;
        self
    }

    pub fn content_height(&self) -> u16 {
        self.lines.len() as u16
    }
}

impl View for Scroll {
    fn measure(&self, available: Size) -> Size {
        Size::new(available.width, self.content_height())
    }

    fn render(&self, area: Rect, surface: &mut Surface, ctx: &RenderCtx) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let content_h = self.content_height();
        let overflow = content_h > area.height;
        let text_width = if overflow && self.scrollbar {
            area.width.saturating_sub(1)
        } else {
            area.width
        };

        let start = self.offset as usize;
        for row in 0..area.height {
            let idx = start + row as usize;
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
    fn draw_scrollbar(&self, area: Rect, content_h: u16, surface: &mut Surface, ctx: &RenderCtx) {
        let track_x = area.right() - 1;
        let track_h = area.height;
        let max_offset = content_h.saturating_sub(area.height).max(1);
        // Thumb size proportional to the visible fraction, at least one cell.
        let thumb_h = ((track_h as u32 * track_h as u32) / content_h as u32).max(1) as u16;
        let thumb_h = thumb_h.min(track_h);
        let travel = track_h.saturating_sub(thumb_h);
        let thumb_y = area.y + ((self.offset as u32 * travel as u32) / max_offset as u32) as u16;
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

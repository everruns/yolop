//! Selectable list (Pi's `SelectList`).
//!
//! [`SelectState`] persists the highlighted index and handles up/down/wrap
//! navigation; [`SelectList`] renders the options, marking the current one with
//! the theme selection style and a caret. Enter is surfaced to the host via
//! [`SelectOutcome`] so the caller decides what "confirm" means.

use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::Line;

use crate::event::{Event, EventFlow, KeyCode};
use crate::geometry::Size;
use crate::surface::Surface;
use crate::view::{RenderCtx, View};

/// Result of feeding an event to a [`SelectState`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectOutcome {
    /// Highlight moved or nothing happened; event consumed status in `.1`.
    Moved(EventFlow),
    /// Enter pressed on the given index.
    Confirmed(usize),
    /// Esc pressed.
    Cancelled,
}

/// Persisted selection index for one list.
#[derive(Clone, Copy, Debug, Default)]
pub struct SelectState {
    selected: usize,
}

impl SelectState {
    pub fn new() -> Self {
        Self { selected: 0 }
    }

    pub fn selected(&self) -> usize {
        self.selected
    }

    /// Set the highlighted index directly. Lets a host drive the selection from
    /// its own state (e.g. mirroring an external index into the list).
    pub fn select(&mut self, index: usize) {
        self.selected = index;
    }

    /// Keep the index in range as the list length changes.
    pub fn clamp(&mut self, len: usize) {
        if len == 0 {
            self.selected = 0;
        } else if self.selected >= len {
            self.selected = len - 1;
        }
    }

    /// Navigate with arrow keys (wrapping), confirm with Enter, cancel on Esc.
    pub fn handle(&mut self, event: &Event, len: usize) -> SelectOutcome {
        if len == 0 {
            return SelectOutcome::Moved(EventFlow::Ignored);
        }
        let Event::Key(k) = event else {
            return SelectOutcome::Moved(EventFlow::Ignored);
        };
        if !k.plain() {
            return SelectOutcome::Moved(EventFlow::Ignored);
        }
        match k.code {
            KeyCode::Up => {
                self.selected = if self.selected == 0 {
                    len - 1
                } else {
                    self.selected - 1
                };
                SelectOutcome::Moved(EventFlow::Consumed)
            }
            KeyCode::Down => {
                self.selected = (self.selected + 1) % len;
                SelectOutcome::Moved(EventFlow::Consumed)
            }
            KeyCode::Enter => SelectOutcome::Confirmed(self.selected),
            KeyCode::Esc => SelectOutcome::Cancelled,
            _ => SelectOutcome::Moved(EventFlow::Ignored),
        }
    }
}

/// Renders `items` with the selected row highlighted. With a [`viewport`] set,
/// a list taller than the viewport is windowed around the selection and a
/// scrollbar is drawn — the primitive for long pickers (hundreds of models).
///
/// [`viewport`]: SelectList::viewport
///
/// ![select demo](https://raw.githubusercontent.com/everruns/yolop/main/crates/tuika/docs/demos/select.gif)
pub struct SelectList {
    items: Vec<Line<'static>>,
    selected: usize,
    /// Max visible rows; `None` shows the whole list.
    viewport: Option<u16>,
    scrollbar: bool,
}

impl SelectList {
    pub fn new(items: Vec<Line<'static>>, state: &SelectState) -> Self {
        Self {
            items,
            selected: state.selected(),
            viewport: None,
            scrollbar: true,
        }
    }

    /// Cap the visible rows to `rows`, windowing a longer list around the
    /// selection so the highlighted row stays on screen.
    pub fn viewport(mut self, rows: u16) -> Self {
        self.viewport = Some(rows.max(1));
        self
    }

    /// Show the overflow scrollbar (default true; only drawn when windowed).
    pub fn scrollbar(mut self, show: bool) -> Self {
        self.scrollbar = show;
        self
    }

    /// The `(start, visible_rows)` window: the whole list unless a `viewport`
    /// smaller than the list is set, in which case a slice centered on the
    /// selection and clamped to the ends.
    fn window(&self) -> (usize, usize) {
        let total = self.items.len();
        match self.viewport {
            Some(v) if total > v as usize => {
                let v = (v as usize).max(1);
                let start = self.selected.saturating_sub(v / 2).min(total - v);
                (start, v)
            }
            _ => (0, total),
        }
    }
}

impl View for SelectList {
    fn measure(&self, available: Size) -> Size {
        let width = self
            .items
            .iter()
            .map(super::text::line_width)
            .max()
            .unwrap_or(0)
            .saturating_add(2); // caret + space
        let (_, rows) = self.window();
        Size::new(width.min(available.width), rows as u16)
    }

    fn render(&self, area: Rect, surface: &mut Surface, ctx: &RenderCtx) {
        let (start, rows) = self.window();
        let overflow = self.items.len() > rows;
        // Reserve the last column for the scrollbar when the list overflows.
        let row_width = if overflow && self.scrollbar {
            area.width.saturating_sub(1)
        } else {
            area.width
        };
        let row_right = area.x.saturating_add(row_width);
        for i in 0..rows {
            let idx = start + i;
            let Some(item) = self.items.get(idx) else {
                break;
            };
            let y = area.y.saturating_add(i as u16);
            if y >= area.bottom() {
                break;
            }
            let selected = idx == self.selected;
            if selected {
                let mut line = surface.child(Rect::new(area.x, y, row_width, 1));
                line.fill(ctx.theme.selection_style());
            }
            let caret = if selected { '›' } else { ' ' };
            let caret_style = if selected {
                ctx.theme.selection_style()
            } else {
                ctx.theme.muted_style()
            };
            surface.set(area.x, y, caret, caret_style);
            let mut x = area.x.saturating_add(2);
            for span in &item.spans {
                if x >= row_right {
                    break;
                }
                let style = if selected {
                    span.style.patch(ctx.theme.selection_style())
                } else {
                    span.style
                };
                x = surface.set_string(x, y, span.content.as_ref(), style);
            }
        }
        if overflow && self.scrollbar && row_width < area.width {
            self.draw_scrollbar(area, start, rows, surface, ctx);
        }
    }
}

impl SelectList {
    /// A right-edge scrollbar whose thumb tracks the window position, mirroring
    /// [`Scroll`](super::Scroll)'s scrollbar.
    fn draw_scrollbar(
        &self,
        area: Rect,
        start: usize,
        rows: usize,
        surface: &mut Surface,
        ctx: &RenderCtx,
    ) {
        let total = self.items.len();
        let track_x = area.right() - 1;
        let track_h = rows as u16;
        let max_start = total.saturating_sub(rows).max(1) as u32;
        let thumb_h = (((rows * rows) / total).max(1) as u16).min(track_h);
        let travel = track_h.saturating_sub(thumb_h);
        let thumb_y = area.y + ((start as u32 * travel as u32) / max_start) as u16;
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

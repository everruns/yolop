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
    /// A fresh state with the first row highlighted.
    pub fn new() -> Self {
        Self { selected: 0 }
    }

    /// The currently highlighted index.
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

    /// Move the highlight up one row, clamping at the top (no wrap). The
    /// non-wrapping stepping primitive; use it when a picker holds at the ends
    /// rather than wrapping the way [`handle`](Self::handle) does.
    pub fn move_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    /// Move the highlight down one row, clamping at the last of `len` rows (no
    /// wrap). The non-wrapping counterpart to [`move_up`](Self::move_up).
    pub fn move_down(&mut self, len: usize) {
        if len == 0 {
            self.selected = 0;
        } else {
            self.selected = (self.selected + 1).min(len - 1);
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
    /// A list of `items` with the row from `state` highlighted.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{Event, EventFlow, Key, KeyCode};
    use crate::style::Theme;
    use crate::test_support::{buffer, rainbow_theme, row};
    use crate::view::{RenderCtx, View};
    use crate::{Size, Surface};
    use ratatui::text::Line;

    #[test]
    fn select_navigation_wraps_and_confirms() {
        let mut s = SelectState::new();
        let down = Event::Key(Key::new(KeyCode::Down));
        let up = Event::Key(Key::new(KeyCode::Up));
        assert_eq!(s.handle(&up, 3), SelectOutcome::Moved(EventFlow::Consumed));
        assert_eq!(s.selected(), 2); // wrapped from 0 to last
        assert_eq!(
            s.handle(&down, 3),
            SelectOutcome::Moved(EventFlow::Consumed)
        );
        assert_eq!(s.selected(), 0); // wrapped back
        let enter = Event::Key(Key::new(KeyCode::Enter));
        assert_eq!(s.handle(&enter, 3), SelectOutcome::Confirmed(0));
        let esc = Event::Key(Key::new(KeyCode::Esc));
        assert_eq!(s.handle(&esc, 3), SelectOutcome::Cancelled);
    }

    #[test]
    fn select_move_up_down_clamp_at_ends() {
        let mut s = SelectState::new();
        // Down steps forward, clamping at the last of `len` rows (no wrap).
        s.move_down(3);
        assert_eq!(s.selected(), 1);
        s.move_down(3);
        assert_eq!(s.selected(), 2);
        s.move_down(3);
        assert_eq!(s.selected(), 2); // held at the bottom, not wrapped
        // Up steps back, clamping at the top.
        s.move_up();
        assert_eq!(s.selected(), 1);
        s.move_up();
        s.move_up();
        assert_eq!(s.selected(), 0); // held at the top, not wrapped
        // Degenerate: an empty list stays at 0.
        s.move_down(0);
        assert_eq!(s.selected(), 0);
    }

    #[test]
    fn select_state_select_sets_index_directly() {
        // A host can drive the highlight from its own state.
        let mut s = SelectState::new();
        s.select(2);
        assert_eq!(s.selected(), 2);
    }

    #[test]
    fn select_highlights_current_row() {
        let items = vec![Line::from("alpha"), Line::from("beta")];
        let mut state = SelectState::new();
        state.handle(&Event::Key(Key::new(KeyCode::Down)), 2); // select beta
        let list = SelectList::new(items, &state);
        let mut buf = buffer(10, 2);
        let theme = Theme::default();
        let ctx = RenderCtx::new(&theme);
        let area = buf.area;
        let mut surface = Surface::new(&mut buf, area);
        list.render(area, &mut surface, &ctx);
        assert!(row(&buf, 1).contains("beta"));
        // Selected row carries the selection background.
        assert_eq!(buf[(0, 1)].bg, theme.selection_bg);
        assert_eq!(buf[(0, 0)].bg, ratatui::style::Color::Reset);
    }

    #[test]
    fn select_viewport_windows_a_long_list_and_keeps_selection_visible() {
        // 20 items, viewport of 4: the selection must always be on screen.
        let items: Vec<Line> = (0..20).map(|i| Line::from(format!("item{i}"))).collect();
        let mut state = SelectState::new();
        state.select(12);
        let theme = Theme::default();
        let list = SelectList::new(items.clone(), &state).viewport(4);
        // Windowed height is the viewport, not the full list.
        assert_eq!(list.measure(Size::new(20, 40)).height, 4);
        let rendered = crate::testing::render(&list, 20, 4, &theme);
        let text = crate::testing::grid(&rendered);
        assert!(
            text.contains("item12"),
            "selection should be visible:\n{text}"
        );
        assert!(
            !text.contains("item0\n") && !text.contains("item19"),
            "far items windowed out"
        );
        // A scrollbar occupies the last column on at least one row.
        let has_scrollbar = (0..4).any(|y| matches!(rendered[(19, y)].symbol(), "█" | "│"));
        assert!(
            has_scrollbar,
            "overflowing list should draw a scrollbar:\n{text}"
        );
    }

    #[test]
    fn select_viewport_shows_whole_list_when_it_fits() {
        let items: Vec<Line> = (0..3).map(|i| Line::from(format!("item{i}"))).collect();
        let state = SelectState::new();
        let list = SelectList::new(items, &state).viewport(8);
        // Fits within the viewport → no windowing, height is the item count.
        assert_eq!(list.measure(Size::new(20, 40)).height, 3);
        let theme = Theme::default();
        let text = crate::testing::grid(&crate::testing::render(&list, 20, 3, &theme));
        assert!(text.contains("item0") && text.contains("item2"));
    }

    #[test]
    fn select_list_selection_uses_theme_slots() {
        let t = rainbow_theme();
        let mut state = SelectState::new();
        state.handle(&Event::Key(Key::new(KeyCode::Down)), 2); // select row 1
        let list = SelectList::new(vec![Line::from("a"), Line::from("b")], &state);
        let mut buf = buffer(10, 2);
        let area = buf.area;
        let ctx = RenderCtx::new(&t);
        let mut surface = Surface::new(&mut buf, area);
        list.render(area, &mut surface, &ctx);
        assert_eq!(buf[(0, 1)].bg, t.selection_bg, "selected row bg");
        assert_eq!(buf[(0, 1)].fg, t.selection_fg, "selected caret fg");
        assert_ne!(
            buf[(0, 0)].bg,
            t.selection_bg,
            "unselected row not highlighted"
        );
    }
}

//! Selectable list (Pi's `SelectList`).
//!
//! [`SelectState`] persists the highlighted index and handles up/down/wrap
//! navigation; [`SelectList`] renders the options, marking the current one with
//! the theme selection style and a caret. Enter is surfaced to the host via
//! [`SelectOutcome`] so the caller decides what "confirm" means.

use ratatui::layout::Rect;
use ratatui::text::Line;

use crate::tuika::event::{Event, EventFlow, KeyCode};
use crate::tuika::geometry::Size;
use crate::tuika::surface::Surface;
use crate::tuika::view::{RenderCtx, View};

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

/// Renders `items` with the selected row highlighted.
pub struct SelectList {
    items: Vec<Line<'static>>,
    selected: usize,
}

impl SelectList {
    pub fn new(items: Vec<Line<'static>>, state: &SelectState) -> Self {
        Self {
            items,
            selected: state.selected(),
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
        Size::new(width.min(available.width), self.items.len() as u16)
    }

    fn render(&self, area: Rect, surface: &mut Surface, ctx: &RenderCtx) {
        for (row, item) in self.items.iter().enumerate() {
            let y = area.y.saturating_add(row as u16);
            if y >= area.bottom() {
                break;
            }
            let selected = row == self.selected;
            if selected {
                let mut line = surface.child(Rect::new(area.x, y, area.width, 1));
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
                if x >= area.right() {
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
    }
}

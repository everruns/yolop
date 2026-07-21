//! Mouse interaction over the rendered cell grid: text selection, click
//! hit-testing, and click detection.
//!
//! Terminals don't hand you selection or clickable regions — once an app
//! enables mouse capture (see [`AltScreen`](crate::AltScreen)) the terminal's
//! own click-drag selection stops working, and every drag arrives as a
//! [`Mouse`] event instead. This module rebuilds those affordances *on top of*
//! the grid the app already rendered:
//!
//! - [`SelectionState`] turns a left button `Down -> Drag -> Up` gesture into a
//!   [`SelectionRange`]; [`selected_text`] reads the text back out of the
//!   [`Buffer`] and [`highlight`] paints the selection into it. Pair with
//!   [`crate::clipboard::write_clipboard`] to copy.
//! - [`HitMap`] maps screen regions to values so a click resolves to whatever
//!   was drawn there (a button, a link, a list row); [`ClickTracker`] turns a
//!   `Down`/`Up` pair on the same cell into a click (and lets a drag cancel it).
//!
//! Selection is *linear* (stream) like a terminal's own: a multi-row selection
//! covers the tail of the first row, all of the middle rows, and the head of
//! the last row — not a rectangular block.
//!
//! **Touch:** terminal emulators deliver touch as mouse events (a tap is a
//! `Down`+`Up`, a swipe is `ScrollUp`/`ScrollDown` or a `Drag`), so touch input
//! flows through this same path — there is no separate touch event to model.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;

use crate::event::{Mouse, MouseButton, MouseKind};

/// A normalized text selection in reading order: `start` is at or before `end`
/// ordered by `(row, column)`. Both endpoints are inclusive cells.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SelectionRange {
    /// `(column, row)` of the first selected cell.
    pub start: (u16, u16),
    /// `(column, row)` of the last selected cell (inclusive).
    pub end: (u16, u16),
}

impl SelectionRange {
    /// Order two cells into reading order (`(row, col)` ascending).
    fn between(a: (u16, u16), b: (u16, u16)) -> Self {
        let (start, end) = if (a.1, a.0) <= (b.1, b.0) {
            (a, b)
        } else {
            (b, a)
        };
        SelectionRange { start, end }
    }

    /// The inclusive `[left, right]` selected column span on `row`, clamped to
    /// `area`. Rows outside the selection return `None`.
    fn row_span(&self, row: u16, area: Rect) -> Option<(u16, u16)> {
        if row < self.start.1 || row > self.end.1 {
            return None;
        }
        let left = if row == self.start.1 {
            self.start.0
        } else {
            area.x
        };
        let right = if row == self.end.1 {
            self.end.0
        } else {
            area.right().saturating_sub(1)
        };
        let left = left.max(area.x);
        let right = right.min(area.right().saturating_sub(1));
        (left <= right).then_some((left, right))
    }

    /// Whether `(column, row)` falls inside the selection within `area`.
    pub fn contains(&self, column: u16, row: u16, area: Rect) -> bool {
        self.row_span(row, area)
            .is_some_and(|(l, r)| column >= l && column <= r)
    }
}

/// Tracks a left-drag text selection across mouse events.
///
/// Left `Down` starts (and clears any previous selection); `Drag` extends;
/// `Up` finishes. A press with no drag (a plain click) leaves no selection.
/// Every other event is ignored. `handle` returns `true` when the selection
/// changed, so a host knows to redraw.
#[derive(Clone, Copy, Debug, Default)]
pub struct SelectionState {
    anchor: (u16, u16),
    cursor: (u16, u16),
    pressed: bool,
    selecting: bool,
}

impl SelectionState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn handle(&mut self, m: &Mouse) -> bool {
        match m.kind {
            MouseKind::Down(MouseButton::Left) => {
                self.anchor = (m.column, m.row);
                self.cursor = (m.column, m.row);
                self.pressed = true;
                let had = self.selecting;
                self.selecting = false;
                // Redraw if we cleared an existing selection.
                had
            }
            MouseKind::Drag(MouseButton::Left) if self.pressed => {
                self.cursor = (m.column, m.row);
                self.selecting = self.cursor != self.anchor;
                true
            }
            MouseKind::Up(MouseButton::Left) if self.pressed => {
                self.cursor = (m.column, m.row);
                self.pressed = false;
                self.selecting = self.cursor != self.anchor;
                true
            }
            _ => false,
        }
    }

    /// Clear the selection (e.g. on Esc or a new turn).
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// The current selection, or `None` when nothing is selected.
    pub fn range(&self) -> Option<SelectionRange> {
        self.selecting
            .then(|| SelectionRange::between(self.anchor, self.cursor))
    }

    pub fn is_active(&self) -> bool {
        self.selecting
    }
}

/// Extract the selected text from `buffer`, reading the grid the way a terminal
/// does: the tail of the first row, whole middle rows, and the head of the last
/// row, joined with `\n`. Trailing blanks on each row are trimmed. Wide glyphs
/// come back once (their trailing spacer cell is empty in the buffer).
pub fn selected_text(buffer: &Buffer, area: Rect, sel: SelectionRange) -> String {
    let mut out = String::new();
    let mut first = true;
    for row in sel.start.1..=sel.end.1 {
        let Some((left, right)) = sel.row_span(row, area) else {
            continue;
        };
        if !first {
            out.push('\n');
        }
        first = false;
        let mut line = String::new();
        for col in left..=right {
            line.push_str(buffer[(col, row)].symbol());
        }
        out.push_str(line.trim_end_matches(' '));
    }
    out
}

/// Paint `style` over the selected cells of `buffer` (patching, so glyphs and
/// foreground survive; typically a reversed or selection-background style).
pub fn highlight(buffer: &mut Buffer, area: Rect, sel: SelectionRange, style: Style) {
    for row in sel.start.1..=sel.end.1 {
        let Some((left, right)) = sel.row_span(row, area) else {
            continue;
        };
        for col in left..=right {
            buffer[(col, row)].set_style(style);
        }
    }
}

/// Maps screen regions to values for click hit-testing.
///
/// Push a region per clickable thing while laying out a frame, then resolve a
/// click position to the value drawn there. Later pushes win, so registering
/// children/overlays after their parents gives the innermost/topmost region
/// precedence.
#[derive(Clone, Debug)]
pub struct HitMap<T> {
    regions: Vec<(Rect, T)>,
}

impl<T> Default for HitMap<T> {
    fn default() -> Self {
        Self {
            regions: Vec::new(),
        }
    }
}

impl<T> HitMap<T> {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register `value` at `area`. A zero-area rect never matches.
    pub fn push(&mut self, area: Rect, value: T) {
        self.regions.push((area, value));
    }

    pub fn clear(&mut self) {
        self.regions.clear();
    }

    /// The value of the last-pushed region containing `(column, row)`.
    pub fn hit(&self, column: u16, row: u16) -> Option<&T> {
        self.regions
            .iter()
            .rev()
            .find(|(r, _)| in_rect(*r, column, row))
            .map(|(_, v)| v)
    }

    /// Resolve a mouse event's position through the map.
    pub fn hit_event(&self, m: &Mouse) -> Option<&T> {
        self.hit(m.column, m.row)
    }
}

fn in_rect(r: Rect, col: u16, row: u16) -> bool {
    r.width > 0 && r.height > 0 && col >= r.x && col < r.right() && row >= r.y && row < r.bottom()
}

/// A completed click: the cell and button of a `Down`/`Up` pair on one cell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Click {
    pub column: u16,
    pub row: u16,
    pub button: MouseButton,
}

/// Turns `Down`/`Up` mouse pairs into [`Click`]s. A press then release on the
/// *same* cell with the *same* button is a click; a drag in between cancels it
/// (that gesture is a selection, not a click). Feed every mouse event; `handle`
/// returns `Some(Click)` only on the completing `Up`.
#[derive(Clone, Copy, Debug, Default)]
pub struct ClickTracker {
    down: Option<(u16, u16, MouseButton)>,
}

impl ClickTracker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn handle(&mut self, m: &Mouse) -> Option<Click> {
        match m.kind {
            MouseKind::Down(button) => {
                self.down = Some((m.column, m.row, button));
                None
            }
            MouseKind::Drag(_) | MouseKind::Moved => {
                // Movement disqualifies the pending press from being a click.
                self.down = None;
                None
            }
            MouseKind::Up(button) => match self.down.take() {
                Some((col, row, pressed))
                    if pressed == button && col == m.column && row == m.row =>
                {
                    Some(Click {
                        column: m.column,
                        row: m.row,
                        button,
                    })
                }
                _ => None,
            },
            _ => None,
        }
    }
}

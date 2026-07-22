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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::Text;
    use crate::event::{Mouse, MouseButton, MouseKind};
    use crate::style::Theme;
    use ratatui::layout::Rect;
    use ratatui::style::{Color, Style};

    fn down(col: u16, row: u16) -> Mouse {
        Mouse::at(MouseKind::Down(MouseButton::Left), col, row)
    }
    fn drag(col: u16, row: u16) -> Mouse {
        Mouse::at(MouseKind::Drag(MouseButton::Left), col, row)
    }
    fn up(col: u16, row: u16) -> Mouse {
        Mouse::at(MouseKind::Up(MouseButton::Left), col, row)
    }

    #[test]
    fn selection_tracks_a_left_drag() {
        let mut sel = SelectionState::new();
        assert!(!sel.handle(&down(1, 0))); // fresh press: nothing to redraw yet
        assert!(sel.handle(&drag(4, 0)));
        assert!(sel.handle(&up(4, 0)));
        let range = sel.range().expect("a drag selects");
        assert_eq!(range.start, (1, 0));
        assert_eq!(range.end, (4, 0));
        assert!(sel.is_active());
    }

    #[test]
    fn selection_normalizes_a_backwards_drag() {
        let mut sel = SelectionState::new();
        sel.handle(&down(5, 1));
        sel.handle(&drag(2, 0));
        sel.handle(&up(2, 0));
        let range = sel.range().expect("selection");
        // Reading order: (col=2,row=0) comes before (col=5,row=1).
        assert_eq!(range.start, (2, 0));
        assert_eq!(range.end, (5, 1));
    }

    #[test]
    fn a_plain_click_leaves_no_selection() {
        let mut sel = SelectionState::new();
        sel.handle(&down(3, 0));
        sel.handle(&up(3, 0)); // released on the same cell, no drag
        assert!(sel.range().is_none());
        assert!(!sel.is_active());
    }

    #[test]
    fn a_new_press_clears_the_previous_selection() {
        let mut sel = SelectionState::new();
        sel.handle(&down(0, 0));
        sel.handle(&drag(3, 0));
        sel.handle(&up(3, 0));
        assert!(sel.range().is_some());
        // Pressing again to start a new gesture must drop the old selection.
        assert!(sel.handle(&down(1, 1)));
        assert!(sel.range().is_none());
    }

    #[test]
    fn selected_text_reads_one_row() {
        let theme = Theme::default();
        let buf = crate::testing::render(&Text::raw("hello world"), 11, 1, &theme);
        let mut sel = SelectionState::new();
        sel.handle(&down(0, 0));
        sel.handle(&drag(4, 0));
        sel.handle(&up(4, 0));
        let text = selected_text(&buf, buf.area, sel.range().unwrap());
        assert_eq!(text, "hello");
    }

    #[test]
    fn selected_text_spans_rows_linearly() {
        use ratatui::text::Line;
        let theme = Theme::default();
        let lines = vec![Line::from("hello"), Line::from("world")];
        let buf = crate::testing::render(&Text::new(lines), 5, 2, &theme);
        let mut sel = SelectionState::new();
        // From the "llo" of row 0 through the "wo" of row 1.
        sel.handle(&down(2, 0));
        sel.handle(&drag(1, 1));
        sel.handle(&up(1, 1));
        let text = selected_text(&buf, buf.area, sel.range().unwrap());
        assert_eq!(text, "llo\nwo");
    }

    #[test]
    fn selected_text_trims_trailing_blanks() {
        let theme = Theme::default();
        let buf = crate::testing::render(&Text::raw("hi"), 10, 1, &theme);
        let mut sel = SelectionState::new();
        sel.handle(&down(0, 0));
        sel.handle(&drag(9, 0)); // drag well past the text into blank cells
        sel.handle(&up(9, 0));
        let text = selected_text(&buf, buf.area, sel.range().unwrap());
        assert_eq!(text, "hi");
    }

    #[test]
    fn highlight_patches_selected_cells_only() {
        let theme = Theme::default();
        let mut buf = crate::testing::render(&Text::raw("hello"), 5, 1, &theme);
        let area = buf.area;
        let mut sel = SelectionState::new();
        sel.handle(&down(0, 0));
        sel.handle(&drag(2, 0));
        sel.handle(&up(2, 0));
        highlight(
            &mut buf,
            area,
            sel.range().unwrap(),
            Style::default().bg(Color::Blue),
        );
        // Selected cells (0..=2) get the highlight bg; the glyph survives.
        for col in 0..=2 {
            assert_eq!(
                buf[(col, 0)].bg,
                Color::Blue,
                "cell {col} should be highlighted"
            );
        }
        assert_eq!(
            buf[(0, 0)].symbol(),
            "h",
            "highlight must not clobber the glyph"
        );
        // An unselected cell is untouched.
        assert_ne!(buf[(4, 0)].bg, Color::Blue);
    }

    #[test]
    fn hit_map_last_region_wins_and_misses_return_none() {
        let mut hits: HitMap<&str> = HitMap::new();
        hits.push(Rect::new(0, 0, 10, 10), "background");
        hits.push(Rect::new(2, 2, 3, 3), "panel"); // pushed later -> wins on overlap
        hits.push(Rect::new(0, 0, 0, 0), "zero"); // zero-area never matches
        assert_eq!(hits.hit(3, 3), Some(&"panel"));
        assert_eq!(hits.hit(8, 8), Some(&"background"));
        assert_eq!(hits.hit(50, 50), None);
        // hit_event resolves an event's coordinates.
        assert_eq!(hits.hit_event(&down(3, 3)), Some(&"panel"));
    }

    #[test]
    fn click_tracker_detects_a_click() {
        let mut clicks = ClickTracker::new();
        assert!(clicks.handle(&down(4, 2)).is_none()); // press alone is not a click
        let click = clicks
            .handle(&up(4, 2))
            .expect("down+up on one cell is a click");
        assert_eq!(
            (click.column, click.row, click.button),
            (4, 2, MouseButton::Left)
        );
    }

    #[test]
    fn click_tracker_drag_cancels_the_click() {
        let mut clicks = ClickTracker::new();
        clicks.handle(&down(4, 2));
        clicks.handle(&drag(6, 2)); // moved -> this is a selection, not a click
        assert!(clicks.handle(&up(6, 2)).is_none());
    }

    #[test]
    fn click_tracker_release_on_another_cell_is_not_a_click() {
        let mut clicks = ClickTracker::new();
        clicks.handle(&down(4, 2));
        assert!(clicks.handle(&up(9, 9)).is_none());
    }
}

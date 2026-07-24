//! A multi-line text editor: buffer, cursor, editing, soft-wrap, and a screen
//! cursor position the host can hand to the terminal.
//!
//! Like the rest of tuika's interactive widgets, state and view are split:
//! [`TextInputState`] owns the text and cursor and applies edits (via
//! [`TextInputState::handle`] or the explicit methods); [`TextInput`] borrows it
//! and renders. The view is word-soft-wrapped to the render width — a logical
//! line longer than the area breaks at the last space that fits (hard-breaking a
//! word longer than the width), and a line that fills the width exactly wraps the
//! cursor onto a fresh row, like a real editor.
//!
//! It is a *rendering + edit model*, not a terminal: the host reads
//! [`TextInputState::cursor_screen`] after layout and calls the backend's
//! `set_cursor_position`. Hosts can configure whether Enter or Shift+Enter
//! submits via [`TextInputMode`]; the other chord inserts a newline.

use ratatui_core::layout::Rect;
use ratatui_core::style::Style;
use unicode_width::UnicodeWidthChar;

use crate::event::{Event, KeyCode};
use crate::geometry::Size;
use crate::surface::Surface;
use crate::view::{RenderCtx, View};

/// The editable text and cursor of a [`TextInput`].
#[derive(Clone, Debug)]
pub struct TextInputState {
    /// Logical lines (split on `\n`); always at least one (possibly empty).
    lines: Vec<String>,
    /// Cursor row (logical line index).
    row: usize,
    /// Cursor column (char index within `lines[row]`).
    col: usize,
    mode: TextInputMode,
}

/// Controls which Enter chord submits a text input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TextInputMode {
    /// Enter submits; Shift+Enter inserts a newline.
    #[default]
    SubmitOnEnter,
    /// Shift+Enter submits; Enter inserts a newline.
    SubmitOnShiftEnter,
}

/// Result of applying an Enter chord to a text input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextInputEvent {
    /// The chord inserted a newline; text changed.
    Changed,
    /// The chord requested submission.
    Submit,
}

impl Default for TextInputState {
    fn default() -> Self {
        Self::new()
    }
}

impl TextInputState {
    /// An empty single-line buffer with cursor at the start.
    pub fn new() -> Self {
        Self {
            lines: vec![String::new()],
            row: 0,
            col: 0,
            mode: TextInputMode::default(),
        }
    }

    /// Set how Enter and Shift+Enter submit or insert newlines.
    pub fn set_mode(&mut self, mode: TextInputMode) {
        self.mode = mode;
    }

    /// Return the current Enter behavior.
    pub fn mode(&self) -> TextInputMode {
        self.mode
    }

    /// Apply an Enter chord according to the configured mode.
    pub fn handle_enter(&mut self, shift: bool) -> TextInputEvent {
        let submit = match self.mode {
            TextInputMode::SubmitOnEnter => !shift,
            TextInputMode::SubmitOnShiftEnter => shift,
        };
        if submit {
            TextInputEvent::Submit
        } else {
            self.newline();
            TextInputEvent::Changed
        }
    }

    /// Seed from `text`, cursor at the end.
    pub fn from_text(text: &str) -> Self {
        let mut s = Self::new();
        s.set_text(text);
        s
    }

    /// The full text, logical lines joined with `\n`.
    pub fn text(&self) -> String {
        self.lines.join("\n")
    }

    /// Whether the buffer is a single empty line.
    pub fn is_empty(&self) -> bool {
        self.lines.len() == 1 && self.lines[0].is_empty()
    }

    /// Number of logical lines.
    pub fn line_count(&self) -> usize {
        self.lines.len()
    }

    /// Cursor as `(row, col)` in logical (char-index) coordinates.
    pub fn cursor(&self) -> (usize, usize) {
        (self.row, self.col)
    }

    /// Replace all text; cursor moves to the end.
    pub fn set_text(&mut self, text: &str) {
        self.lines = text.split('\n').map(str::to_string).collect();
        if self.lines.is_empty() {
            self.lines.push(String::new());
        }
        self.row = self.lines.len() - 1;
        self.col = self.lines[self.row].chars().count();
    }

    /// Clear to a single empty line.
    pub fn clear(&mut self) {
        *self = Self::new();
    }

    /// Move the cursor to `(row, col)`, clamped into the buffer. Lets a host
    /// mirror an external editor's cursor into this state for rendering.
    pub fn set_cursor(&mut self, row: usize, col: usize) {
        self.row = row.min(self.lines.len().saturating_sub(1));
        self.col = col.min(self.lines[self.row].chars().count());
    }

    fn row_chars(&self, row: usize) -> Vec<char> {
        self.lines[row].chars().collect()
    }

    fn set_row(&mut self, row: usize, chars: Vec<char>) {
        self.lines[row] = chars.into_iter().collect();
    }

    /// Insert one char at the cursor.
    pub fn insert_char(&mut self, ch: char) {
        if ch == '\n' {
            self.newline();
            return;
        }
        let mut chars = self.row_chars(self.row);
        let at = self.col.min(chars.len());
        chars.insert(at, ch);
        self.set_row(self.row, chars);
        self.col = at + 1;
    }

    /// Insert a string (honoring embedded newlines) at the cursor.
    pub fn insert_str(&mut self, s: &str) {
        for ch in s.chars() {
            self.insert_char(ch);
        }
    }

    /// Split the current line at the cursor into two lines.
    pub fn newline(&mut self) {
        let chars = self.row_chars(self.row);
        let at = self.col.min(chars.len());
        let tail: String = chars[at..].iter().collect();
        let head: String = chars[..at].iter().collect();
        self.lines[self.row] = head;
        self.lines.insert(self.row + 1, tail);
        self.row += 1;
        self.col = 0;
    }

    /// Delete the char before the cursor (joining lines at column 0).
    pub fn backspace(&mut self) {
        if self.col > 0 {
            let mut chars = self.row_chars(self.row);
            chars.remove(self.col - 1);
            self.col -= 1;
            self.set_row(self.row, chars);
        } else if self.row > 0 {
            let cur = self.lines.remove(self.row);
            self.row -= 1;
            self.col = self.lines[self.row].chars().count();
            self.lines[self.row].push_str(&cur);
        }
    }

    /// Delete the char at the cursor (joining the next line at line end).
    pub fn delete(&mut self) {
        let mut chars = self.row_chars(self.row);
        if self.col < chars.len() {
            chars.remove(self.col);
            self.set_row(self.row, chars);
        } else if self.row + 1 < self.lines.len() {
            let next = self.lines.remove(self.row + 1);
            self.lines[self.row].push_str(&next);
        }
    }

    fn clamp_col(&mut self) {
        self.col = self.col.min(self.lines[self.row].chars().count());
    }

    /// Move one char left, wrapping to the end of the previous line.
    pub fn move_left(&mut self) {
        if self.col > 0 {
            self.col -= 1;
        } else if self.row > 0 {
            self.row -= 1;
            self.col = self.lines[self.row].chars().count();
        }
    }

    /// Move one char right, wrapping to the start of the next line.
    pub fn move_right(&mut self) {
        let len = self.lines[self.row].chars().count();
        if self.col < len {
            self.col += 1;
        } else if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.col = 0;
        }
    }

    /// Move to the previous line (clamping column), or to line start at the top.
    pub fn move_up(&mut self) {
        if self.row > 0 {
            self.row -= 1;
            self.clamp_col();
        } else {
            self.col = 0;
        }
    }

    /// Move to the next line (clamping column), or to line end at the bottom.
    pub fn move_down(&mut self) {
        if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.clamp_col();
        } else {
            self.col = self.lines[self.row].chars().count();
        }
    }

    /// Move the cursor to the start of the current line.
    pub fn move_home(&mut self) {
        self.col = 0;
    }

    /// Move the cursor to the end of the current line.
    pub fn move_end(&mut self) {
        self.col = self.lines[self.row].chars().count();
    }

    /// The column of the previous word start on the current line: skip trailing
    /// whitespace, then the word itself. Used by word-move and word-delete.
    fn prev_word_col(&self) -> usize {
        let chars = self.row_chars(self.row);
        let mut i = self.col.min(chars.len());
        while i > 0 && chars[i - 1].is_whitespace() {
            i -= 1;
        }
        while i > 0 && !chars[i - 1].is_whitespace() {
            i -= 1;
        }
        i
    }

    /// The column of the next word end on the current line: skip leading
    /// whitespace, then the word itself.
    fn next_word_col(&self) -> usize {
        let chars = self.row_chars(self.row);
        let len = chars.len();
        let mut i = self.col.min(len);
        while i < len && chars[i].is_whitespace() {
            i += 1;
        }
        while i < len && !chars[i].is_whitespace() {
            i += 1;
        }
        i
    }

    /// Move to the previous word boundary, crossing to the prior line at col 0.
    pub fn move_word_left(&mut self) {
        if self.col == 0 {
            self.move_left();
            return;
        }
        self.col = self.prev_word_col();
    }

    /// Move to the next word boundary, crossing to the next line at line end.
    pub fn move_word_right(&mut self) {
        if self.col >= self.lines[self.row].chars().count() {
            self.move_right();
            return;
        }
        self.col = self.next_word_col();
    }

    /// Delete from the cursor back to the previous word boundary (joins the
    /// prior line when already at col 0).
    pub fn delete_word_left(&mut self) {
        if self.col == 0 {
            self.backspace();
            return;
        }
        let start = self.prev_word_col();
        let mut chars = self.row_chars(self.row);
        chars.drain(start..self.col);
        self.col = start;
        self.set_row(self.row, chars);
    }

    /// Delete from the cursor forward to the next word boundary (joins the next
    /// line when already at line end).
    pub fn delete_word_right(&mut self) {
        let len = self.lines[self.row].chars().count();
        if self.col >= len {
            self.delete();
            return;
        }
        let end = self.next_word_col();
        let mut chars = self.row_chars(self.row);
        chars.drain(self.col..end);
        self.set_row(self.row, chars);
    }

    /// Delete from the cursor to the end of the line; at line end, joins the
    /// next line (emacs `C-k`).
    pub fn kill_to_line_end(&mut self) {
        let mut chars = self.row_chars(self.row);
        if self.col < chars.len() {
            chars.truncate(self.col);
            self.set_row(self.row, chars);
        } else {
            self.delete();
        }
    }

    /// Delete from the start of the line to the cursor (emacs `C-u`).
    pub fn kill_to_line_start(&mut self) {
        let chars = self.row_chars(self.row);
        let tail: Vec<char> = chars[self.col.min(chars.len())..].to_vec();
        self.set_row(self.row, tail);
        self.col = 0;
    }

    /// Apply an input event. Returns `true` when the buffer or cursor changed.
    /// Enter inserts a newline; a host that submits on Enter should intercept it
    /// before calling this.
    ///
    /// Beyond the plain keys, an emacs-style keymap covers the readline bindings
    /// a terminal composer is expected to honor (so the widget matches what
    /// `ratatui-textarea` gave hosts before): `C-a`/`C-e` line start/end,
    /// `C-f`/`C-b` char move, `C-p`/`C-n` line move, `C-h`/`C-d` delete,
    /// `C-k`/`C-u` kill to line end/start, `C-w`/`M-Backspace` delete word back,
    /// `M-f`/`M-b` word move, `M-d` delete word forward.
    pub fn handle(&mut self, event: &Event) -> bool {
        match event {
            Event::Key(k) if k.ctrl && !k.alt => match k.code {
                KeyCode::Char('a') => {
                    self.move_home();
                    true
                }
                KeyCode::Char('e') => {
                    self.move_end();
                    true
                }
                KeyCode::Char('f') => {
                    self.move_right();
                    true
                }
                KeyCode::Char('b') => {
                    self.move_left();
                    true
                }
                KeyCode::Char('p') => {
                    self.move_up();
                    true
                }
                KeyCode::Char('n') => {
                    self.move_down();
                    true
                }
                KeyCode::Char('h') => {
                    self.backspace();
                    true
                }
                KeyCode::Char('d') => {
                    self.delete();
                    true
                }
                KeyCode::Char('k') => {
                    self.kill_to_line_end();
                    true
                }
                KeyCode::Char('u') => {
                    self.kill_to_line_start();
                    true
                }
                KeyCode::Char('w') => {
                    self.delete_word_left();
                    true
                }
                _ => false,
            },
            Event::Key(k) if k.alt && !k.ctrl => match k.code {
                KeyCode::Char('f') => {
                    self.move_word_right();
                    true
                }
                KeyCode::Char('b') => {
                    self.move_word_left();
                    true
                }
                KeyCode::Char('d') => {
                    self.delete_word_right();
                    true
                }
                KeyCode::Backspace => {
                    self.delete_word_left();
                    true
                }
                _ => false,
            },
            Event::Key(k) if !k.ctrl && !k.alt => match k.code {
                KeyCode::Char(c) => {
                    self.insert_char(c);
                    true
                }
                KeyCode::Enter => {
                    self.newline();
                    true
                }
                KeyCode::Backspace => {
                    self.backspace();
                    true
                }
                KeyCode::Delete => {
                    self.delete();
                    true
                }
                KeyCode::Left => {
                    self.move_left();
                    true
                }
                KeyCode::Right => {
                    self.move_right();
                    true
                }
                KeyCode::Up => {
                    self.move_up();
                    true
                }
                KeyCode::Down => {
                    self.move_down();
                    true
                }
                KeyCode::Home => {
                    self.move_home();
                    true
                }
                KeyCode::End => {
                    self.move_end();
                    true
                }
                _ => false,
            },
            Event::Paste(text) => {
                self.insert_str(text);
                true
            }
            _ => false,
        }
    }

    /// Number of visual rows the text occupies at `width`.
    pub fn visual_height(&self, width: u16) -> u16 {
        wrap_visual_rows(&self.lines, width).len().max(1) as u16
    }

    /// The cursor's visual `(row, col)` in wrapped coordinates at `width`.
    fn visual_cursor(&self, width: u16) -> (u16, u16) {
        visual_cursor_at(&self.lines, self.row, self.col, width)
    }

    /// The visual-row scroll offset that keeps the cursor visible in a
    /// `height`-row viewport at `width`: once the text is taller than the
    /// viewport the cursor rests on the last visible row; otherwise 0. A bounded
    /// composer (see [`TextInput`]) renders and places its cursor through this.
    pub fn scroll_offset(&self, width: u16, height: u16) -> u16 {
        self.visual_cursor(width)
            .0
            .saturating_sub(height.saturating_sub(1))
    }

    /// The cursor cell in terminal coordinates, given the rendered `area`,
    /// accounting for scroll-to-cursor when the text is taller than the area.
    pub fn cursor_screen(&self, area: Rect) -> (u16, u16) {
        let (vrow, vcol) = self.visual_cursor(area.width);
        let offset = vrow.saturating_sub(area.height.saturating_sub(1));
        let x = area
            .x
            .saturating_add(vcol.min(area.width.saturating_sub(1)));
        let y = area
            .y
            .saturating_add((vrow - offset).min(area.height.saturating_sub(1)));
        (x, y)
    }
}

/// One wrapped visual row: which logical line it came from, the char index in
/// that line where it starts, and its chars.
struct VisualRow {
    logical: usize,
    start: usize,
    chars: Vec<char>,
}

/// The cursor's visual `(row, col)` for `lines` with the logical cursor at
/// `(row, col)`, word-soft-wrapped to `width`. Reuses [`wrap_visual_rows`] so the
/// rendered scroll offset and the placed cursor always agree with what's drawn.
fn visual_cursor_at(lines: &[String], row: usize, col: usize, width: u16) -> (u16, u16) {
    let rows = wrap_visual_rows(lines, width);
    let mut last_on_line: Option<(usize, usize)> = None; // (visual index, start col)
    for (vi, vr) in rows.iter().enumerate() {
        if vr.logical > row {
            break;
        }
        if vr.logical != row {
            continue;
        }
        let end = vr.start + vr.chars.len();
        last_on_line = Some((vi, vr.start));
        // A col at this row's end belongs to the next row's start, so only claim
        // it here when it falls strictly inside — except the line's last row,
        // handled below.
        if col >= vr.start && col < end {
            return (vi as u16, (col - vr.start) as u16);
        }
    }
    // Cursor at the end of the logical line: rest at the end of its last row
    // (which is an empty trailing row when the text filled the width exactly).
    if let Some((vi, start)) = last_on_line {
        return (vi as u16, col.saturating_sub(start) as u16);
    }
    (rows.len().saturating_sub(1) as u16, 0)
}

/// Word-soft-wrap `lines` to `width`: each logical line breaks at the last space
/// that fits, falling back to a hard char-break for a word longer than `width`.
/// A line whose final row fills the width exactly emits a trailing empty row so
/// the cursor can rest on a fresh line. Shared by [`visual_cursor_at`] (cursor
/// math) and [`TextInput`] (rendering) so both wrap identically.
fn wrap_visual_rows(lines: &[String], width: u16) -> Vec<VisualRow> {
    let width = width.max(1) as usize;
    let mut rows = Vec::new();
    for (r, line) in lines.iter().enumerate() {
        let chars: Vec<char> = line.chars().collect();
        if chars.is_empty() {
            rows.push(VisualRow {
                logical: r,
                start: 0,
                chars: Vec::new(),
            });
            continue;
        }
        let mut start = 0;
        let mut last_filled = false;
        while start < chars.len() {
            let remaining = chars.len() - start;
            let end = if remaining <= width {
                chars.len()
            } else {
                // Break after the last space within the width window; if the
                // window holds no space, hard-break at the width boundary.
                let hard = start + width;
                let brk = (start + 1..hard).rev().find(|&i| chars[i] == ' ');
                brk.map(|i| i + 1).unwrap_or(hard)
            };
            last_filled = end - start == width;
            rows.push(VisualRow {
                logical: r,
                start,
                chars: chars[start..end].to_vec(),
            });
            start = end;
        }
        if last_filled {
            rows.push(VisualRow {
                logical: r,
                start: chars.len(),
                chars: Vec::new(),
            });
        }
    }
    rows
}

/// Renders a snapshot of a [`TextInputState`]'s wrapped text.
///
/// Owns its lines (cloned from the state at construction, like [`Scroll`]) so it
/// is `'static` and composes into a [`view!`](crate::view!) tree. When the text
/// is taller than the render area it **scrolls to the cursor** (the cursor's
/// visual row stays on screen), so it backs a bounded composer without losing the
/// caret. The host places the terminal cursor through
/// [`TextInputState::cursor_screen`], which derives the same offset.
///
/// [`Scroll`]: crate::Scroll
///
/// ![textinput demo](https://raw.githubusercontent.com/everruns/yolop/main/crates/tuika/docs/demos/textinput.gif)
pub struct TextInput {
    lines: Vec<String>,
    /// Logical cursor `(row, col)`, so a bounded render can scroll to it.
    cursor: (usize, usize),
    style: Style,
}

impl TextInput {
    /// Snapshot `state`'s text and cursor for rendering.
    pub fn new(state: &TextInputState) -> Self {
        Self {
            lines: state.lines.clone(),
            cursor: (state.row, state.col),
            style: Style::default(),
        }
    }

    /// Set the text style applied to rendered glyphs.
    pub fn style(mut self, style: Style) -> Self {
        self.style = style;
        self
    }

    /// Visual-row offset that keeps the cursor on screen in `height` rows.
    fn scroll_offset(&self, width: u16, height: u16) -> u16 {
        visual_cursor_at(&self.lines, self.cursor.0, self.cursor.1, width)
            .0
            .saturating_sub(height.saturating_sub(1))
    }
}

impl View for TextInput {
    fn measure(&self, available: Size) -> Size {
        let height = wrap_visual_rows(&self.lines, available.width).len().max(1) as u16;
        Size::new(available.width, height)
    }

    fn render(&self, area: Rect, surface: &mut Surface, _ctx: &RenderCtx) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let offset = self.scroll_offset(area.width, area.height) as usize;
        for (i, vr) in wrap_visual_rows(&self.lines, area.width)
            .into_iter()
            .enumerate()
            .skip(offset)
        {
            let y = area.y.saturating_add((i - offset) as u16);
            if y >= area.bottom() {
                break;
            }
            let mut x = area.x;
            for ch in vr.chars {
                let w = UnicodeWidthChar::width(ch).unwrap_or(0) as u16;
                if w == 0 || x >= area.right() {
                    continue;
                }
                surface.set(x, y, ch, self.style);
                x = x.saturating_add(w);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{Event, Key, KeyCode};
    use crate::test_support::{render_el, render_view_rows};
    use crate::view::element;
    use ratatui_core::layout::Rect;

    fn press(state: &mut TextInputState, code: KeyCode) -> bool {
        state.handle(&Event::Key(Key::new(code)))
    }

    fn press_ctrl(state: &mut TextInputState, code: KeyCode) -> bool {
        state.handle(&Event::Key(Key {
            code,
            ctrl: true,
            alt: false,
            shift: false,
        }))
    }

    fn press_alt(state: &mut TextInputState, code: KeyCode) -> bool {
        state.handle(&Event::Key(Key {
            code,
            ctrl: false,
            alt: true,
            shift: false,
        }))
    }

    fn type_str(state: &mut TextInputState, s: &str) {
        for ch in s.chars() {
            assert!(press(state, KeyCode::Char(ch)));
        }
    }

    #[test]
    fn text_input_starts_empty() {
        let state = TextInputState::new();
        assert!(state.is_empty());
        assert_eq!(state.text(), "");
        assert_eq!(state.cursor(), (0, 0));
        assert_eq!(state.line_count(), 1);
    }

    #[test]
    fn text_input_types_and_edits() {
        let mut state = TextInputState::new();
        type_str(&mut state, "helo");
        assert_eq!(state.text(), "helo");
        assert_eq!(state.cursor(), (0, 4));

        // Move back and insert the missing 'l' → "hello".
        press(&mut state, KeyCode::Left);
        press(&mut state, KeyCode::Left);
        assert_eq!(state.cursor(), (0, 2));
        assert!(press(&mut state, KeyCode::Char('l')));
        assert_eq!(state.text(), "hello");
        assert_eq!(state.cursor(), (0, 3));
    }

    #[test]
    fn text_input_backspace_and_delete() {
        let mut state = TextInputState::from_text("abc");
        assert_eq!(state.cursor(), (0, 3));
        press(&mut state, KeyCode::Backspace);
        assert_eq!(state.text(), "ab");
        press(&mut state, KeyCode::Home);
        press(&mut state, KeyCode::Delete);
        assert_eq!(state.text(), "b");
        assert_eq!(state.cursor(), (0, 0));
    }

    #[test]
    fn text_input_newline_splits_and_backspace_joins() {
        let mut state = TextInputState::from_text("abcd");
        press(&mut state, KeyCode::Home);
        press(&mut state, KeyCode::Right);
        press(&mut state, KeyCode::Right);
        assert_eq!(state.cursor(), (0, 2));
        press(&mut state, KeyCode::Enter);
        assert_eq!(state.text(), "ab\ncd");
        assert_eq!(state.line_count(), 2);
        assert_eq!(state.cursor(), (1, 0));

        // Backspace at column 0 rejoins the two logical lines.
        press(&mut state, KeyCode::Backspace);
        assert_eq!(state.text(), "abcd");
        assert_eq!(state.cursor(), (0, 2));
        assert_eq!(state.line_count(), 1);
    }

    #[test]
    fn text_input_vertical_movement_clamps_column() {
        let mut state = TextInputState::from_text("longline\nhi");
        // Cursor is at end of "hi" (row 1, col 2). Move up onto the longer line:
        // column is preserved (2), not clamped, because "longline" is longer.
        press(&mut state, KeyCode::Up);
        assert_eq!(state.cursor(), (0, 2));
        // From end of "longline" move down — clamps onto the shorter "hi".
        press(&mut state, KeyCode::End);
        assert_eq!(state.cursor(), (0, 8));
        press(&mut state, KeyCode::Down);
        assert_eq!(state.cursor(), (1, 2));
    }

    #[test]
    fn text_input_paste_inserts_multiline() {
        let mut state = TextInputState::new();
        assert!(state.handle(&Event::Paste("one\ntwo".to_string())));
        assert_eq!(state.text(), "one\ntwo");
        assert_eq!(state.line_count(), 2);
        assert_eq!(state.cursor(), (1, 3));
    }

    #[test]
    fn text_input_unbound_ctrl_keys_ignored() {
        // A ctrl combo with no binding (C-z) is a no-op the host can repurpose.
        let mut state = TextInputState::from_text("x");
        assert!(!press_ctrl(&mut state, KeyCode::Char('z')));
        assert_eq!(state.text(), "x");
    }

    #[test]
    fn text_input_emacs_cursor_bindings() {
        let mut state = TextInputState::from_text("hello");
        // C-a → line start, C-e → line end, C-f/C-b → char right/left.
        assert!(press_ctrl(&mut state, KeyCode::Char('a')));
        assert_eq!(state.cursor(), (0, 0));
        assert!(press_ctrl(&mut state, KeyCode::Char('f')));
        assert_eq!(state.cursor(), (0, 1));
        assert!(press_ctrl(&mut state, KeyCode::Char('e')));
        assert_eq!(state.cursor(), (0, 5));
        assert!(press_ctrl(&mut state, KeyCode::Char('b')));
        assert_eq!(state.cursor(), (0, 4));

        // C-p / C-n move between logical lines.
        state = TextInputState::from_text("ab\ncd");
        press(&mut state, KeyCode::Home);
        assert!(press_ctrl(&mut state, KeyCode::Char('p')));
        assert_eq!(state.cursor(), (0, 0));
        assert!(press_ctrl(&mut state, KeyCode::Char('n')));
        assert_eq!(state.cursor(), (1, 0));
    }

    #[test]
    fn text_input_emacs_delete_bindings() {
        // C-h backspaces, C-d deletes forward.
        let mut state = TextInputState::from_text("abc");
        assert!(press_ctrl(&mut state, KeyCode::Char('h')));
        assert_eq!(state.text(), "ab");
        press(&mut state, KeyCode::Home);
        assert!(press_ctrl(&mut state, KeyCode::Char('d')));
        assert_eq!(state.text(), "b");
    }

    #[test]
    fn text_input_kill_to_line_end_and_start() {
        // C-k kills from the cursor to end of line.
        let mut state = TextInputState::from_text("hello world");
        press(&mut state, KeyCode::Home);
        press(&mut state, KeyCode::Right);
        press(&mut state, KeyCode::Right);
        press(&mut state, KeyCode::Right);
        press(&mut state, KeyCode::Right);
        press(&mut state, KeyCode::Right); // cursor after "hello"
        assert!(press_ctrl(&mut state, KeyCode::Char('k')));
        assert_eq!(state.text(), "hello");
        assert_eq!(state.cursor(), (0, 5));

        // C-k at line end joins the next line.
        let mut state = TextInputState::from_text("ab\ncd");
        press(&mut state, KeyCode::Home);
        press(&mut state, KeyCode::Up);
        press(&mut state, KeyCode::End);
        assert!(press_ctrl(&mut state, KeyCode::Char('k')));
        assert_eq!(state.text(), "abcd");

        // C-u kills from line start to the cursor.
        let mut state = TextInputState::from_text("hello world");
        assert!(press_ctrl(&mut state, KeyCode::Char('u')));
        assert_eq!(state.text(), "");
        assert_eq!(state.cursor(), (0, 0));
    }

    #[test]
    fn text_input_word_move_and_delete() {
        // M-b / M-f jump by word; C-w / M-d delete a word back / forward.
        let mut state = TextInputState::from_text("foo bar baz");
        assert!(press_alt(&mut state, KeyCode::Char('b')));
        assert_eq!(state.cursor(), (0, 8)); // start of "baz"
        assert!(press_alt(&mut state, KeyCode::Char('b')));
        assert_eq!(state.cursor(), (0, 4)); // start of "bar"

        // C-w at the start of "bar" deletes the previous word "foo ".
        assert!(press_ctrl(&mut state, KeyCode::Char('w')));
        assert_eq!(state.text(), "bar baz");
        assert_eq!(state.cursor(), (0, 0));

        // M-f to the end of "bar", then M-d deletes the next word " baz".
        assert!(press_alt(&mut state, KeyCode::Char('f')));
        assert_eq!(state.cursor(), (0, 3)); // end of "bar"
        assert!(press_alt(&mut state, KeyCode::Char('d')));
        assert_eq!(state.text(), "bar");

        // M-Backspace deletes the previous word too.
        let mut state = TextInputState::from_text("alpha beta");
        assert!(press_alt(&mut state, KeyCode::Backspace));
        assert_eq!(state.text(), "alpha ");
    }

    #[test]
    fn text_input_scrolls_to_cursor_when_taller_than_area() {
        // 10 single-row lines; cursor at the end (line 9).
        let mut state = TextInputState::new();
        for i in 0..10 {
            type_str(&mut state, &format!("line{i}"));
            if i < 9 {
                state.newline();
            }
        }
        // A 3-row viewport shows the last three rows (containing the cursor), not the
        // top — the composer scrolls to the caret.
        let out = render_view_rows(&TextInput::new(&state), 10, 3);
        assert_eq!(out, vec!["line7", "line8", "line9"]);
        // The placed cursor sits on the last visible row, consistent with the scroll.
        let (_, y) = state.cursor_screen(Rect::new(0, 0, 10, 3));
        assert_eq!(y, 2);
        // Move to the top: the viewport follows the cursor back up.
        for _ in 0..9 {
            state.move_up();
        }
        let out = render_view_rows(&TextInput::new(&state), 10, 3);
        assert_eq!(out, vec!["line0", "line1", "line2"]);
        assert_eq!(state.cursor_screen(Rect::new(0, 0, 10, 3)).1, 0);
    }

    #[test]
    fn text_input_renders_wrapped_rows() {
        let mut state = TextInputState::new();
        type_str(&mut state, "abcdef");
        // Width 4 wraps "abcdef" onto two visual rows: "abcd" / "ef".
        assert_eq!(state.visual_height(4), 2);
        let out = render_view_rows(&TextInput::new(&state), 4, 2);
        assert_eq!(out[0], "abcd");
        assert_eq!(out[1], "ef");
    }

    #[test]
    fn text_input_word_wraps_at_spaces() {
        let mut state = TextInputState::new();
        type_str(&mut state, "hello world foo");
        // Width 8 breaks at the last space that fits, not mid-word: "hello " then
        // "world " then "foo".
        assert_eq!(state.visual_height(8), 3);
        let out = render_view_rows(&TextInput::new(&state), 8, 3);
        assert_eq!(out[0], "hello");
        assert_eq!(out[1], "world");
        assert_eq!(out[2], "foo");
    }

    #[test]
    fn text_input_hard_breaks_overlong_word() {
        let mut state = TextInputState::new();
        type_str(&mut state, "abcdefghij");
        // A single word longer than the width still hard-breaks so it fits.
        assert_eq!(state.visual_height(4), 3);
        let out = render_view_rows(&TextInput::new(&state), 4, 3);
        assert_eq!(out[0], "abcd");
        assert_eq!(out[1], "efgh");
        assert_eq!(out[2], "ij");
    }

    #[test]
    fn text_input_cursor_tracks_word_wrap() {
        let mut state = TextInputState::from_text("hello world");
        // Cursor at end (col 11). Width 8 → "hello " / "world"; cursor sits after
        // "world" on the second visual row at col 5.
        let area = Rect::new(0, 0, 8, 3);
        assert_eq!(state.cursor_screen(area), (5, 1));
        // Move to just after the space (col 6, start of "world") → row 1, col 0.
        state.move_home();
        for _ in 0..6 {
            state.move_right();
        }
        assert_eq!(state.cursor_screen(area), (0, 1));
    }

    #[test]
    fn text_input_cursor_screen_follows_wrap() {
        let mut state = TextInputState::new();
        type_str(&mut state, "abcd");
        // At width 4 the line fills exactly, so the cursor rests on a fresh row.
        assert_eq!(state.visual_height(4), 2);
        let area = Rect::new(2, 1, 4, 3);
        // Cursor after "abcd" → visual row 1, col 0, offset by area origin.
        assert_eq!(state.cursor_screen(area), (2, 2));
        // Move home → back to the first visual row.
        press(&mut state, KeyCode::Home);
        assert_eq!(state.cursor_screen(area), (2, 1));
    }

    #[test]
    fn text_input_set_and_clear() {
        let mut state = TextInputState::new();
        state.set_text("hello\nworld");
        assert_eq!(state.cursor(), (1, 5));
        assert_eq!(state.line_count(), 2);
        state.clear();
        assert!(state.is_empty());
        assert_eq!(state.cursor(), (0, 0));
    }

    #[test]
    fn text_input_set_cursor_clamps() {
        let mut state = TextInputState::from_text("hi\nthere");
        state.set_cursor(0, 1);
        assert_eq!(state.cursor(), (0, 1));
        // Row past the end clamps to the last line; col past its end clamps too.
        state.set_cursor(9, 9);
        assert_eq!(state.cursor(), (1, 5));
    }

    #[test]
    fn text_input_composes_into_view_tree() {
        // Owning its snapshot makes TextInput `'static`, so it splices into a
        // `view!` tree via `element(...)` — the property the fullscreen composer
        // relies on.
        let mut state = TextInputState::new();
        type_str(&mut state, "hi");
        let tree = element(TextInput::new(&state));
        let out = render_el(&tree, 4, 1);
        assert_eq!(out[0], "hi");
    }

    #[test]
    fn submit_on_enter_mode_uses_shift_enter_for_newline() {
        let mut state = TextInputState::new();
        assert_eq!(state.mode(), TextInputMode::SubmitOnEnter);
        assert_eq!(state.handle_enter(false), TextInputEvent::Submit);
        assert_eq!(state.handle_enter(true), TextInputEvent::Changed);
        assert_eq!(state.text(), "\n");
    }

    #[test]
    fn submit_on_shift_enter_mode_reverses_enter_chords() {
        let mut state = TextInputState::new();
        state.set_mode(TextInputMode::SubmitOnShiftEnter);
        assert_eq!(state.handle_enter(false), TextInputEvent::Changed);
        assert_eq!(state.handle_enter(true), TextInputEvent::Submit);
        assert_eq!(state.text(), "\n");
    }
}

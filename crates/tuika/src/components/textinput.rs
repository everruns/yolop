//! A multi-line text editor: buffer, cursor, editing, soft-wrap, and a screen
//! cursor position the host can hand to the terminal.
//!
//! Like the rest of tuika's interactive widgets, state and view are split:
//! [`TextInputState`] owns the text and cursor and applies edits (via
//! [`TextInputState::handle`] or the explicit methods); [`TextInput`] borrows it
//! and renders. The view is char-soft-wrapped to the render width — a logical
//! line longer than the area wraps onto extra visual rows, and a line that fills
//! the width exactly wraps the cursor onto a fresh row, like a real editor.
//!
//! It is a *rendering + edit model*, not a terminal: the host reads
//! [`TextInputState::cursor_screen`] after layout and calls the backend's
//! `set_cursor_position`, and decides what a submit means (this widget treats
//! Enter as a newline; a chat composer maps its own submit key before feeding
//! events here).

use ratatui::layout::Rect;
use ratatui::style::Style;
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
}

impl Default for TextInputState {
    fn default() -> Self {
        Self::new()
    }
}

impl TextInputState {
    pub fn new() -> Self {
        Self {
            lines: vec![String::new()],
            row: 0,
            col: 0,
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

    pub fn move_left(&mut self) {
        if self.col > 0 {
            self.col -= 1;
        } else if self.row > 0 {
            self.row -= 1;
            self.col = self.lines[self.row].chars().count();
        }
    }

    pub fn move_right(&mut self) {
        let len = self.lines[self.row].chars().count();
        if self.col < len {
            self.col += 1;
        } else if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.col = 0;
        }
    }

    pub fn move_up(&mut self) {
        if self.row > 0 {
            self.row -= 1;
            self.clamp_col();
        } else {
            self.col = 0;
        }
    }

    pub fn move_down(&mut self) {
        if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.clamp_col();
        } else {
            self.col = self.lines[self.row].chars().count();
        }
    }

    pub fn move_home(&mut self) {
        self.col = 0;
    }

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

    /// Char-soft-wrap every logical line to `width`, returning the visual rows as
    /// `(logical_row, chars)`. A line that fills the width exactly emits a
    /// trailing empty row so the cursor can rest on a fresh line.
    fn visual_rows(&self, width: u16) -> Vec<(usize, Vec<char>)> {
        wrap_visual_rows(&self.lines, width)
    }

    /// Number of visual rows the text occupies at `width`.
    pub fn visual_height(&self, width: u16) -> u16 {
        self.visual_rows(width).len().max(1) as u16
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

/// The cursor's visual `(row, col)` for `lines` with the logical cursor at
/// `(row, col)`, char-soft-wrapped to `width`. Shared by [`TextInputState`] and
/// [`TextInput`] so the rendered scroll offset and the placed cursor agree.
fn visual_cursor_at(lines: &[String], row: usize, col: usize, width: u16) -> (u16, u16) {
    let width = width.max(1) as usize;
    let mut vrow: usize = 0;
    for (r, line) in lines.iter().enumerate() {
        let len = line.chars().count();
        if r == row {
            return ((vrow + col / width) as u16, (col % width) as u16);
        }
        vrow += (len / width) + 1; // rows for this line (incl. trailing/empty)
    }
    (vrow as u16, 0)
}

/// Char-soft-wrap `lines` to `width`, returning visual rows as
/// `(logical_row, chars)`. A line that fills the width exactly emits a trailing
/// empty row so the cursor can rest on a fresh line. Shared by [`TextInputState`]
/// (cursor math) and [`TextInput`] (rendering).
fn wrap_visual_rows(lines: &[String], width: u16) -> Vec<(usize, Vec<char>)> {
    let width = width.max(1) as usize;
    let mut rows = Vec::new();
    for (r, line) in lines.iter().enumerate() {
        let chars: Vec<char> = line.chars().collect();
        if chars.is_empty() {
            rows.push((r, Vec::new()));
            continue;
        }
        let mut start = 0;
        while start < chars.len() {
            let end = (start + width).min(chars.len());
            rows.push((r, chars[start..end].to_vec()));
            start = end;
        }
        if chars.len().is_multiple_of(width) {
            rows.push((r, Vec::new()));
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
    pub fn new(state: &TextInputState) -> Self {
        Self {
            lines: state.lines.clone(),
            cursor: (state.row, state.col),
            style: Style::default(),
        }
    }

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
        for (i, (_logical, chars)) in wrap_visual_rows(&self.lines, area.width)
            .into_iter()
            .enumerate()
            .skip(offset)
        {
            let y = area.y.saturating_add((i - offset) as u16);
            if y >= area.bottom() {
                break;
            }
            let mut x = area.x;
            for ch in chars {
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

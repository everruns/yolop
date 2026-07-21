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

    /// Apply an input event. Returns `true` when the buffer or cursor changed.
    /// Enter inserts a newline; a host that submits on Enter should intercept it
    /// before calling this.
    pub fn handle(&mut self, event: &Event) -> bool {
        match event {
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
        let width = width.max(1) as usize;
        let mut vrow: usize = 0;
        for (r, line) in self.lines.iter().enumerate() {
            let len = line.chars().count();
            if r == self.row {
                let vr = self.col / width;
                let vc = self.col % width;
                return ((vrow + vr) as u16, vc as u16);
            }
            vrow += (len / width) + 1; // rows for this line (incl. trailing/empty)
        }
        (vrow as u16, 0)
    }

    /// The cursor cell in terminal coordinates, given the rendered `area`.
    /// Clamped inside `area`.
    pub fn cursor_screen(&self, area: Rect) -> (u16, u16) {
        let (vrow, vcol) = self.visual_cursor(area.width);
        let x = area
            .x
            .saturating_add(vcol.min(area.width.saturating_sub(1)));
        let y = area
            .y
            .saturating_add(vrow.min(area.height.saturating_sub(1)));
        (x, y)
    }
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
/// is `'static` and composes into a [`view!`](crate::view!) tree. The host places
/// the terminal cursor separately via [`TextInputState::cursor_screen`].
///
/// [`Scroll`]: crate::Scroll
///
/// ![textinput demo](https://raw.githubusercontent.com/everruns/yolop/main/crates/tuika/docs/demos/textinput.gif)
pub struct TextInput {
    lines: Vec<String>,
    style: Style,
}

impl TextInput {
    pub fn new(state: &TextInputState) -> Self {
        Self {
            lines: state.lines.clone(),
            style: Style::default(),
        }
    }

    pub fn style(mut self, style: Style) -> Self {
        self.style = style;
        self
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
        for (vrow, (_logical, chars)) in wrap_visual_rows(&self.lines, area.width)
            .into_iter()
            .enumerate()
        {
            let y = area.y.saturating_add(vrow as u16);
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

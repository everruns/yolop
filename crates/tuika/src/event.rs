//! Input events, decoupled from crossterm.
//!
//! Components handle `tuika` events, not raw crossterm events, so the widget
//! layer stays testable without a terminal. The host ([`super::host`])
//! translates crossterm into these. This mirrors Codex's event-stream input
//! model: a small, explicit event enum flowing to focused components.

/// A translated input event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// A key press.
    Key(Key),
    /// A mouse action.
    Mouse(Mouse),
    /// Bracketed-paste payload.
    Paste(String),
    /// Terminal resized to the given cell dimensions.
    Resize {
        /// New width in cells.
        width: u16,
        /// New height in cells.
        height: u16,
    },
}

/// A key press with its modifier state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Key {
    /// The key that was pressed.
    pub code: KeyCode,
    /// Ctrl held during the press.
    pub ctrl: bool,
    /// Alt held during the press.
    pub alt: bool,
    /// Shift held during the press.
    pub shift: bool,
}

impl Key {
    /// A key with the given code and no modifiers held.
    pub fn new(code: KeyCode) -> Self {
        Self {
            code,
            ctrl: false,
            alt: false,
            shift: false,
        }
    }

    /// True when neither Ctrl nor Alt is held (Shift is ignored, as it is
    /// already folded into the character for text input).
    pub fn plain(&self) -> bool {
        !self.ctrl && !self.alt
    }
}

/// A physical/logical key, independent of modifier state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyCode {
    /// A printable character.
    Char(char),
    /// Enter/Return.
    Enter,
    /// Escape.
    Esc,
    /// Backspace (delete before cursor).
    Backspace,
    /// Delete (delete under/after cursor).
    Delete,
    /// Tab.
    Tab,
    /// Back-tab (Shift-Tab).
    BackTab,
    /// Up arrow.
    Up,
    /// Down arrow.
    Down,
    /// Left arrow.
    Left,
    /// Right arrow.
    Right,
    /// Home.
    Home,
    /// End.
    End,
    /// Page Up.
    PageUp,
    /// Page Down.
    PageDown,
}

/// A mouse event at a terminal cell, with modifier state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mouse {
    /// What the pointer did (button press/release/drag, move, or scroll).
    pub kind: MouseKind,
    /// Cell column (0-based, terminal coordinates).
    pub column: u16,
    /// Cell row (0-based, terminal coordinates).
    pub row: u16,
    /// Shift held during the event.
    pub shift: bool,
    /// Ctrl held during the event.
    pub ctrl: bool,
    /// Alt held during the event.
    pub alt: bool,
    /// Super/Command held during the event.
    pub super_key: bool,
}

impl Mouse {
    /// A bare event at `(column, row)` with the given kind and no modifiers —
    /// convenience for tests and synthetic events.
    pub fn at(kind: MouseKind, column: u16, row: u16) -> Self {
        Self {
            kind,
            column,
            row,
            shift: false,
            ctrl: false,
            alt: false,
            super_key: false,
        }
    }

    /// True when no modifier keys are held. Terminals also use Shift-drag to
    /// bypass application mouse capture for native selection, so a host that
    /// implements its own selection should generally act only on `plain()`
    /// left-drags and leave Shift-drags to the terminal.
    pub fn plain(&self) -> bool {
        !self.shift && !self.ctrl && !self.alt && !self.super_key
    }
}

/// Which mouse button an event refers to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseButton {
    /// Left (primary) button.
    Left,
    /// Right (secondary) button.
    Right,
    /// Middle button.
    Middle,
}

/// The kind of mouse interaction an event represents.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseKind {
    /// Button pressed.
    Down(MouseButton),
    /// Button released.
    Up(MouseButton),
    /// Pointer moved with a button held (a drag of that button).
    Drag(MouseButton),
    /// Pointer moved with no button held.
    Moved,
    /// Wheel scrolled up.
    ScrollUp,
    /// Wheel scrolled down.
    ScrollDown,
    /// Wheel scrolled left.
    ScrollLeft,
    /// Wheel scrolled right.
    ScrollRight,
}

/// Whether a component consumed an event or let it bubble.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventFlow {
    /// The event was handled; stop propagating.
    Consumed,
    /// The event was not handled; keep bubbling.
    Ignored,
}

impl EventFlow {
    /// True for [`EventFlow::Consumed`].
    pub fn consumed(self) -> bool {
        matches!(self, EventFlow::Consumed)
    }
}

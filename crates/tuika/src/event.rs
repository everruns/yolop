//! Input events, decoupled from crossterm.
//!
//! Components handle `tuika` events, not raw crossterm events, so the widget
//! layer stays testable without a terminal. The host ([`super::host`])
//! translates crossterm into these. This mirrors Codex's event-stream input
//! model: a small, explicit event enum flowing to focused components.

/// A translated input event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    Key(Key),
    Mouse(Mouse),
    /// Bracketed-paste payload.
    Paste(String),
    Resize {
        width: u16,
        height: u16,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Key {
    pub code: KeyCode,
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
}

impl Key {
    pub fn new(code: KeyCode) -> Self {
        Self {
            code,
            ctrl: false,
            alt: false,
            shift: false,
        }
    }

    pub fn plain(&self) -> bool {
        !self.ctrl && !self.alt
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyCode {
    Char(char),
    Enter,
    Esc,
    Backspace,
    Delete,
    Tab,
    BackTab,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mouse {
    pub kind: MouseKind,
    /// Cell column (0-based, terminal coordinates).
    pub column: u16,
    /// Cell row (0-based, terminal coordinates).
    pub row: u16,
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
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
        }
    }

    /// True when no modifier keys are held. Terminals also use Shift-drag to
    /// bypass application mouse capture for native selection, so a host that
    /// implements its own selection should generally act only on `plain()`
    /// left-drags and leave Shift-drags to the terminal.
    pub fn plain(&self) -> bool {
        !self.shift && !self.ctrl && !self.alt
    }
}

/// Which mouse button an event refers to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}

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
    ScrollUp,
    ScrollDown,
    ScrollLeft,
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
    pub fn consumed(self) -> bool {
        matches!(self, EventFlow::Consumed)
    }
}

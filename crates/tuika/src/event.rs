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
    pub column: u16,
    pub row: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseKind {
    Down,
    Up,
    ScrollUp,
    ScrollDown,
    Moved,
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

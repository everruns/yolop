//! Focus registry and input ownership.
//!
//! Focusable regions register a stable string id each frame. The registry
//! tracks which id currently holds focus and cycles through them with
//! Tab/BackTab. Overlays claim *input ownership*: while an overlay is open, the
//! registry reports its id as focused and the host routes events there,
//! regardless of the base tree's focus ring — the missing "input ownership
//! across overlay and non-overlay components" piece from Pi.

use super::event::{Event, EventFlow, KeyCode};

/// Tracks the focus ring and the currently focused region.
#[derive(Clone, Debug, Default)]
pub struct FocusRegistry {
    order: Vec<String>,
    focused: Option<String>,
    /// When set, this id owns all input (an open overlay).
    owner: Option<String>,
}

impl FocusRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Begin a frame: clear the per-frame focus ring. Registrations follow.
    pub fn begin_frame(&mut self) {
        self.order.clear();
    }

    /// Register a focusable region for this frame, in tab order.
    pub fn register(&mut self, id: impl Into<String>) {
        let id = id.into();
        if self.focused.is_none() {
            self.focused = Some(id.clone());
        }
        self.order.push(id);
    }

    /// Give exclusive input ownership to `id` (e.g. an overlay) until cleared.
    pub fn set_owner(&mut self, id: impl Into<String>) {
        self.owner = Some(id.into());
    }

    /// Release input ownership.
    pub fn clear_owner(&mut self) {
        self.owner = None;
    }

    /// The id that should receive input: the owner if any, else the focused id.
    pub fn active(&self) -> Option<&str> {
        self.owner.as_deref().or(self.focused.as_deref())
    }

    /// Whether `id` is the active input target.
    pub fn is_active(&self, id: &str) -> bool {
        self.active() == Some(id)
    }

    /// Whether `id` holds focus in the base ring (ignores overlay ownership).
    pub fn is_focused(&self, id: &str) -> bool {
        self.focused.as_deref() == Some(id)
    }

    fn advance(&mut self, delta: isize) {
        if self.order.is_empty() {
            return;
        }
        let len = self.order.len() as isize;
        let current = self
            .focused
            .as_ref()
            .and_then(|f| self.order.iter().position(|id| id == f))
            .map(|p| p as isize)
            .unwrap_or(0);
        let next = ((current + delta) % len + len) % len;
        self.focused = Some(self.order[next as usize].clone());
    }

    /// Route Tab/BackTab to move focus. Ignored while an overlay owns input.
    pub fn handle(&mut self, event: &Event) -> EventFlow {
        if self.owner.is_some() {
            return EventFlow::Ignored;
        }
        let Event::Key(k) = event else {
            return EventFlow::Ignored;
        };
        match k.code {
            KeyCode::Tab if k.plain() => {
                self.advance(1);
                EventFlow::Consumed
            }
            KeyCode::BackTab => {
                self.advance(-1);
                EventFlow::Consumed
            }
            _ => EventFlow::Ignored,
        }
    }
}

// Which input surface owns an event.
//
// Decision: ownership is resolved in exactly one place, and every event kind
// routes through it. Before this, `handle_key` and `handle_paste` each
// re-derived precedence with their own `if` chain, and the paste chain had
// simply never grown the sandbox-approval branch the key chain had — so text
// pasted while an approval dialog owned the screen landed in the composer
// behind it. Deriving precedence twice is what made that possible, so the fix
// is one table, not a seventh branch.
//
// The surfaces below are yolop's, but the route is tuika's `Router`
// (see `knowledge/specs/tuika.md`): the toolkit owns the routing model, this
// module owns which of yolop's modal states claims input.

use tuika::Event;
use tuika::focus::FocusRegistry;

/// Reverse-history search. Owns the keyboard outright while open.
pub(crate) const HISTORY_SEARCH: &str = "history-search";
/// The y/a/n sandbox approval prompt.
pub(crate) const SANDBOX_APPROVAL: &str = "sandbox-approval";
/// The background-task sidebar, when the user focused it deliberately.
pub(crate) const BACKGROUND_PANEL: &str = "background-panel";
/// An extension `ui/ask` prompt.
pub(crate) const ASK: &str = "extension-ask";
/// The Esc-to-cancel claim a running turn has on the keyboard.
pub(crate) const TURN_CANCEL: &str = "turn-cancel";
/// The first-run setup wizard.
pub(crate) const SETUP: &str = "setup";
/// The prompt composer: the base ring, and the only surface always present.
pub(crate) const COMPOSER: &str = "composer";

/// The modal state that decides who owns input, sampled once per event.
///
/// A plain snapshot rather than a borrow of `App`, so resolving ownership never
/// conflicts with the `&mut self` a route stage then takes.
pub(crate) struct InputSurfaces {
    pub(crate) searching: bool,
    pub(crate) awaiting_sandbox_approval: bool,
    pub(crate) background_panel_focused: bool,
    pub(crate) asking: bool,
    pub(crate) busy: bool,
    pub(crate) in_setup: bool,
}

impl InputSurfaces {
    /// The surface that owns `event`, in yolop's precedence order.
    ///
    /// `event` is a parameter because [`TURN_CANCEL`] is a partial claim: a
    /// running turn takes Esc (arm, then cancel) and nothing else, so every
    /// other key still reaches the setup wizard or the composer under it. The
    /// remaining surfaces are true modals and ignore the event entirely.
    pub(crate) fn owner(&self, event: &Event) -> &'static str {
        if self.searching {
            return HISTORY_SEARCH;
        }
        if self.awaiting_sandbox_approval {
            return SANDBOX_APPROVAL;
        }
        if self.background_panel_focused {
            return BACKGROUND_PANEL;
        }
        if self.asking {
            return ASK;
        }
        if self.busy && is_escape(event) {
            return TURN_CANCEL;
        }
        if self.in_setup {
            return SETUP;
        }
        COMPOSER
    }

    /// The surface a clipboard paste (Ctrl+V) should fill, for a caller that
    /// has no terminal event to resolve against.
    pub(crate) fn paste_owner(&self) -> &'static str {
        self.owner(&Event::Paste(String::new()))
    }

    /// The frame's focus state: the composer is the base ring, and whichever
    /// modal claims `event` owns input over it.
    pub(crate) fn focus(&self, event: &Event) -> FocusRegistry {
        let mut focus = FocusRegistry::new();
        focus.begin_frame();
        focus.register(COMPOSER);
        let owner = self.owner(event);
        if owner != COMPOSER {
            focus.set_owner(owner);
        }
        focus
    }
}

fn is_escape(event: &Event) -> bool {
    matches!(event, Event::Key(key) if key.code == tuika::event::KeyCode::Esc)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tuika::event::{Key, KeyCode};

    fn surfaces() -> InputSurfaces {
        InputSurfaces {
            searching: false,
            awaiting_sandbox_approval: false,
            background_panel_focused: false,
            asking: false,
            busy: false,
            in_setup: false,
        }
    }

    fn key(code: KeyCode) -> Event {
        Event::Key(Key {
            code,
            ctrl: false,
            alt: false,
            shift: false,
        })
    }

    fn paste() -> Event {
        Event::Paste("payload".to_string())
    }

    #[test]
    fn the_composer_owns_input_when_nothing_is_modal() {
        assert_eq!(surfaces().owner(&key(KeyCode::Char('a'))), COMPOSER);
        assert_eq!(surfaces().owner(&paste()), COMPOSER);
    }

    #[test]
    fn a_modal_owns_every_event_kind_not_just_keys() {
        // The divergence this module exists to prevent: ownership must not
        // depend on which kind of event arrived.
        let modal = InputSurfaces {
            awaiting_sandbox_approval: true,
            ..surfaces()
        };
        assert_eq!(modal.owner(&key(KeyCode::Char('y'))), SANDBOX_APPROVAL);
        assert_eq!(modal.owner(&paste()), SANDBOX_APPROVAL);
    }

    #[test]
    fn search_outranks_every_other_surface() {
        let all = InputSurfaces {
            searching: true,
            awaiting_sandbox_approval: true,
            background_panel_focused: true,
            asking: true,
            busy: true,
            in_setup: true,
        };
        assert_eq!(all.owner(&key(KeyCode::Esc)), HISTORY_SEARCH);
    }

    #[test]
    fn a_running_turn_claims_escape_only() {
        let busy = InputSurfaces {
            busy: true,
            in_setup: true,
            ..surfaces()
        };
        assert_eq!(busy.owner(&key(KeyCode::Esc)), TURN_CANCEL);
        // Anything else falls through to the surface under the running turn.
        assert_eq!(busy.owner(&key(KeyCode::Char('a'))), SETUP);
        assert_eq!(busy.owner(&paste()), SETUP);
    }

    #[test]
    fn an_open_ask_outranks_a_running_turns_escape() {
        let asking = InputSurfaces {
            asking: true,
            busy: true,
            ..surfaces()
        };
        assert_eq!(asking.owner(&key(KeyCode::Esc)), ASK);
    }

    #[test]
    fn focus_makes_the_owner_active_over_the_composer_ring() {
        let focus = surfaces().focus(&paste());
        assert_eq!(focus.active(), Some(COMPOSER));

        let modal = InputSurfaces {
            asking: true,
            ..surfaces()
        };
        assert_eq!(modal.focus(&paste()).active(), Some(ASK));
    }
}

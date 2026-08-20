//! The app-global key bindings, declared through `tuika`'s [keymap
//! engine](tuika::keymap).
//!
//! yolop's always-on chord shortcuts (reverse-history search, interrupt, quit,
//! the activity rail, image paste) used to live as a hand-rolled
//! `match` at the top of [`App::handle_key`](super::App::handle_key). They now
//! resolve through a single [`tuika::keymap::Keymap`], so the bindings are declarative
//! and discoverable in one place — and yolop dogfoods the toolkit's own keymap.
//!
//! These bindings are deliberately *global*: they fire regardless of the app's
//! mode (mid-turn, during setup, or with an overlay open), which is why they sit
//! in one always-active layer with no [`when`](tuika::keymap::Layer::when) gate. Modal
//! key handling (setup steps, the activity rail, `ui/ask` prompts) stays with
//! the owning handler, which the engine's mode-gated layers could absorb later.

use tuika::keymap::{Keymap, Layer};

/// A global action a chord shortcut can trigger. Each maps to a method on
/// [`App`](super::App) in [`App::handle_key`](super::App::handle_key).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GlobalAction {
    /// Open Ctrl+R reverse-history search over past prompts.
    ReverseSearch,
    /// Ctrl+C: interrupt the current turn, or arm/confirm exit when idle.
    Interrupt,
    /// Ctrl+D: quit the session.
    Quit,
    /// Ctrl+B: focus or close the activity rail.
    ToggleBackground,
    /// Ctrl+V: paste an image (or large text) from the clipboard into the composer.
    PasteImage,
    /// Ctrl+O: expand or collapse retained work details for the latest compact turn.
    ToggleWorkDetails,
}

/// Build yolop's global keymap: one always-active layer of chord shortcuts.
///
/// The labels are carried on each binding so a future help overlay or
/// [`KeyHints`](tuika::components::KeyHints) row can render them straight from
/// [`Keymap::hints`](tuika::keymap::Keymap::hints).
pub(crate) fn global_keymap() -> Keymap<GlobalAction> {
    Keymap::new().layer(
        Layer::new("global")
            .bind_labeled("ctrl+r", "search history", GlobalAction::ReverseSearch)
            .bind_labeled("ctrl+c", "interrupt", GlobalAction::Interrupt)
            .bind_labeled("ctrl+d", "quit", GlobalAction::Quit)
            .bind_labeled("ctrl+b", "activity", GlobalAction::ToggleBackground)
            .bind_labeled("ctrl+v", "paste image", GlobalAction::PasteImage)
            .bind_labeled("ctrl+o", "work details", GlobalAction::ToggleWorkDetails),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tuika::event::{Key, KeyCode};
    use tuika::keymap::Dispatch;

    fn ctrl(c: char) -> Key {
        Key {
            code: KeyCode::Char(c),
            ctrl: true,
            alt: false,
            shift: false,
        }
    }

    #[test]
    fn binds_every_global_chord() {
        let mut keymap = global_keymap();
        assert_eq!(
            keymap.dispatch(ctrl('r')),
            Dispatch::Command(GlobalAction::ReverseSearch)
        );
        assert_eq!(
            keymap.dispatch(ctrl('c')),
            Dispatch::Command(GlobalAction::Interrupt)
        );
        assert_eq!(
            keymap.dispatch(ctrl('d')),
            Dispatch::Command(GlobalAction::Quit)
        );
        assert_eq!(
            keymap.dispatch(ctrl('b')),
            Dispatch::Command(GlobalAction::ToggleBackground)
        );
        assert_eq!(
            keymap.dispatch(ctrl('v')),
            Dispatch::Command(GlobalAction::PasteImage)
        );
        assert_eq!(
            keymap.dispatch(ctrl('o')),
            Dispatch::Command(GlobalAction::ToggleWorkDetails)
        );
    }

    #[test]
    fn leaves_unbound_keys_for_the_composer() {
        let mut keymap = global_keymap();
        // A plain letter and an unbound chord both fall through to the host.
        assert_eq!(
            keymap.dispatch(Key::new(KeyCode::Char('a'))),
            Dispatch::Unmatched
        );
        assert_eq!(keymap.dispatch(ctrl('z')), Dispatch::Unmatched);
    }

    #[test]
    fn every_binding_carries_a_help_label() {
        let hints = global_keymap().hints();
        assert_eq!(hints.len(), 6);
        assert!(hints.iter().all(|hint| hint.label.is_some()));
        assert!(hints.iter().any(|hint| hint.keys == "Ctrl+R"));
    }
}

//! The app-global key bindings, declared through `tuika`'s [keymap
//! engine](tuika::keymap).
//!
//! yolop's always-on chord shortcuts (reverse-history search, interrupt, quit,
//! the background task panel, image paste) used to live as a hand-rolled
//! `match` at the top of [`App::handle_key`](super::App::handle_key). They now
//! resolve through a single [`tuika::Keymap`], so the bindings are declarative
//! and discoverable in one place — and yolop dogfoods the toolkit's own keymap.
//!
//! These bindings are deliberately *global*: they fire regardless of the app's
//! mode (mid-turn, during setup, or with an overlay open), which is why they sit
//! in one always-active layer with no [`when`](tuika::Layer::when) gate. Modal
//! key handling (setup steps, the background panel, `ui/ask` prompts) stays with
//! the owning handler, which the engine's mode-gated layers could absorb later.

use tuika::{Keymap, Layer};

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
    /// Ctrl+B: toggle the background task-tree panel.
    ToggleBackground,
    /// Ctrl+V: paste an image (or large text) from the clipboard into the composer.
    PasteImage,
}

/// Build yolop's global keymap: one always-active layer of chord shortcuts.
///
/// The labels are carried on each binding so a future help overlay or
/// [`KeyHints`](tuika::KeyHints) row can render them straight from
/// [`Keymap::hints`](tuika::Keymap::hints).
pub(crate) fn global_keymap() -> Keymap<GlobalAction> {
    Keymap::new().layer(
        Layer::new("global")
            .bind_labeled("ctrl+r", "search history", GlobalAction::ReverseSearch)
            .bind_labeled("ctrl+c", "interrupt", GlobalAction::Interrupt)
            .bind_labeled("ctrl+d", "quit", GlobalAction::Quit)
            .bind_labeled("ctrl+b", "background tasks", GlobalAction::ToggleBackground)
            .bind_labeled("ctrl+v", "paste image", GlobalAction::PasteImage),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tuika::Dispatch;
    use tuika::event::{Key, KeyCode};

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
        assert_eq!(hints.len(), 5);
        assert!(hints.iter().all(|hint| hint.label.is_some()));
        assert!(hints.iter().any(|hint| hint.keys == "Ctrl+R"));
    }
}

//! Session modes: yolop surfaces its [`ApprovalMode`] (off/normal/protective)
//! as ACP session modes, so an editor can switch the approval level from its
//! own mode picker. The mode is the *same* central setting driven by `/setup
//! approval` and the `set_approval_mode` tool — one vocabulary across every
//! front end (TUI, settings, ACP), rather than a second ACP-only taxonomy.
//!
//! Because yolop is a single-user CLI whose `SettingsStore` is shared across
//! every session a client opens, `session/set_mode` changes the level globally
//! (exactly like `/setup approval`), not per-session.

use crate::settings::ApprovalMode;

use super::protocol::{SessionMode, SessionModeId, SessionModeState};

/// All approval levels, ordered strictest → laxest for the mode picker.
const MODES: [ApprovalMode; 3] = [
    ApprovalMode::Protective,
    ApprovalMode::Normal,
    ApprovalMode::Off,
];

/// Stable ACP mode id for a level — its canonical settings name (`"off"` …).
pub(super) fn mode_id(mode: ApprovalMode) -> SessionModeId {
    SessionModeId::new(mode.as_str())
}

/// Human-readable name + description shown for a level in the picker.
fn describe(mode: ApprovalMode) -> (&'static str, &'static str) {
    match mode {
        ApprovalMode::Protective => (
            "Protective",
            "Ask before any state-changing action: writing or deleting files, \
             git commits/pushes, installing packages, or any non-read-only \
             command.",
        ),
        ApprovalMode::Normal => (
            "Normal",
            "Ask only before destructive, irreversible, or outward-facing \
             actions (deletes, rm -rf, force-push, publishing). Ordinary edits \
             proceed without asking.",
        ),
        ApprovalMode::Off => ("Off", "Never pause for approval; act autonomously."),
    }
}

/// The full mode set plus the currently active one, for a `session/new` or
/// `session/load` response (and the `modes` advertisement generally).
pub(super) fn session_mode_state(current: ApprovalMode) -> SessionModeState {
    let available = MODES
        .iter()
        .map(|&mode| {
            let (name, description) = describe(mode);
            SessionMode::new(mode_id(mode), name).description(Some(description.to_string()))
        })
        .collect();
    SessionModeState::new(mode_id(current), available)
}

/// Resolve an ACP mode id back into an [`ApprovalMode`]. Accepts the same
/// canonical names and aliases as `/setup approval`, so a client that sends a
/// synonym still lands on the right level.
pub(super) fn approval_mode_from_id(id: &str) -> Option<ApprovalMode> {
    ApprovalMode::parse(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_lists_all_modes_with_current_selected() {
        let state = session_mode_state(ApprovalMode::Protective);
        assert_eq!(state.current_mode_id, mode_id(ApprovalMode::Protective));
        let ids: Vec<String> = state
            .available_modes
            .iter()
            .map(|m| m.id.to_string())
            .collect();
        assert_eq!(ids, vec!["protective", "normal", "off"]);
        assert!(
            state
                .available_modes
                .iter()
                .all(|m| m.description.is_some())
        );
    }

    #[test]
    fn mode_id_roundtrips_through_approval_mode_from_id() {
        for mode in MODES {
            let id = mode_id(mode);
            assert_eq!(approval_mode_from_id(&id.to_string()), Some(mode));
        }
    }

    #[test]
    fn approval_mode_from_id_accepts_aliases_and_rejects_junk() {
        assert_eq!(
            approval_mode_from_id("paranoid"),
            Some(ApprovalMode::Protective)
        );
        assert_eq!(approval_mode_from_id("yolo"), Some(ApprovalMode::Off));
        assert_eq!(approval_mode_from_id("whenever"), None);
    }
}

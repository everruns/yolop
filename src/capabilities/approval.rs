//! Host adapter for the upstream soft-approval capability.
//!
//! Soft approval started here and was generalized into `soft_approval` in
//! `everruns-builtins`, so the prompt block, the three tools, and the pause
//! store now live upstream and this module is the seam that makes them fit a
//! single-user terminal host.
//!
//! The one host concern is where the level lives. Upstream treats it as
//! per-session state layered over capability config, which is right for a
//! server holding many sessions. Yolop's level is central configuration
//! (`approval_mode` in settings.toml, see [`crate::config::ApprovalMode`]):
//! it is shown in the status bar, set by `/setup approval`, and must survive a
//! restart. [`SettingsApprovalModes`] implements the upstream store over
//! `ConfigService` and `SettingsStore` so `set_approval_mode` writes through to
//! settings.toml and every session reads the same level.

use std::sync::Arc;

use everruns_contracts::typed_id::SessionId;

use crate::config::ApprovalMode;
use crate::config::SettingsStore;
use crate::config::service::ConfigService;

pub(crate) use everruns_builtins::soft_approval::SOFT_APPROVAL_CAPABILITY_ID;
use everruns_builtins::soft_approval::{
    ApprovalMode as UpstreamApprovalMode, ApprovalModeStore, SoftApprovalCapability,
};
pub use everruns_builtins::soft_approval::{PendingApproval, PendingApprovalStore};

/// Yolop's level as upstream spells it. Total both ways: the enums carry the
/// same three variants because upstream's is a copy of this one.
fn to_upstream(mode: ApprovalMode) -> UpstreamApprovalMode {
    match mode {
        ApprovalMode::Protective => UpstreamApprovalMode::Protective,
        ApprovalMode::Normal => UpstreamApprovalMode::Normal,
        ApprovalMode::Off => UpstreamApprovalMode::Off,
    }
}

fn from_upstream(mode: UpstreamApprovalMode) -> ApprovalMode {
    match mode {
        UpstreamApprovalMode::Protective => ApprovalMode::Protective,
        UpstreamApprovalMode::Normal => ApprovalMode::Normal,
        UpstreamApprovalMode::Off => ApprovalMode::Off,
    }
}

/// The `<soft_approval>` block for a level, in yolop's vocabulary.
///
/// Thin wrapper over the upstream renderer so the prompt-budget test keeps
/// speaking [`crate::config::ApprovalMode`]. Returns `None` for
/// [`ApprovalMode::Off`], which contributes nothing to the prompt.
///
/// Test-only: the capability renders its own contribution now, so nothing in
/// the running host asks for the text.
#[cfg(test)]
pub(crate) fn render_approval_block(mode: ApprovalMode) -> Option<String> {
    everruns_builtins::soft_approval::render_approval_block(to_upstream(mode))
}

/// The upstream level store backed by yolop's central setting.
///
/// Reads go through `ConfigService` on every call rather than capturing a
/// session-start value, so `/setup approval`, `set_approval_mode`, and
/// `set_config approval_mode` all take effect on the next turn. Writes go to
/// `SettingsStore`, which is what makes the level outlive the session.
pub(crate) struct SettingsApprovalModes {
    config: Arc<dyn ConfigService>,
    settings: Arc<SettingsStore>,
}

impl ApprovalModeStore for SettingsApprovalModes {
    fn mode(&self, _session_id: &SessionId) -> Option<UpstreamApprovalMode> {
        // Always `Some`: yolop's level is central, so capability config never
        // gets a say and every session resolves to the same answer.
        Some(to_upstream(self.config.approval_mode()))
    }

    fn set_mode(&self, _session_id: &SessionId, mode: UpstreamApprovalMode) -> Result<(), String> {
        self.settings
            .set_approval_mode(from_upstream(mode))
            .map_err(|e| format!("could not save approval level: {e}"))
    }
}

/// Build the upstream capability over yolop's setting.
///
/// Returns the pause store alongside it because the host reads that store when
/// a turn ends, so a pause renders as a pause instead of a turn that stopped
/// mid-sentence.
pub(crate) fn soft_approval_capability(
    config: Arc<dyn ConfigService>,
    settings: Arc<SettingsStore>,
) -> (SoftApprovalCapability, PendingApprovalStore) {
    let capability = SoftApprovalCapability::with_mode_store(Arc::new(SettingsApprovalModes {
        config,
        settings,
    }));
    let pending = capability.pending();
    (capability, pending)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A store backed by a real settings.toml in a temp dir, so a write-through
    /// is checked against the same path production uses.
    fn store() -> (tempfile::TempDir, Arc<SettingsStore>) {
        let tmp = tempfile::tempdir().expect("tmp");
        let settings = Arc::new(SettingsStore::open(tmp.path().join("settings.toml")));
        (tmp, settings)
    }

    #[test]
    fn the_level_round_trips_through_both_vocabularies() {
        for mode in [
            ApprovalMode::Protective,
            ApprovalMode::Normal,
            ApprovalMode::Off,
        ] {
            assert_eq!(from_upstream(to_upstream(mode)), mode);
            // The two enums agree on the wire name, which is what settings.toml
            // and the status bar show.
            assert_eq!(to_upstream(mode).as_str(), mode.as_str());
        }
    }

    #[test]
    fn the_store_reports_the_central_level_for_any_session() {
        let (_tmp, settings) = store();
        settings
            .set_approval_mode(ApprovalMode::Protective)
            .expect("set level");
        let store = SettingsApprovalModes {
            config: settings.clone(),
            settings: settings.clone(),
        };

        // Two unrelated sessions see one central answer, unlike upstream's
        // per-session default.
        let a = SessionId::new();
        let b = SessionId::new();
        assert_eq!(store.mode(&a), Some(UpstreamApprovalMode::Protective));
        assert_eq!(store.mode(&b), store.mode(&a));
    }

    /// The whole point of the adapter: the level in settings.toml is the level
    /// the upstream capability puts in the prompt. Covers the seam end to end,
    /// because a store that compiles but is never consulted would still pass
    /// the unit tests above.
    #[tokio::test]
    async fn the_capability_prompts_at_the_level_from_settings() {
        use everruns_core::Capability as _;

        let (_tmp, settings) = store();
        settings
            .set_approval_mode(ApprovalMode::Protective)
            .expect("set level");
        let (capability, _pending) = soft_approval_capability(settings.clone(), settings.clone());
        let ctx =
            everruns_core::capabilities::SystemPromptContext::without_file_store(SessionId::new());

        let prompt = capability
            .system_prompt_contribution(&ctx)
            .await
            .expect("protective contributes a block");
        assert!(prompt.contains("<soft_approval>"));
        assert!(
            prompt.contains("level protective"),
            "the central setting did not reach the prompt: {prompt}"
        );

        // `off` is the one level that contributes nothing at all.
        settings
            .set_approval_mode(ApprovalMode::Off)
            .expect("set level");
        assert!(
            capability.system_prompt_contribution(&ctx).await.is_none(),
            "off must contribute no block"
        );
    }

    #[test]
    fn setting_the_level_writes_through_to_settings() {
        let (_tmp, settings) = store();
        let store = SettingsApprovalModes {
            config: settings.clone(),
            settings: settings.clone(),
        };

        store
            .set_mode(&SessionId::new(), UpstreamApprovalMode::Off)
            .expect("write through");

        // The durable setting moved, not just an in-memory override.
        assert_eq!(settings.approval_mode(), ApprovalMode::Off);
    }
}

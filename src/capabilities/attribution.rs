// The `attribution` capability — yolop's optional commit / PR attribution.
//
// When enabled (central `attribution` flag in settings.toml), this capability
// injects guidance asking the model to keep the user's git identity intact and
// append a single `Co-Authored-By` trailer to commits it creates, plus a short
// footer to pull request descriptions. It is prompt guidance only — there is no
// hard enforcement — so it lives entirely in `system_prompt_contribution` and
// reads the flag live through the shared config service each turn.

use crate::config::service::ConfigService;
use async_trait::async_trait;
use everruns_core::capabilities::{Capability, CapabilityStatus, SystemPromptContext};
use std::sync::Arc;

pub(crate) const ATTRIBUTION_CAPABILITY_ID: &str = "yolop_attribution";
pub(crate) const YOLOP_ATTRIBUTION_TRAILER: &str = "Co-Authored-By: yolop <yolop@everruns.com>";
pub(crate) const YOLOP_PR_ATTRIBUTION: &str = "Produced by [yolop](https://everruns.com/yolop)";

/// Render the `## Attribution` system-prompt block. Pure so it is unit-testable
/// without a `SystemPromptContext`.
fn yolop_attribution_prompt() -> String {
    format!(
        "\
## Attribution

When you create or amend commits for changes you made, keep the user's
git author/committer identity intact and append this git trailer once:
{YOLOP_ATTRIBUTION_TRAILER}

When creating or editing pull request descriptions with `gh`, add this
footer once:
{YOLOP_PR_ATTRIBUTION}

Do not add duplicate attribution or session links."
    )
}

pub(crate) struct AttributionCapability {
    /// Reads through the shared config service rather than the concrete store —
    /// it only needs to know whether attribution is enabled.
    pub(crate) config: Arc<dyn ConfigService>,
}

#[async_trait]
impl Capability for AttributionCapability {
    fn id(&self) -> &str {
        ATTRIBUTION_CAPABILITY_ID
    }
    fn name(&self) -> &str {
        "Yolop Attribution"
    }
    fn description(&self) -> &str {
        "Adds optional commit and pull request attribution guidance."
    }
    fn status(&self) -> CapabilityStatus {
        CapabilityStatus::Available
    }
    fn category(&self) -> Option<&str> {
        Some("Examples")
    }
    async fn system_prompt_contribution(&self, _ctx: &SystemPromptContext) -> Option<String> {
        self.config
            .attribution_enabled()
            .then(yolop_attribution_prompt)
    }
    fn system_prompt_preview(&self) -> Option<String> {
        Some(yolop_attribution_prompt())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SettingsStore;

    #[tokio::test]
    async fn attribution_prompt_follows_settings() {
        let tmp = tempfile::tempdir().expect("tmp");
        let settings = Arc::new(SettingsStore::open(tmp.path().join("settings.toml")));
        let capability = AttributionCapability {
            config: settings.clone(),
        };
        let ctx =
            SystemPromptContext::without_file_store(everruns_core::typed_id::SessionId::new());

        let enabled = capability
            .system_prompt_contribution(&ctx)
            .await
            .expect("enabled attribution prompt");
        assert!(enabled.contains(YOLOP_ATTRIBUTION_TRAILER));
        assert_eq!(
            YOLOP_PR_ATTRIBUTION,
            "Produced by [yolop](https://everruns.com/yolop)"
        );
        assert!(enabled.contains(YOLOP_PR_ATTRIBUTION));

        settings
            .set_attribution(false)
            .expect("disable attribution");
        assert!(capability.system_prompt_contribution(&ctx).await.is_none());
    }
}

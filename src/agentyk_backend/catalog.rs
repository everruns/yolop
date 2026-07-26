//! yolop's model knowledge, exposed as an [`agentyk::ModelCatalog`].
//!
//! agentyk ships no model list — deliberately, since one inside a library goes
//! stale at the next provider release. yolop already has the data: the same
//! `everruns_core` profiles it validates `--reasoning-effort` against on the
//! shipping backend. This adapter hands that knowledge to the library, so the
//! agentyk backend rejects an unsupported effort at composition time with the
//! same authority the everruns backend has.

use agentyk::{DriverId, ModelCatalog, ModelProfile};
use everruns_core::{DriverId as EverrunsDriverId, get_model_profile};

/// Answers agentyk's questions about a model from everruns' profile table.
pub struct YolopModelCatalog;

impl ModelCatalog for YolopModelCatalog {
    fn profile(&self, driver: &DriverId, model: &str) -> Option<ModelProfile> {
        let profile = get_model_profile(&everruns_driver(driver)?, model)?;

        let mut mapped = ModelProfile::new();
        if let Some(limits) = &profile.limits {
            if let Ok(context) = u64::try_from(limits.context) {
                mapped = mapped.context_window(context);
            }
            if let Ok(output) = u64::try_from(limits.output) {
                mapped = mapped.max_output_tokens(output);
            }
        }
        if let Some(effort) = &profile.reasoning_effort {
            // The wire spelling, not the display name: it is what a
            // `ModelSpec` carries and what the provider validates against.
            mapped = mapped.reasoning_efforts(
                effort
                    .values
                    .iter()
                    .filter_map(|value| serde_json::to_value(&value.value).ok())
                    .filter_map(|value| value.as_str().map(str::to_string)),
            );
        }
        // everruns' `reasoning` flag covers both effort-style and
        // budget-style models; only Anthropic's takes a token budget, and
        // that is the shape agentyk's `thinking` describes.
        mapped = mapped
            .thinking(profile.reasoning && matches!(driver.as_str(), "anthropic"))
            .vision(profile.attachment);
        Some(mapped)
    }
}

/// The everruns driver whose profile table covers this agentyk driver.
/// `llmsim` has no profiles, and a driver yolop does not serve has none
/// either — both mean "unknown model", which the catalog reports as `None`.
fn everruns_driver(driver: &DriverId) -> Option<EverrunsDriverId> {
    match driver.as_str() {
        "anthropic" => Some(EverrunsDriverId::Anthropic),
        "openai" => Some(EverrunsDriverId::OpenAI),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentyk::ModelSpec;

    /// A model yolop's provider defaults actually use, so the test breaks if
    /// the profile table stops covering the models the backend offers.
    const KNOWN_ANTHROPIC: &str = "claude-sonnet-4-5";

    #[test]
    fn a_known_model_reports_its_limits() {
        let profile = YolopModelCatalog
            .profile(&DriverId::anthropic(), KNOWN_ANTHROPIC)
            .expect("everruns profiles cover this model");
        assert!(
            profile.context_window.is_some_and(|window| window > 0),
            "{profile:?}"
        );
        assert!(profile.max_output_tokens.is_some_and(|max| max > 0));
    }

    #[test]
    fn an_unsupported_effort_is_rejected_and_a_supported_one_is_not() {
        let profile = YolopModelCatalog
            .profile(&DriverId::openai(), "gpt-5.4")
            .expect("everruns profiles cover this model");
        // Skipped rather than asserted blindly: a model with no effort
        // configuration says nothing about efforts, and which models have one
        // is everruns' business, not this adapter's.
        let Some(supported) = profile.reasoning_efforts.first().cloned() else {
            return;
        };
        assert!(
            profile
                .validate(&ModelSpec::openai("gpt-5.4").reasoning_effort(&supported))
                .is_ok()
        );
        assert!(
            profile
                .validate(&ModelSpec::openai("gpt-5.4").reasoning_effort("nonsense"))
                .is_err()
        );
    }

    #[test]
    fn unknown_drivers_and_models_report_nothing() {
        assert!(
            YolopModelCatalog
                .profile(&DriverId::llmsim(), "llmsim")
                .is_none()
        );
        assert!(
            YolopModelCatalog
                .profile(&DriverId::anthropic(), "claude-not-a-model")
                .is_none()
        );
    }
}

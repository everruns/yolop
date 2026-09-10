//! Process-wide cache of the model profiles providers advertise at discovery
//! time.
//!
//! The static registry in `everruns_provider::model_profiles` is curated data
//! that grows on release cadence, so it is always behind a gateway's catalog.
//! Gateways describe their own models instead: OpenRouter's `/models` carries a
//! `supported_parameters` array, and its driver turns that into a
//! [`ModelProfile`] on every [`DiscoveredModel`] (`reasoning` in the array is
//! what says the model takes a reasoning effort at all).
//!
//! Yolop's effort selector and per-turn defaults are synchronous, so they
//! cannot query discovery themselves. Every path that already lists models
//! (the pre-turn availability check, the `/model` browser) records what it saw
//! here, and the lookups read it as one layer of the merge: the curated
//! registry first where it speaks, then this, then yolop's own metadata for
//! families neither source describes yet.
//!
//! Empty until a discovery call has run in this process. That is the point of
//! the layering: a miss is a miss, not a wrong answer.

use std::collections::HashMap;
use std::sync::{LazyLock, RwLock};

use everruns_provider::{DiscoveredModel, DriverId, ModelProfile, ReasoningEffortConfig};

type ProfileKey = (String, String);

static DISCOVERED: LazyLock<RwLock<HashMap<ProfileKey, ModelProfile>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

fn key(provider_type: &DriverId, model_id: &str) -> ProfileKey {
    (
        provider_type.to_string(),
        model_id.trim().to_ascii_lowercase(),
    )
}

/// Record every profile a discovery response carried. Models the provider
/// described without a profile are skipped rather than cached as "no metadata",
/// so a thinner response never masks a richer earlier one.
pub(crate) fn remember(provider_type: &DriverId, models: &[DiscoveredModel]) {
    let fresh = models
        .iter()
        .filter_map(|model| {
            let profile = model.discovered_profile.clone()?;
            Some((key(provider_type, &model.model_id), profile))
        })
        .collect::<Vec<_>>();
    if fresh.is_empty() {
        return;
    }
    let mut cache = DISCOVERED
        .write()
        .expect("discovered profile lock poisoned");
    cache.extend(fresh);
}

/// The profile this provider advertised for `model`, if discovery has run.
pub(crate) fn profile(provider_type: &DriverId, model: &str) -> Option<ModelProfile> {
    DISCOVERED
        .read()
        .expect("discovered profile lock poisoned")
        .get(&key(provider_type, model))
        .cloned()
}

/// The effort scale this provider advertised for `model`.
pub(crate) fn reasoning_effort_config(
    provider_type: &DriverId,
    model: &str,
) -> Option<ReasoningEffortConfig> {
    profile(provider_type, model).and_then(|profile| profile.reasoning_effort)
}

/// A profile shaped like the one OpenRouter's driver derives from
/// `supported_parameters`, with a marker default so a test can tell it apart
/// from the curated registry's answer.
#[cfg(test)]
pub(crate) fn advertised_profile_for_test() -> ModelProfile {
    use everruns_provider::{ReasoningEffort, ReasoningEffortValue};

    let mut profile = everruns_provider::model_profiles::get_model_profile(
        &DriverId::OpenRouter,
        "nvidia/nemotron-3-super-120b-a12b",
    )
    .expect("a registry profile to take the shape from");
    profile.reasoning = true;
    profile.reasoning_effort = Some(ReasoningEffortConfig {
        values: vec![ReasoningEffortValue {
            value: ReasoningEffort::High,
            name: "High".to_string(),
        }],
        default: ReasoningEffort::High,
    });
    profile
}

#[cfg(test)]
mod tests {
    use super::*;
    use everruns_provider::ReasoningEffort;

    fn model(model_id: &str, profile: Option<ModelProfile>) -> DiscoveredModel {
        DiscoveredModel {
            model_id: model_id.to_string(),
            display_name: None,
            created_at: None,
            owned_by: None,
            capabilities: vec!["chat".to_string()],
            discovered_profile: profile,
        }
    }

    // The cache is process-wide, so tests use ids of their own rather than
    // clearing it out from under each other.
    #[test]
    fn advertised_efforts_are_readable_after_discovery() {
        let id = "test-vendor/advertised-reasoner";
        remember(
            &DriverId::OpenRouter,
            &[
                model(id, Some(advertised_profile_for_test())),
                model("test-vendor/undescribed", None),
            ],
        );

        let config = reasoning_effort_config(&DriverId::OpenRouter, id)
            .expect("the advertised effort scale");
        assert_eq!(config.default, ReasoningEffort::High);
        // Case-insensitive on the model id, as provider catalogs are.
        assert!(
            reasoning_effort_config(&DriverId::OpenRouter, "Test-Vendor/Advertised-Reasoner")
                .is_some()
        );
        // A model discovery described without a profile stays a miss, so the
        // caller falls through to the next layer of the merge.
        assert!(profile(&DriverId::OpenRouter, "test-vendor/undescribed").is_none());
        // Provider-scoped: the same id on another driver is a different model.
        assert!(profile(&DriverId::Meta, id).is_none());
    }
}

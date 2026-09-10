//! Reasoning-effort metadata the shared model-profile registry does not carry,
//! and recovery from endpoints that mandate reasoning.
//!
//! Two gaps show up on gateways, where a model id can name an endpoint the
//! profile registry has never seen (`meta/muse-spark-1.3-contributor` on
//! OpenRouter):
//!
//!   - the effort picker has nothing to offer, because both the options and the
//!     default come from the profile, so `/effort` reports "current model
//!     profile does not expose reasoning efforts";
//!   - the turn is then sent with no reasoning control at all, and an endpoint
//!     that mandates reasoning rejects it with
//!     `400 Reasoning is mandatory for this endpoint and cannot be disabled`.
//!
//! The registry is upstream data and only grows on release cadence, so families
//! known to require reasoning are listed here by family prefix: a new point
//! release (`muse-spark-1.3`) or tier (`-contributor`) is covered the day it
//! ships instead of erroring until the next dependency bump. When the registry
//! does carry an effort config it wins; this is a fallback, never an override.

use everruns_core::{InputMessage, ReasoningConfig};
use everruns_provider::{DriverId, ReasoningEffort, ReasoningEffortConfig, ReasoningEffortValue};

/// A model family whose endpoints reject a request that carries no reasoning
/// effort, and the provider surfaces where that is true.
struct ReasoningRequirement {
    /// Matched as a prefix of the bare model id, so every point release and
    /// pricing tier of the family is covered.
    family: &'static str,
    /// Driver ids ([`DriverId::as_str`]) whose endpoint mandates reasoning for
    /// this family. Scoped rather than global because mandating reasoning is a
    /// property of the endpoint, not of the weights: Meta's own API documents a
    /// model-determined default for Muse, while OpenRouter's Muse endpoint
    /// rejects a turn that names no effort at all.
    drivers: &'static [&'static str],
}

const REASONING_REQUIREMENTS: &[ReasoningRequirement] = &[ReasoningRequirement {
    family: "muse-spark",
    drivers: &["openrouter"],
}];

/// The effort to fall back to when an endpoint demands reasoning and neither
/// the user nor a profile has named a level. Mid-scale on purpose: the point is
/// to make the turn legal, not to pick a spend level on the user's behalf.
pub(crate) const DEFAULT_REQUIRED_EFFORT: &str = "medium";

/// The bare model id: no vendor prefix (`meta/muse-spark-1.3` -> `muse-spark-1.3`)
/// and no OpenRouter variant suffix (`:free`, `:nitro`), lowercased.
fn bare_model_id(model: &str) -> String {
    let model = model.rsplit('/').next().unwrap_or(model);
    let model = model.split(':').next().unwrap_or(model);
    model.trim().to_ascii_lowercase()
}

/// Whether this model's endpoint on this provider mandates reasoning, so a turn
/// without a reasoning effort is rejected before it starts.
pub(crate) fn requires_reasoning(provider_type: &DriverId, model: &str) -> bool {
    let bare = bare_model_id(model);
    REASONING_REQUIREMENTS.iter().any(|requirement| {
        bare.starts_with(requirement.family)
            && requirement
                .drivers
                .iter()
                .any(|driver| *driver == provider_type.as_str())
    })
}

/// Effort options for a model the profile registry and the provider's own
/// catalog leave undescribed. `None` for anything but the reasoning-required
/// surfaces: guessing a scale for an endpoint that is happy without reasoning
/// would offer levels the provider may reject.
pub(crate) fn fallback_reasoning_effort_config(
    provider_type: &DriverId,
    model: &str,
) -> Option<ReasoningEffortConfig> {
    if !requires_reasoning(provider_type, model) {
        return None;
    }
    Some(ReasoningEffortConfig {
        values: vec![
            effort(ReasoningEffort::Low, "Low"),
            effort(ReasoningEffort::Medium, "Medium"),
            effort(ReasoningEffort::High, "High"),
        ],
        default: ReasoningEffort::Medium,
    })
}

fn effort(value: ReasoningEffort, name: &str) -> ReasoningEffortValue {
    ReasoningEffortValue {
        value,
        name: name.to_string(),
    }
}

/// Whether a failed turn failed because the endpoint mandates reasoning.
///
/// Matched on the provider's message rather than a status code: the drivers
/// flatten provider errors into text, and 400 alone says nothing about the
/// cause. Kept deliberately loose (`reasoning` plus a mandate word) so a
/// reworded upstream message still routes to the same recovery.
pub(crate) fn is_reasoning_required_error(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    if !message.contains("reasoning") {
        return false;
    }
    message.contains("mandatory")
        || message.contains("cannot be disabled")
        || message.contains("can not be disabled")
        || message.contains("is required")
}

/// The effort to retry a mandatory-reasoning failure with, or `None` when the
/// failure is not one this can fix.
///
/// A turn that already carried an effort is not retried: the endpoint rejected
/// a request that named a level, so sending another one only spends a second
/// turn on the same error. The user is pointed at the picker instead.
pub(crate) fn recovery_effort(
    error: &str,
    current_effort: Option<&str>,
    profile_default: Option<&str>,
) -> Option<String> {
    if current_effort.is_some() || !is_reasoning_required_error(error) {
        return None;
    }
    Some(
        profile_default
            .unwrap_or(DEFAULT_REQUIRED_EFFORT)
            .to_string(),
    )
}

/// Set `effort` on an already-built input message, so a turn can be retried
/// with reasoning without rebuilding its content (images, provenance metadata
/// and tags are what a wake or a resumed turn carries, and are not
/// reconstructible from the prompt text).
pub(crate) fn apply_reasoning_effort(input: &mut InputMessage, effort: &str) {
    let Some(effort) = ReasoningEffort::parse(effort) else {
        return;
    };
    let mut controls = input.controls.take().unwrap_or_default();
    controls.reasoning = Some(ReasoningConfig {
        effort: Some(effort),
    });
    input.controls = Some(controls);
}

/// What to tell the user when a mandatory-reasoning failure cannot be repaired
/// automatically, i.e. the turn already named an effort the endpoint rejected.
pub(crate) fn reasoning_error_hint(error: &str, current_effort: Option<&str>) -> Option<String> {
    if !is_reasoning_required_error(error) {
        return None;
    }
    match current_effort {
        Some(effort) => Some(format!(
            "this endpoint rejected reasoning effort `{effort}`; run /effort to pick another level, or /model to switch models"
        )),
        None => Some(
            "this endpoint requires reasoning; run /effort to pick a level, or /model to switch models"
                .to_string(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn muse_requires_reasoning_on_the_gateway_across_tiers_and_releases() {
        let openrouter = DriverId::OpenRouter;
        assert!(requires_reasoning(&openrouter, "meta/muse-spark-1.2"));
        // The id that started this: an OpenRouter-prefixed point release the
        // profile registry has never seen.
        assert!(requires_reasoning(
            &openrouter,
            "meta/muse-spark-1.3-contributor"
        ));
        assert!(requires_reasoning(&openrouter, "meta/muse-spark-1.3:free"));
        assert!(!requires_reasoning(&openrouter, "openai/gpt-5.5"));
        assert!(!requires_reasoning(
            &openrouter,
            "anthropic/claude-opus-4-8"
        ));
        // Meta's own API documents a model-determined default, so its surface
        // keeps upstream's answer rather than being given one here.
        assert!(!requires_reasoning(&DriverId::Meta, "muse-spark-1.2"));
    }

    #[test]
    fn fallback_config_offers_a_scale_only_where_reasoning_is_required() {
        let config = fallback_reasoning_effort_config(
            &DriverId::OpenRouter,
            "meta/muse-spark-1.3-contributor",
        )
        .expect("reasoning-required models expose an effort scale");
        assert_eq!(config.default, ReasoningEffort::Medium);
        assert_eq!(
            config
                .values
                .iter()
                .map(|value| value.value.as_str())
                .collect::<Vec<_>>(),
            vec!["low", "medium", "high"]
        );
        assert!(
            fallback_reasoning_effort_config(&DriverId::OpenRouter, "qwen/qwen3.7-max").is_none()
        );
    }

    #[test]
    fn openrouter_mandatory_reasoning_error_is_recognized() {
        let error = "LLM error: provider 'openrouter': OpenAI Responses API error (400 Bad Request): \
             {\"error\":{\"message\":\"Reasoning is mandatory for this endpoint and cannot be disabled.\",\"code\":400}}";
        assert!(is_reasoning_required_error(error));
        assert!(!is_reasoning_required_error(
            "LLM error: provider 'openrouter': 401 Unauthorized"
        ));
        // A rate-limit message that happens to mention a mandatory field must
        // not be routed into reasoning recovery.
        assert!(!is_reasoning_required_error(
            "provider error: field `model` is mandatory"
        ));
    }

    #[test]
    fn recovery_uses_the_profile_default_then_falls_back_to_medium() {
        let error = "Reasoning is mandatory for this endpoint and cannot be disabled.";
        assert_eq!(
            recovery_effort(error, None, Some("high")).as_deref(),
            Some("high")
        );
        assert_eq!(
            recovery_effort(error, None, None).as_deref(),
            Some(DEFAULT_REQUIRED_EFFORT)
        );
    }

    #[test]
    fn a_turn_that_already_named_an_effort_is_not_retried() {
        let error = "Reasoning is mandatory for this endpoint and cannot be disabled.";
        assert_eq!(recovery_effort(error, Some("low"), Some("medium")), None);
        let hint = reasoning_error_hint(error, Some("low")).expect("rejected effort earns a hint");
        assert!(
            hint.contains("/effort"),
            "hint should point at the picker: {hint}"
        );
    }

    #[test]
    fn applying_an_effort_keeps_the_rest_of_the_message() {
        use everruns_core::{ContentPart, MessageRole};

        let mut input = InputMessage {
            role: MessageRole::User,
            content: vec![ContentPart::text("hello")],
            controls: None,
            metadata: None,
            tags: vec!["wake".to_string()],
        };

        apply_reasoning_effort(&mut input, "medium");

        assert_eq!(
            input
                .controls
                .as_ref()
                .and_then(|controls| controls.reasoning.as_ref())
                .and_then(|reasoning| reasoning.effort),
            Some(ReasoningEffort::Medium)
        );
        assert_eq!(input.content.len(), 1);
        assert_eq!(input.tags, vec!["wake".to_string()]);
    }

    #[test]
    fn unrelated_failures_are_left_alone() {
        assert_eq!(
            recovery_effort("connection reset by peer", None, None),
            None
        );
        assert_eq!(reasoning_error_hint("connection reset by peer", None), None);
    }
}

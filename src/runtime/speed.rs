//! Service-tier ("speed") selection for OpenAI models.
//!
//! Everruns 0.33 (#3925) carries a per-turn `controls.speed` that the OpenAI
//! drivers send as `service_tier`: `flex`, `default`, `priority`, `fast`
//! (OpenAI's newer name for priority) or `ultrafast`. Yolop exposes it as one
//! setting rather than as model-list variants, because the tier a model offers
//! comes from its upstream profile, not from yolop: GPT-6.1 Sol lists Flex,
//! Standard and Fast (2x), GPT-6 Astra adds Ultrafast (6x), and models without
//! a speed config list none. The engine drops a tier the selected model's
//! profile does not list, with a warning, so a global `ultrafast` stays safe
//! when the user switches to a model that cannot serve it.
//!
//! Precedence: `YOLOP_SPEED`, then the `speed` setting. `default` (or unset)
//! sends nothing, which leaves the provider's standard tier in place.

use anyhow::{Result, anyhow};

/// Environment override for the service tier; wins over the setting.
pub const SPEED_ENV: &str = "YOLOP_SPEED";

/// Every tier value the upstream `controls.speed` accepts.
pub const SPEED_VALUES: &[&str] = &["flex", "default", "priority", "fast", "ultrafast"];

/// Normalize and validate a tier name. Returns `Ok(None)` for blank or
/// `default`, which both mean "no override".
pub fn parse_speed(value: &str) -> Result<Option<String>> {
    let value = value.trim().to_ascii_lowercase();
    if value.is_empty() || value == "default" {
        return Ok(None);
    }
    if SPEED_VALUES.contains(&value.as_str()) {
        Ok(Some(value))
    } else {
        Err(anyhow!(
            "unknown speed `{value}`; expected one of: {}",
            SPEED_VALUES.join(", ")
        ))
    }
}

/// The tier to request on the next turn, from the env override and setting.
/// A malformed value is logged and ignored rather than failing the turn: the
/// setter already validates, so this only guards a hand-edited file or env.
pub fn resolve_speed(env: Option<&str>, setting: Option<&str>) -> Option<String> {
    for (source, value) in [(SPEED_ENV, env), ("speed", setting)] {
        let Some(value) = value.filter(|value| !value.trim().is_empty()) else {
            continue;
        };
        match parse_speed(value) {
            Ok(speed) => return speed,
            Err(err) => {
                tracing::warn!(source, error = %err, "ignoring invalid speed");
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_every_upstream_tier() {
        for tier in ["flex", "priority", "fast", "ultrafast"] {
            assert_eq!(parse_speed(tier).unwrap().as_deref(), Some(tier));
        }
        assert_eq!(parse_speed(" Fast ").unwrap().as_deref(), Some("fast"));
    }

    #[test]
    fn default_and_blank_mean_no_override() {
        assert_eq!(parse_speed("default").unwrap(), None);
        assert_eq!(parse_speed("  ").unwrap(), None);
    }

    #[test]
    fn rejects_unknown_tiers() {
        let err = parse_speed("turbo").unwrap_err().to_string();
        assert!(err.contains("ultrafast"), "{err}");
    }

    #[test]
    fn speed_rides_on_the_turn_controls_beside_reasoning() {
        use everruns_core::{
            ContentPart, Controls, InputMessage, ReasoningConfig, RuntimeMessageRole,
        };
        use everruns_provider::ReasoningEffort;
        let message = |controls| InputMessage {
            role: RuntimeMessageRole::User,
            content: vec![ContentPart::text("hi")],
            controls,
            metadata: None,
            tags: vec![],
        };
        let with_effort = message(Some(Controls {
            reasoning: Some(ReasoningConfig {
                effort: ReasoningEffort::parse("high"),
            }),
            ..Default::default()
        }));
        let input = super::super::apply_speed(with_effort, Some("fast".to_string()));
        let controls = input.controls.expect("controls");
        assert_eq!(controls.speed.as_deref(), Some("fast"));
        assert!(controls.reasoning.is_some(), "effort must survive");

        let plain = super::super::apply_speed(message(None), Some("ultrafast".to_string()));
        assert_eq!(
            plain.controls.and_then(|c| c.speed).as_deref(),
            Some("ultrafast")
        );
        assert!(
            super::super::apply_speed(message(None), None)
                .controls
                .is_none()
        );
    }

    #[test]
    fn env_beats_setting_and_invalid_values_are_skipped() {
        assert_eq!(
            resolve_speed(Some("ultrafast"), Some("flex")).as_deref(),
            Some("ultrafast")
        );
        assert_eq!(resolve_speed(None, Some("fast")).as_deref(), Some("fast"));
        // An explicit env `default` turns a configured tier off for this run.
        assert_eq!(resolve_speed(Some("default"), Some("fast")), None);
        // A malformed env value falls through to the setting.
        assert_eq!(
            resolve_speed(Some("turbo"), Some("flex")).as_deref(),
            Some("flex")
        );
        assert_eq!(resolve_speed(None, None), None);
    }
}

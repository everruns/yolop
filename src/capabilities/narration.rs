//! Human-readable, argument-aware narration for yolop-owned tools.
//!
//! everruns falls back to `"Ran {display_name}"` when a tool does not implement
//! [`everruns_core::tools::Tool::narrate`]. These helpers keep transcript lines
//! stable across started/completed phases and include safe argument detail.

use everruns_core::tool_narration::{
    ToolNarrationPhase, arg_str, labeled_phrase, safe_arg_str, truncate,
};
use everruns_core::tool_types::ToolCall;
use serde_json::Value;

/// Present-tense label for started and completed; explicit failure wording.
pub fn stable_labeled(verb: &str, detail: Option<String>, phase: ToolNarrationPhase) -> String {
    let failed = format!("Could not {}", verb.to_lowercase());
    labeled_phrase(verb, verb, &failed, detail, phase)
}

pub fn narrate_get_config(tool_call: &ToolCall, phase: ToolNarrationPhase) -> String {
    let detail = arg_str(&tool_call.arguments, &["key"]).map(|key| truncate(key, 48));
    stable_labeled("Get config", detail, phase)
}

pub fn narrate_set_config(tool_call: &ToolCall, phase: ToolNarrationPhase) -> String {
    stable_labeled("Set config", set_config_detail(&tool_call.arguments), phase)
}

fn set_config_detail(arguments: &Value) -> Option<String> {
    let key = arg_str(arguments, &["key"])?;
    if arguments.get("json").is_some_and(|v| !v.is_null()) {
        return Some(format!("{key}=…"));
    }
    let value = safe_arg_str(arguments, &["value"])?;
    if key.starts_with("tokens.") || key.contains("token") {
        return Some(format!("{key}=…"));
    }
    let display_value: String = match key {
        "attribution" | "proactive_wake" | "wake" => {
            let normalized = value.to_ascii_lowercase();
            match normalized.as_str() {
                "on" | "true" | "yes" | "1" => "true".to_string(),
                "off" | "false" | "no" | "0" => "false".to_string(),
                _ => normalized,
            }
        }
        _ => value.to_string(),
    };
    Some(format!("{key}={display_value}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use everruns_core::tool_types::ToolCall;
    use serde_json::json;

    fn call(arguments: Value) -> ToolCall {
        ToolCall {
            id: "call-1".to_owned(),
            name: "set_config".to_owned(),
            arguments,
        }
    }

    #[test]
    fn set_config_narration_includes_key_and_value() {
        let narration = narrate_set_config(
            &call(json!({ "key": "attribution", "value": "on" })),
            ToolNarrationPhase::Completed,
        );
        assert_eq!(narration, "Set config: attribution=true");
    }

    #[test]
    fn set_config_narration_redacts_tokens() {
        let narration = narrate_set_config(
            &call(json!({ "key": "tokens.openai", "value": "sk-secret" })),
            ToolNarrationPhase::Completed,
        );
        assert_eq!(narration, "Set config: tokens.openai=…");
    }

    #[test]
    fn get_config_narration_uses_bare_verb_without_key() {
        let call = ToolCall {
            id: "call-1".to_owned(),
            name: "get_config".to_owned(),
            arguments: json!({}),
        };
        assert_eq!(
            narrate_get_config(&call, ToolNarrationPhase::Completed),
            "Get config"
        );
    }

    #[test]
    fn stable_labeled_avoids_ran_prefix_on_completed() {
        assert_eq!(
            stable_labeled(
                "Run command",
                Some("/help".to_string()),
                ToolNarrationPhase::Completed
            ),
            "Run command: /help"
        );
    }
}

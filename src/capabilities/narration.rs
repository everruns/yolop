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

/// Everruns `session_tasks` tools otherwise fall back to "Running List Tasks".
pub fn narrate_session_task_tool(
    tool_call: &ToolCall,
    phase: ToolNarrationPhase,
) -> Option<String> {
    match tool_call.name.as_str() {
        "list_tasks" => Some(narrate_list_tasks(tool_call, phase)),
        "get_task" => Some(narrate_get_task(tool_call, phase)),
        "wait_task" => Some(narrate_wait_task(tool_call, phase)),
        "message_task" => Some(narrate_message_task(tool_call, phase)),
        "cancel_task" => Some(narrate_cancel_task(tool_call, phase)),
        _ => None,
    }
}

pub fn narrate_list_tasks(tool_call: &ToolCall, phase: ToolNarrationPhase) -> String {
    let detail = list_tasks_detail(&tool_call.arguments);
    stable_labeled("List tasks", detail, phase)
}

pub fn narrate_get_task(tool_call: &ToolCall, phase: ToolNarrationPhase) -> String {
    let detail = arg_str(&tool_call.arguments, &["task_id"]).map(|id| truncate(id, 48));
    stable_labeled("Get task", detail, phase)
}

pub fn narrate_wait_task(tool_call: &ToolCall, phase: ToolNarrationPhase) -> String {
    let detail = arg_str(&tool_call.arguments, &["task_id"]).map(|id| truncate(id, 48));
    stable_labeled("Wait for task", detail, phase)
}

pub fn narrate_message_task(tool_call: &ToolCall, phase: ToolNarrationPhase) -> String {
    let detail = arg_str(&tool_call.arguments, &["task_id"]).map(|id| truncate(id, 48));
    stable_labeled("Message task", detail, phase)
}

pub fn narrate_cancel_task(tool_call: &ToolCall, phase: ToolNarrationPhase) -> String {
    let detail = arg_str(&tool_call.arguments, &["task_id"]).map(|id| truncate(id, 48));
    stable_labeled("Cancel task", detail, phase)
}

/// Everruns `spawn_background` otherwise falls back to "Running Spawn Background".
pub fn narrate_spawn_background(tool_call: &ToolCall, phase: ToolNarrationPhase) -> String {
    stable_labeled(
        "Spawn background",
        spawn_background_detail(&tool_call.arguments),
        phase,
    )
}

fn list_tasks_detail(arguments: &Value) -> Option<String> {
    let state = arg_str(arguments, &["state"]);
    let kind = arg_str(arguments, &["kind"]);
    match (state, kind) {
        (Some(state), Some(kind)) => Some(format!("{state}, {kind}")),
        (Some(state), None) => Some(state.to_string()),
        (None, Some(kind)) => Some(kind.to_string()),
        (None, None) => None,
    }
}

fn spawn_background_detail(arguments: &Value) -> Option<String> {
    if let Some(title) = arg_str(arguments, &["title"]) {
        return Some(truncate(title, 48));
    }
    let tool = arg_str(arguments, &["tool"])?;
    let scheduled = arguments
        .get("schedule")
        .is_some_and(|value| !value.is_null());
    let command = arguments
        .get("args")
        .and_then(|args| arg_str(args, &["command", "commands"]))
        .map(|command| truncate(command, 40));
    match (scheduled, command) {
        (true, Some(command)) => Some(format!("schedule {tool} `{command}`")),
        (true, None) => Some(format!("schedule {tool}")),
        (false, Some(command)) => Some(format!("{tool} `{command}`")),
        (false, None) => Some(tool.to_string()),
    }
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

    #[test]
    fn wait_task_narration_uses_human_verb_and_task_id() {
        let call = ToolCall {
            id: "call-1".to_owned(),
            name: "wait_task".to_owned(),
            arguments: json!({ "task_id": "task_ci_watch" }),
        };
        assert_eq!(
            narrate_wait_task(&call, ToolNarrationPhase::Started),
            "Wait for task: task_ci_watch"
        );
    }

    #[test]
    fn list_tasks_narration_includes_filters() {
        let call = ToolCall {
            id: "call-1".to_owned(),
            name: "list_tasks".to_owned(),
            arguments: json!({ "state": "running", "kind": "background_tool" }),
        };
        assert_eq!(
            narrate_list_tasks(&call, ToolNarrationPhase::Completed),
            "List tasks: running, background_tool"
        );
    }

    #[test]
    fn spawn_background_narration_prefers_title_then_command() {
        let titled = ToolCall {
            id: "call-1".to_owned(),
            name: "spawn_background".to_owned(),
            arguments: json!({
                "tool": "bash",
                "title": "Wait for CI",
                "args": { "command": "gh pr checks --watch" }
            }),
        };
        assert_eq!(
            narrate_spawn_background(&titled, ToolNarrationPhase::Started),
            "Spawn background: Wait for CI"
        );

        let with_command = ToolCall {
            id: "call-2".to_owned(),
            name: "spawn_background".to_owned(),
            arguments: json!({
                "tool": "bash",
                "args": { "command": "gh pr checks --watch" }
            }),
        };
        assert_eq!(
            narrate_spawn_background(&with_command, ToolNarrationPhase::Completed),
            "Spawn background: bash `gh pr checks --watch`"
        );
    }

    #[test]
    fn session_task_dispatcher_covers_known_tools() {
        let call = ToolCall {
            id: "call-1".to_owned(),
            name: "get_task".to_owned(),
            arguments: json!({ "task_id": "task_1" }),
        };
        assert_eq!(
            narrate_session_task_tool(&call, ToolNarrationPhase::Started).as_deref(),
            Some("Get task: task_1")
        );
        let unknown = ToolCall {
            id: "call-2".to_owned(),
            name: "mystery".to_owned(),
            arguments: json!({}),
        };
        assert!(narrate_session_task_tool(&unknown, ToolNarrationPhase::Started).is_none());
    }
}

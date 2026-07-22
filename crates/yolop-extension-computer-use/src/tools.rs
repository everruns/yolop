//! The ten official Computer Use methods: their upstream MCP names, exact input
//! schemas, and descriptions, plus the `press_key` alias normalization.
//!
//! Ported verbatim from `codex-computer-use-mcp`'s `src/tools.ts`
//! (`EXPECTED_OFFICIAL_INPUT_SCHEMAS`, `OFFICIAL_TOOL_METADATA`,
//! `normalizeKeyExpression`). The schemas are the drift-check baseline: at
//! dispatch time the broker compares the app-server's advertised inventory
//! against these and fails closed on mismatch, so a silent upstream schema
//! change can never reach the model.

use serde_json::{Value, json};

/// The upstream MCP method names, in a stable order. The yolop-facing tool
/// names are these prefixed with `computer_use_` (see `main.rs`).
pub const OFFICIAL_METHODS: &[&str] = &[
    "list_apps",
    "get_app_state",
    "click",
    "perform_secondary_action",
    "set_value",
    "select_text",
    "scroll",
    "drag",
    "press_key",
    "type_text",
];

const APP_DESC: &str = "App name, full app path, or unambiguous bundle identifier";

/// Exact upstream MCP input schema for a method, or `Value::Null` for an
/// unknown method. Mirrors `EXPECTED_OFFICIAL_INPUT_SCHEMAS`.
pub fn input_schema(method: &str) -> Value {
    match method {
        "list_apps" => json!({ "type": "object", "properties": {}, "additionalProperties": false }),
        "get_app_state" => json!({
            "type": "object",
            "properties": { "app": { "description": APP_DESC, "type": "string" } },
            "required": ["app"],
            "additionalProperties": false,
        }),
        "click" => json!({
            "type": "object",
            "properties": {
                "app": { "description": APP_DESC, "type": "string" },
                "click_count": { "description": "Number of clicks. Defaults to 1", "type": "integer" },
                "element_index": { "description": "Element index to click", "type": "string" },
                "mouse_button": { "description": "Mouse button to click. Defaults to left.", "enum": ["left", "right", "middle"], "type": "string" },
                "x": { "description": "X coordinate in screenshot pixel coordinates", "type": "number" },
                "y": { "description": "Y coordinate in screenshot pixel coordinates", "type": "number" },
            },
            "required": ["app"],
            "additionalProperties": false,
        }),
        "perform_secondary_action" => json!({
            "type": "object",
            "properties": {
                "action": { "description": "Secondary accessibility action name", "type": "string" },
                "app": { "description": APP_DESC, "type": "string" },
                "element_index": { "description": "Element identifier", "type": "string" },
            },
            "required": ["app", "element_index", "action"],
            "additionalProperties": false,
        }),
        "set_value" => json!({
            "type": "object",
            "properties": {
                "app": { "description": APP_DESC, "type": "string" },
                "element_index": { "description": "Element identifier", "type": "string" },
                "value": { "description": "Value to assign", "type": "string" },
            },
            "required": ["app", "element_index", "value"],
            "additionalProperties": false,
        }),
        "select_text" => json!({
            "type": "object",
            "properties": {
                "app": { "description": "App name or bundle identifier", "type": "string" },
                "element_index": { "description": "Text element identifier", "type": "string" },
                "prefix": { "description": "Optional text immediately before the target, used to disambiguate repeated matches", "type": "string" },
                "selection": { "description": "Whether to select the text or place the cursor before or after it. Defaults to text.", "enum": ["text", "cursor_before", "cursor_after"], "type": "string" },
                "suffix": { "description": "Optional text immediately after the target, used to disambiguate repeated matches", "type": "string" },
                "text": { "description": "Target text as shown in the accessibility tree", "type": "string" },
            },
            "required": ["app", "element_index", "text"],
            "additionalProperties": false,
        }),
        "scroll" => json!({
            "type": "object",
            "properties": {
                "app": { "description": APP_DESC, "type": "string" },
                "direction": { "description": "Scroll direction: up, down, left, or right", "type": "string" },
                "element_index": { "description": "Element identifier", "type": "string" },
                "pages": { "description": "Number of pages to scroll. Fractional values are supported. Defaults to 1", "type": "number" },
            },
            "required": ["app", "element_index", "direction"],
            "additionalProperties": false,
        }),
        "drag" => json!({
            "type": "object",
            "properties": {
                "app": { "description": APP_DESC, "type": "string" },
                "from_x": { "description": "Start X coordinate", "type": "number" },
                "from_y": { "description": "Start Y coordinate", "type": "number" },
                "to_x": { "description": "End X coordinate", "type": "number" },
                "to_y": { "description": "End Y coordinate", "type": "number" },
            },
            "required": ["app", "from_x", "from_y", "to_x", "to_y"],
            "additionalProperties": false,
        }),
        "press_key" => json!({
            "type": "object",
            "properties": {
                "app": { "description": APP_DESC, "type": "string" },
                "key": { "description": "Key or key combination to press", "type": "string" },
            },
            "required": ["app", "key"],
            "additionalProperties": false,
        }),
        "type_text" => json!({
            "type": "object",
            "properties": {
                "app": { "description": APP_DESC, "type": "string" },
                "text": { "description": "Literal text to type", "type": "string" },
            },
            "required": ["app", "text"],
            "additionalProperties": false,
        }),
        _ => Value::Null,
    }
}

/// Exact upstream description for a method (`OFFICIAL_TOOL_METADATA`). The
/// descriptions the model sees live in `plugin.json`; this is the drift-check
/// baseline the manifest test holds them to, so it is test-only in the binary.
#[cfg_attr(not(test), allow(dead_code))]
pub fn description(method: &str) -> &'static str {
    match method {
        "list_apps" => {
            "List the apps on this computer. Returns the set of apps that are currently running, as well as any that have been used in the last 14 days, including details on usage frequency"
        }
        "get_app_state" => {
            "Start an app use session if needed, then get the state of the app's key window and return a screenshot and accessibility tree. This must be called once per assistant turn before interacting with the app"
        }
        "click" => "Click an element by index or pixel coordinates from screenshot",
        "perform_secondary_action" => {
            "Invoke a secondary accessibility action exposed by an element"
        }
        "set_value" => "Set the value of a settable accessibility element",
        "select_text" => {
            "Select text inside a text element, or place the text cursor before or after it. Provide text exactly as it appears in the accessibility tree, including any Markdown formatting. If the text is not unique, provide surrounding prefix or suffix text to disambiguate it."
        }
        "scroll" => "Scroll an element in a direction by a number of pages",
        "drag" => "Drag from one point to another using pixel coordinates",
        "press_key" => {
            "Press a key or key-combination on the keyboard, including modifier and navigation keys.\n  - This supports xdotool's `key` syntax.\n  - Examples: \"a\", \"Return\", \"Tab\", \"super+c\", \"Up\", \"KP_0\" (for the numpad 0 key)."
        }
        "type_text" => "Type literal text using keyboard input",
        _ => "",
    }
}

/// The two read-only inspection methods. yolop marks their tools `never_defer`
/// so their schemas are always loaded — mirroring Pi's "progressive
/// activation", which keeps only the inspection tools active until an app has
/// been inspected. The `never_defer` flags live in `plugin.json`; this is the
/// baseline the manifest test checks them against.
#[cfg_attr(not(test), allow(dead_code))]
pub const INSPECTION_METHODS: &[&str] = &["list_apps", "get_app_state"];

fn key_alias(part: &str) -> Option<&'static str> {
    Some(match part.to_ascii_lowercase().as_str() {
        "cmd" | "command" | "meta" => "Meta_L",
        "ctrl" | "control" => "Control_L",
        "shift" => "Shift_L",
        "alt" | "option" => "Alt_L",
        "enter" | "return" => "Return",
        "esc" | "escape" => "Escape",
        "backspace" => "BackSpace",
        "delete" => "Delete",
        "pageup" => "Page_Up",
        "pagedown" => "Page_Down",
        "arrowup" => "Up",
        "arrowdown" => "Down",
        "arrowleft" => "Left",
        "arrowright" => "Right",
        _ => return None,
    })
}

/// Normalize a `press_key` expression to the xdotool key syntax the official
/// tool expects (`normalizeKeyExpression`): map common aliases, lower-case a
/// bare single upper-case letter, keep everything else verbatim.
pub fn normalize_key_expression(value: &str) -> String {
    value
        .split('+')
        .map(|part| {
            let trimmed = part.trim();
            if let Some(alias) = key_alias(trimmed) {
                return alias.to_string();
            }
            if trimmed.len() == 1 && trimmed.chars().all(|c| c.is_ascii_uppercase()) {
                return trimmed.to_ascii_lowercase();
            }
            trimmed.to_string()
        })
        .collect::<Vec<_>>()
        .join("+")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ten_official_methods_all_have_schema_and_description() {
        assert_eq!(OFFICIAL_METHODS.len(), 10);
        for &m in OFFICIAL_METHODS {
            assert!(input_schema(m).is_object(), "{m} schema");
            assert!(!description(m).is_empty(), "{m} description");
        }
        assert!(input_schema("nope").is_null());
    }

    #[test]
    fn key_normalization_matches_upstream() {
        assert_eq!(normalize_key_expression("cmd+c"), "Meta_L+c");
        assert_eq!(normalize_key_expression("A"), "a");
        assert_eq!(normalize_key_expression("Return"), "Return");
        assert_eq!(normalize_key_expression("super+c"), "super+c");
        assert_eq!(
            normalize_key_expression("ctrl+ Shift +t"),
            "Control_L+Shift_L+t"
        );
    }
}

//! YEP (yolop extension protocol) wire types. See `specs/extensions.md`.
//!
//! Conventions (adopted from the mira eval protocol): newline-delimited JSON
//! over stdio, field-based message classification (`method` present ⇒
//! request/notification, absent ⇒ response), `MAJOR.MINOR` versioning with
//! major-match compatibility, and hard forward-compat rules — every payload
//! parses leniently (no deny-unknown-fields) and defaults missing fields.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

/// The protocol version this yolop speaks.
pub const PROTOCOL_VERSION: &str = "1.0";

/// Compatibility is by major: `1.x` talks to `1.y`, refuses `2.x`.
pub fn version_compatible(server_version: &str) -> bool {
    fn major(v: &str) -> Option<&str> {
        v.split('.').next().filter(|m| !m.is_empty())
    }
    match (major(PROTOCOL_VERSION), major(server_version)) {
        (Some(ours), Some(theirs)) => ours == theirs,
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Envelope

/// One parsed wire line, classified by fields (never by direction):
/// a line bearing `method` is a request (with `id`) or notification (without);
/// only a `method`-less line is a response.
#[derive(Debug)]
pub enum Incoming {
    Response {
        id: u64,
        result: Result<Value, ErrorObject>,
    },
    Request {
        id: u64,
        method: String,
        params: Value,
    },
    Notification {
        method: String,
        params: Value,
    },
}

/// Classify one wire line. `None` for lines that are not valid envelopes
/// (the caller logs and skips them; a bad line never kills the connection).
pub fn classify_line(line: &str) -> Option<Incoming> {
    let value: Value = serde_json::from_str(line).ok()?;
    let obj = value.as_object()?;
    let id = obj.get("id").and_then(Value::as_u64);
    let method = obj.get("method").and_then(Value::as_str);
    let params = obj.get("params").cloned().unwrap_or(Value::Null);
    match (method, id) {
        (Some(method), Some(id)) => Some(Incoming::Request {
            id,
            method: method.to_string(),
            params,
        }),
        (Some(method), None) => Some(Incoming::Notification {
            method: method.to_string(),
            params,
        }),
        (None, Some(id)) => {
            let result = match obj.get("error") {
                Some(error) => Err(serde_json::from_value(error.clone()).unwrap_or_else(|_| {
                    ErrorObject::message("malformed error object on the wire")
                })),
                None => Ok(obj.get("result").cloned().unwrap_or(Value::Null)),
            };
            Some(Incoming::Response { id, result })
        }
        (None, None) => None,
    }
}

pub fn request_line(id: u64, method: &str, params: &Value) -> String {
    let mut obj = Map::new();
    obj.insert("id".into(), json!(id));
    obj.insert("method".into(), json!(method));
    if !params.is_null() {
        obj.insert("params".into(), params.clone());
    }
    Value::Object(obj).to_string()
}

pub fn notification_line(method: &str, params: &Value) -> String {
    let mut obj = Map::new();
    obj.insert("method".into(), json!(method));
    if !params.is_null() {
        obj.insert("params".into(), params.clone());
    }
    Value::Object(obj).to_string()
}

pub fn response_error_line(id: u64, error: &ErrorObject) -> String {
    json!({ "id": id, "error": error }).to_string()
}

/// JSON-RPC-shaped error object. Everything beyond `message` is optional and
/// defaulted, so a peer that sends bare `{ "message": … }` still parses.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorObject {
    #[serde(default)]
    pub code: i64,
    pub message: String,
    #[serde(default)]
    pub retryable: bool,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub data: Value,
}

impl ErrorObject {
    pub fn message(message: impl Into<String>) -> Self {
        Self {
            code: 0,
            message: message.into(),
            retryable: false,
            data: Value::Null,
        }
    }

    pub fn method_not_found(method: &str) -> Self {
        Self {
            code: -32601,
            message: format!("method not supported by this host: {method}"),
            retryable: false,
            data: Value::Null,
        }
    }
}

// ---------------------------------------------------------------------------
// initialize

/// Host → server `initialize` params.
#[derive(Debug, Clone, Serialize)]
pub struct InitializeParams {
    pub protocol_version: String,
    pub session_id: String,
    pub workspace_root: String,
    pub config: Value,
    /// Host feature tokens the server may rely on (`streaming`, `cancel`, …).
    pub capabilities: Vec<String>,
}

/// Server → host `initialize` result. Lenient: unknown fields ignored,
/// missing fields defaulted, per the forward-compat rules.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct InitializeResult {
    #[serde(default)]
    pub protocol_version: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub capability_params: CapabilityParams,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct CapabilityParams {
    #[serde(default)]
    pub prompt: Option<PromptParams>,
    /// Tool names the server actually serves this run. The definitions
    /// (description/schema/policy) live in the package manifest; the
    /// handshake may only narrow that set (see D4 manifest clamp).
    #[serde(default)]
    pub tools: Vec<ServedTool>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct PromptParams {
    /// Static system-prompt contribution.
    #[serde(default, rename = "static")]
    pub static_text: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServedTool {
    pub name: String,
}

// ---------------------------------------------------------------------------
// tool/call

#[derive(Debug, Clone, Serialize)]
pub struct ToolCallParams {
    pub tool_call_id: String,
    pub name: String,
    pub args: Value,
}

/// Payload of a `tool/update` notification, correlated to its in-flight
/// `tool/call` by `request_id` (a notification cannot carry the envelope
/// `id` — that field would classify the line as a response).
#[derive(Debug, Clone, Deserialize)]
pub struct ToolUpdateParams {
    #[serde(default)]
    pub request_id: u64,
    #[serde(default)]
    pub output: String,
}

// ---------------------------------------------------------------------------
// hook/fire

/// Host → server `hook/fire` params: a subscribed lifecycle event fired for
/// one tool call (any tool, not just the extension's own).
#[derive(Debug, Clone, Serialize)]
pub struct HookFireParams<'a> {
    /// `pre_tool_use` or `post_tool_use`.
    pub event: &'a str,
    pub tool_name: &'a str,
    pub args: &'a Value,
}

/// Server → host `hook/fire` result. Lenient/defaulted: a bare `{}` means
/// "allow, unchanged".
#[derive(Debug, Clone, Default, Deserialize)]
pub struct HookDecision {
    /// `true` blocks the tool call (pre_tool_use only).
    #[serde(default)]
    pub block: bool,
    /// Message shown to the model/audit log when blocking.
    #[serde(default)]
    pub reason: String,
    /// Optional user-facing message when blocking.
    #[serde(default)]
    pub user_message: Option<String>,
}

// ---------------------------------------------------------------------------
// prompt/contribution

/// Server → host `prompt/contribution` result: a dynamic system-prompt
/// contribution recomputed per turn (only when the manifest declares
/// `dynamic_prompt`).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PromptContribution {
    #[serde(default)]
    pub text: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classification_is_field_based() {
        match classify_line(r#"{"id":3,"method":"tool/call","params":{}}"#) {
            Some(Incoming::Request { id: 3, method, .. }) => assert_eq!(method, "tool/call"),
            other => panic!("expected request, got {other:?}"),
        }
        match classify_line(r#"{"method":"tool/update","params":{"request_id":3}}"#) {
            Some(Incoming::Notification { method, .. }) => assert_eq!(method, "tool/update"),
            other => panic!("expected notification, got {other:?}"),
        }
        match classify_line(r#"{"id":3,"result":{"ok":true}}"#) {
            Some(Incoming::Response {
                id: 3,
                result: Ok(v),
            }) => assert_eq!(v["ok"], true),
            other => panic!("expected response, got {other:?}"),
        }
        assert!(classify_line(r#"{"neither":true}"#).is_none());
        assert!(classify_line("not json").is_none());
    }

    #[test]
    fn bare_error_message_parses_with_defaults() {
        match classify_line(r#"{"id":1,"error":{"message":"boom"}}"#) {
            Some(Incoming::Response {
                result: Err(err), ..
            }) => {
                assert_eq!(err.message, "boom");
                assert_eq!(err.code, 0);
                assert!(!err.retryable);
            }
            other => panic!("expected error response, got {other:?}"),
        }
    }

    #[test]
    fn initialize_result_ignores_unknown_and_defaults_missing() {
        let result: InitializeResult = serde_json::from_value(json!({
            "protocol_version": "1.3",
            "name": "lsp",
            "future_field": {"x": 1},
            "capability_params": { "tools": [{"name": "lsp_definition", "extra": true}] }
        }))
        .expect("lenient parse");
        assert_eq!(result.protocol_version, "1.3");
        assert_eq!(result.capability_params.tools[0].name, "lsp_definition");
        assert!(result.capability_params.prompt.is_none());
    }

    #[test]
    fn version_compat_is_by_major() {
        assert!(version_compatible("1.0"));
        assert!(version_compatible("1.7"));
        assert!(!version_compatible("2.0"));
        assert!(!version_compatible(""));
    }
}

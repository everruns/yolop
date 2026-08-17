//! Provider-independent validation for model-authored tool arguments.
//!
//! Provider constrained decoding is helpful but cannot be the execution
//! boundary: not every provider supports it, and deferred tools advertise a
//! permissive stub. This hook validates against the authoritative full schema
//! immediately before execution and returns a compact, value-free correction.

use async_trait::async_trait;
use everruns_core::ToolContext;
use everruns_core::tool_hooks::{PreToolUseDecision, PreToolUseHook};
use everruns_core::{Capability, CapabilityStatus};
use everruns_provider::{ToolCall, ToolDefinition};
use jsonschema::error::ValidationErrorKind;
use jsonschema::{Draft, Validator};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

pub(crate) const TOOL_ARGUMENT_VALIDATION_CAPABILITY_ID: &str = "yolop_tool_argument_validation";
const VALIDATOR_CACHE_LIMIT: usize = 128;
const EXPECTED_SHAPE_DEPTH: usize = 3;
const EXPECTED_OBJECT_PROPERTIES: usize = 32;
const MAX_ARGUMENT_BYTES: usize = 1024 * 1024;
const MAX_DIAGNOSTIC_BYTES: usize = 8 * 1024;
const MAX_DIAGNOSTIC_STRING_CHARS: usize = 300;

pub(crate) struct ToolArgumentValidationCapability;

#[async_trait]
impl Capability for ToolArgumentValidationCapability {
    fn id(&self) -> &str {
        TOOL_ARGUMENT_VALIDATION_CAPABILITY_ID
    }

    fn name(&self) -> &str {
        "Tool Argument Validation"
    }

    fn description(&self) -> &str {
        "Validates tool arguments against their full JSON Schema before execution and returns structured correction guidance."
    }

    fn status(&self) -> CapabilityStatus {
        CapabilityStatus::Available
    }

    fn category(&self) -> Option<&str> {
        Some("Guardrails")
    }

    fn is_guardrail(&self) -> bool {
        true
    }

    fn pre_tool_use_hooks(&self) -> Vec<Arc<dyn PreToolUseHook>> {
        vec![Arc::new(ToolArgumentValidationHook::default())]
    }
}

#[derive(Default)]
struct ToolArgumentValidationHook {
    validators: Mutex<HashMap<[u8; 32], Arc<Validator>>>,
}

impl ToolArgumentValidationHook {
    fn validator(&self, schema: &Value) -> Result<Arc<Validator>, String> {
        let encoded = serde_json::to_vec(schema).map_err(|error| error.to_string())?;
        let key: [u8; 32] = Sha256::digest(encoded).into();
        if let Some(validator) = self
            .validators
            .lock()
            .expect("tool validator cache lock")
            .get(&key)
            .cloned()
        {
            return Ok(validator);
        }

        let validator = Arc::new(
            Validator::options()
                .with_draft(Draft::Draft202012)
                .build(schema)
                .map_err(|error| error.to_string())?,
        );
        let mut cache = self.validators.lock().expect("tool validator cache lock");
        // Extension-provided schemas can vary. Bound persistent memory while
        // still validating one-off schemas with the locally compiled value.
        if cache.len() < VALIDATOR_CACHE_LIMIT {
            cache.insert(key, validator.clone());
        }
        Ok(validator)
    }
}

#[async_trait]
impl PreToolUseHook for ToolArgumentValidationHook {
    async fn before_exec(
        &self,
        tool_call: ToolCall,
        tool_def: &ToolDefinition,
        _context: &ToolContext,
    ) -> PreToolUseDecision {
        if serde_json::to_vec(&tool_call.arguments)
            .is_ok_and(|arguments| arguments.len() > MAX_ARGUMENT_BYTES)
        {
            return PreToolUseDecision::Block {
                reason: oversized_argument_diagnostic(tool_def.name(), &tool_call.arguments),
                tool_call,
                user_message: None,
            };
        }
        let schema = tool_def.full_parameters();
        let validator = match self.validator(schema) {
            Ok(validator) => validator,
            Err(error) => {
                // Tool definitions are trusted host/extension configuration. A
                // broken schema must be observable without disabling the tool;
                // its executor remains the final validation boundary.
                tracing::warn!(
                    tool = tool_def.name(),
                    error = %error,
                    "skipping pre-execution validation for invalid tool schema"
                );
                return PreToolUseDecision::Continue(tool_call);
            }
        };

        let Some(error) = validator.iter_errors(&tool_call.arguments).next() else {
            return PreToolUseDecision::Continue(tool_call);
        };
        let reason = validation_diagnostic(tool_def.name(), schema, &tool_call.arguments, &error);
        PreToolUseDecision::Block {
            tool_call,
            reason,
            user_message: None,
        }
    }
}

fn validation_diagnostic(
    tool_name: &str,
    root_schema: &Value,
    arguments: &Value,
    error: &jsonschema::ValidationError<'_>,
) -> String {
    let mut path = error.instance_path().as_str().to_string();
    let mut missing = false;
    if let ValidationErrorKind::Required { property } = error.kind()
        && let Some(property) = property.as_str()
    {
        path.push('/');
        path.push_str(&escape_pointer_segment(property));
        missing = true;
    }
    let (safe_path, target_schema) = schema_at_instance_path(root_schema, &path);
    let received = if missing {
        "missing"
    } else {
        arguments
            .pointer(&safe_path)
            .map(json_type)
            .unwrap_or("missing")
    };

    let diagnostic = json!({
        "error": "invalid_tool_arguments",
        "tool": truncate_chars(tool_name, 128),
        "path": if safe_path.is_empty() { "/".to_string() } else { truncate_chars(&safe_path, 512) },
        "message": validation_message(error.kind()),
        "expected": expected_shape(target_schema, 0),
        "received": received,
        "retryable": true,
        "correction": "Correct the arguments to the expected shape and call this tool once more."
    });
    let encoded = serde_json::to_string(&diagnostic)
        .expect("validation diagnostic contains only JSON values");
    if encoded.len() <= MAX_DIAGNOSTIC_BYTES {
        return encoded;
    }
    serde_json::to_string(&json!({
        "error": "invalid_tool_arguments",
        "tool": truncate_chars(tool_name, 128),
        "path": if safe_path.is_empty() { "/".to_string() } else { truncate_chars(&safe_path, 512) },
        "message": validation_message(error.kind()),
        "expected": { "type": schema_type(target_schema) },
        "received": received,
        "retryable": true,
        "correction": "Correct the arguments to the expected shape and call this tool once more."
    }))
    .expect("compact validation diagnostic contains only JSON values")
}

fn oversized_argument_diagnostic(tool_name: &str, arguments: &Value) -> String {
    serde_json::to_string(&json!({
        "error": "tool_arguments_too_large",
        "tool": truncate_chars(tool_name, 128),
        "path": "/",
        "message": "tool arguments exceed the 1 MiB validation limit",
        "received": json_type(arguments),
        "retryable": false,
        "correction": "Reduce the argument payload before calling this tool again."
    }))
    .expect("oversized diagnostic contains only JSON values")
}

fn validation_message(kind: &ValidationErrorKind) -> &'static str {
    match kind {
        ValidationErrorKind::Required { .. } => "a required argument is missing",
        ValidationErrorKind::Type { .. } => "an argument has the wrong JSON type",
        ValidationErrorKind::AdditionalProperties { .. } => {
            "the object contains an unsupported argument"
        }
        ValidationErrorKind::Enum { .. } => "an argument is not one of the allowed values",
        ValidationErrorKind::MinItems { .. } => "an array has too few items",
        ValidationErrorKind::MaxItems { .. } => "an array has too many items",
        ValidationErrorKind::MinLength { .. } => "a string is too short",
        ValidationErrorKind::MaxLength { .. } => "a string is too long",
        _ => "the arguments do not match the tool schema",
    }
}

/// Follow only schema-declared properties and array indices. If an input path
/// contains an unknown key, stop at its trusted parent so the diagnostic never
/// reflects model-authored property names.
fn schema_at_instance_path<'a>(schema: &'a Value, path: &str) -> (String, &'a Value) {
    let mut current = schema;
    let mut safe_segments = Vec::new();
    for raw in path
        .split('/')
        .skip(1)
        .filter(|segment| !segment.is_empty())
    {
        let segment = raw.replace("~1", "/").replace("~0", "~");
        if current.get("type").and_then(Value::as_str) == Some("array")
            && segment.parse::<usize>().is_ok()
        {
            let Some(items) = current.get("items") else {
                break;
            };
            current = items;
            safe_segments.push(segment);
            continue;
        }
        let Some(property) = current
            .get("properties")
            .and_then(Value::as_object)
            .and_then(|properties| properties.get(&segment))
        else {
            break;
        };
        current = property;
        safe_segments.push(segment);
    }
    let safe_path = safe_segments
        .iter()
        .map(|segment| format!("/{}", escape_pointer_segment(segment)))
        .collect::<String>();
    (safe_path, current)
}

fn expected_shape(schema: &Value, depth: usize) -> Value {
    if depth >= EXPECTED_SHAPE_DEPTH {
        return json!({ "type": schema_type(schema) });
    }
    let mut shape = Map::new();
    shape.insert("type".to_string(), Value::String(schema_type(schema)));
    for keyword in [
        "minItems",
        "maxItems",
        "minLength",
        "maxLength",
        "minimum",
        "maximum",
    ] {
        if let Some(value) = schema.get(keyword) {
            shape.insert(keyword.to_string(), value.clone());
        }
    }
    if let Some(description) = schema.get("description").and_then(Value::as_str) {
        shape.insert(
            "description".to_string(),
            Value::String(truncate_chars(description, MAX_DIAGNOSTIC_STRING_CHARS)),
        );
    }
    if let Some(values) = schema.get("enum").and_then(Value::as_array) {
        shape.insert(
            "enum".to_string(),
            Value::Array(values.iter().take(32).map(bounded_schema_value).collect()),
        );
    }
    if let Some(items) = schema.get("items") {
        shape.insert("items".to_string(), expected_shape(items, depth + 1));
    }
    if let Some(required) = schema.get("required").and_then(Value::as_array) {
        shape.insert(
            "required".to_string(),
            Value::Array(required.iter().take(32).map(bounded_schema_value).collect()),
        );
    }
    if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
        let properties = properties
            .iter()
            .take(EXPECTED_OBJECT_PROPERTIES)
            .map(|(name, property)| {
                (
                    truncate_chars(name, MAX_DIAGNOSTIC_STRING_CHARS),
                    expected_shape(property, depth + 1),
                )
            })
            .collect();
        shape.insert("properties".to_string(), Value::Object(properties));
    }
    Value::Object(shape)
}

fn schema_type(schema: &Value) -> String {
    match schema.get("type") {
        Some(Value::String(value)) => truncate_chars(value, 64),
        Some(Value::Array(values)) => values
            .iter()
            .filter_map(Value::as_str)
            .take(8)
            .collect::<Vec<_>>()
            .join(" | "),
        _ if schema.get("anyOf").is_some() => "anyOf".to_string(),
        _ => "schema".to_string(),
    }
}

fn bounded_schema_value(value: &Value) -> Value {
    match value {
        Value::String(value) => Value::String(truncate_chars(value, MAX_DIAGNOSTIC_STRING_CHARS)),
        Value::Null | Value::Bool(_) | Value::Number(_) => value.clone(),
        Value::Array(_) | Value::Object(_) => Value::String(schema_type(value)),
    }
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let truncated = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_some() {
        format!("{truncated}…")
    } else {
        truncated
    }
}

fn json_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(number) if number.is_i64() || number.is_u64() => "integer",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn escape_pointer_segment(segment: &str) -> String {
    segment.replace('~', "~0").replace('/', "~1")
}

#[cfg(test)]
mod tests {
    use super::*;
    use everruns_provider::typed_id::SessionId;
    use everruns_provider::{BuiltinTool, DeferrablePolicy, ToolHints, ToolPolicy};

    fn checkpoint_schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "missing_evidence": {
                    "type": "array",
                    "description": "Evidence still needed as an array of strings.",
                    "maxItems": 6,
                    "items": { "type": "string" }
                }
            },
            "required": ["missing_evidence"],
            "additionalProperties": false
        })
    }

    fn tool_definition(parameters: Value, full_parameters: Option<Value>) -> ToolDefinition {
        ToolDefinition::Builtin(BuiltinTool {
            name: "progress_checkpoint".to_string(),
            display_name: None,
            description: "test".to_string(),
            parameters,
            policy: ToolPolicy::Auto,
            category: None,
            deferrable: DeferrablePolicy::Automatic,
            hints: ToolHints::default(),
            full_parameters,
        })
    }

    fn call(arguments: Value) -> ToolCall {
        ToolCall {
            id: "call-1".to_string(),
            name: "progress_checkpoint".to_string(),
            arguments,
        }
    }

    #[tokio::test]
    async fn wrong_type_is_blocked_with_structured_value_free_correction() {
        let hook = ToolArgumentValidationHook::default();
        let secret = "still need private-token-value";
        let decision = hook
            .before_exec(
                call(json!({ "missing_evidence": secret })),
                &tool_definition(checkpoint_schema(), None),
                &ToolContext::new(SessionId::new()),
            )
            .await;

        let PreToolUseDecision::Block { reason, .. } = decision else {
            panic!("malformed arguments must be blocked");
        };
        let diagnostic: Value = serde_json::from_str(&reason).expect("structured correction");
        assert_eq!(diagnostic["error"], "invalid_tool_arguments");
        assert_eq!(diagnostic["path"], "/missing_evidence");
        assert_eq!(diagnostic["expected"]["type"], "array");
        assert_eq!(diagnostic["expected"]["items"]["type"], "string");
        assert_eq!(diagnostic["received"], "string");
        assert_eq!(diagnostic["retryable"], true);
        assert!(!reason.contains(secret), "diagnostic must not echo values");
    }

    #[tokio::test]
    async fn validation_uses_full_schema_when_advertised_schema_is_deferred() {
        let hook = ToolArgumentValidationHook::default();
        let definition = tool_definition(
            json!({ "type": "object", "additionalProperties": true }),
            Some(checkpoint_schema()),
        );

        let decision = hook
            .before_exec(
                call(json!({ "missing_evidence": "one string" })),
                &definition,
                &ToolContext::new(SessionId::new()),
            )
            .await;

        assert!(matches!(decision, PreToolUseDecision::Block { .. }));
    }

    #[tokio::test]
    async fn valid_arguments_continue_unchanged() {
        let hook = ToolArgumentValidationHook::default();
        let original = call(json!({ "missing_evidence": ["one check"] }));
        let decision = hook
            .before_exec(
                original.clone(),
                &tool_definition(checkpoint_schema(), None),
                &ToolContext::new(SessionId::new()),
            )
            .await;

        let PreToolUseDecision::Continue(actual) = decision else {
            panic!("valid arguments must continue");
        };
        assert_eq!(actual.arguments, original.arguments);
    }

    #[tokio::test]
    async fn oversized_arguments_are_blocked_without_echoing_content() {
        let hook = ToolArgumentValidationHook::default();
        let marker = "private-oversized-value";
        let decision = hook
            .before_exec(
                call(json!({
                    "missing_evidence": [format!("{marker}{}", "x".repeat(MAX_ARGUMENT_BYTES))]
                })),
                &tool_definition(checkpoint_schema(), None),
                &ToolContext::new(SessionId::new()),
            )
            .await;

        let PreToolUseDecision::Block { reason, .. } = decision else {
            panic!("oversized arguments must be blocked");
        };
        assert!(reason.contains("tool_arguments_too_large"));
        assert!(!reason.contains(marker));
        assert!(reason.len() <= MAX_DIAGNOSTIC_BYTES);
    }
}

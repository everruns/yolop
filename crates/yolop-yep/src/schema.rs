//! JSON Schema for the YEP wire payloads — emitted to
//! `schema/yep/v1/schema.json` by the `schema-gen` binary (repo tooling).
//!
//! This is the payload-level companion to [`crate::meta`]: where `meta.json`
//! lists the method + capability *vocabulary*, `schema.json` gives the full
//! Draft 2020-12 shape of every request/result payload, so an author writing a
//! server in any language can validate what they send and receive. It is
//! generated from the `#[derive(JsonSchema)]` payload structs behind the
//! `schema` feature, and a drift test keeps the committed file in lockstep.

use crate::protocol::{
    CapabilityParams, ErrorObject, HookDecision, HookFireParams, InitializeParams,
    InitializeResult, PromptContribution, StatusChangedParams, ToolCallParams, ToolUpdateParams,
};
use schemars::generate::SchemaSettings;
use serde_json::{Value, json};

/// Build the combined payload schema document: a `messages` map from method
/// name to its `params`/`result` payload refs, plus a `$defs` block with the
/// full schema of every referenced type.
fn schema_document() -> Value {
    let mut generator = SchemaSettings::draft2020_12().into_generator();

    // A `$ref` to each payload, registering it in the shared `$defs`.
    let ref_for = |value: schemars::Schema| value.to_value();
    let init_params = ref_for(generator.subschema_for::<InitializeParams>());
    let init_result = ref_for(generator.subschema_for::<InitializeResult>());
    let tool_call = ref_for(generator.subschema_for::<ToolCallParams>());
    let tool_update = ref_for(generator.subschema_for::<ToolUpdateParams>());
    let hook_fire = ref_for(generator.subschema_for::<HookFireParams>());
    let hook_decision = ref_for(generator.subschema_for::<HookDecision>());
    let prompt_contribution = ref_for(generator.subschema_for::<PromptContribution>());
    let status_changed = ref_for(generator.subschema_for::<StatusChangedParams>());
    let capability_params = ref_for(generator.subschema_for::<CapabilityParams>());
    let error = ref_for(generator.subschema_for::<ErrorObject>());

    // Method → payloads. Directions mirror `meta::METHODS`; only messages that
    // carry a typed payload appear (bare notifications like `initialized` and
    // `shutdown` are payload-less).
    let messages = json!({
        "initialize": { "params": init_params, "result": init_result },
        "tool/call": { "params": tool_call },
        "tool/update": { "params": tool_update },
        "status/changed": { "params": status_changed },
        "hook/fire": { "params": hook_fire, "result": hook_decision },
        "prompt/contribution": { "result": prompt_contribution },
    });

    let defs: Value = generator.take_definitions(true).into();

    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "title": "YEP payload schemas",
        "protocol_version": crate::protocol::PROTOCOL_VERSION,
        "description": "JSON Schema for yolop extension protocol (YEP) request \
                        and result payloads. Companion to meta.json.",
        "capability_params": capability_params,
        "error": error,
        "messages": messages,
        "$defs": defs,
    })
}

/// The canonical pretty-printed `schema/yep/v1/schema.json`, trailing newline.
///
/// Object keys are sorted recursively so the output is byte-identical whether
/// `serde_json` is built with `preserve_order` (insertion order, as it is under
/// the full workspace) or without it (sorted) — otherwise the committed file
/// and the drift-test regeneration would disagree across build configurations.
pub fn schema_json() -> String {
    let mut json =
        serde_json::to_string_pretty(&sort_keys(schema_document())).expect("schema serializes");
    json.push('\n');
    json
}

/// Recursively sort every object's keys, so serialization order does not depend
/// on `serde_json`'s map backend.
fn sort_keys(value: Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut entries: Vec<(String, Value)> =
                map.into_iter().map(|(k, v)| (k, sort_keys(v))).collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            Value::Object(entries.into_iter().collect())
        }
        Value::Array(items) => Value::Array(items.into_iter().map(sort_keys).collect()),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_has_defs_for_every_payload() {
        let doc = schema_document();
        let defs = doc["$defs"].as_object().expect("$defs object");
        for name in [
            "InitializeParams",
            "InitializeResult",
            "ToolCallParams",
            "HookFireParams",
            "HookDecision",
        ] {
            assert!(defs.contains_key(name), "missing $def for {name}: {defs:?}");
        }
        // Messages reference the definitions.
        assert!(doc["messages"]["initialize"]["params"]["$ref"].is_string());
    }

    /// Drift guard: the committed `schema/yep/v1/schema.json` must match what
    /// the generator produces. Runs under `--all-features` (CI's coverage job),
    /// so a payload-shape change can't merge without regenerating the artifact.
    /// Run `cargo run -p yolop-yep --features schema --bin schema-gen` to fix.
    ///
    /// The file lives at the repo root, outside the crate, so it is absent when
    /// the published crate is tested standalone — then the guard is a no-op.
    #[test]
    fn committed_schema_json_is_up_to_date() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../schema/yep/v1/schema.json");
        let Ok(committed) = std::fs::read_to_string(&path) else {
            return; // not in the repo tree; nothing to guard
        };
        assert_eq!(
            committed,
            schema_json(),
            "schema/yep/v1/schema.json is stale — run \
             `cargo run -p yolop-yep --features schema --bin schema-gen`"
        );
    }
}

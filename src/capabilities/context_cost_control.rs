use async_trait::async_trait;
use everruns_core::RuntimeMessage;
use everruns_core::builtins::apply_cost_control_masking;
use everruns_core::capabilities::{ModelViewContext, ModelViewProvider};
use everruns_core::{Capability, CapabilityStatus};
use std::sync::Arc;

pub(crate) const CONTEXT_COST_CONTROL_CAPABILITY_ID: &str = "context_cost_control";

/// Recoverable bounds for fresh results and prompt masking for stale payloads.
///
/// Full outputs remain in session storage, while cold turns remain discoverable
/// through `query_history`. This capability only avoids paying to resend bulky
/// old observations. It deliberately does not activate the runtime's compaction
/// cascade or claim ownership of the infinity-context retrieval budget.
pub(crate) struct ContextCostControlCapability;

#[async_trait]
impl Capability for ContextCostControlCapability {
    fn id(&self) -> &str {
        CONTEXT_COST_CONTROL_CAPABILITY_ID
    }

    fn name(&self) -> &str {
        "Context Cost Control"
    }

    fn description(&self) -> &str {
        "Preserves oversized results in files and masks stale payloads in the model view."
    }

    fn status(&self) -> CapabilityStatus {
        CapabilityStatus::Available
    }

    fn category(&self) -> Option<&str> {
        Some("Optimization")
    }

    fn post_tool_exec_hooks(&self) -> Vec<Arc<dyn everruns_core::tool_hooks::PostToolExecHook>> {
        vec![Arc::new(RecoverableOutputHook)]
    }

    fn model_view_provider(&self) -> Option<Arc<dyn ModelViewProvider>> {
        Some(Arc::new(ContextCostControlModelViewProvider))
    }
}

// Persist before the runtime's final serialized-text limit can cut JSON in
// half. The same full_output contract covers structured file/tool results.
struct RecoverableOutputHook;
#[async_trait]
impl everruns_core::tool_hooks::PostToolExecHook for RecoverableOutputHook {
    async fn after_exec(
        &self,
        call: &everruns_contracts::ToolCall,
        _definition: &everruns_contracts::ToolDefinition,
        result: &mut everruns_contracts::ToolResult,
        context: &everruns_core::ToolContext,
    ) {
        let Some(value) = result.result.as_ref() else {
            return;
        };
        let Ok(serialized) = serde_json::to_string(value) else {
            return;
        };
        if serialized.len() <= 24 * 1024 {
            return;
        }
        let Some(store) = context.file_store.as_ref() else {
            return;
        };
        let hash = <sha2::Sha256 as sha2::Digest>::digest(call.id.as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let path = format!("/outputs/{hash}.result.json");
        if let Err(error) = store
            .write_file(context.session_id, &path, &serialized, "utf-8")
            .await
        {
            tracing::warn!(%error, "could not persist oversized tool result");
            return;
        }
        let mut end = serialized.len().min(4000);
        while !serialized.is_char_boundary(end) {
            end -= 1;
        }
        let displayed = store.display_path(&path);
        let mut envelope = serde_json::json!({
            "truncated": true, "original_bytes": serialized.len(),
            "preview": &serialized[..end], "full_output": displayed,
            "output_files": [displayed],
            "hint": "Full JSON result is saved. Use read_file with offset/limit or grep_files on full_output for missing evidence."
        });
        for key in [
            "success",
            "exit_code",
            "timed_out",
            "error",
            "output_limited",
        ] {
            if let Some(field) = value.get(key) {
                // A tool may put its whole output in `error`. Do not reinsert
                // an unbounded field into the bounded recovery envelope.
                envelope[key] = match field {
                    serde_json::Value::String(text) => {
                        let mut end = text.len().min(2000);
                        while !text.is_char_boundary(end) {
                            end -= 1;
                        }
                        serde_json::Value::String(text[..end].to_string())
                    }
                    serde_json::Value::Bool(_)
                    | serde_json::Value::Number(_)
                    | serde_json::Value::Null => field.clone(),
                    _ => continue,
                };
            }
        }
        result.result = Some(envelope);
    }
}

struct ContextCostControlModelViewProvider;

impl ModelViewProvider for ContextCostControlModelViewProvider {
    fn apply_model_view(
        &self,
        messages: Vec<RuntimeMessage>,
        config: &serde_json::Value,
        context: &ModelViewContext<'_>,
    ) -> Vec<RuntimeMessage> {
        let config =
            everruns_core::builtins::compaction::RuntimeCompactionConfig::from_json(config);
        let result = apply_cost_control_masking(&messages, &config, context.prior_usage);
        if result.masked_count > 0 {
            tracing::debug!(
                session_id = %context.session_id,
                masked_count = result.masked_count,
                tool_result_bytes_before = result.tool_result_bytes_before,
                tool_result_bytes_after = result.tool_result_bytes_after,
                "masked stale tool results in prompt view"
            );
        }
        result.messages
    }

    fn priority(&self) -> i32 {
        50
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use everruns_contracts::ToolCall;
    use everruns_contracts::typed_id::SessionId;
    #[tokio::test]
    async fn large_result_keeps_valid_json_and_recoverable_full_evidence() {
        use everruns_core::{SessionFileSystem, ToolContext};
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(everruns_core::host::RealDiskFileStore::new(dir.path()).unwrap());
        let id = SessionId::new();
        let context = ToolContext::with_file_store(id, store.clone());
        let original = serde_json::json!({"content":"line\n".repeat(30_000), "error":"diagnostic ".repeat(20_000), "exit_code":101, "success":false});
        let call = ToolCall {
            id: "large-call".into(),
            name: "read_file".into(),
            arguments: serde_json::json!({"path":"test.log"}),
        };
        let def = everruns_contracts::ToolDefinition::function(
            "read_file",
            "read file",
            serde_json::json!({}),
        );
        let mut result = everruns_contracts::ToolResult {
            tool_call_id: call.id.clone(),
            result: Some(original.clone()),
            images: None,
            error: None,
            connection_required: None,
            raw_output: None,
        };
        for hook in ContextCostControlCapability.post_tool_exec_hooks() {
            hook.after_exec(&call, &def, &mut result, &context).await;
        }
        let value = result.result.as_ref().unwrap();
        assert!(serde_json::to_vec(value).unwrap().len() < 32 * 1024);
        assert_eq!(value["exit_code"], 101);
        assert!(value["error"].as_str().unwrap().len() <= 2000);
        let path = value["full_output"].as_str().expect("recovery path");
        let file = store.read_file(id, path).await.unwrap().unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(file.content.as_deref().unwrap()).unwrap(),
            original
        );
    }

    #[test]
    fn masks_old_large_tool_results_without_dropping_messages() {
        let mut messages = Vec::new();
        for index in 0..4 {
            let call_id = format!("call_{index}");
            messages.push(RuntimeMessage::assistant_with_tools(
                "",
                vec![ToolCall {
                    id: call_id.clone(),
                    name: "read_file".to_string(),
                    arguments: serde_json::json!({ "path": format!("file_{index}") }),
                }],
            ));
            messages.push(RuntimeMessage::tool_result(
                call_id,
                Some(serde_json::json!({ "output": "x".repeat(10_000) })),
                None,
            ));
        }
        let bytes_before = serde_json::to_vec(&messages)
            .expect("serialize messages")
            .len();

        let reduced = ContextCostControlModelViewProvider.apply_model_view(
            messages.clone(),
            &serde_json::json!({}),
            &ModelViewContext {
                session_id: SessionId::new(),
                prior_usage: None,
                provider_managed_reduction: false,
            },
        );
        let bytes_after = serde_json::to_vec(&reduced)
            .expect("serialize reduced messages")
            .len();

        assert_eq!(reduced.len(), messages.len());
        assert!(bytes_after * 4 < bytes_before * 3);
        assert_ne!(reduced[1].content, messages[1].content);
        assert_eq!(reduced[6].content, messages[6].content);
        assert_eq!(reduced[7].content, messages[7].content);
    }
}

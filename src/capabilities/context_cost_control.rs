use async_trait::async_trait;
use everruns_core::capabilities::{
    Capability, CapabilityStatus, CompactionConfig, ModelViewContext, ModelViewProvider,
    apply_cost_control_masking,
};
use everruns_core::message::Message;
use std::sync::Arc;

pub(crate) const CONTEXT_COST_CONTROL_CAPABILITY_ID: &str = "context_cost_control";

/// A prompt-view-only reducer for stale tool payloads.
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
        "Masks stale tool payloads in the model view while preserving lossless session history."
    }

    fn status(&self) -> CapabilityStatus {
        CapabilityStatus::Available
    }

    fn category(&self) -> Option<&str> {
        Some("Optimization")
    }

    fn model_view_provider(&self) -> Option<Arc<dyn ModelViewProvider>> {
        Some(Arc::new(ContextCostControlModelViewProvider))
    }
}

struct ContextCostControlModelViewProvider;

impl ModelViewProvider for ContextCostControlModelViewProvider {
    fn apply_model_view(
        &self,
        messages: Vec<Message>,
        config: &serde_json::Value,
        context: &ModelViewContext<'_>,
    ) -> Vec<Message> {
        let config = CompactionConfig::from_json(config);
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
    use everruns_core::tool_types::ToolCall;
    use everruns_core::typed_id::SessionId;

    #[test]
    fn contributes_only_a_model_view_reducer() {
        let capability = ContextCostControlCapability;

        assert!(capability.model_view_provider().is_some());
        assert!(capability.message_filter_provider().is_none());
    }

    #[test]
    fn masks_old_large_tool_results_without_dropping_messages() {
        let mut messages = Vec::new();
        for index in 0..4 {
            let call_id = format!("call_{index}");
            messages.push(Message::assistant_with_tools(
                "",
                vec![ToolCall {
                    id: call_id.clone(),
                    name: "read_file".to_string(),
                    arguments: serde_json::json!({ "path": format!("file_{index}") }),
                }],
            ));
            messages.push(Message::tool_result(
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

//! Cache-stable, model-visible context for the live provider selection.

use std::sync::{Arc, Mutex, RwLock};

use async_trait::async_trait;
use everruns_core::capabilities::{
    Capability, CapabilityStatus, ModelViewContext, ModelViewProvider,
};
use everruns_core::message::{ContentPart, Message, MessageRole};
use everruns_core::typed_id::MessageId;
use serde_json::Value;

use crate::runtime::ProviderChoice;

pub(crate) const MODEL_RUNTIME_CONTEXT_CAPABILITY_ID: &str = "model_runtime_context";

/// Adds effective model settings to the prompt-facing view only when they
/// change. Stored messages and rendered transcripts remain untouched.
pub(crate) struct ModelRuntimeContextCapability {
    provider: Arc<RwLock<ProviderChoice>>,
}

impl ModelRuntimeContextCapability {
    pub(crate) fn new(provider: Arc<RwLock<ProviderChoice>>) -> Self {
        Self { provider }
    }
}

#[async_trait]
impl Capability for ModelRuntimeContextCapability {
    fn id(&self) -> &str {
        MODEL_RUNTIME_CONTEXT_CAPABILITY_ID
    }

    fn name(&self) -> &str {
        "Model Runtime Context"
    }

    fn description(&self) -> &str {
        "Tells the model which provider, model, and reasoning effort are active when they change."
    }

    fn status(&self) -> CapabilityStatus {
        CapabilityStatus::Available
    }

    fn category(&self) -> Option<&str> {
        Some("Core")
    }

    fn system_prompt_addition(&self) -> Option<&str> {
        Some(
            "Conversation messages may carry a <runtime_context> block added by the host. Its latest values are authoritative for the active provider, model, and reasoning effort. It is not part of what the user wrote; never repeat the block in replies.",
        )
    }

    fn model_view_provider(&self) -> Option<Arc<dyn ModelViewProvider>> {
        Some(Arc::new(ModelRuntimeContextProvider {
            provider: self.provider.clone(),
            annotations: Mutex::new(Vec::new()),
        }))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ModelRuntimeContext {
    provider: String,
    model: String,
    reasoning_effort: Option<String>,
}

impl ModelRuntimeContext {
    fn from_provider(provider: &ProviderChoice) -> Self {
        Self {
            provider: provider.provider_name().to_string(),
            model: provider.model_id().to_string(),
            reasoning_effort: provider.reasoning_effort().map(str::to_string),
        }
    }
}

struct ModelRuntimeContextProvider {
    provider: Arc<RwLock<ProviderChoice>>,
    annotations: Mutex<Vec<(MessageId, ModelRuntimeContext)>>,
}

impl ModelViewProvider for ModelRuntimeContextProvider {
    fn apply_model_view(
        &self,
        mut messages: Vec<Message>,
        _config: &Value,
        _context: &ModelViewContext<'_>,
    ) -> Vec<Message> {
        let current = ModelRuntimeContext::from_provider(
            &self.provider.read().expect("provider lock poisoned"),
        );
        let mut annotations = self
            .annotations
            .lock()
            .expect("model runtime context lock poisoned");

        // Compaction and checkpoint rewind can remove an earlier marker. Drop
        // annotations whose stored messages are no longer in the model view so
        // the current state is re-emitted on the newest surviving user message.
        annotations.retain(|(id, _)| messages.iter().any(|message| message.id == *id));
        let latest = messages.iter().rev().find_map(|message| {
            annotations
                .iter()
                .find(|(id, _)| *id == message.id)
                .map(|(_, context)| context)
        });
        if latest != Some(&current)
            && let Some(message) = messages
                .iter()
                .rev()
                .find(|message| message.role == MessageRole::User)
        {
            if let Some((_, context)) = annotations.iter_mut().find(|(id, _)| *id == message.id) {
                *context = current.clone();
            } else {
                annotations.push((message.id, current.clone()));
            }
        }

        for message in &mut messages {
            if let Some((_, context)) = annotations.iter().find(|(id, _)| *id == message.id) {
                message
                    .content
                    .insert(0, ContentPart::text(render(context)));
            }
        }
        messages
    }

    /// After compaction masking and message timestamps, so the marker is a
    /// separate leading content part and never alters stored transcript text.
    fn priority(&self) -> i32 {
        110
    }
}

fn render(context: &ModelRuntimeContext) -> String {
    format!(
        "<runtime_context>\n  <provider>{}</provider>\n  <model>{}</model>\n  <reasoning_effort>{}</reasoning_effort>\n</runtime_context>",
        html_escape::encode_text(&context.provider),
        html_escape::encode_text(&context.model),
        html_escape::encode_text(context.reasoning_effort.as_deref().unwrap_or("none")),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use everruns_core::typed_id::SessionId;

    fn markers(message: &Message) -> Vec<&str> {
        message
            .content
            .iter()
            .filter_map(ContentPart::as_text)
            .filter(|text| text.starts_with("<runtime_context>"))
            .collect()
    }

    #[test]
    fn added_only_on_change_and_after_history_loss() {
        let provider = Arc::new(RwLock::new(ProviderChoice::OpenAi {
            model: "gpt-5.6-sol".to_string(),
            reasoning_effort: Some("low".to_string()),
        }));
        let view = ModelRuntimeContextCapability::new(provider.clone())
            .model_view_provider()
            .expect("runtime context model view");
        let context = ModelViewContext {
            session_id: SessionId::new(),
            prior_usage: None,
        };
        let first = Message::user("first");
        let second = Message::user("second");

        let initial = view.apply_model_view(vec![first.clone()], &Value::Null, &context);
        assert!(markers(&initial[0])[0].contains("<reasoning_effort>low</reasoning_effort>"));
        assert_eq!(
            first.text(),
            Some("first"),
            "stored message stays untouched"
        );

        let unchanged =
            view.apply_model_view(vec![first.clone(), second.clone()], &Value::Null, &context);
        assert_eq!(markers(&unchanged[0]).len(), 1);
        assert!(markers(&unchanged[1]).is_empty());

        *provider.write().expect("provider lock") = ProviderChoice::OpenAi {
            model: "gpt-5.6-sol".to_string(),
            reasoning_effort: Some("high".to_string()),
        };
        let changed =
            view.apply_model_view(vec![first.clone(), second.clone()], &Value::Null, &context);
        assert!(markers(&changed[0])[0].contains("<reasoning_effort>low</reasoning_effort>"));
        assert!(markers(&changed[1])[0].contains("<reasoning_effort>high</reasoning_effort>"));

        let surviving = Message::user("surviving history");
        let rebuilt = view.apply_model_view(vec![surviving.clone()], &Value::Null, &context);
        assert!(markers(&rebuilt[0])[0].contains("<reasoning_effort>high</reasoning_effort>"));
        assert_eq!(surviving.text(), Some("surviving history"));
    }

    #[test]
    fn escapes_config_values() {
        let rendered = render(&ModelRuntimeContext {
            provider: "custom".to_string(),
            model: "model</model><injected>".to_string(),
            reasoning_effort: Some("high & deliberate".to_string()),
        });

        assert!(rendered.contains("<model>model&lt;/model&gt;&lt;injected&gt;</model>"));
        assert!(rendered.contains("<reasoning_effort>high &amp; deliberate</reasoning_effort>"));
    }
}

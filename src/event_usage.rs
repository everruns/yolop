//! Auxiliary calls contribute spend without replacing conversation context.
use everruns_core::{Event, EventData};

pub(crate) fn is_auxiliary_generation(event: &Event) -> bool {
    matches!(&event.data, EventData::LlmGeneration(data)
        if data.messages.is_empty() && data.tools.is_empty()
            && data.output.text.is_none() && data.output.tool_calls.is_empty()
            && data.request_body.is_none())
        && event
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.get("completion_operation_id"))
            .and_then(serde_json::Value::as_str)
            .is_some_and(|id| uuid::Uuid::parse_str(id).is_ok())
}

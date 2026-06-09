//! ACP schema types sourced from the upstream Rust SDK.
//!
//! yolop still owns its runtime bridge and server loop, but the wire data model
//! comes from `agent-client-protocol` so schema changes are not mirrored by hand.

pub use agent_client_protocol::schema::{
    AgentCapabilities, AuthenticateRequest as AuthenticateParams,
    AuthenticateResponse as AuthenticateResult, AvailableCommand, AvailableCommandInput,
    AvailableCommandsUpdate, CancelNotification, Content, ContentBlock, ContentChunk,
    InitializeRequest as InitializeParams, InitializeResponse as InitializeResult,
    NewSessionRequest as NewSessionParams, NewSessionResponse as NewSessionResult, Plan, PlanEntry,
    PlanEntryPriority, PlanEntryStatus, PromptCapabilities, PromptRequest as PromptParams,
    PromptResponse as PromptResult, ProtocolVersion, SessionNotification, SessionUpdate,
    StopReason, TextContent, ToolCall, ToolCallContent, ToolCallStatus, ToolCallUpdate,
    ToolCallUpdateFields, ToolKind, UnstructuredCommandInput,
};
use serde_json::{Map, Value};

pub const PROTOCOL_VERSION: ProtocolVersion = ProtocolVersion::V1;

pub fn text_block(text: impl Into<String>) -> ContentBlock {
    ContentBlock::Text(TextContent::new(text))
}

pub fn text_chunk(text: impl Into<String>) -> ContentChunk {
    ContentChunk::new(text_block(text))
}

pub fn content(text: impl Into<String>) -> ToolCallContent {
    ToolCallContent::Content(Content::new(text_block(text)))
}

pub fn meta(value: Value) -> Option<Map<String, Value>> {
    match value {
        Value::Object(map) => Some(map),
        _ => None,
    }
}

/// Extract the concatenated plain text from an inbound prompt's content
/// blocks, ignoring non-text blocks (images, resources). Newline-joined so a
/// multi-block prompt reads naturally.
pub fn prompt_text(blocks: &[ContentBlock]) -> String {
    let mut parts = Vec::new();
    for block in blocks {
        if let ContentBlock::Text(text) = block {
            parts.push(text.text.clone());
        }
    }
    parts.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sdk_initialize_result_serializes_camel_case() {
        let result = InitializeResult::new(PROTOCOL_VERSION).agent_capabilities(
            AgentCapabilities::new()
                .load_session(false)
                .prompt_capabilities(PromptCapabilities::new().embedded_context(true)),
        );
        let v = serde_json::to_value(&result).unwrap();
        assert_eq!(v["protocolVersion"], 1);
        assert_eq!(v["agentCapabilities"]["loadSession"], false);
        assert_eq!(
            v["agentCapabilities"]["promptCapabilities"]["embeddedContext"],
            true
        );
        assert!(v["authMethods"].as_array().unwrap().is_empty());
    }

    #[test]
    fn sdk_message_chunk_uses_snake_case_discriminator() {
        let update =
            agent_client_protocol::schema::SessionUpdate::AgentMessageChunk(text_chunk("hi"));
        let v = serde_json::to_value(&update).unwrap();
        assert_eq!(v["sessionUpdate"], "agent_message_chunk");
        assert_eq!(v["content"]["type"], "text");
        assert_eq!(v["content"]["text"], "hi");
    }

    #[test]
    fn prompt_text_concatenates_text_blocks_only() {
        let blocks = vec![
            text_block("hello"),
            ContentBlock::Image(agent_client_protocol::schema::ImageContent::new(
                "image/png",
                "...",
            )),
            text_block("world"),
        ];
        assert_eq!(prompt_text(&blocks), "hello\nworld");
    }
}

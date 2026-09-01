//! ACP schema types sourced from the upstream Rust SDK.
//!
//! yolop still owns its runtime bridge and server loop, but the wire data model
//! comes from `agent-client-protocol` so schema changes are not mirrored by hand.

pub use agent_client_protocol::schema::ProtocolVersion;
#[cfg(test)]
pub(crate) use agent_client_protocol::schema::v1::ImageContent;
pub use agent_client_protocol::schema::v1::{
    AgentCapabilities, AuthMethod, AuthenticateRequest as AuthenticateParams,
    AuthenticateResponse as AuthenticateResult, AvailableCommand, AvailableCommandInput,
    AvailableCommandsUpdate, CancelNotification, ConfigOptionUpdate, Content, ContentBlock,
    ContentChunk, CurrentModeUpdate, EmbeddedResource, EmbeddedResourceResource,
    InitializeRequest as InitializeParams, InitializeResponse as InitializeResult,
    LoadSessionRequest as LoadSessionParams, LoadSessionResponse as LoadSessionResult,
    McpCapabilities, McpServer, NewSessionRequest as NewSessionParams,
    NewSessionResponse as NewSessionResult, PermissionOption, PermissionOptionKind, Plan,
    PlanEntry, PlanEntryPriority, PlanEntryStatus, PromptCapabilities,
    PromptRequest as PromptParams, PromptResponse as PromptResult, RequestPermissionOutcome,
    RequestPermissionRequest as RequestPermissionParams, ResourceLink, SessionConfigId,
    SessionConfigOption, SessionConfigOptionCategory, SessionConfigSelectOption, SessionInfoUpdate,
    SessionMode, SessionModeId, SessionModeState, SessionNotification, SessionUpdate,
    SetSessionConfigOptionRequest, SetSessionConfigOptionResponse,
    SetSessionModeRequest as SetSessionModeParams, SetSessionModeResponse as SetSessionModeResult,
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

/// Concatenated plain **user** text from a prompt's content blocks, ignoring
/// every non-text block. Newline-joined so a multi-block prompt reads naturally.
///
/// This is the text used for slash-command detection and the checkpoint label,
/// so it deliberately excludes embedded/linked resources — a file attached to
/// `/setup` must not derail command parsing. The model-facing text (which does
/// fold in resources) is [`prompt_model_text`].
pub fn prompt_text(blocks: &[ContentBlock]) -> String {
    let mut parts = Vec::new();
    for block in blocks {
        if let ContentBlock::Text(text) = block {
            parts.push(text.text.clone());
        }
    }
    parts.join("\n")
}

/// The prompt text handed to the model: user text plus any embedded or linked
/// resource context (`embeddedContext` blocks and resource links) that
/// [`prompt_text`] drops. Images ride as separate content parts and audio is
/// unsupported, so both are skipped here.
pub fn prompt_model_text(blocks: &[ContentBlock]) -> String {
    let mut parts = Vec::new();
    for block in blocks {
        match block {
            ContentBlock::Text(text) => parts.push(text.text.clone()),
            ContentBlock::Resource(resource) => parts.push(embedded_resource_text(resource)),
            ContentBlock::ResourceLink(link) => parts.push(resource_link_text(link)),
            _ => {}
        }
    }
    parts.join("\n")
}

/// Render an embedded resource as model context: text contents are inlined
/// inside a delimited block tagged with the resource URI; binary contents are
/// referenced by URI and MIME type rather than dumped as base64.
fn embedded_resource_text(resource: &EmbeddedResource) -> String {
    match &resource.resource {
        EmbeddedResourceResource::TextResourceContents(text) => {
            format!(
                "<resource uri=\"{}\">\n{}\n</resource>",
                text.uri, text.text
            )
        }
        EmbeddedResourceResource::BlobResourceContents(blob) => format!(
            "[embedded resource {} omitted: {} binary content]",
            blob.uri,
            blob.mime_type.as_deref().unwrap_or("unknown type")
        ),
        // `EmbeddedResourceResource` is non-exhaustive: note an unknown kind
        // rather than dropping it silently.
        _ => "[embedded resource of an unsupported kind omitted]".to_string(),
    }
}

/// Render a resource link as a one-line reference (name, URI, optional
/// description) so the model knows the resource was attached.
fn resource_link_text(link: &ResourceLink) -> String {
    match link.description.as_deref().filter(|d| !d.is_empty()) {
        Some(description) => {
            format!(
                "[linked resource: {} ({}) — {description}]",
                link.name, link.uri
            )
        }
        None => format!("[linked resource: {} ({})]", link.name, link.uri),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::ImageContent;

    #[test]
    fn sdk_initialize_result_serializes_camel_case() {
        let result = InitializeResult::new(PROTOCOL_VERSION).agent_capabilities(
            AgentCapabilities::new()
                .load_session(true)
                .prompt_capabilities(PromptCapabilities::new().embedded_context(true)),
        );
        let v = serde_json::to_value(&result).unwrap();
        assert_eq!(v["protocolVersion"], 1);
        assert_eq!(v["agentCapabilities"]["loadSession"], true);
        assert_eq!(
            v["agentCapabilities"]["promptCapabilities"]["embeddedContext"],
            true
        );
        assert!(v["authMethods"].as_array().unwrap().is_empty());
    }

    #[test]
    fn sdk_message_chunk_uses_snake_case_discriminator() {
        let update = SessionUpdate::AgentMessageChunk(text_chunk("hi"));
        let v = serde_json::to_value(&update).unwrap();
        assert_eq!(v["sessionUpdate"], "agent_message_chunk");
        assert_eq!(v["content"]["type"], "text");
        assert_eq!(v["content"]["text"], "hi");
    }

    #[test]
    fn prompt_text_concatenates_text_blocks_only() {
        let blocks = vec![
            text_block("hello"),
            ContentBlock::Image(ImageContent::new("image/png", "...")),
            text_block("world"),
        ];
        assert_eq!(prompt_text(&blocks), "hello\nworld");
    }

    #[test]
    fn prompt_model_text_folds_embedded_text_resource() {
        use agent_client_protocol::schema::v1::{
            EmbeddedResource, EmbeddedResourceResource, TextResourceContents,
        };
        let blocks = vec![
            text_block("check this file"),
            ContentBlock::Resource(EmbeddedResource::new(
                EmbeddedResourceResource::TextResourceContents(TextResourceContents::new(
                    "fn main() {}",
                    "file:///main.rs",
                )),
            )),
        ];
        let text = prompt_model_text(&blocks);
        assert!(text.contains("check this file"));
        assert!(text.contains("file:///main.rs"));
        assert!(text.contains("fn main() {}"));
        // Command-detection text stays pure so an attached resource can't
        // derail slash-command parsing.
        assert_eq!(prompt_text(&blocks), "check this file");
    }

    #[test]
    fn prompt_model_text_references_blob_and_link_without_inlining_bytes() {
        use agent_client_protocol::schema::v1::{
            BlobResourceContents, EmbeddedResource, EmbeddedResourceResource, ResourceLink,
        };
        let blocks = vec![
            ContentBlock::Resource(EmbeddedResource::new(
                EmbeddedResourceResource::BlobResourceContents(BlobResourceContents::new(
                    "QUJD",
                    "file:///logo.png",
                )),
            )),
            ContentBlock::ResourceLink(ResourceLink::new("Design doc", "https://example.com/doc")),
        ];
        let text = prompt_model_text(&blocks);
        assert!(text.contains("file:///logo.png"));
        assert!(!text.contains("QUJD"), "binary blob must not be inlined");
        assert!(text.contains("Design doc"));
        assert!(text.contains("https://example.com/doc"));
    }
}

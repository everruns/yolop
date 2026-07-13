use crate::codex_auth::{self, CODEX_ORIGINATOR};
use async_trait::async_trait;
use eventsource_stream::Eventsource;
use everruns_core::driver_registry::{
    ChatDriver, DriverConfig, LlmCallConfig, LlmCompletionMetadata, LlmContentPart, LlmMessage,
    LlmMessageContent, LlmMessageRole, LlmResponseStream, LlmStreamError, LlmStreamEvent,
    ProviderMetadata,
};
use everruns_core::driver_registry::{DiscoveredModel, DriverRegistry};
use everruns_core::error::{AgentLoopError, Result as EverrunsResult};
use everruns_core::tool_types::{ToolCall, ToolDefinition};
use everruns_core::{DriverId, ModelProfile, get_model_profile};
use futures::StreamExt;
use reqwest::header::{HeaderMap, HeaderValue};
use serde::Serialize;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

pub const CODEX_DRIVER_ID: &str = "openai-codex";
const CODEX_RESPONSES_URL: &str = "https://chatgpt.com/backend-api/codex/responses";
const CODEX_BETA_HEADER: &str = "responses=experimental";

#[derive(Debug, Clone)]
struct CodexTokens {
    access_token: String,
    refresh_token: Option<String>,
    expires_at: Option<i64>,
    account_id: Option<String>,
}

#[derive(Clone)]
pub struct CodexChatDriver {
    client: reqwest::Client,
    tokens: Arc<tokio::sync::Mutex<CodexTokens>>,
}

pub fn register_driver(registry: &mut DriverRegistry) {
    registry.register_external(CODEX_DRIVER_ID, |config| {
        Box::new(CodexChatDriver::from_config(config))
    });
}

pub(crate) fn model_profile(model_id: &str) -> Option<ModelProfile> {
    get_model_profile(&DriverId::OpenAI, model_id)
}

impl CodexChatDriver {
    fn from_config(config: &DriverConfig) -> Self {
        let access_token = config
            .api_key
            .clone()
            .or_else(|| metadata_extra_string(&config.metadata, "access_token"))
            .unwrap_or_default();
        let refresh_token = config.metadata.refresh_token.clone();
        let expires_at = metadata_extra_i64(&config.metadata, "expires_at")
            .or_else(|| metadata_extra_i64(&config.metadata, "expires_at_ms"));
        let account_id = config
            .metadata
            .account_id
            .clone()
            .or_else(|| codex_auth::extract_account_id(&access_token));
        Self {
            client: reqwest::Client::new(),
            tokens: Arc::new(tokio::sync::Mutex::new(CodexTokens {
                access_token,
                refresh_token,
                expires_at,
                account_id,
            })),
        }
    }

    async fn token_snapshot(&self) -> EverrunsResult<CodexTokens> {
        let mut guard = self.tokens.lock().await;
        if guard.access_token.is_empty() {
            return Err(AgentLoopError::llm(
                "Codex provider requires a saved OAuth access token",
            ));
        }
        if codex_auth::should_refresh(guard.expires_at)
            && let Some(refresh_token) = guard.refresh_token.clone()
        {
            let refreshed = codex_auth::refresh_with_token(&refresh_token)
                .await
                .map_err(|err| {
                    AgentLoopError::llm(format!("Codex token refresh failed: {err:#}"))
                })?;
            guard.access_token = refreshed.access_token;
            guard.refresh_token = refreshed.refresh_token.or(Some(refresh_token));
            guard.expires_at = refreshed.expires_at;
            guard.account_id = refreshed
                .account_id
                .or_else(|| codex_auth::extract_account_id(&guard.access_token));
        }
        Ok(guard.clone())
    }
}

#[async_trait]
impl ChatDriver for CodexChatDriver {
    async fn chat_completion_stream(
        &self,
        messages: Vec<LlmMessage>,
        config: &LlmCallConfig,
    ) -> EverrunsResult<LlmResponseStream> {
        let tokens = self.token_snapshot().await?;
        let (instructions, input) = build_input(&messages);
        let request = CodexResponsesRequest {
            model: config.model.clone(),
            store: false,
            input,
            instructions,
            temperature: config.temperature,
            max_output_tokens: config.max_tokens,
            stream: true,
            tools: (!config.tools.is_empty()).then(|| convert_tools(&config.tools)),
            reasoning: config
                .reasoning_effort
                .clone()
                .map(|effort| CodexReasoning {
                    effort,
                    summary: "auto".to_string(),
                }),
        };

        let mut headers = HeaderMap::new();
        headers.insert("OpenAI-Beta", HeaderValue::from_static(CODEX_BETA_HEADER));
        headers.insert("originator", HeaderValue::from_static(CODEX_ORIGINATOR));
        if let Some(account_id) = &tokens.account_id
            && let Ok(value) = HeaderValue::from_str(account_id)
        {
            headers.insert("chatgpt-account-id", value.clone());
            headers.insert("ChatGPT-Account-Id", value);
        }
        if let Some(session_id) = config.metadata.get("session_id")
            && let Ok(value) = HeaderValue::from_str(session_id)
        {
            headers.insert("session_id", value);
        }

        let response = self
            .client
            .post(CODEX_RESPONSES_URL)
            .bearer_auth(&tokens.access_token)
            .headers(headers)
            .json(&request)
            .send()
            .await
            .map_err(|err| AgentLoopError::llm(format!("Failed to send Codex request: {err}")))?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(AgentLoopError::llm_kind(
                everruns_core::error::LlmErrorKind::from_provider_status(status.as_u16(), &body),
                format!("Codex API error ({status}): {body}"),
            ));
        }

        let event_stream = response.bytes_stream().eventsource();
        let model = config.model.clone();
        let input_tokens = Arc::new(Mutex::new(0u32));
        let output_tokens = Arc::new(Mutex::new(0u32));
        let cache_read_tokens = Arc::new(Mutex::new(None::<u32>));
        let finish_reason = Arc::new(Mutex::new(None::<String>));
        let accumulated_tool_calls = Arc::new(Mutex::new(Vec::<ToolCallAccumulator>::new()));

        let converted: LlmResponseStream = Box::pin(event_stream.then(move |result| {
            let model = model.clone();
            let input_tokens = Arc::clone(&input_tokens);
            let output_tokens = Arc::clone(&output_tokens);
            let cache_read_tokens = Arc::clone(&cache_read_tokens);
            let finish_reason = Arc::clone(&finish_reason);
            let accumulated_tool_calls = Arc::clone(&accumulated_tool_calls);
            async move {
                match result {
                    Ok(event) => Ok(handle_event(
                        &event.data,
                        &model,
                        &input_tokens,
                        &output_tokens,
                        &cache_read_tokens,
                        &finish_reason,
                        &accumulated_tool_calls,
                    )),
                    Err(err) => Ok(LlmStreamEvent::Error(
                        format!("Codex stream error: {err}").into(),
                    )),
                }
            }
        }));
        Ok(converted)
    }

    async fn list_models(&self) -> EverrunsResult<Option<Vec<DiscoveredModel>>> {
        Ok(None)
    }
}

#[derive(Debug, Serialize)]
struct CodexResponsesRequest {
    model: String,
    store: bool,
    input: Vec<CodexInputItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    instructions: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_output_tokens: Option<u32>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<CodexTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<CodexReasoning>,
}

#[derive(Debug, Serialize)]
struct CodexReasoning {
    effort: String,
    summary: String,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum CodexInputItem {
    Message {
        r#type: String,
        role: String,
        content: CodexContent,
    },
    FunctionCall {
        r#type: String,
        call_id: String,
        name: String,
        arguments: String,
    },
    FunctionCallOutput {
        r#type: String,
        call_id: String,
        output: String,
    },
    Reasoning {
        r#type: String,
        id: String,
        encrypted_content: String,
    },
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum CodexContent {
    Text(String),
    Parts(Vec<CodexContentPart>),
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
#[allow(clippy::enum_variant_names)]
enum CodexContentPart {
    InputText {
        r#type: String,
        text: String,
    },
    InputImage {
        r#type: String,
        image_url: String,
    },
    InputAudio {
        r#type: String,
        input_audio: CodexInputAudio,
    },
}

#[derive(Debug, Serialize)]
struct CodexInputAudio {
    data: String,
    format: String,
}

#[derive(Debug, Serialize)]
struct CodexTool {
    r#type: String,
    name: String,
    description: String,
    parameters: Value,
}

#[derive(Clone, Default)]
struct ToolCallAccumulator {
    id: String,
    call_id: String,
    name: String,
    arguments: String,
}

fn build_input(messages: &[LlmMessage]) -> (Option<String>, Vec<CodexInputItem>) {
    let mut instructions = Vec::new();
    let mut input = Vec::new();
    let mut reasoning_counter = 0usize;
    for message in messages {
        if message.role == LlmMessageRole::System {
            instructions.push(message.content.to_text());
            continue;
        }
        if message.role == LlmMessageRole::Assistant
            && let Some(encrypted_content) = &message.thinking_signature
        {
            reasoning_counter += 1;
            input.push(CodexInputItem::Reasoning {
                r#type: "reasoning".to_string(),
                id: format!("rs_{reasoning_counter:08x}"),
                encrypted_content: encrypted_content.clone(),
            });
        }
        if message.role == LlmMessageRole::Assistant
            && message
                .tool_calls
                .as_ref()
                .is_some_and(|calls| !calls.is_empty())
        {
            if !message.content.to_text().is_empty() {
                input.push(convert_message(message));
            }
            if let Some(tool_calls) = &message.tool_calls {
                for call in tool_calls {
                    input.push(CodexInputItem::FunctionCall {
                        r#type: "function_call".to_string(),
                        call_id: call.id.clone(),
                        name: call.name.clone(),
                        arguments: call.arguments.to_string(),
                    });
                }
            }
            continue;
        }
        input.push(convert_message(message));
    }
    let instructions = (!instructions.is_empty()).then(|| instructions.join("\n\n"));
    (instructions, drop_orphaned_function_outputs(input))
}

fn convert_message(message: &LlmMessage) -> CodexInputItem {
    if message.role == LlmMessageRole::Tool
        && let Some(tool_call_id) = &message.tool_call_id
    {
        return CodexInputItem::FunctionCallOutput {
            r#type: "function_call_output".to_string(),
            call_id: tool_call_id.clone(),
            output: message.content.to_text(),
        };
    }
    let content = match &message.content {
        LlmMessageContent::Text(text) => CodexContent::Text(text.clone()),
        LlmMessageContent::Parts(parts) => CodexContent::Parts(
            parts
                .iter()
                .map(|part| match part {
                    LlmContentPart::Text { text } => CodexContentPart::InputText {
                        r#type: "input_text".to_string(),
                        text: text.clone(),
                    },
                    LlmContentPart::Image { url } => CodexContentPart::InputImage {
                        r#type: "input_image".to_string(),
                        image_url: url.clone(),
                    },
                    LlmContentPart::Audio { url } => CodexContentPart::InputAudio {
                        r#type: "input_audio".to_string(),
                        input_audio: CodexInputAudio {
                            data: url.clone(),
                            format: "wav".to_string(),
                        },
                    },
                })
                .collect(),
        ),
    };
    CodexInputItem::Message {
        r#type: "message".to_string(),
        role: match message.role {
            LlmMessageRole::System => "developer",
            LlmMessageRole::User => "user",
            LlmMessageRole::Assistant => "assistant",
            LlmMessageRole::Tool => "tool",
        }
        .to_string(),
        content,
    }
}

fn drop_orphaned_function_outputs(input: Vec<CodexInputItem>) -> Vec<CodexInputItem> {
    let call_ids: std::collections::HashSet<String> = input
        .iter()
        .filter_map(|item| match item {
            CodexInputItem::FunctionCall { call_id, .. } => Some(call_id.clone()),
            _ => None,
        })
        .collect();
    if call_ids.is_empty() {
        return input
            .into_iter()
            .filter(|item| !matches!(item, CodexInputItem::FunctionCallOutput { .. }))
            .collect();
    }
    input
        .into_iter()
        .filter(|item| match item {
            CodexInputItem::FunctionCallOutput { call_id, .. } => call_ids.contains(call_id),
            _ => true,
        })
        .collect()
}

fn convert_tools(tools: &[ToolDefinition]) -> Vec<CodexTool> {
    tools
        .iter()
        .map(|tool| CodexTool {
            r#type: "function".to_string(),
            name: tool.name().to_string(),
            description: tool.description().to_string(),
            parameters: sanitize_parameters(tool.parameters()),
        })
        .collect()
}

fn sanitize_parameters(params: &Value) -> Value {
    let mut params = params.clone();
    if let Some(obj) = params.as_object_mut()
        && obj.get("type").and_then(Value::as_str) == Some("object")
        && !obj.contains_key("properties")
    {
        obj.insert("properties".to_string(), Value::Object(Default::default()));
    }
    params
}

/// The literal placeholder the OpenAI Responses API injects for an *empty*
/// reasoning-summary part — a part that carries a `**bold heading**` but no
/// prose. It arrives verbatim in `response.reasoning_summary_text.delta`.
const REASONING_SUMMARY_PLACEHOLDER: &str = "<!-- -->";

/// Drop empty reasoning-summary placeholders from a streamed summary delta.
///
/// Without this, `<!-- -->` reaches every downstream consumer of the thinking
/// stream — the ACP thought view (`acp/bridge.rs`) and the session log — since
/// the TUI is the only surface that already discards summary text. We strip the
/// marker at the driver so all surfaces stay clean at the source.
///
/// This mirrors the Codex fix for the same OpenAI Responses behavior:
///   - PR: https://github.com/openai/codex/pull/31652
///   - `split_reasoning_summary_parts` in
///     codex-rs/tui/src/history_cell/messages.rs (strips a part whose body
///     equals `<!-- -->`).
///
/// FUTURE — recheck whether this workaround is still needed. OpenAI may stop
/// emitting the placeholder server-side, at which point this can be deleted.
/// To verify, stream a reasoning-heavy prompt with `summary: "detailed"` and
/// grep the raw SSE for the marker across many runs; if it never appears, drop
/// this guard:
///   curl https://api.openai.com/v1/responses -H "Authorization: Bearer $KEY" \
///     -d '{"model":"gpt-5.6-sol","stream":true,
///          "reasoning":{"effort":"high","summary":"detailed"},
///          "input":[{"role":"user","content":"<reasoning-heavy prompt>"}]}' \
///     | grep -- '<!--'
fn strip_reasoning_placeholder(delta: &str) -> String {
    delta.replace(REASONING_SUMMARY_PLACEHOLDER, "")
}

fn handle_event(
    event_data: &str,
    model: &str,
    input_tokens: &Mutex<u32>,
    output_tokens: &Mutex<u32>,
    cache_read_tokens: &Mutex<Option<u32>>,
    finish_reason: &Mutex<Option<String>>,
    accumulated_tool_calls: &Mutex<Vec<ToolCallAccumulator>>,
) -> LlmStreamEvent {
    let Ok(json) = serde_json::from_str::<Value>(event_data) else {
        return LlmStreamEvent::TextDelta(String::new());
    };
    match json.get("type").and_then(Value::as_str) {
        Some("response.output_text.delta") => json
            .get("delta")
            .and_then(Value::as_str)
            .map(|delta| LlmStreamEvent::TextDelta(delta.to_string()))
            .unwrap_or_else(|| LlmStreamEvent::TextDelta(String::new())),
        Some("response.reasoning_summary_text.delta")
        | Some("response.reasoning_text.delta")
        | Some("response.reasoning.delta") => json
            .get("delta")
            .and_then(Value::as_str)
            .map(|delta| match strip_reasoning_placeholder(delta) {
                cleaned if cleaned.is_empty() => LlmStreamEvent::TextDelta(String::new()),
                cleaned => LlmStreamEvent::ThinkingDelta(cleaned),
            })
            .unwrap_or_else(|| LlmStreamEvent::TextDelta(String::new())),
        Some("response.function_call_arguments.delta") => {
            if let (Some(item_id), Some(delta)) = (
                json.get("item_id").and_then(Value::as_str),
                json.get("delta").and_then(Value::as_str),
            ) {
                let mut acc = accumulated_tool_calls.lock().expect("tool call lock");
                if let Some(call) = acc.iter_mut().find(|call| call.id == item_id) {
                    call.arguments.push_str(delta);
                } else {
                    acc.push(ToolCallAccumulator {
                        id: item_id.to_string(),
                        arguments: delta.to_string(),
                        ..Default::default()
                    });
                }
            }
            LlmStreamEvent::TextDelta(String::new())
        }
        Some("response.output_item.added") => {
            if let Some(item) = json.get("item")
                && item.get("type").and_then(Value::as_str) == Some("function_call")
            {
                upsert_tool_call(item, accumulated_tool_calls);
            }
            LlmStreamEvent::TextDelta(String::new())
        }
        Some("response.output_item.done") => {
            if let Some(item) = json.get("item")
                && item.get("type").and_then(Value::as_str) == Some("function_call")
            {
                upsert_tool_call(item, accumulated_tool_calls);
                let calls = accumulated_tool_calls
                    .lock()
                    .expect("tool call lock")
                    .iter()
                    .filter(|call| !call.name.is_empty())
                    .map(tool_call_from_accumulator)
                    .collect::<Vec<_>>();
                *finish_reason.lock().expect("finish reason lock") = Some("tool_calls".to_string());
                return LlmStreamEvent::ToolCalls(calls);
            }
            LlmStreamEvent::TextDelta(String::new())
        }
        Some("response.completed") => done_event(
            &json,
            model,
            input_tokens,
            output_tokens,
            cache_read_tokens,
            finish_reason,
        ),
        Some("response.failed") | Some("error") => LlmStreamEvent::Error(codex_stream_error(&json)),
        _ => LlmStreamEvent::TextDelta(String::new()),
    }
}

fn upsert_tool_call(item: &Value, accumulated_tool_calls: &Mutex<Vec<ToolCallAccumulator>>) {
    let id = item
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let call_id = item
        .get("call_id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let name = item
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let arguments = item
        .get("arguments")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let mut acc = accumulated_tool_calls.lock().expect("tool call lock");
    if let Some(existing) = acc.iter_mut().find(|call| call.id == id) {
        if !call_id.is_empty() {
            existing.call_id = call_id;
        }
        if !name.is_empty() {
            existing.name = name;
        }
        if !arguments.is_empty() {
            existing.arguments = arguments;
        }
    } else {
        acc.push(ToolCallAccumulator {
            id,
            call_id,
            name,
            arguments,
        });
    }
}

fn tool_call_from_accumulator(call: &ToolCallAccumulator) -> ToolCall {
    ToolCall {
        id: if call.call_id.is_empty() {
            call.id.clone()
        } else {
            call.call_id.clone()
        },
        name: call.name.clone(),
        arguments: serde_json::from_str(&call.arguments)
            .unwrap_or_else(|_| json!({ "raw_arguments": call.arguments })),
    }
}

fn done_event(
    json: &Value,
    model: &str,
    input_tokens: &Mutex<u32>,
    output_tokens: &Mutex<u32>,
    cache_read_tokens: &Mutex<Option<u32>>,
    finish_reason: &Mutex<Option<String>>,
) -> LlmStreamEvent {
    let response = json.get("response").unwrap_or(json);
    if let Some(usage) = response.get("usage") {
        if let Some(value) = usage
            .get("input_tokens")
            .or_else(|| usage.get("prompt_tokens"))
            .and_then(Value::as_u64)
        {
            *input_tokens.lock().expect("input token lock") = value as u32;
        }
        if let Some(value) = usage
            .get("output_tokens")
            .or_else(|| usage.get("completion_tokens"))
            .and_then(Value::as_u64)
        {
            *output_tokens.lock().expect("output token lock") = value as u32;
        }
        if let Some(value) = usage
            .get("input_tokens_details")
            .and_then(|details| details.get("cached_tokens"))
            .and_then(Value::as_u64)
        {
            *cache_read_tokens.lock().expect("cache token lock") = Some(value as u32);
        }
    }
    let input = *input_tokens.lock().expect("input token lock");
    let output = *output_tokens.lock().expect("output token lock");
    let finish = finish_reason
        .lock()
        .expect("finish reason lock")
        .clone()
        .unwrap_or_else(|| {
            response
                .get("status")
                .and_then(Value::as_str)
                .filter(|status| *status != "completed")
                .unwrap_or("stop")
                .to_string()
        });
    LlmStreamEvent::Done(Box::new(LlmCompletionMetadata {
        total_tokens: Some(input + output),
        prompt_tokens: Some(input),
        completion_tokens: Some(output),
        cache_read_tokens: *cache_read_tokens.lock().expect("cache token lock"),
        cache_creation_tokens: None,
        provider_cost_usd: None,
        model: Some(model.to_string()),
        finish_reason: Some(finish),
        retry_metadata: None,
        response_id: response
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_string),
        phase: None,
    }))
}

fn codex_stream_error(json: &Value) -> LlmStreamError {
    let error = json.get("error").or_else(|| {
        json.get("response")
            .and_then(|response| response.get("error"))
    });
    let code = error
        .and_then(|error| error.get("code"))
        .and_then(Value::as_str);
    let status = error
        .and_then(provider_error_status)
        .or_else(|| provider_error_status(json));
    let message = error
        .and_then(|error| {
            error
                .get("message")
                .and_then(Value::as_str)
                .or_else(|| error.as_str())
        })
        .unwrap_or("Codex stream error");

    LlmStreamError::provider(code, status, message)
}

fn provider_error_status(value: &Value) -> Option<u16> {
    value
        .get("status_code")
        .or_else(|| value.get("status"))
        .and_then(Value::as_u64)
        .and_then(|status| u16::try_from(status).ok())
}

fn metadata_extra_string(metadata: &ProviderMetadata, key: &str) -> Option<String> {
    metadata
        .extra
        .as_ref()
        .and_then(|extra| extra.get(key))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn metadata_extra_i64(metadata: &ProviderMetadata, key: &str) -> Option<i64> {
    metadata
        .extra
        .as_ref()
        .and_then(|extra| extra.get(key))
        .and_then(Value::as_i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use everruns_core::driver_registry::{LlmMessage, LlmMessageRole};

    #[test]
    fn converts_tool_result_to_function_call_output() {
        let mut message = LlmMessage::text(LlmMessageRole::Tool, "ok");
        message.tool_call_id = Some("call_1".to_string());
        let (_, input) = build_input(&[
            LlmMessage {
                role: LlmMessageRole::Assistant,
                content: LlmMessageContent::Text(String::new()),
                tool_calls: Some(vec![ToolCall {
                    id: "call_1".to_string(),
                    name: "do_it".to_string(),
                    arguments: json!({}),
                }]),
                tool_call_id: None,
                phase: None,
                thinking: None,
                thinking_signature: None,
            },
            message,
        ]);
        assert!(matches!(
            input.last(),
            Some(CodexInputItem::FunctionCallOutput { call_id, .. }) if call_id == "call_1"
        ));
    }

    #[test]
    fn parses_text_delta_event() {
        let event = handle_event(
            r#"{"type":"response.output_text.delta","delta":"hello"}"#,
            "gpt-test",
            &Mutex::new(0),
            &Mutex::new(0),
            &Mutex::new(None),
            &Mutex::new(None),
            &Mutex::new(Vec::new()),
        );
        assert!(matches!(event, LlmStreamEvent::TextDelta(text) if text == "hello"));
    }

    #[test]
    fn response_failed_preserves_provider_error_code() {
        let event = handle_event(
            r#"{
                "type": "response.failed",
                "response": {
                    "id": "resp_failed",
                    "status": "failed",
                    "error": {
                        "code": "processing_error",
                        "message": "An error occurred while processing your request. Please include the request ID req_test_processing_error in your message."
                    }
                }
            }"#,
            "gpt-test",
            &Mutex::new(0),
            &Mutex::new(0),
            &Mutex::new(None),
            &Mutex::new(None),
            &Mutex::new(Vec::new()),
        );

        let LlmStreamEvent::Error(error) = event else {
            panic!("expected structured stream error");
        };
        assert_eq!(error.code.as_deref(), Some("processing_error"));
        assert_eq!(error.status, None);
        assert!(error.message.contains("req_test_processing_error"));
        assert_eq!(
            error.kind(),
            everruns_core::error::LlmErrorKind::Unavailable
        );
    }

    #[test]
    fn error_event_preserves_provider_status() {
        let event = handle_event(
            r#"{
                "type": "error",
                "error": {
                    "code": "server_error",
                    "status_code": 503,
                    "message": "Service temporarily unavailable"
                }
            }"#,
            "gpt-test",
            &Mutex::new(0),
            &Mutex::new(0),
            &Mutex::new(None),
            &Mutex::new(None),
            &Mutex::new(Vec::new()),
        );

        let LlmStreamEvent::Error(error) = event else {
            panic!("expected structured stream error");
        };
        assert_eq!(error.code.as_deref(), Some("server_error"));
        assert_eq!(error.status, Some(503));
        assert_eq!(error.message, "Service temporarily unavailable");
    }

    fn reasoning_event(delta_json: &str) -> LlmStreamEvent {
        handle_event(
            delta_json,
            "gpt-test",
            &Mutex::new(0),
            &Mutex::new(0),
            &Mutex::new(None),
            &Mutex::new(None),
            &Mutex::new(Vec::new()),
        )
    }

    #[test]
    fn keeps_reasoning_summary_prose() {
        // A normal summary part with real prose passes through as thinking.
        let event = reasoning_event(
            r#"{"type":"response.reasoning_summary_text.delta","delta":"**Planning** real prose"}"#,
        );
        assert!(
            matches!(event, LlmStreamEvent::ThinkingDelta(text) if text == "**Planning** real prose")
        );
    }

    #[test]
    fn drops_empty_reasoning_summary_placeholder() {
        // An empty summary part arrives as the literal `<!-- -->` placeholder
        // (see PR #31652 linked above). It must not reach the thinking stream.
        let event = reasoning_event(
            r#"{"type":"response.reasoning_summary_text.delta","delta":"<!-- -->"}"#,
        );
        assert!(matches!(event, LlmStreamEvent::TextDelta(text) if text.is_empty()));
    }

    #[test]
    fn strips_placeholder_but_keeps_heading_in_same_delta() {
        // When a whole part arrives in one delta, drop the marker and keep the heading.
        let event = reasoning_event(
            r#"{"type":"response.reasoning_summary_text.delta","delta":"**Header**\n\n<!-- -->"}"#,
        );
        assert!(matches!(event, LlmStreamEvent::ThinkingDelta(text) if text == "**Header**\n\n"));
    }

    #[test]
    fn codex_request_serializes_store_false() {
        let request = CodexResponsesRequest {
            model: "gpt-5.5".to_string(),
            store: false,
            input: vec![CodexInputItem::Message {
                r#type: "message".to_string(),
                role: "user".to_string(),
                content: CodexContent::Text("hi".to_string()),
            }],
            instructions: None,
            temperature: None,
            max_output_tokens: None,
            stream: true,
            tools: None,
            reasoning: None,
        };

        let json = serde_json::to_value(request).expect("serialize request");
        assert_eq!(json.get("store"), Some(&serde_json::Value::Bool(false)));
        assert!(json.get("metadata").is_none());
        assert!(json.get("previous_response_id").is_none());
    }
}

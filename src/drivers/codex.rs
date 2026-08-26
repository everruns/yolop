use crate::auth::codex::CODEX_ORIGINATOR;
use crate::config::{CodexAuth, SettingsStore};
use async_trait::async_trait;
use eventsource_stream::Eventsource;
use everruns_provider::ProviderEndpoint;
use everruns_provider::driver_registry::DriverConfig;
use everruns_provider::error::Result as EverrunsResult;
use everruns_provider::{AgentLoopError, LlmErrorKind};
use everruns_provider::{
    ChatDriver, LlmCallConfig, LlmCompletionMetadata, LlmContentPart, LlmMessage,
    LlmMessageContent, LlmMessageRole, LlmResponseStream, LlmStreamError, LlmStreamEvent,
    ProviderMetadata, ProviderOpaqueContext,
};
use everruns_provider::{
    CompactContent, CompactContentPart, CompactOutputItem, CompactRequest, CompactResponse,
    DriverId, ModelProfile, get_model_profile,
};
use everruns_provider::{DiscoveredModel, DriverRegistry};
use everruns_provider::{ToolCall, ToolDefinition};
use futures::StreamExt;
use reqwest::StatusCode;
use reqwest::header::{HeaderMap, HeaderValue};
use serde::Serialize;
use serde_json::{Value, json};
use std::future::Future;
use std::sync::{Arc, Mutex};

pub const CODEX_DRIVER_ID: &str = "openai-codex";
const CODEX_RESPONSES_URL: &str = "https://chatgpt.com/backend-api/codex/responses";
const CODEX_BETA_HEADER: &str = "responses=experimental";
const REAUTH_MESSAGE: &str =
    "Codex login is no longer valid. Run `/setup` and sign in with Codex again.";

/// Host-side Codex OAuth storage. Reload must hit disk so another process that
/// already rotated the refresh token is visible before we call `/oauth/token`.
pub(crate) trait CodexAuthStore: Send + Sync {
    fn load_from_disk(&self) -> Option<CodexAuth>;
    fn save(&self, auth: CodexAuth) -> anyhow::Result<()>;
    fn clear(&self) -> anyhow::Result<()>;
}

impl CodexAuthStore for SettingsStore {
    fn load_from_disk(&self) -> Option<CodexAuth> {
        self.refresh_codex_auth_from_disk()
    }

    fn save(&self, auth: CodexAuth) -> anyhow::Result<()> {
        self.set_codex_auth(auth)
    }

    fn clear(&self) -> anyhow::Result<()> {
        self.clear_codex_auth().map(|_| ())
    }
}

#[derive(Debug, Clone)]
struct CodexTokens {
    access_token: String,
    refresh_token: Option<String>,
    expires_at: Option<i64>,
    account_id: Option<String>,
    /// Preserved across refresh for settings.toml; not sent on API calls.
    email: Option<String>,
}

#[derive(Clone)]
pub struct CodexChatDriver {
    client: reqwest::Client,
    responses_url: String,
    tokens: Arc<tokio::sync::Mutex<CodexTokens>>,
    refresh_gate: Arc<tokio::sync::Mutex<()>>,
    auth_store: Option<Arc<dyn CodexAuthStore>>,
}

pub fn register_driver(registry: &mut DriverRegistry, settings: Arc<SettingsStore>) {
    let auth_store: Arc<dyn CodexAuthStore> = settings;
    let refresh_gate = Arc::new(tokio::sync::Mutex::new(()));
    registry.register_external(CODEX_DRIVER_ID, move |config| {
        Box::new(CodexChatDriver::from_config_with_refresh_gate(
            config,
            Some(auth_store.clone()),
            refresh_gate.clone(),
        ))
    });
}

pub(crate) fn model_profile(model_id: &str) -> Option<ModelProfile> {
    get_model_profile(&DriverId::OpenAI, model_id)
}

impl CodexChatDriver {
    fn from_config_with_refresh_gate(
        config: &DriverConfig,
        auth_store: Option<Arc<dyn CodexAuthStore>>,
        refresh_gate: Arc<tokio::sync::Mutex<()>>,
    ) -> Self {
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
            .or_else(|| crate::auth::codex::extract_account_id(&access_token));
        let email = auth_store
            .as_ref()
            .and_then(|store| store.load_from_disk())
            .and_then(|auth| auth.email);
        Self {
            client: reqwest::Client::new(),
            responses_url: CODEX_RESPONSES_URL.to_string(),
            tokens: Arc::new(tokio::sync::Mutex::new(CodexTokens {
                access_token,
                refresh_token,
                expires_at,
                account_id,
                email,
            })),
            refresh_gate,
            auth_store,
        }
    }

    async fn token_snapshot(&self) -> EverrunsResult<CodexTokens> {
        self.refresh_tokens_with(|refresh_token| async move {
            crate::auth::codex::refresh_with_token(&refresh_token).await
        })
        .await
    }

    async fn refresh_tokens_with<F, Fut>(&self, refresh: F) -> EverrunsResult<CodexTokens>
    where
        F: Fn(String) -> Fut,
        Fut: Future<Output = anyhow::Result<CodexAuth>>,
    {
        // Refresh tokens are single-use. Factories can create multiple driver
        // instances, so serialize the disk reload + rotation across all of them.
        let _refresh_guard = self.refresh_gate.lock().await;
        let mut guard = self.tokens.lock().await;
        ensure_fresh_tokens(&mut guard, self.auth_store.as_deref(), refresh).await?;
        Ok(guard.clone())
    }
}

/// Refresh Codex OAuth tokens when near expiry, persisting the rotated pair.
///
/// OpenAI refresh tokens are single-use. Without persisting, the next process
/// start reuses the spent token and fails with `refresh_token_reused`. Before
/// refreshing we adopt fresher on-disk credentials (another process may have
/// rotated already). On `refresh_token_reused` we reload once; if still stuck,
/// clear broken login and ask the user to run `/setup` — do not auto-open a
/// browser mid-turn.
async fn ensure_fresh_tokens<F, Fut>(
    tokens: &mut CodexTokens,
    store: Option<&dyn CodexAuthStore>,
    refresh: F,
) -> EverrunsResult<()>
where
    F: Fn(String) -> Fut,
    Fut: Future<Output = anyhow::Result<CodexAuth>>,
{
    if tokens.access_token.is_empty() {
        return Err(AgentLoopError::llm_kind(
            LlmErrorKind::Authentication,
            "Codex provider requires a saved OAuth access token",
        ));
    }

    if let Some(store) = store {
        adopt_disk_auth(tokens, store);
    }

    if !crate::auth::codex::should_refresh(tokens.expires_at) {
        return Ok(());
    }

    let Some(refresh_token) = tokens.refresh_token.clone() else {
        return Ok(());
    };

    match refresh(refresh_token.clone()).await {
        Ok(refreshed) => {
            apply_refreshed(tokens, refreshed, &refresh_token);
            persist_tokens(tokens, store)?;
            Ok(())
        }
        Err(err) if crate::auth::codex::is_refresh_token_reused(&err) => {
            recover_from_refresh_token_reused(tokens, store, refresh, &refresh_token).await
        }
        Err(err) => Err(classified_refresh_error(&err)),
    }
}

async fn recover_from_refresh_token_reused<F, Fut>(
    tokens: &mut CodexTokens,
    store: Option<&dyn CodexAuthStore>,
    refresh: F,
    spent_refresh_token: &str,
) -> EverrunsResult<()>
where
    F: Fn(String) -> Fut,
    Fut: Future<Output = anyhow::Result<CodexAuth>>,
{
    if let Some(store) = store {
        adopt_disk_auth(tokens, store);
        if !crate::auth::codex::should_refresh(tokens.expires_at) {
            return Ok(());
        }
        if let Some(retry_token) = tokens.refresh_token.clone()
            && retry_token != spent_refresh_token
        {
            match refresh(retry_token.clone()).await {
                Ok(refreshed) => {
                    apply_refreshed(tokens, refreshed, &retry_token);
                    persist_tokens(tokens, Some(store))?;
                    return Ok(());
                }
                Err(err) if crate::auth::codex::is_refresh_token_reused(&err) => {
                    // Fall through to clear + re-auth message.
                }
                Err(err) => {
                    return Err(classified_refresh_error(&err));
                }
            }
        }
        if let Err(clear_err) = store.clear() {
            tracing::warn!(error = %clear_err, "failed to clear invalid Codex auth");
        }
    }
    Err(AgentLoopError::llm_kind(
        LlmErrorKind::Authentication,
        REAUTH_MESSAGE,
    ))
}

fn classified_refresh_error(error: &anyhow::Error) -> AgentLoopError {
    let message = format!("{error:#}");
    let lower = message.to_ascii_lowercase();
    let kind = if lower.contains("(429") {
        LlmErrorKind::RateLimited
    } else if lower.contains("refresh codex token")
        || ["(500", "(502", "(503", "(504"]
            .iter()
            .any(|status| lower.contains(status))
    {
        LlmErrorKind::Unavailable
    } else {
        LlmErrorKind::Authentication
    };
    AgentLoopError::llm_kind(kind, format!("Codex token refresh failed: {message}"))
}

fn codex_response_error(
    operation: &str,
    status: StatusCode,
    body: String,
    store: Option<&dyn CodexAuthStore>,
) -> AgentLoopError {
    let kind = LlmErrorKind::from_provider_status(status.as_u16(), &body);
    if status == StatusCode::UNAUTHORIZED {
        if let Some(store) = store
            && let Err(clear_err) = store.clear()
        {
            tracing::warn!(error = %clear_err, "failed to clear invalid Codex auth");
        }
        return AgentLoopError::llm_kind(LlmErrorKind::Authentication, REAUTH_MESSAGE);
    }
    if kind == LlmErrorKind::Authentication {
        return AgentLoopError::llm_kind(
            kind,
            "Codex access was denied. Run `/setup` to sign in again, or choose another provider.",
        );
    }
    AgentLoopError::llm_kind(kind, format!("Codex {operation} error ({status}): {body}"))
}

fn adopt_disk_auth(tokens: &mut CodexTokens, store: &dyn CodexAuthStore) {
    let Some(disk) = store.load_from_disk() else {
        return;
    };
    if disk.access_token == tokens.access_token
        && disk.refresh_token == tokens.refresh_token
        && disk.expires_at == tokens.expires_at
    {
        return;
    }
    tokens.access_token = disk.access_token;
    tokens.refresh_token = disk.refresh_token;
    tokens.expires_at = disk.expires_at;
    if disk.account_id.is_some() {
        tokens.account_id = disk.account_id;
    }
    if disk.email.is_some() {
        tokens.email = disk.email;
    }
}

fn apply_refreshed(tokens: &mut CodexTokens, refreshed: CodexAuth, used_refresh_token: &str) {
    tokens.access_token = refreshed.access_token;
    tokens.refresh_token = refreshed
        .refresh_token
        .or_else(|| Some(used_refresh_token.to_string()));
    tokens.expires_at = refreshed.expires_at;
    tokens.account_id = refreshed
        .account_id
        .or_else(|| tokens.account_id.clone())
        .or_else(|| crate::auth::codex::extract_account_id(&tokens.access_token));
    if refreshed.email.is_some() {
        tokens.email = refreshed.email;
    }
}

fn persist_tokens(tokens: &CodexTokens, store: Option<&dyn CodexAuthStore>) -> EverrunsResult<()> {
    let Some(store) = store else {
        return Ok(());
    };
    store
        .save(CodexAuth {
            access_token: tokens.access_token.clone(),
            refresh_token: tokens.refresh_token.clone(),
            expires_at: tokens.expires_at,
            account_id: tokens.account_id.clone(),
            email: tokens.email.clone(),
        })
        .map_err(|err| {
            AgentLoopError::llm(format!(
                "Codex token refresh succeeded but saving credentials failed: {err:#}"
            ))
        })
}

#[async_trait]
impl ChatDriver for CodexChatDriver {
    // 0.17.26 separated providers from drivers: endpoint and auth policy now
    // arrive per call instead of being baked into the driver. Codex is bound to
    // ChatGPT's own endpoint and signs with rotating OAuth tokens it owns, so it
    // ignores the supplied endpoint — the same thing upstream's own
    // `ProviderBoundDriver` does for drivers that carry their own binding.
    async fn chat_completion_stream(
        &self,
        _endpoint: &ProviderEndpoint,
        messages: Vec<LlmMessage>,
        config: &LlmCallConfig,
    ) -> EverrunsResult<LlmResponseStream> {
        let tokens = self.token_snapshot().await?;
        let (instructions, transcript_input) = build_input(&messages);
        let input = compose_input(config.provider_opaque_context.as_ref(), transcript_input);
        let request = CodexResponsesRequest {
            model: config.model.clone(),
            store: false,
            input,
            instructions,
            temperature: config.temperature,
            max_output_tokens: config.max_tokens,
            stream: true,
            tools: (!config.tools.is_empty()).then(|| convert_tools(&config.tools)),
            reasoning: config.reasoning_effort.map(|effort| CodexReasoning {
                effort: effort.as_str().to_string(),
                summary: "auto".to_string(),
            }),
        };

        let headers = codex_headers(&tokens, config.metadata.get("session_id"));

        let response = self
            .client
            .post(&self.responses_url)
            .bearer_auth(&tokens.access_token)
            .headers(headers)
            .json(&request)
            .send()
            .await
            .map_err(|err| AgentLoopError::llm(format!("Failed to send Codex request: {err}")))?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(codex_response_error(
                "API",
                status,
                body,
                self.auth_store.as_deref(),
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

    async fn list_models(
        &self,
        _endpoint: &ProviderEndpoint,
    ) -> EverrunsResult<Option<Vec<DiscoveredModel>>> {
        Ok(None)
    }

    fn supports_compact(&self) -> bool {
        true
    }

    fn effective_context_window(&self, model: &str) -> Option<usize> {
        model_profile(model).and_then(|profile| {
            profile
                .limits
                .map(|limits| usize::try_from(limits.context).unwrap_or(usize::MAX))
        })
    }

    async fn compact(
        &self,
        _endpoint: &ProviderEndpoint,
        request: CompactRequest,
    ) -> EverrunsResult<Option<CompactResponse>> {
        let tokens = self.token_snapshot().await?;
        let response = self
            .client
            .post(format!(
                "{}/compact",
                self.responses_url.trim_end_matches('/')
            ))
            .bearer_auth(&tokens.access_token)
            .headers(codex_headers(&tokens, None))
            .json(&request)
            .send()
            .await
            .map_err(|err| {
                AgentLoopError::llm(format!("Failed to send Codex compact request: {err}"))
            })?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(codex_response_error(
                "compact API",
                status,
                body,
                self.auth_store.as_deref(),
            ));
        }

        let compact = response
            .json::<CompactResponse>()
            .await
            .map_err(|err| AgentLoopError::llm(format!("Invalid Codex compact response: {err}")))?;
        Ok(Some(compact))
    }
}

fn codex_headers(tokens: &CodexTokens, session_id: Option<&String>) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert("OpenAI-Beta", HeaderValue::from_static(CODEX_BETA_HEADER));
    headers.insert("originator", HeaderValue::from_static(CODEX_ORIGINATOR));
    if let Some(account_id) = &tokens.account_id
        && let Ok(value) = HeaderValue::from_str(account_id)
    {
        headers.insert("chatgpt-account-id", value.clone());
        headers.insert("ChatGPT-Account-Id", value);
    }
    if let Some(session_id) = session_id
        && let Ok(value) = HeaderValue::from_str(session_id)
    {
        headers.insert("session_id", value);
    }
    headers
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
    Compaction {
        r#type: String,
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
    strict: bool,
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
    for message in messages {
        if message.role == LlmMessageRole::System {
            instructions.push(message.content.to_text());
            continue;
        }
        if message.role == LlmMessageRole::Assistant {
            // 0.19 replaced the single flat `thinking_signature` with an ordered
            // list of reasoning parts, each carrying the id the provider issued.
            // Codex keys replay by that id, so send the real one; a part with no
            // encrypted payload has nothing to replay and is skipped.
            for part in &message.reasoning {
                let (Some(item_id), Some(encrypted)) = (&part.item_id, &part.encrypted) else {
                    continue;
                };
                input.push(CodexInputItem::Reasoning {
                    r#type: "reasoning".to_string(),
                    id: item_id.clone(),
                    encrypted_content: encrypted.clone(),
                });
            }
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
    (instructions, repair_unpaired_function_items(input))
}

fn compose_input(
    compacted_context: Option<&ProviderOpaqueContext>,
    transcript_input: Vec<CodexInputItem>,
) -> Vec<CodexInputItem> {
    let Some(ProviderOpaqueContext::OpenResponsesCompact { output }) = compacted_context else {
        return transcript_input;
    };

    output
        .iter()
        .map(compact_output_item)
        .chain(transcript_input)
        .collect()
}

fn compact_output_item(item: &CompactOutputItem) -> CodexInputItem {
    match item {
        CompactOutputItem::Message { role, content } => CodexInputItem::Message {
            r#type: "message".to_string(),
            role: role.clone(),
            content: match content {
                CompactContent::Text(text) => CodexContent::Text(text.clone()),
                CompactContent::Parts(parts) => CodexContent::Parts(
                    parts
                        .iter()
                        .map(|part| match part {
                            CompactContentPart::InputText { text } => CodexContentPart::InputText {
                                r#type: "input_text".to_string(),
                                text: text.clone(),
                            },
                            CompactContentPart::InputImage { image_url } => {
                                CodexContentPart::InputImage {
                                    r#type: "input_image".to_string(),
                                    image_url: image_url.clone(),
                                }
                            }
                        })
                        .collect(),
                ),
            },
        },
        CompactOutputItem::Compaction { encrypted_content } => CodexInputItem::Compaction {
            r#type: "compaction".to_string(),
            encrypted_content: encrypted_content.clone(),
        },
    }
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

fn repair_unpaired_function_items(input: Vec<CodexInputItem>) -> Vec<CodexInputItem> {
    let call_ids: std::collections::HashSet<String> = input
        .iter()
        .filter_map(|item| match item {
            CodexInputItem::FunctionCall { call_id, .. } => Some(call_id.clone()),
            _ => None,
        })
        .collect();
    let output_ids: std::collections::HashSet<String> = input
        .iter()
        .filter_map(|item| match item {
            CodexInputItem::FunctionCallOutput { call_id, .. } => Some(call_id.clone()),
            _ => None,
        })
        .collect();
    let unpaired: std::collections::HashSet<String> = call_ids
        .symmetric_difference(&output_ids)
        .cloned()
        .collect();

    if unpaired.is_empty() {
        return input;
    }

    tracing::warn!(
        unpaired_call_ids = ?unpaired,
        "dropping unpaired function calls and outputs before Codex request"
    );

    input
        .into_iter()
        .filter(|item| match item {
            CodexInputItem::FunctionCall { call_id, .. }
            | CodexInputItem::FunctionCallOutput { call_id, .. } => {
                !unpaired.contains(call_id.as_str())
            }
            _ => true,
        })
        .collect()
}

fn convert_tools(tools: &[ToolDefinition]) -> Vec<CodexTool> {
    tools
        .iter()
        .map(|tool| {
            let parameters = sanitize_parameters(tool.parameters());
            CodexTool {
                r#type: "function".to_string(),
                name: tool.name().to_string(),
                description: tool.description().to_string(),
                strict: openai_strict_schema_compatible(&parameters),
                parameters,
            }
        })
        .collect()
}

/// OpenAI strict tool schemas require closed objects with every property named
/// in `required`. Stay conservative: an unsupported construct falls back to the
/// provider's ordinary best-effort decoding and the host validator remains the
/// authoritative execution boundary.
fn openai_strict_schema_compatible(schema: &Value) -> bool {
    fn visit(schema: &Value, depth: usize, property_count: &mut usize) -> bool {
        if depth > 10 {
            return false;
        }
        let Some(object) = schema.as_object() else {
            return false;
        };
        const SUPPORTED: &[&str] = &[
            "$defs",
            "$ref",
            "additionalProperties",
            "anyOf",
            "const",
            "description",
            "enum",
            "exclusiveMaximum",
            "exclusiveMinimum",
            "format",
            "items",
            "maxItems",
            "maxLength",
            "maxProperties",
            "maximum",
            "minItems",
            "minLength",
            "minProperties",
            "minimum",
            "multipleOf",
            "pattern",
            "properties",
            "required",
            "title",
            "type",
            "uniqueItems",
        ];
        if object.keys().any(|key| !SUPPORTED.contains(&key.as_str())) {
            return false;
        }

        if object.get("type").and_then(Value::as_str) == Some("object") {
            if object.get("additionalProperties") != Some(&Value::Bool(false)) {
                return false;
            }
            let Some(properties) = object.get("properties").and_then(Value::as_object) else {
                return false;
            };
            let Some(required_values) = object.get("required").and_then(Value::as_array) else {
                return false;
            };
            let required = required_values
                .iter()
                .filter_map(Value::as_str)
                .collect::<std::collections::HashSet<_>>();
            if required.len() != required_values.len()
                || required.len() != properties.len()
                || properties
                    .keys()
                    .any(|key| !required.contains(key.as_str()))
            {
                return false;
            }
            *property_count += properties.len();
            if *property_count > 5_000
                || properties
                    .values()
                    .any(|property| !visit(property, depth + 1, property_count))
            {
                return false;
            }
        }

        if object
            .get("items")
            .is_some_and(|child| !visit(child, depth + 1, property_count))
        {
            return false;
        }
        if let Some(any_of) = object.get("anyOf") {
            let Some(children) = any_of.as_array() else {
                return false;
            };
            if children.is_empty()
                || children
                    .iter()
                    .any(|child| !visit(child, depth + 1, property_count))
            {
                return false;
            }
        }
        if let Some(definitions) = object.get("$defs") {
            let Some(definitions) = definitions.as_object() else {
                return false;
            };
            if definitions
                .values()
                .any(|child| !visit(child, depth + 1, property_count))
            {
                return false;
            }
        }
        true
    }

    // This byte bound is stricter than OpenAI's 120k aggregate limit for
    // schema-authored names/enum/const strings and is cheap to establish.
    if schema.get("type").and_then(Value::as_str) != Some("object")
        || !serde_json::to_vec(schema).is_ok_and(|encoded| encoded.len() <= 120_000)
    {
        return false;
    }
    let mut property_count = 0;
    visit(schema, 1, &mut property_count)
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
        // `summary` separates a provider-curated gloss from raw chain-of-thought.
        // Codex sends both on distinct event types, so the distinction is read
        // off the wire rather than guessed downstream.
        Some(event @ "response.reasoning_summary_text.delta")
        | Some(event @ "response.reasoning_text.delta")
        | Some(event @ "response.reasoning.delta") => {
            let summary = event == "response.reasoning_summary_text.delta";
            json.get("delta")
                .and_then(Value::as_str)
                .map(|delta| match strip_reasoning_placeholder(delta) {
                    cleaned if cleaned.is_empty() => LlmStreamEvent::TextDelta(String::new()),
                    cleaned => LlmStreamEvent::ReasoningDelta {
                        delta: cleaned,
                        summary,
                    },
                })
                .unwrap_or_else(|| LlmStreamEvent::TextDelta(String::new()))
        }
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
        cache_diagnostics: None,
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
    use everruns_provider::{BuiltinTool, DeferrablePolicy, ToolDefinition, ToolHints, ToolPolicy};
    use everruns_provider::{LlmMessage, LlmMessageRole};
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::Mutex as StdMutex;
    use std::sync::mpsc;
    use std::thread;

    #[derive(Default)]
    struct MemoryAuthStore {
        auth: StdMutex<Option<CodexAuth>>,
        saves: StdMutex<usize>,
        clears: StdMutex<usize>,
    }

    impl CodexAuthStore for MemoryAuthStore {
        fn load_from_disk(&self) -> Option<CodexAuth> {
            self.auth.lock().expect("auth lock").clone()
        }

        fn save(&self, auth: CodexAuth) -> anyhow::Result<()> {
            *self.auth.lock().expect("auth lock") = Some(auth);
            *self.saves.lock().expect("saves lock") += 1;
            Ok(())
        }

        fn clear(&self) -> anyhow::Result<()> {
            *self.auth.lock().expect("auth lock") = None;
            *self.clears.lock().expect("clears lock") += 1;
            Ok(())
        }
    }

    fn expired_tokens(refresh: &str) -> CodexTokens {
        CodexTokens {
            access_token: "access-old".to_string(),
            refresh_token: Some(refresh.to_string()),
            expires_at: Some(crate::auth::codex::now_epoch_millis() - 1_000),
            account_id: Some("acc".to_string()),
            email: Some("user@example.com".to_string()),
        }
    }

    fn test_driver(responses_url: String) -> CodexChatDriver {
        CodexChatDriver {
            client: reqwest::Client::new(),
            responses_url,
            tokens: Arc::new(tokio::sync::Mutex::new(CodexTokens {
                access_token: "test-access-token".to_string(),
                refresh_token: None,
                expires_at: Some(crate::auth::codex::now_epoch_millis() + 3_600_000),
                account_id: Some("account-test".to_string()),
                email: None,
            })),
            refresh_gate: Arc::new(tokio::sync::Mutex::new(())),
            auth_store: None,
        }
    }

    fn tool_with_schema(name: &str, parameters: Value) -> ToolDefinition {
        ToolDefinition::Builtin(BuiltinTool {
            name: name.to_string(),
            display_name: None,
            description: "test tool".to_string(),
            parameters,
            policy: ToolPolicy::Auto,
            category: None,
            deferrable: DeferrablePolicy::Automatic,
            hints: ToolHints::default(),
            full_parameters: None,
        })
    }

    #[test]
    fn compatible_tool_schema_requests_strict_decoding() {
        let tools = convert_tools(&[tool_with_schema(
            "checkpoint",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "missing_evidence": {
                        "type": "array",
                        "items": { "type": "string" }
                    }
                },
                "required": ["missing_evidence"],
                "additionalProperties": false
            }),
        )]);
        let wire = serde_json::to_value(tools).expect("serialize tools");

        assert_eq!(wire[0]["strict"], true);
    }

    #[test]
    fn permissive_or_optional_tool_schema_does_not_request_strict_decoding() {
        let tools = convert_tools(&[
            tool_with_schema(
                "deferred",
                serde_json::json!({ "type": "object", "additionalProperties": true }),
            ),
            tool_with_schema(
                "optional",
                serde_json::json!({
                    "type": "object",
                    "properties": { "value": { "type": "string" } },
                    "required": [],
                    "additionalProperties": false
                }),
            ),
            tool_with_schema(
                "malformed",
                serde_json::json!({
                    "type": "object",
                    "properties": "not-an-object",
                    "required": [],
                    "additionalProperties": false
                }),
            ),
        ]);
        let wire = serde_json::to_value(tools).expect("serialize tools");

        assert_eq!(wire[0]["strict"], false);
        assert_eq!(wire[1]["strict"], false);
        assert_eq!(wire[2]["strict"], false);
    }

    fn serve_one_json_response(response_body: &'static str) -> (String, mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock Codex server");
        let address = listener.local_addr().expect("mock server address");
        let (request_tx, request_rx) = mpsc::channel();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept Codex request");
            let mut request = Vec::new();
            let mut buffer = [0u8; 4096];
            let header_end = loop {
                let count = stream.read(&mut buffer).expect("read Codex request");
                assert!(count > 0, "request ended before headers");
                request.extend_from_slice(&buffer[..count]);
                if let Some(index) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                    break index + 4;
                }
            };
            let headers = String::from_utf8_lossy(&request[..header_end]);
            let content_length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().expect("content length"))
                })
                .unwrap_or(0);
            while request.len() < header_end + content_length {
                let count = stream.read(&mut buffer).expect("read Codex request body");
                assert!(count > 0, "request body ended early");
                request.extend_from_slice(&buffer[..count]);
            }
            request_tx
                .send(String::from_utf8(request).expect("UTF-8 request"))
                .expect("capture request");

            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                response_body.len(),
                response_body
            );
            stream
                .write_all(response.as_bytes())
                .expect("write compact response");
        });
        (format!("http://{address}/responses"), request_rx)
    }

    #[test]
    fn compacted_context_is_preserved_in_order_before_raw_suffix() {
        let context = ProviderOpaqueContext::OpenResponsesCompact {
            output: vec![
                CompactOutputItem::Message {
                    role: "user".to_string(),
                    content: CompactContent::Text("retained".to_string()),
                },
                CompactOutputItem::Compaction {
                    encrypted_content: "opaque-checkpoint".to_string(),
                },
            ],
        };
        let suffix = vec![CodexInputItem::Message {
            r#type: "message".to_string(),
            role: "user".to_string(),
            content: CodexContent::Text("fresh suffix".to_string()),
        }];

        assert_eq!(
            serde_json::to_value(compose_input(Some(&context), suffix)).unwrap(),
            json!([
                { "type": "message", "role": "user", "content": "retained" },
                { "type": "compaction", "encrypted_content": "opaque-checkpoint" },
                { "type": "message", "role": "user", "content": "fresh suffix" }
            ])
        );
    }

    #[test]
    fn reports_the_codex_model_context_window_to_runtime_policy() {
        let driver = test_driver("http://127.0.0.1:1/responses".to_string());

        assert_eq!(
            ChatDriver::effective_context_window(&driver, "gpt-5.6-sol"),
            Some(1_050_000)
        );
    }

    #[tokio::test]
    async fn compact_calls_codex_endpoint_with_native_request_and_auth_headers() {
        let (responses_url, request_rx) = serve_one_json_response(
            r#"{"output":[{"type":"compaction","encrypted_content":"opaque-result"}],"usage":{"input_tokens":100,"output_tokens":12,"total_tokens":112}}"#,
        );
        let driver = test_driver(responses_url);

        let response = ChatDriver::compact(
            &driver,
            &ProviderEndpoint::default(),
            CompactRequest {
                model: "gpt-5.6".to_string(),
                input: vec![everruns_provider::compact::CompactInputItem::Message {
                    role: "user".to_string(),
                    content: CompactContent::Text("compact me".to_string()),
                }],
                previous_response_id: None,
                instructions: Some("instructions".to_string()),
            },
        )
        .await
        .expect("compact request")
        .expect("native compact response");

        assert!(matches!(
            response.output.as_slice(),
            [CompactOutputItem::Compaction { encrypted_content }]
                if encrypted_content == "opaque-result"
        ));
        let request = request_rx.recv().expect("captured request");
        assert!(request.starts_with("POST /responses/compact HTTP/1.1\r\n"));
        assert!(
            request
                .to_ascii_lowercase()
                .contains("authorization: bearer test-access-token")
        );
        assert!(
            request
                .to_ascii_lowercase()
                .contains("chatgpt-account-id: account-test")
        );
        let body = request.split_once("\r\n\r\n").expect("request body").1;
        let body: Value = serde_json::from_str(body).expect("compact JSON body");
        assert_eq!(body["model"], "gpt-5.6");
        assert_eq!(body["input"][0]["type"], "message");
        assert!(body.get("previous_response_id").is_none());
    }

    #[tokio::test]
    async fn refresh_persists_rotated_tokens_to_store() {
        let store = MemoryAuthStore::default();
        store
            .save(CodexAuth {
                access_token: "access-old".to_string(),
                refresh_token: Some("refresh-old".to_string()),
                expires_at: Some(crate::auth::codex::now_epoch_millis() - 1_000),
                account_id: Some("acc".to_string()),
                email: Some("user@example.com".to_string()),
            })
            .unwrap();
        let mut tokens = expired_tokens("refresh-old");
        let refresh_calls = Arc::new(StdMutex::new(0usize));
        let refresh_calls_clone = refresh_calls.clone();

        ensure_fresh_tokens(&mut tokens, Some(&store), move |_rt| {
            let refresh_calls_clone = refresh_calls_clone.clone();
            async move {
                *refresh_calls_clone.lock().expect("calls") += 1;
                Ok(CodexAuth {
                    access_token: "access-new".to_string(),
                    refresh_token: Some("refresh-new".to_string()),
                    expires_at: Some(crate::auth::codex::now_epoch_millis() + 3_600_000),
                    account_id: Some("acc".to_string()),
                    email: None,
                })
            }
        })
        .await
        .expect("refresh");

        assert_eq!(*refresh_calls.lock().unwrap(), 1);
        assert_eq!(tokens.access_token, "access-new");
        assert_eq!(tokens.refresh_token.as_deref(), Some("refresh-new"));
        assert_eq!(tokens.email.as_deref(), Some("user@example.com"));
        assert_eq!(*store.saves.lock().unwrap(), 2); // initial seed + persist
        let saved = store.load_from_disk().expect("saved");
        assert_eq!(saved.access_token, "access-new");
        assert_eq!(saved.refresh_token.as_deref(), Some("refresh-new"));
        assert_eq!(saved.email.as_deref(), Some("user@example.com"));
    }

    #[tokio::test]
    async fn adopts_fresher_disk_auth_instead_of_refreshing() {
        let store = MemoryAuthStore::default();
        store
            .save(CodexAuth {
                access_token: "access-from-disk".to_string(),
                refresh_token: Some("refresh-from-disk".to_string()),
                expires_at: Some(crate::auth::codex::now_epoch_millis() + 3_600_000),
                account_id: Some("acc".to_string()),
                email: Some("user@example.com".to_string()),
            })
            .unwrap();
        let mut tokens = expired_tokens("refresh-old");
        let refresh_calls = Arc::new(StdMutex::new(0usize));
        let refresh_calls_clone = refresh_calls.clone();

        ensure_fresh_tokens(&mut tokens, Some(&store), move |_rt| {
            let refresh_calls_clone = refresh_calls_clone.clone();
            async move {
                *refresh_calls_clone.lock().expect("calls") += 1;
                Err(anyhow::anyhow!("should not refresh"))
            }
        })
        .await
        .expect("adopt disk");

        assert_eq!(*refresh_calls.lock().unwrap(), 0);
        assert_eq!(tokens.access_token, "access-from-disk");
        assert_eq!(tokens.refresh_token.as_deref(), Some("refresh-from-disk"));
    }

    #[tokio::test]
    async fn shared_refresh_gate_prevents_duplicate_single_use_rotation() {
        let store = Arc::new(MemoryAuthStore::default());
        store
            .save(CodexAuth {
                access_token: "access-old".to_string(),
                refresh_token: Some("refresh-once".to_string()),
                expires_at: Some(crate::auth::codex::now_epoch_millis() - 1_000),
                account_id: Some("acc".to_string()),
                email: Some("user@example.com".to_string()),
            })
            .unwrap();
        let auth_store: Arc<dyn CodexAuthStore> = store.clone();
        let refresh_gate = Arc::new(tokio::sync::Mutex::new(()));
        let make_driver = || CodexChatDriver {
            client: reqwest::Client::new(),
            responses_url: CODEX_RESPONSES_URL.to_string(),
            tokens: Arc::new(tokio::sync::Mutex::new(expired_tokens("refresh-once"))),
            refresh_gate: refresh_gate.clone(),
            auth_store: Some(auth_store.clone()),
        };
        let first = make_driver();
        let second = make_driver();
        let refresh_calls = Arc::new(StdMutex::new(0usize));

        let first_calls = refresh_calls.clone();
        let second_calls = refresh_calls.clone();
        let (first_result, second_result) = tokio::join!(
            first.refresh_tokens_with(move |_refresh_token| {
                let calls = first_calls.clone();
                async move {
                    *calls.lock().expect("calls") += 1;
                    tokio::task::yield_now().await;
                    Ok(CodexAuth {
                        access_token: "access-new".to_string(),
                        refresh_token: Some("refresh-next".to_string()),
                        expires_at: Some(crate::auth::codex::now_epoch_millis() + 3_600_000),
                        account_id: Some("acc".to_string()),
                        email: None,
                    })
                }
            }),
            second.refresh_tokens_with(move |_refresh_token| {
                let calls = second_calls.clone();
                async move {
                    *calls.lock().expect("calls") += 1;
                    Ok(CodexAuth {
                        access_token: "unexpected-second-refresh".to_string(),
                        refresh_token: Some("unexpected".to_string()),
                        expires_at: Some(crate::auth::codex::now_epoch_millis() + 3_600_000),
                        account_id: Some("acc".to_string()),
                        email: None,
                    })
                }
            })
        );

        assert_eq!(*refresh_calls.lock().unwrap(), 1);
        assert_eq!(first_result.unwrap().access_token, "access-new");
        assert_eq!(second_result.unwrap().access_token, "access-new");
        assert_eq!(store.load_from_disk().unwrap().access_token, "access-new");
    }

    #[tokio::test]
    async fn refresh_token_reused_recovers_from_disk_winner() {
        // First load looks like memory (stale); after reuse failure the disk
        // has another process's winner that no longer needs refresh.
        #[derive(Default)]
        struct FlipStore {
            loads: StdMutex<usize>,
            clears: StdMutex<usize>,
        }
        impl CodexAuthStore for FlipStore {
            fn load_from_disk(&self) -> Option<CodexAuth> {
                let mut loads = self.loads.lock().expect("loads");
                *loads += 1;
                if *loads == 1 {
                    return Some(CodexAuth {
                        access_token: "access-old".to_string(),
                        refresh_token: Some("refresh-spent".to_string()),
                        expires_at: Some(crate::auth::codex::now_epoch_millis() - 1_000),
                        account_id: Some("acc".to_string()),
                        email: Some("user@example.com".to_string()),
                    });
                }
                Some(CodexAuth {
                    access_token: "access-winner".to_string(),
                    refresh_token: Some("refresh-winner".to_string()),
                    expires_at: Some(crate::auth::codex::now_epoch_millis() + 3_600_000),
                    account_id: Some("acc".to_string()),
                    email: Some("user@example.com".to_string()),
                })
            }
            fn save(&self, _auth: CodexAuth) -> anyhow::Result<()> {
                Ok(())
            }
            fn clear(&self) -> anyhow::Result<()> {
                *self.clears.lock().expect("clears") += 1;
                Ok(())
            }
        }

        let flip = FlipStore::default();
        let mut tokens = expired_tokens("refresh-spent");
        ensure_fresh_tokens(&mut tokens, Some(&flip), |_rt| async {
            Err(anyhow::anyhow!(
                "Codex token refresh failed (401 Unauthorized): {{\"error\":{{\"code\":\"refresh_token_reused\"}}}}"
            ))
        })
        .await
        .expect("recover from disk");

        assert_eq!(tokens.access_token, "access-winner");
        assert_eq!(tokens.refresh_token.as_deref(), Some("refresh-winner"));
        assert_eq!(*flip.clears.lock().unwrap(), 0);
    }

    #[tokio::test]
    async fn refresh_token_reused_clears_auth_and_asks_for_setup() {
        let store = MemoryAuthStore::default();
        store
            .save(CodexAuth {
                access_token: "access-old".to_string(),
                refresh_token: Some("refresh-spent".to_string()),
                expires_at: Some(crate::auth::codex::now_epoch_millis() - 1_000),
                account_id: Some("acc".to_string()),
                email: Some("user@example.com".to_string()),
            })
            .unwrap();
        let mut tokens = expired_tokens("refresh-spent");

        let err = ensure_fresh_tokens(&mut tokens, Some(&store), |_rt| async {
            Err(anyhow::anyhow!(
                "Codex token refresh failed (401 Unauthorized): {{\"error\":{{\"code\":\"refresh_token_reused\"}}}}"
            ))
        })
        .await
        .expect_err("must fail");

        assert!(err.to_string().contains("/setup"));
        assert!(err.to_string().contains("login is no longer valid"));
        assert_eq!(err.llm_error_kind(), Some(LlmErrorKind::Authentication));
        assert!(store.load_from_disk().is_none());
        assert_eq!(*store.clears.lock().unwrap(), 1);
    }

    #[test]
    fn invalidated_api_token_clears_auth_and_asks_for_setup() {
        let store = MemoryAuthStore::default();
        store
            .save(CodexAuth {
                access_token: "invalidated-access".to_string(),
                refresh_token: Some("invalidated-refresh".to_string()),
                expires_at: None,
                account_id: Some("acc".to_string()),
                email: Some("user@example.com".to_string()),
            })
            .unwrap();

        let err = codex_response_error(
            "API",
            StatusCode::UNAUTHORIZED,
            r#"{"error":{"code":"token_invalidated"}}"#.to_string(),
            Some(&store),
        );

        assert_eq!(err.llm_error_kind(), Some(LlmErrorKind::Authentication));
        assert!(err.to_string().contains("/setup"));
        assert!(!err.to_string().contains("token_invalidated"));
        assert!(store.load_from_disk().is_none());
        assert_eq!(*store.clears.lock().unwrap(), 1);
    }

    #[test]
    fn refresh_failures_keep_transient_and_permanent_classification() {
        let unavailable = classified_refresh_error(&anyhow::anyhow!(
            "Codex token refresh failed (503 Service Unavailable)"
        ));
        let rate_limited = classified_refresh_error(&anyhow::anyhow!(
            "Codex token refresh failed (429 Too Many Requests)"
        ));
        let authentication = classified_refresh_error(&anyhow::anyhow!(
            "Codex token refresh failed (401 Unauthorized)"
        ));

        assert_eq!(
            unavailable.llm_error_kind(),
            Some(LlmErrorKind::Unavailable)
        );
        assert_eq!(
            rate_limited.llm_error_kind(),
            Some(LlmErrorKind::RateLimited)
        );
        assert_eq!(
            authentication.llm_error_kind(),
            Some(LlmErrorKind::Authentication)
        );
    }

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
                reasoning: Vec::new(),
            },
            message,
        ]);
        assert!(matches!(
            input.last(),
            Some(CodexInputItem::FunctionCallOutput { call_id, .. }) if call_id == "call_1"
        ));
    }

    #[test]
    fn build_input_replays_provider_reasoning_ids_in_order() {
        use everruns_provider::reasoning::ReasoningContentPart;

        let mut assistant = LlmMessage::text(LlmMessageRole::Assistant, "");
        assistant.reasoning = vec![
            ReasoningContentPart::opaque("openai")
                .with_item_id("rs_first")
                .with_encrypted("blob-1"),
            // No encrypted payload: nothing to replay, so Codex must not be
            // sent a reasoning item it cannot key.
            ReasoningContentPart::opaque("openai").with_item_id("rs_summary_only"),
            ReasoningContentPart::opaque("openai")
                .with_item_id("rs_second")
                .with_encrypted("blob-2"),
        ];

        let (_, input) = build_input(&[assistant]);

        let replayed: Vec<(&str, &str)> = input
            .iter()
            .filter_map(|item| match item {
                CodexInputItem::Reasoning {
                    id,
                    encrypted_content,
                    ..
                } => Some((id.as_str(), encrypted_content.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(
            replayed,
            vec![("rs_first", "blob-1"), ("rs_second", "blob-2")],
            "reasoning must replay with the provider's own ids, in emission order"
        );
    }

    #[test]
    fn raw_reasoning_text_is_not_flagged_as_a_summary() {
        // `response.reasoning_text.delta` is the model's own words, not a
        // curated gloss, so consumers must be able to label it correctly.
        let event =
            reasoning_event(r#"{"type":"response.reasoning_text.delta","delta":"raw thought"}"#);
        assert!(
            matches!(event, LlmStreamEvent::ReasoningDelta { delta, summary } if delta == "raw thought" && !summary)
        );
    }

    #[test]
    fn build_input_drops_only_unpaired_items_from_parallel_tool_batch() {
        let mut paired_result = LlmMessage::text(LlmMessageRole::Tool, "skill instructions");
        paired_result.tool_call_id = Some("call_paired".to_string());
        let mut orphaned_result = LlmMessage::text(LlmMessageRole::Tool, "stale output");
        orphaned_result.tool_call_id = Some("call_missing_call".to_string());
        let (_, input) = build_input(&[
            LlmMessage {
                role: LlmMessageRole::Assistant,
                content: LlmMessageContent::Text(String::new()),
                tool_calls: Some(vec![
                    ToolCall {
                        id: "call_missing_output".to_string(),
                        name: "bash".to_string(),
                        arguments: json!({}),
                    },
                    ToolCall {
                        id: "call_paired".to_string(),
                        name: "activate_skill".to_string(),
                        arguments: json!({}),
                    },
                ]),
                tool_call_id: None,
                phase: None,
                reasoning: Vec::new(),
            },
            paired_result,
            orphaned_result,
        ]);

        assert!(!input.iter().any(|item| matches!(
            item,
            CodexInputItem::FunctionCall { call_id, .. } if call_id == "call_missing_output"
        )));
        assert!(input.iter().any(|item| matches!(
            item,
            CodexInputItem::FunctionCall { call_id, .. } if call_id == "call_paired"
        )));
        assert!(input.iter().any(|item| matches!(
            item,
            CodexInputItem::FunctionCallOutput { call_id, .. } if call_id == "call_paired"
        )));
        assert!(!input.iter().any(|item| matches!(
            item,
            CodexInputItem::FunctionCallOutput { call_id, .. } if call_id == "call_missing_call"
        )));
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
            everruns_provider::error::LlmErrorKind::Unavailable
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
            matches!(event, LlmStreamEvent::ReasoningDelta { delta, summary } if delta == "**Planning** real prose" && summary)
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
        assert!(
            matches!(event, LlmStreamEvent::ReasoningDelta { delta, .. } if delta == "**Header**\n\n")
        );
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

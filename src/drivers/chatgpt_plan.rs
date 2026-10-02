// ChatGPT plan usage over the public Responses API.
//
// A login from the open-source Sign in with ChatGPT route (`crate::auth::siwc`)
// carries a token whose audience is `https://api.openai.com/v1`. Turns go to
// `POST /v1/responses` with it as the bearer, not to the Codex backend.
//
// Decisions:
// - No new HTTP client: this wraps everruns' Open Responses driver (the same
//   wire driver `everruns-openai` builds on) with every optional OpenAI
//   extension off: no stateful continuation, no hosted tools or tool_search,
//   no background mode, no explicit cache controls.
// - The route's preview limits are applied as a request extension that edits
//   the serialized body: the fields the docs list as rejected are removed,
//   `store: false` and `stream: true` are forced, and `system` items become
//   `developer`. `service_tier` is dropped too: the docs name a service-tier
//   override among unsupported capabilities, so the `speed` setting does not
//   apply on this route (inferred, not listed field by field).
// - Auth is a `ProviderAuth` that refreshes per request with the client saved
//   on the token set, sharing the Codex driver's refresh gate and disk
//   adoption so two processes never race a rotating refresh token.
// - Native compaction and model listing are off until the route is proven:
//   the docs document only `POST /v1/responses`, and `/v1/models` answers in a
//   ChatGPT-specific shape the shared parser does not read.
//
// See knowledge/specs/chatgpt-sign-in.md.
use super::codex::{CodexAuthStore, CodexTokens, ensure_fresh_tokens, metadata_extra_string};
use async_trait::async_trait;
use everruns_provider::error::Result as EverrunsResult;
use everruns_provider::message::Message as LlmMessage;
use everruns_provider::{
    AgentLoopError, ChatDriver, DiscoveredModel, LlmCallConfig, LlmErrorKind, LlmResponseStream,
    OpenResponsesProtocolChatDriver, OpenResponsesRequestExtension, ProviderAuth,
    ProviderAuthRequest, ProviderEndpoint, ProviderMetadata, RuntimeProvider,
};
use reqwest::header::HeaderMap;
use serde_json::Value;
use std::sync::Arc;

/// `flow` metadata value the runtime sets for an open-source login.
pub const OPEN_SOURCE_FLOW: &str = "open-source";

/// Request fields the route rejects in preview, plus `previous_response_id`
/// (history goes in `input`) and `service_tier` (see the header).
pub const REJECTED_FIELDS: &[&str] = &[
    "background",
    "conversation",
    "max_output_tokens",
    "max_tool_calls",
    "metadata",
    "moderation",
    "multi_agent",
    "prompt",
    "prompt_cache_retention",
    "safety_identifier",
    "temperature",
    "top_logprobs",
    "top_p",
    "truncation",
    "user",
    "previous_response_id",
    "service_tier",
];

/// Tool types the route accepts at the top level. Hosted tools other than web
/// search, and `tool_search`, are rejected.
const ACCEPTED_TOOL_TYPES: &[&str] = &["function", "custom", "namespace", "web_search"];

/// Whether the resolved model's credentials came from the open-source route.
pub fn is_open_source_login(metadata: &ProviderMetadata) -> bool {
    metadata_extra_string(metadata, "flow").as_deref() == Some(OPEN_SOURCE_FLOW)
}

/// Fit a serialized Responses request to the route's preview limits.
pub fn shape_request(body: &mut Value) {
    let Some(object) = body.as_object_mut() else {
        return;
    };
    for field in REJECTED_FIELDS {
        object.remove(*field);
    }
    object.insert("store".to_string(), Value::Bool(false));
    object.insert("stream".to_string(), Value::Bool(true));
    if let Some(input) = object.get_mut("input").and_then(Value::as_array_mut) {
        for item in input {
            if item.get("role").and_then(Value::as_str) == Some("system") {
                item["role"] = Value::String("developer".to_string());
            }
        }
    }
    let empty_tools = match object.get_mut("tools").and_then(Value::as_array_mut) {
        Some(tools) => {
            tools.retain(|tool| {
                tool.get("type")
                    .and_then(Value::as_str)
                    .is_some_and(|kind| ACCEPTED_TOOL_TYPES.contains(&kind))
            });
            tools.is_empty()
        }
        None => false,
    };
    if empty_tools {
        object.remove("tools");
        object.remove("tool_choice");
    }
}

/// The error kind for the route's documented plan-usage codes, so a usage
/// limit reads as an exhausted plan and an ineligible account as an auth
/// problem rather than a generic failure.
pub fn plan_error_kind(body: &str) -> Option<LlmErrorKind> {
    let code = serde_json::from_str::<Value>(body).ok().and_then(|value| {
        value
            .pointer("/error/code")
            .and_then(Value::as_str)
            .map(str::to_string)
    })?;
    match code.as_str() {
        "subscription_sharing_usage_limit_exceeded" => Some(LlmErrorKind::QuotaExhausted),
        "subscription_sharing_user_not_eligible"
        | "subscription_sharing_invalid_user"
        | "chatpass_v2_scope_not_authorized"
        | "chatpass_v2_invalid_authorization_context" => Some(LlmErrorKind::Authentication),
        "subscription_sharing_unsupported_capability"
        | "subscription_sharing_route_not_supported" => Some(LlmErrorKind::InvalidRequest),
        "subscription_sharing_usage_unavailable" | "subscription_sharing_user_unavailable" => {
            Some(LlmErrorKind::Unavailable)
        }
        _ => None,
    }
}

struct PlanRequestShape;

impl OpenResponsesRequestExtension for PlanRequestShape {
    fn decorate(&self, body: &mut Value, _config: &LlmCallConfig) -> EverrunsResult<()> {
        shape_request(body);
        Ok(())
    }

    fn classify_error(
        &self,
        _status: u16,
        _headers: &HeaderMap,
        error_body: &str,
    ) -> Option<LlmErrorKind> {
        plan_error_kind(error_body)
    }
}

/// Bearer auth from the saved open-source login, refreshed near expiry with
/// the client that issued it.
struct PlanAuth {
    tokens: Arc<tokio::sync::Mutex<CodexTokens>>,
    refresh_gate: Arc<tokio::sync::Mutex<()>>,
    store: Option<Arc<dyn CodexAuthStore>>,
    token_url: String,
}

#[async_trait]
impl ProviderAuth for PlanAuth {
    async fn headers(
        &self,
        _request: ProviderAuthRequest<'_>,
    ) -> EverrunsResult<Vec<(String, String)>> {
        // Refresh tokens rotate; serialize with every other ChatGPT driver.
        let _gate = self.refresh_gate.lock().await;
        let mut tokens = self.tokens.lock().await;
        let token_url = self.token_url.clone();
        ensure_fresh_tokens(
            &mut tokens,
            self.store.as_deref(),
            move |client_id, refresh| {
                let token_url = token_url.clone();
                async move {
                    crate::auth::siwc::refresh_with_token_at(&token_url, &client_id, &refresh).await
                }
            },
        )
        .await?;
        if tokens.open_source_scopes().is_some_and(|scopes| {
            !scopes.is_empty() && !crate::auth::siwc::grants_plan_usage(scopes)
        }) {
            return Err(AgentLoopError::llm_kind(
                LlmErrorKind::Authentication,
                "ChatGPT plan use is not enabled for this sign-in. Run `/setup` and allow it, or choose another provider.",
            ));
        }
        Ok(vec![(
            "authorization".to_string(),
            format!("Bearer {}", tokens.access_token()),
        )])
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// The `codex` provider's driver for an open-source login.
pub struct ChatGptPlanDriver {
    inner: OpenResponsesProtocolChatDriver,
    endpoint: ProviderEndpoint,
}

impl ChatGptPlanDriver {
    pub(super) fn new(
        tokens: CodexTokens,
        store: Option<Arc<dyn CodexAuthStore>>,
        refresh_gate: Arc<tokio::sync::Mutex<()>>,
        base_url: String,
        token_url: String,
    ) -> Self {
        let inner = OpenResponsesProtocolChatDriver::new()
            .with_stateful_responses(false)
            .with_native_features(false, false)
            .with_hosted_tools(false)
            .with_background_mode(false)
            .with_prompt_cache_options(false)
            .with_request_extension(Arc::new(PlanRequestShape));
        let auth = PlanAuth {
            tokens: Arc::new(tokio::sync::Mutex::new(tokens)),
            refresh_gate,
            store,
            token_url,
        };
        let endpoint = RuntimeProvider::new("chatgpt-plan", inner.clone())
            .base_url(base_url)
            .auth(auth)
            .endpoint()
            .clone();
        Self { inner, endpoint }
    }
}

#[async_trait]
impl ChatDriver for ChatGptPlanDriver {
    // Like the Codex driver, this one owns its endpoint and rotating
    // credentials, so it ignores the endpoint the runtime passes in.
    async fn chat_completion_stream(
        &self,
        _endpoint: &ProviderEndpoint,
        messages: Vec<LlmMessage>,
        config: &LlmCallConfig,
    ) -> EverrunsResult<LlmResponseStream> {
        self.inner
            .chat_completion_stream(&self.endpoint, messages, config)
            .await
    }

    async fn list_models(
        &self,
        _endpoint: &ProviderEndpoint,
    ) -> EverrunsResult<Option<Vec<DiscoveredModel>>> {
        Ok(None)
    }

    fn supports_compact(&self) -> bool {
        false
    }

    fn supports_stateful_responses(&self) -> bool {
        false
    }

    fn effective_context_window(&self, model: &str) -> Option<usize> {
        super::codex::model_profile(model).and_then(|profile| {
            profile
                .limits
                .map(|limits| usize::try_from(limits.context).unwrap_or(usize::MAX))
        })
    }

    fn supports_parallel_tool_calls(&self, model: &str) -> bool {
        self.inner.supports_parallel_tool_calls(model)
    }

    fn supports_response_format(&self, model: &str) -> bool {
        self.inner.supports_response_format(model)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{CodexAuth, OpenSourceGrant};
    use everruns_provider::driver_registry::DriverConfig;
    use everruns_provider::message::MessageRole as LlmMessageRole;
    use futures::StreamExt;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::Mutex as StdMutex;

    #[test]
    fn shape_request_drops_rejected_fields_and_forces_store_and_stream() {
        let mut body = serde_json::json!({
            "model": "gpt-6.1-sol",
            "input": [
                {"type": "message", "role": "system", "content": "be brief"},
                {"type": "message", "role": "user", "content": "hi"}
            ],
            "instructions": "you are yolop",
            "stream": false,
            "store": true,
            "max_output_tokens": 1000,
            "temperature": 0.2,
            "top_p": 0.9,
            "metadata": {"session_id": "s"},
            "previous_response_id": "resp_1",
            "service_tier": "priority",
            "truncation": "auto",
            "reasoning": {"effort": "high"},
            "include": ["reasoning.encrypted_content"],
            "tools": [
                {"type": "function", "name": "read"},
                {"type": "tool_search"},
                {"type": "image_generation"}
            ]
        });
        shape_request(&mut body);
        for field in REJECTED_FIELDS {
            assert!(body.get(*field).is_none(), "{field} survived: {body}");
        }
        assert_eq!(body["store"], false);
        assert_eq!(body["stream"], true);
        assert_eq!(body["input"][0]["role"], "developer");
        assert_eq!(body["input"][1]["role"], "user");
        assert_eq!(body["instructions"], "you are yolop");
        assert_eq!(body["reasoning"]["effort"], "high");
        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], "read");

        let mut hosted_only = serde_json::json!({
            "tools": [{"type": "file_search"}],
            "tool_choice": "auto",
        });
        shape_request(&mut hosted_only);
        assert!(hosted_only.get("tools").is_none());
        assert!(hosted_only.get("tool_choice").is_none());
    }

    #[test]
    fn plan_error_codes_map_to_kinds() {
        let body = |code: &str| format!(r#"{{"error":{{"code":"{code}","param":null}}}}"#);
        assert_eq!(
            plan_error_kind(&body("subscription_sharing_usage_limit_exceeded")),
            Some(LlmErrorKind::QuotaExhausted)
        );
        assert_eq!(
            plan_error_kind(&body("subscription_sharing_user_not_eligible")),
            Some(LlmErrorKind::Authentication)
        );
        assert_eq!(
            plan_error_kind(&body("subscription_sharing_unsupported_capability")),
            Some(LlmErrorKind::InvalidRequest)
        );
        assert_eq!(
            plan_error_kind(&body("subscription_sharing_usage_unavailable")),
            Some(LlmErrorKind::Unavailable)
        );
        assert_eq!(plan_error_kind(r#"{"detail":"not enabled"}"#), None);
        assert_eq!(plan_error_kind(&body("something_else")), None);
    }

    #[test]
    fn open_source_flow_is_read_from_metadata() {
        let open = ProviderMetadata {
            extra: Some(serde_json::json!({ "flow": OPEN_SOURCE_FLOW })),
            ..ProviderMetadata::default()
        };
        assert!(is_open_source_login(&open));
        assert!(!is_open_source_login(&ProviderMetadata::default()));
    }

    #[derive(Default)]
    struct MemoryStore {
        auth: StdMutex<Option<CodexAuth>>,
    }

    impl CodexAuthStore for MemoryStore {
        fn load_from_disk(&self) -> Option<CodexAuth> {
            self.auth.lock().unwrap().clone()
        }
        fn save(&self, auth: CodexAuth) -> anyhow::Result<()> {
            *self.auth.lock().unwrap() = Some(auth);
            Ok(())
        }
        fn clear(&self) -> anyhow::Result<()> {
            *self.auth.lock().unwrap() = None;
            Ok(())
        }
    }

    /// A mock of both the token endpoint and `/v1/responses`. Every raw
    /// request is recorded.
    fn serve() -> (String, Arc<StdMutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(StdMutex::new(Vec::new()));
        let seen_clone = seen.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let mut request = Vec::new();
                let mut buffer = [0u8; 16384];
                let header_end = loop {
                    let n = stream.read(&mut buffer).unwrap_or(0);
                    if n == 0 {
                        break None;
                    }
                    request.extend_from_slice(&buffer[..n]);
                    if let Some(i) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                        break Some(i + 4);
                    }
                };
                let Some(header_end) = header_end else {
                    continue;
                };
                let head = String::from_utf8_lossy(&request[..header_end]).to_string();
                let length = head
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                while request.len() < header_end + length {
                    let n = stream.read(&mut buffer).unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    request.extend_from_slice(&buffer[..n]);
                }
                seen_clone
                    .lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&request).to_string());
                let response = if head.starts_with("POST /token") {
                    let body = r#"{"access_token":"access-new","refresh_token":"refresh-new","expires_in":3600,"scope":"openid chatgpt.tokens.use.direct"}"#;
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                } else {
                    let body = concat!(
                        "event: response.output_text.delta\n",
                        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Hello\"}\n\n",
                        "event: response.completed\n",
                        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"status\":\"completed\",\"output\":[],\"usage\":{\"input_tokens\":3,\"output_tokens\":1}}}\n\n",
                    );
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                };
                let _ = stream.write_all(response.as_bytes());
            }
        });
        (base, seen)
    }

    fn expired_open_source_tokens() -> CodexTokens {
        let config = DriverConfig {
            provider: everruns_provider::ProviderKey::new("codex"),
            provider_type: everruns_provider::DriverId::external(
                super::super::codex::CODEX_DRIVER_ID,
            ),
            api_key: Some("access-old".to_string()),
            credentials: Default::default(),
            base_url: None,
            metadata: ProviderMetadata {
                refresh_token: Some("refresh-old".to_string()),
                account_id: None,
                extra: Some(serde_json::json!({
                    "flow": OPEN_SOURCE_FLOW,
                    "client_id": "oaiapp_issued",
                    "expires_at": crate::auth::codex::now_epoch_millis() - 1_000,
                })),
            },
        };
        CodexTokens::from_config(&config, None)
    }

    #[tokio::test]
    async fn turn_refreshes_with_issued_client_and_sends_a_shaped_request() {
        let (base, seen) = serve();
        let store = Arc::new(MemoryStore::default());
        store
            .save(CodexAuth {
                access_token: "access-old".to_string(),
                refresh_token: Some("refresh-old".to_string()),
                expires_at: Some(crate::auth::codex::now_epoch_millis() - 1_000),
                account_id: None,
                email: Some("user@example.com".to_string()),
                client_id: Some("oaiapp_issued".to_string()),
                open_source: Some(OpenSourceGrant {
                    id_token: Some("id.token.kept".to_string()),
                    scopes: vec!["chatgpt.tokens.use.direct".to_string()],
                    subject: Some("user-sub-1".to_string()),
                }),
            })
            .unwrap();
        let driver = ChatGptPlanDriver::new(
            expired_open_source_tokens(),
            Some(store.clone() as Arc<dyn CodexAuthStore>),
            Arc::new(tokio::sync::Mutex::new(())),
            format!("{base}/v1"),
            format!("{base}/token"),
        );

        let mut config = LlmCallConfig::default();
        config.model = "gpt-6.1-sol".to_string();
        config.max_tokens = Some(4096);
        config.temperature = Some(0.3);
        config.previous_response_id = Some("resp_prev".to_string());
        config.speed = Some("fast".to_string());
        config
            .metadata
            .insert("session_id".to_string(), "sess-1".to_string());
        let messages = vec![
            LlmMessage::text(LlmMessageRole::System, "You are yolop."),
            LlmMessage::text(LlmMessageRole::User, "hi"),
        ];
        let mut stream = driver
            .chat_completion_stream(&ProviderEndpoint::default(), messages, &config)
            .await
            .expect("stream opens");
        let mut text = String::new();
        while let Some(event) = stream.next().await {
            if let Ok(everruns_provider::LlmStreamEvent::TextDelta(delta)) = event {
                text.push_str(&delta);
            }
        }
        assert_eq!(text, "Hello");

        let requests = seen.lock().unwrap().clone();
        let refresh = requests
            .iter()
            .find(|r| r.starts_with("POST /token"))
            .expect("refresh request");
        assert!(refresh.contains("client_id=oaiapp_issued"), "{refresh}");
        assert!(
            refresh.contains("resource=https%3A%2F%2Fapi.openai.com%2Fv1"),
            "{refresh}"
        );
        let turn = requests
            .iter()
            .find(|r| r.starts_with("POST /v1/responses"))
            .expect("responses request");
        assert!(
            turn.to_ascii_lowercase()
                .contains("authorization: bearer access-new"),
            "{turn}"
        );
        let body: Value =
            serde_json::from_str(turn.split("\r\n\r\n").nth(1).unwrap()).expect("json body");
        assert_eq!(body["store"], false);
        assert_eq!(body["stream"], true);
        for field in REJECTED_FIELDS {
            assert!(body.get(*field).is_none(), "{field} sent: {body}");
        }
        assert!(
            !body.to_string().contains("\"role\":\"system\""),
            "system item sent: {body}"
        );

        // The rotated pair was saved with the issuing client and the ID token.
        let saved = store.load_from_disk().unwrap();
        assert_eq!(saved.refresh_token.as_deref(), Some("refresh-new"));
        assert_eq!(saved.client_id.as_deref(), Some("oaiapp_issued"));
        let grant = saved.open_source.unwrap();
        assert_eq!(grant.id_token.as_deref(), Some("id.token.kept"));
        assert_eq!(grant.subject.as_deref(), Some("user-sub-1"));
    }

    #[tokio::test]
    async fn login_without_plan_scope_is_refused_before_sending() {
        let (base, seen) = serve();
        let mut tokens = expired_open_source_tokens();
        tokens.set_open_source_for_test(OpenSourceGrant {
            id_token: None,
            scopes: vec!["openid".to_string()],
            subject: None,
        });
        tokens.set_expires_at_for_test(Some(crate::auth::codex::now_epoch_millis() + 3_600_000));
        let driver = ChatGptPlanDriver::new(
            tokens,
            None,
            Arc::new(tokio::sync::Mutex::new(())),
            format!("{base}/v1"),
            format!("{base}/token"),
        );
        let mut config = LlmCallConfig::default();
        config.model = "gpt-6.1-sol".to_string();
        let result = driver
            .chat_completion_stream(
                &ProviderEndpoint::default(),
                vec![LlmMessage::text(LlmMessageRole::User, "hi")],
                &config,
            )
            .await;
        let err = match result {
            Ok(_) => panic!("a login without the plan scope must not send a turn"),
            Err(err) => err,
        };
        assert!(err.to_string().contains("plan use"), "{err}");
        assert!(
            !seen
                .lock()
                .unwrap()
                .iter()
                .any(|r| r.starts_with("POST /v1/responses"))
        );
    }
}

//! Settings and registry adapters. Everruns owns the protocols and token lifecycle.
use crate::config::{CodexAuth, SettingsStore};
use async_trait::async_trait;
use everruns_contracts::driver_registry::{BoxedChatDriver, DriverConfig};
use everruns_contracts::{DriverRegistry, ModelProfile, ProviderMetadata};
use everruns_drivers::chatgpt::auth::{MemoryTokenStore, RotatingAuth, TokenRoute, TokenStore};
pub use everruns_drivers::codex::CODEX_DRIVER_ID;
use everruns_drivers::codex::driver as codex_driver;
use everruns_drivers::{chatgpt, codex};
use std::{path::Path, sync::Arc};

pub(crate) fn model_profile(model: &str) -> Option<ModelProfile> {
    everruns_drivers::codex::model_profile(model)
}
pub(crate) async fn settings_rotation_lock(path: &Path) -> anyhow::Result<Box<dyn Send>> {
    let path = path.to_owned();
    Ok(Box::new(
        tokio::task::spawn_blocking(move || crate::config::oauth_store::rotation_lock(&path))
            .await??,
    ))
}

struct SettingsTokens(Arc<SettingsStore>);
#[async_trait]
impl TokenStore for SettingsTokens {
    async fn lock(&self) -> anyhow::Result<Box<dyn Send>> {
        settings_rotation_lock(self.0.path()).await
    }
    async fn load(&self) -> anyhow::Result<Option<CodexAuth>> {
        self.0.refresh_codex_auth_from_disk_checked()
    }
    async fn save(&self, auth: CodexAuth) -> anyhow::Result<()> {
        self.0.set_codex_auth_under_lease(auth)
    }
    async fn compare_and_save(
        &self,
        previous: &CodexAuth,
        auth: CodexAuth,
    ) -> anyhow::Result<bool> {
        self.0.compare_and_set_codex_auth(previous, auth)
    }
}

fn extra(metadata: &ProviderMetadata, name: &str) -> Option<String> {
    metadata
        .extra
        .as_ref()?
        .get(name)?
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}
pub fn register_driver(registry: &mut DriverRegistry, settings: Arc<SettingsStore>) {
    registry.register_external(CODEX_DRIVER_ID, move |config| {
        build_driver(config, Some(settings.clone()))
    });
}

/// Register Codex for one-shot use from already-resolved config (ephemeral
/// grant, no settings store). Model discovery resolves tokens through
/// `model_with_provider`, so it never needs live settings access.
pub fn register_ephemeral(registry: &mut DriverRegistry) {
    registry.register_external(CODEX_DRIVER_ID, move |config| build_driver(config, None));
}

fn build_driver(config: &DriverConfig, tokens: Option<Arc<SettingsStore>>) -> BoxedChatDriver {
    let plan = extra(&config.metadata, "flow").as_deref() == Some("open-source");
    let route = if plan {
        TokenRoute::ChatGptPlan
    } else {
        TokenRoute::Codex
    };
    let stored =
        tokens.filter(|_| extra(&config.metadata, "auth_source").as_deref() == Some("settings"));
    // Only the base_url override is user input, so only it can fail the
    // build. Defaults always build (both routes are covered by upstream
    // tests), which the expect below relies on.
    let grant = || CodexAuth {
        access_token: config
            .api_key
            .clone()
            .or_else(|| extra(&config.metadata, "access_token"))
            .unwrap_or_default(),
        refresh_token: config.metadata.refresh_token.clone(),
        expires_at: config
            .metadata
            .extra
            .as_ref()
            .and_then(|e| e.get("expires_at").or_else(|| e.get("expires_at_ms")))
            .and_then(serde_json::Value::as_i64),
        account_id: config.metadata.account_id.clone(),
        email: None,
        client_id: extra(&config.metadata, "client_id"),
        open_source: None,
    };
    let build = |base_url: Option<&str>| {
        let builder = if let Some(tokens) = stored.clone() {
            codex_driver::driver(Arc::new(SettingsTokens(tokens))).route(route)
        } else {
            codex_driver::ephemeral(grant()).route(route)
        };
        let builder = match base_url {
            Some(url) => builder.base_url(url),
            None => builder,
        };
        // The ChatGptPlan route rejects the originator option; only the
        // Codex route tags refreshes.
        let builder = if plan {
            builder
        } else {
            builder.originator("yolop")
        };
        builder.id(config.provider.clone()).build()
    };
    match build(config.base_url.as_deref()) {
        Ok(provider) => provider.into_boxed_driver(),
        Err(first) => match build(None) {
            Ok(provider) => {
                tracing::warn!(
                    provider = ?config.provider,
                    error = %first,
                    "codex driver build failed, retrying without base_url override"
                );
                provider.into_boxed_driver()
            }
            Err(err) => {
                tracing::warn!(
                    provider = ?config.provider,
                    error = %err,
                    "codex driver build failed, deferring auth to request time"
                );
                legacy_driver(config, stored, grant(), route)
            }
        },
    }
}

/// Legacy non-validating constructor. Driver construction must stay
/// infallible so states like "no login stored yet" keep flowing to the
/// setup UI instead of panicking; auth errors then surface at request time.
fn legacy_driver(
    config: &DriverConfig,
    stored: Option<Arc<SettingsStore>>,
    grant: CodexAuth,
    route: TokenRoute,
) -> BoxedChatDriver {
    let store: Arc<dyn TokenStore> = match stored {
        Some(tokens) => Arc::new(SettingsTokens(tokens)),
        None => Arc::new(MemoryTokenStore::new(grant)),
    };
    let auth = RotatingAuth::new(store, route).with_originator("yolop");
    let mut provider = if matches!(route, TokenRoute::ChatGptPlan) {
        chatgpt::provider(config.provider.clone(), auth)
    } else {
        codex::provider(config.provider.clone(), auth)
    };
    if let Some(base_url) = &config.base_url {
        provider = provider.base_url(base_url);
    }
    provider.into_boxed_driver()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::OpenSourceGrant;
    use everruns_contracts::{
        LlmCallConfig, Message, MessageRole, ProviderConfig, ProviderEndpoint,
    };
    use futures::StreamExt;
    use serde_json::json;

    #[tokio::test]
    async fn registry_routes_use_current_settings_and_stateless_shared_transport() {
        for plan in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            let settings = Arc::new(SettingsStore::open(dir.path().join("settings.toml")));
            let mut auth = CodexAuth {
                access_token: "before".into(),
                refresh_token: None,
                expires_at: None,
                account_id: (!plan).then(|| "account_test".into()),
                email: None,
                client_id: Some("app_test".into()),
                open_source: plan.then(|| OpenSourceGrant {
                    id_token: None,
                    scopes: vec!["chatgpt.tokens.use.direct".into()],
                    subject: Some("subject".into()),
                }),
            };
            if plan {
                settings.save_chatgpt_login(None, auth.clone()).unwrap();
            } else {
                settings.set_codex_auth(auth.clone()).unwrap();
            }
            let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
            let base_url = format!("http://{}/v1", server.server_addr());
            let mut registry = DriverRegistry::new();
            register_driver(&mut registry, settings.clone());
            let config = ProviderConfig::new(everruns_contracts::DriverId::external(CODEX_DRIVER_ID))
            .with_api_key("stale-config-token")
            .with_base_url(base_url)
            .with_metadata(ProviderMetadata {
                extra: Some(json!({"flow":if plan { "open-source" } else { "codex" },"auth_source":"settings"})),
                ..Default::default()
            });
            let driver = registry.create_chat_driver(&config).unwrap();
            // The driver must reload disk, rather than retaining its creation snapshot.
            auth.access_token = "current-settings-token".into();
            settings.set_codex_auth(auth).unwrap();
            let mut call = LlmCallConfig::new("test-model");
            // Real runtime calls always carry session and harness metadata.
            call.metadata
                .insert("session_id".into(), "session_test".into());
            call.metadata.insert("app".into(), "yolop".into());
            call.metadata
                .insert("yolop_version".into(), env!("CARGO_PKG_VERSION").into());
            let request = std::thread::spawn(move || {
                let mut request = server
                    .recv_timeout(std::time::Duration::from_secs(10))
                    .unwrap()
                    .expect("shared driver request");
                assert_eq!(request.url(), "/v1/responses");
                let authorization = request
                    .headers()
                    .iter()
                    .find(|h| h.field.equiv("authorization"))
                    .unwrap();
                assert_eq!(
                    authorization.value.as_str(),
                    "Bearer current-settings-token"
                );
                let account = request
                    .headers()
                    .iter()
                    .find(|h| h.field.equiv("chatgpt-account-id"));
                if plan {
                    assert!(account.is_none());
                } else {
                    assert_eq!(account.unwrap().value.as_str(), "account_test");
                    assert_eq!(
                        request
                            .headers()
                            .iter()
                            .find(|h| h.field.equiv("session_id"))
                            .unwrap()
                            .value
                            .as_str(),
                        "session_test"
                    );
                    assert_eq!(
                        request
                            .headers()
                            .iter()
                            .find(|h| h.field.equiv("originator"))
                            .unwrap()
                            .value
                            .as_str(),
                        "yolop"
                    );
                }
                let mut body = String::new();
                request.as_reader().read_to_string(&mut body).unwrap();
                let body: serde_json::Value = serde_json::from_str(&body).unwrap();
                assert_eq!(body["store"], false);
                assert_eq!(body["stream"], true);
                assert_eq!(body["instructions"], "instructions");
                let output = body["input"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|item| item["type"] == "function_call_output")
                    .expect("model receives tool error output");
                assert_eq!(output["call_id"], "semantic-error");
                assert_eq!(
                    output["output"],
                    "Tool error: progress_checkpoint rejected: no progress checkpoint is currently required"
                );
                let sse = "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"shared-ok\"}\n\nevent: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"r1\",\"status\":\"completed\",\"output\":[]}}\n\n";
                let response = if body.get("metadata").is_some() {
                    tiny_http::Response::from_string(
                        r#"{"detail":"Unsupported parameter: metadata"}"#,
                    )
                    .with_status_code(400)
                } else {
                    tiny_http::Response::from_string(sse).with_header(
                        tiny_http::Header::from_bytes("Content-Type", "text/event-stream").unwrap(),
                    )
                };
                request.respond(response).unwrap();
            });
            let events: Vec<_> = driver
                .chat_completion_stream(
                    &ProviderEndpoint::default(),
                    vec![
                        Message::text(MessageRole::System, "instructions"),
                        Message::text(MessageRole::User, "hi"),
                        everruns_core::llm_conversions::llm_message_from_message_with_images(
                            &everruns_core::RuntimeMessage::assistant_with_tools(
                                "",
                                vec![everruns_contracts::ToolCall {
                                    id: "semantic-error".into(),
                                    name: "progress_checkpoint".into(),
                                    arguments: json!({}),
                                }],
                            ),
                            &Default::default(),
                        ),
                        everruns_core::llm_conversions::llm_message_from_message_with_images(
                            &everruns_core::RuntimeMessage::tool_result(
                                "semantic-error",
                                None,
                                Some("progress_checkpoint rejected: no progress checkpoint is currently required".into()),
                            ),
                            &Default::default(),
                        ),
                    ],
                    &call,
                )
                .await
                .unwrap()
                .collect()
                .await;
            assert!(events.iter().all(Result::is_ok), "{events:?}");
            assert!(events.iter().any(|e| matches!(e, Ok(everruns_contracts::LlmStreamEvent::TextDelta(t)) if t == "shared-ok")));
            request.join().unwrap();
        }
    }

    #[tokio::test]
    async fn registry_plan_route_rejects_missing_consent_before_network_access() {
        let dir = tempfile::tempdir().unwrap();
        let settings = Arc::new(SettingsStore::open(dir.path().join("settings.toml")));
        settings
            .save_chatgpt_login(
                None,
                CodexAuth {
                    access_token: "token".into(),
                    refresh_token: None,
                    expires_at: None,
                    account_id: None,
                    email: None,
                    client_id: Some("app_test".into()),
                    open_source: Some(OpenSourceGrant {
                        id_token: None,
                        scopes: vec![],
                        subject: Some("subject".into()),
                    }),
                },
            )
            .unwrap();
        let mut registry = DriverRegistry::new();
        register_driver(&mut registry, settings);
        let config = ProviderConfig::new(everruns_contracts::DriverId::external(CODEX_DRIVER_ID))
            .with_metadata(ProviderMetadata {
                extra: Some(json!({"flow":"open-source","auth_source":"settings"})),
                ..Default::default()
            });
        let driver = registry.create_chat_driver(&config).unwrap();
        let result = driver
            .chat_completion_stream(
                &ProviderEndpoint::default(),
                vec![],
                &LlmCallConfig::new("test-model"),
            )
            .await;
        let error = match result {
            Ok(_) => panic!("missing consent must fail"),
            Err(e) => e,
        };
        assert_eq!(
            error.llm_error_kind(),
            Some(everruns_contracts::LlmErrorKind::Authentication)
        );
        assert!(
            error
                .to_string()
                .contains("ChatGPT Plan use was not granted"),
            "{error}"
        );
    }

    #[test]
    fn ephemeral_builder_builds_without_network() {
        let grant = CodexAuth {
            access_token: "test-token".to_string(),
            refresh_token: None,
            expires_at: None,
            account_id: None,
            email: None,
            client_id: None,
            open_source: None,
        };
        // The ChatGptPlan route rejects the originator option, so only
        // the Codex route tags its refreshes.
        // The ChatGptPlan route requires the grant to carry its issuing
        // client, mirroring what `model_with_provider` always sets for an
        // open-source login.
        let grant = CodexAuth {
            client_id: Some("test-client".to_string()),
            open_source: Some(crate::config::OpenSourceGrant {
                id_token: None,
                scopes: vec!["chatgpt.tokens.use.direct".to_string()],
                subject: Some("test-subject".to_string()),
            }),
            ..grant
        };
        let chatgpt = codex_driver::ephemeral(grant)
            .route(TokenRoute::ChatGptPlan)
            .build()
            .expect("ephemeral builder builds for ChatGptPlan");
        let _driver = chatgpt.into_boxed_driver();
        let grant = CodexAuth {
            access_token: "test-token".to_string(),
            refresh_token: None,
            expires_at: None,
            account_id: None,
            email: None,
            client_id: None,
            open_source: None,
        };
        let codex = codex_driver::ephemeral(grant)
            .route(TokenRoute::Codex)
            .originator("yolop-test")
            .build()
            .expect("ephemeral builder builds for Codex");
        let _driver = codex.into_boxed_driver();
    }
}

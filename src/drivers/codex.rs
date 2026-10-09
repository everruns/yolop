//! Settings and registry adapters. Everruns owns the protocols and token lifecycle.
use crate::config::{CodexAuth, SettingsStore};
use async_trait::async_trait;
use everruns_contracts::{DriverRegistry, ModelProfile, ProviderMetadata};
use everruns_drivers::chatgpt::auth::{RotatingAuth, TokenRoute, TokenStore};
pub use everruns_drivers::codex::CODEX_DRIVER_ID;
use std::{path::Path, sync::Arc};
use tokio::sync::Mutex;

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
struct EphemeralTokens {
    gate: Arc<Mutex<()>>,
    auth: Mutex<CodexAuth>,
}
#[async_trait]
impl TokenStore for EphemeralTokens {
    async fn lock(&self) -> anyhow::Result<Box<dyn Send>> {
        Ok(Box::new(self.gate.clone().lock_owned().await))
    }
    async fn load(&self) -> anyhow::Result<Option<CodexAuth>> {
        Ok(Some(self.auth.lock().await.clone()))
    }
    async fn save(&self, auth: CodexAuth) -> anyhow::Result<()> {
        *self.auth.lock().await = auth;
        Ok(())
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
    let tokens: Arc<dyn TokenStore> = Arc::new(SettingsTokens(settings));
    registry.register_external(CODEX_DRIVER_ID, move |config| {
        let plan = extra(&config.metadata, "flow").as_deref() == Some("open-source");
        let store: Arc<dyn TokenStore> =
            if extra(&config.metadata, "auth_source").as_deref() == Some("settings") {
                tokens.clone()
            } else {
                Arc::new(EphemeralTokens {
                    gate: Arc::new(Mutex::new(())),
                    auth: Mutex::new(CodexAuth {
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
                    }),
                })
            };
        let auth = RotatingAuth::new(
            store,
            if plan {
                TokenRoute::ChatGptPlan
            } else {
                TokenRoute::Codex
            },
        )
        .with_originator("yolop");
        let mut provider = if plan {
            everruns_drivers::chatgpt::provider(config.provider.clone(), auth)
        } else {
            everruns_drivers::codex::provider(config.provider.clone(), auth)
        };
        if let Some(base_url) = &config.base_url {
            provider = provider.base_url(base_url);
        }
        provider.into_boxed_driver()
    });
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
                request
                    .respond(tiny_http::Response::from_string(sse).with_header(
                        tiny_http::Header::from_bytes("Content-Type", "text/event-stream").unwrap(),
                    ))
                    .unwrap();
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
                    &LlmCallConfig::new("test-model"),
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
        assert!(error.to_string().contains("plan use was not granted"));
    }
}

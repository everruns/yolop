//! Settings and registry adapters. Everruns owns the protocols and token lifecycle.
use crate::config::{CodexAuth, SettingsStore};
use async_trait::async_trait;
use everruns_drivers::chatgpt::auth::{RotatingAuth, TokenRoute, TokenStore};
pub use everruns_drivers::codex::CODEX_DRIVER_ID;
use everruns_provider::{DriverRegistry, ModelProfile, ProviderMetadata};
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
        let provider = if plan {
            everruns_drivers::chatgpt::provider(config.provider.clone(), auth)
        } else {
            everruns_drivers::codex::provider(config.provider.clone(), auth)
        };
        provider.into_boxed_driver()
    });
}

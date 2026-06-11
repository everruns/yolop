// Configuration as a service.
//
// A read-oriented handle over the settings file that any capability can depend
// on to read schema-described configuration — typed getters plus a generic
// `current(key)` keyed by the schema in `crate::config_schema`. It is backed by
// the shared `SettingsStore` (the single writer), so reads always reflect the
// latest persisted state without each capability re-parsing the TOML or
// reaching into store internals.
//
// Capabilities that only *read* config (e.g. attribution, and a future
// approval/mode capability) take an `Arc<dyn ConfigService>` instead of an
// `Arc<SettingsStore>`; capabilities that *write* (setup, config) keep the
// concrete store. `SettingsStore` implements `ConfigService`, so the same
// shared handle satisfies both.

use crate::config_schema::{KeyTarget, parse_key};
use crate::runtime::SUPPORTED_PROVIDERS;
use crate::settings::{ApprovalMode, Settings, SettingsStore};
use serde_json::Value;

/// Read-only view of yolop configuration, injectable into capabilities.
///
/// Typed getters cover the common cases; `current` reads any key by its schema
/// name so a capability can consume configuration it does not own without
/// knowing the storage layout. Secrets are never returned in the clear.
///
/// This is a deliberately broad service contract: some getters exist for future
/// read-only consumers and are not yet wired into a caller, so `dead_code` is
/// allowed on the trait surface.
#[allow(dead_code)]
pub trait ConfigService: Send + Sync {
    /// Current in-memory settings snapshot (the source of truth in memory).
    fn snapshot(&self) -> Settings;

    /// The soft-approval paranoia level.
    fn approval_mode(&self) -> ApprovalMode {
        self.snapshot().approval_mode()
    }

    /// The configured default provider, if any.
    fn default_provider(&self) -> Option<String> {
        self.snapshot().provider
    }

    /// The global fallback model spec, if any.
    fn default_model(&self) -> Option<String> {
        self.snapshot().default_model
    }

    /// Whether commit/PR attribution is enabled.
    fn attribution_enabled(&self) -> bool {
        self.snapshot().attribution_enabled()
    }

    /// Whether an API token is stored for `provider` (presence only).
    fn token_present(&self, provider: &str) -> bool {
        self.snapshot().has_token(provider)
    }

    /// The per-provider model spec, if any.
    fn model_for(&self, provider: &str) -> Option<String> {
        self.snapshot().model_for(provider).map(str::to_string)
    }

    /// The per-provider endpoint base URL, if any.
    fn base_url_for(&self, provider: &str) -> Option<String> {
        self.snapshot().base_url_for(provider).map(str::to_string)
    }

    /// Read any configuration value by its schema key (e.g. `default_provider`,
    /// `models.openai`). Secrets are reduced to `stored`/`unset`. Returns the
    /// schema validation error for unknown or malformed keys.
    fn current(&self, key: &str) -> Result<Value, String> {
        let target = parse_key(key)?;
        Ok(current_value(&self.snapshot(), &target))
    }
}

impl ConfigService for SettingsStore {
    fn snapshot(&self) -> Settings {
        SettingsStore::snapshot(self)
    }
}

/// The current value of a single target, with secrets reduced to presence only.
pub(crate) fn current_value(settings: &Settings, target: &KeyTarget) -> Value {
    match target {
        KeyTarget::DefaultProvider => settings
            .provider
            .clone()
            .map(Value::String)
            .unwrap_or(Value::Null),
        KeyTarget::DefaultModel => settings
            .default_model()
            .map(|s| Value::String(s.to_string()))
            .unwrap_or(Value::Null),
        KeyTarget::Attribution => Value::Bool(settings.attribution_enabled()),
        KeyTarget::ApprovalMode => Value::String(settings.approval_mode().as_str().to_string()),
        KeyTarget::Model(p) => settings
            .model_for(p)
            .map(|s| Value::String(s.to_string()))
            .unwrap_or(Value::Null),
        KeyTarget::Token(p) => Value::String(
            if settings.has_token(p) {
                "stored"
            } else {
                "unset"
            }
            .to_string(),
        ),
        KeyTarget::BaseUrl(p) => settings
            .base_url_for(p)
            .map(|s| Value::String(s.to_string()))
            .unwrap_or(Value::Null),
    }
}

/// Full current state of a provider-scoped table (secrets redacted). Only
/// supported providers are listed, so reads stay consistent with the providers
/// `set_config` can address — tolerant loading may leave unknown provider
/// entries in the file, but they are not manageable here.
pub(crate) fn scoped_current(settings: &Settings, key: &str) -> Value {
    let mut map = serde_json::Map::new();
    let supported = |provider: &String| SUPPORTED_PROVIDERS.contains(&provider.as_str());
    match key {
        "tokens" => {
            for provider in settings.tokens.keys().filter(|p| supported(p)) {
                map.insert(provider.clone(), Value::String("stored".to_string()));
            }
        }
        "models" => {
            for (provider, spec) in settings.models.iter().filter(|(p, _)| supported(p)) {
                map.insert(provider.clone(), Value::String(spec.clone()));
            }
        }
        "base_urls" => {
            for (provider, url) in settings.base_urls.iter().filter(|(p, _)| supported(p)) {
                map.insert(provider.clone(), Value::String(url.clone()));
            }
        }
        _ => {}
    }
    Value::Object(map)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn service_reads_typed_and_by_key() {
        let tmp = tempfile::tempdir().expect("tmp");
        let store = Arc::new(SettingsStore::open(tmp.path().join("settings.toml")));
        store.set_provider(Some("anthropic".to_string())).unwrap();
        store
            .set_default_model(Some("claude-sonnet-4-5".to_string()))
            .unwrap();
        store
            .set_token("openai".to_string(), "sk-secret".to_string())
            .unwrap();
        store
            .set_model("openai".to_string(), "gpt-5.5 high".to_string())
            .unwrap();
        store
            .set_base_url("custom".to_string(), "http://localhost:8000/v1".to_string())
            .unwrap();

        // Inject as the trait object capabilities would receive.
        let svc: Arc<dyn ConfigService> = store.clone();
        assert_eq!(svc.default_provider().as_deref(), Some("anthropic"));
        assert_eq!(svc.default_model().as_deref(), Some("claude-sonnet-4-5"));
        assert!(svc.token_present("openai"));
        assert!(svc.attribution_enabled());
        assert_eq!(svc.model_for("openai").as_deref(), Some("gpt-5.5 high"));
        assert_eq!(
            svc.base_url_for("custom").as_deref(),
            Some("http://localhost:8000/v1")
        );

        // Generic schema-keyed read, with secrets reduced to presence.
        assert_eq!(svc.current("default_provider").unwrap(), "anthropic");
        assert_eq!(svc.current("tokens.openai").unwrap(), "stored");
        assert!(
            !svc.current("tokens.openai")
                .unwrap()
                .to_string()
                .contains("sk-secret"),
            "secret value must never be returned"
        );
        assert!(svc.current("frobnicate").is_err());
    }
}

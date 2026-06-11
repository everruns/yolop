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
/// `current` reads any value by its schema key so a capability can consume
/// configuration it does not own without knowing the storage layout; the two
/// semantic getters cover the prompt-shaping reads that have dedicated
/// consumers. Secrets are never returned in the clear. Capabilities add a typed
/// getter here when they grow a real need rather than carrying speculative
/// surface.
pub trait ConfigService: Send + Sync {
    /// Current in-memory settings snapshot (the source of truth in memory).
    fn snapshot(&self) -> Settings;

    /// The soft-approval paranoia level (consumed by `ApprovalCapability`).
    fn approval_mode(&self) -> ApprovalMode {
        self.snapshot().approval_mode()
    }

    /// Whether commit/PR attribution is enabled (consumed by
    /// `AttributionCapability`).
    fn attribution_enabled(&self) -> bool {
        self.snapshot().attribution_enabled()
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
            .default_provider
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
    fn service_reads_semantic_getters_and_by_key() {
        let tmp = tempfile::tempdir().expect("tmp");
        let store = Arc::new(SettingsStore::open(tmp.path().join("settings.toml")));
        store
            .set_default_provider(Some("anthropic".to_string()))
            .unwrap();
        store
            .set_token("openai".to_string(), "sk-secret".to_string())
            .unwrap();
        store
            .set_approval_mode(crate::settings::ApprovalMode::Protective)
            .unwrap();

        // Inject as the trait object capabilities would receive.
        let svc: Arc<dyn ConfigService> = store.clone();
        // Semantic getters used by the prompt-shaping capabilities.
        assert!(svc.attribution_enabled());
        assert_eq!(
            svc.approval_mode(),
            crate::settings::ApprovalMode::Protective
        );

        // Generic schema-keyed read covers everything else, with secrets
        // reduced to presence.
        assert_eq!(svc.current("default_provider").unwrap(), "anthropic");
        assert_eq!(svc.current("approval_mode").unwrap(), "protective");
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

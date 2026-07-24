//! Per-extension secret storage and the redacting [`Secret`] type.
//!
//! Extension credentials (a `secret: true` config field — e.g. a Logfire write
//! token) live in the shared credential store (`connections.toml`, written
//! owner-only 0600, redacted in output), **never** in `settings.toml` config.
//! Keeping them out of config is the core leak guard: the agent-readable config
//! surface (`list_extensions`, `get_config`) never carries a secret value, and
//! entry happens out-of-band through a user prompt, so a secret never enters the
//! model's context. Secrets reach the extension server host→server only — as
//! injected env at spawn (per the field's `env` mapping) — not via the wire
//! `initialize.config`.
//!
//! The `secret` marker decouples the *concept* from the *location*: today the
//! value sits in `connections.toml`; moving it to an OS keychain or a vault
//! later changes only this module, not any extension manifest.

use super::package::ExtensionManifest;
use crate::connectors::ConnectionStore;
use anyhow::Result;
use std::collections::BTreeMap;
use std::sync::Arc;

/// A secret string that never reveals itself through `Debug` (so it can't leak
/// into logs, tracing, or panic output). Read the value only at the point of
/// use via [`Secret::expose`].
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The raw value. Call this only where the secret is actually needed
    /// (env injection, an auth header) — never to log or return to the agent.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(***)")
    }
}

/// Per-extension secrets over the shared credential store. Keyed `ext:<name>`
/// so extension secrets never collide with connector credentials in the same
/// file.
#[derive(Clone)]
pub struct ExtensionSecrets {
    store: Arc<ConnectionStore>,
}

impl ExtensionSecrets {
    pub fn new(store: Arc<ConnectionStore>) -> Self {
        Self { store }
    }

    fn key(ext: &str) -> String {
        format!("ext:{ext}")
    }

    /// The stored value of one secret field, if set and non-empty.
    pub fn get(&self, ext: &str, field: &str) -> Option<Secret> {
        self.store
            .get(&Self::key(ext))
            .and_then(|conn| conn.fields.get(field).cloned())
            .filter(|value| !value.trim().is_empty())
            .map(Secret)
    }

    /// Whether a secret field has a stored value — the redacted status the agent
    /// is allowed to see (never the value itself).
    pub fn is_set(&self, ext: &str, field: &str) -> bool {
        self.get(ext, field).is_some()
    }

    /// Store (or replace) one secret field. Persists owner-only via the store.
    pub fn set(&self, ext: &str, field: &str, value: Secret) -> Result<()> {
        let mut conn = self.store.get(&Self::key(ext)).unwrap_or_default();
        conn.fields.insert(field.to_string(), value.0);
        self.store.save(&Self::key(ext), conn)
    }

    /// Drop all secrets for an extension (e.g. on uninstall).
    pub fn clear(&self, ext: &str) -> Result<bool> {
        self.store.clear(&Self::key(ext))
    }

    /// Env-var name → value for the manifest's secret fields that declare an
    /// `env` mapping and have a stored value — the injection set for spawn.
    pub fn env_overrides(&self, manifest: &ExtensionManifest) -> BTreeMap<String, String> {
        let mut env = BTreeMap::new();
        for field in manifest.secret_fields() {
            if let (Some(name), Some(secret)) =
                (field.env.as_ref(), self.get(&manifest.name, &field.name))
            {
                env.insert(name.clone(), secret.expose().to_string());
            }
        }
        env
    }

    /// Test/utility constructor over a throwaway store path.
    #[cfg(test)]
    pub fn open_at(path: std::path::PathBuf) -> Self {
        Self::new(Arc::new(ConnectionStore::open(path)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn manifest_with_secret() -> ExtensionManifest {
        crate::extensions::package::parse_manifest(
            &json!({
                "name": "logfire", "description": "t",
                "yolop": {
                    "protocol_version": "1.0",
                    "capabilityServer": { "command": "x" },
                    "trace": true,
                    "config_schema": {
                        "type": "object",
                        "required": ["token"],
                        "properties": {
                            "token": { "type": "string", "secret": true, "env": "LOGFIRE_TOKEN" },
                            "region": { "type": "string", "default": "us" }
                        }
                    }
                }
            })
            .to_string(),
        )
        .unwrap()
    }

    #[test]
    fn secret_debug_is_redacted() {
        let s = Secret::new("pylf_v1_supersecret");
        assert_eq!(format!("{s:?}"), "Secret(***)");
        assert!(!format!("{s:?}").contains("supersecret"));
        assert_eq!(s.expose(), "pylf_v1_supersecret");
    }

    #[test]
    fn set_get_and_env_overrides_roundtrip_via_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let secrets = ExtensionSecrets::open_at(tmp.path().join("connections.toml"));
        assert!(!secrets.is_set("logfire", "token"));

        secrets
            .set("logfire", "token", Secret::new("pylf_v1_abc"))
            .unwrap();
        assert!(secrets.is_set("logfire", "token"));
        assert_eq!(
            secrets.get("logfire", "token").unwrap().expose(),
            "pylf_v1_abc"
        );

        // Env override maps the field's `env` name to the value.
        let env = secrets.env_overrides(&manifest_with_secret());
        assert_eq!(
            env.get("LOGFIRE_TOKEN").map(String::as_str),
            Some("pylf_v1_abc")
        );
        // Non-secret fields never appear in the env-override set.
        assert!(!env.contains_key("region"));
    }

    #[test]
    fn config_fields_parse_secret_env_and_required() {
        let manifest = manifest_with_secret();
        let token = manifest
            .config_fields()
            .into_iter()
            .find(|f| f.name == "token")
            .unwrap();
        assert!(token.secret);
        assert!(token.required);
        assert_eq!(token.env.as_deref(), Some("LOGFIRE_TOKEN"));
        assert_eq!(manifest.secret_fields().len(), 1);
    }
}

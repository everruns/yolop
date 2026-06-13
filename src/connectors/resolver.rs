//! Resolves connector credentials for sandbox capabilities at tool time.

use async_trait::async_trait;
use everruns_core::Result;
use everruns_core::traits::UserConnectionResolver;
use everruns_core::typed_id::SessionId;
use std::sync::Arc;

use crate::connectors::store::ConnectionStore;

/// Environment-variable fallbacks keyed by connector provider id.
pub(crate) fn env_credential(provider: &str) -> Option<String> {
    let var = match provider {
        "daytona" => "DAYTONA_API_KEY",
        _ => return None,
    };
    std::env::var(var)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// Resolves stored connector credentials for upstream sandbox capabilities.
pub struct YolopConnectionResolver {
    store: Arc<ConnectionStore>,
}

impl YolopConnectionResolver {
    pub fn new(store: Arc<ConnectionStore>) -> Self {
        Self { store }
    }

    fn stored_token(&self, provider: &str) -> Option<String> {
        let conn = self.store.get(provider)?;
        conn.fields
            .get("api_key")
            .cloned()
            .or_else(|| conn.fields.values().next().cloned())
            .filter(|v| !v.trim().is_empty())
    }

    fn resolve_token(&self, provider: &str) -> Option<String> {
        self.stored_token(provider)
            .or_else(|| env_credential(provider))
    }
}

#[async_trait]
impl UserConnectionResolver for YolopConnectionResolver {
    async fn get_connection_token(
        &self,
        _session_id: SessionId,
        provider: &str,
    ) -> Result<Option<String>> {
        Ok(self.resolve_token(provider))
    }

    async fn get_connection_metadata(
        &self,
        _session_id: SessionId,
        provider: &str,
    ) -> Result<Option<serde_json::Value>> {
        Ok(self.store.get(provider).and_then(|c| c.metadata.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[tokio::test]
    async fn prefers_stored_credential_over_env() {
        let tmp = tempfile::tempdir().expect("tmp");
        let store = Arc::new(ConnectionStore::open(tmp.path().join("connections.toml")));
        store
            .save(
                "daytona",
                crate::connectors::store::StoredConnection {
                    fields: BTreeMap::from([("api_key".to_string(), "stored-key".to_string())]),
                    metadata: None,
                },
            )
            .expect("save");
        let resolver = YolopConnectionResolver::new(store);
        let session_id = SessionId::new();
        let token = resolver
            .get_connection_token(session_id, "daytona")
            .await
            .expect("resolve");
        assert_eq!(token.as_deref(), Some("stored-key"));
    }

    #[tokio::test]
    async fn env_fallback_when_not_stored() {
        let tmp = tempfile::tempdir().expect("tmp");
        let store = Arc::new(ConnectionStore::open(tmp.path().join("connections.toml")));
        let resolver = YolopConnectionResolver::new(store);
        let session_id = SessionId::new();
        // SAFETY: test is single-threaded; env var is restored before return.
        unsafe {
            std::env::set_var("DAYTONA_API_KEY", "env-key");
        }
        let token = resolver
            .get_connection_token(session_id, "daytona")
            .await
            .expect("resolve");
        unsafe {
            std::env::remove_var("DAYTONA_API_KEY");
        }
        assert_eq!(token.as_deref(), Some("env-key"));
    }
}

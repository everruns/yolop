//! Suppress discovery only in read projections of attached MCP records.

use async_trait::async_trait;
use everruns_contracts::runtime::credential_services::CredentialService;
use everruns_contracts::{AgentLoopError, Result, typed_id::SessionId};
use everruns_core::session_mcp_servers::{SESSION_MCP_SERVER_KV_PREFIX, SessionMcpServer};
use everruns_core::session_services::{
    BoundSecret, BoundSecretInfo, KeyInfo, SecretInfo, SessionStorageStore,
};
use std::sync::Arc;

pub(super) struct DiscoveryStorage(pub(super) Arc<dyn SessionStorageStore>);

#[async_trait]
impl SessionStorageStore for DiscoveryStorage {
    fn credential_service(&self) -> Option<Arc<dyn CredentialService>> {
        self.0.credential_service()
    }

    async fn get_value(&self, session: SessionId, key: &str) -> Result<Option<String>> {
        let value = self.0.get_value(session, key).await?;
        if !key.starts_with(SESSION_MCP_SERVER_KV_PREFIX) {
            return Ok(value);
        }
        let Some(mut record) = value
            .as_deref()
            .and_then(|value| serde_json::from_str::<SessionMcpServer>(value).ok())
        else {
            // Preserve malformed records for native validation and diagnostics.
            return Ok(value);
        };
        record.server.tool_discovery = false;
        serde_json::to_string(&record)
            .map(Some)
            .map_err(|error| AgentLoopError::Internal(error.into()))
    }

    async fn set_value(&self, session: SessionId, key: &str, value: &str) -> Result<()> {
        self.0.set_value(session, key, value).await
    }

    // In particular, do not inherit the non-atomic read/delete default or
    // return a discovery projection when an authoritative record is consumed.
    async fn take_value(&self, session: SessionId, key: &str) -> Result<Option<String>> {
        self.0.take_value(session, key).await
    }

    async fn delete_value(&self, session: SessionId, key: &str) -> Result<bool> {
        self.0.delete_value(session, key).await
    }

    async fn list_keys(&self, session: SessionId) -> Result<Vec<KeyInfo>> {
        self.0.list_keys(session).await
    }

    async fn set_secret(&self, session: SessionId, name: &str, value: &str) -> Result<()> {
        self.0.set_secret(session, name, value).await
    }

    async fn get_secret(&self, session: SessionId, name: &str) -> Result<Option<String>> {
        self.0.get_secret(session, name).await
    }

    async fn delete_secret(&self, session: SessionId, name: &str) -> Result<bool> {
        self.0.delete_secret(session, name).await
    }

    async fn list_secrets(&self, session: SessionId) -> Result<Vec<SecretInfo>> {
        self.0.list_secrets(session).await
    }

    async fn list_bound_secrets(&self, session: SessionId) -> Result<Vec<BoundSecretInfo>> {
        self.0.list_bound_secrets(session).await
    }

    async fn resolve_bound_secrets(&self, session: SessionId) -> Result<Vec<BoundSecret>> {
        self.0.resolve_bound_secrets(session).await
    }
}

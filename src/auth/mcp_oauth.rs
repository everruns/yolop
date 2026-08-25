//! Yolop persistence and transport adapters for Everruns' shared MCP OAuth.

use crate::connectors::{ConnectionStore, StoredConnection};
use async_trait::async_trait;
use everruns_core::{
    EgressRequest, EgressResponse, EgressResult, EgressService, EgressStreamResponse,
};
use everruns_host::DirectEgressService;
use everruns_mcp::oauth::McpTokenStore;
use std::collections::BTreeMap;
use std::sync::Arc;

pub(crate) type McpOAuthTokenSet = everruns_mcp::oauth::TokenSet;

const KIND_FIELD: &str = "kind";
const KIND_OAUTH: &str = "oauth";
const ACCESS_TOKEN_FIELD: &str = "access_token";
const REFRESH_TOKEN_FIELD: &str = "refresh_token";
const TOKEN_TYPE_FIELD: &str = "token_type";
const EXPIRES_AT_FIELD: &str = "expires_at";
const SCOPE_FIELD: &str = "scope";
const TOKEN_ENDPOINT_FIELD: &str = "token_endpoint";
const CLIENT_ID_FIELD: &str = "client_id";
const CLIENT_SECRET_FIELD: &str = "client_secret";

pub(crate) fn connection_id(provider_id: &str) -> String {
    format!("mcp-oauth:{provider_id}")
}

pub(crate) fn save_tokens(
    store: &ConnectionStore,
    provider_id: &str,
    tokens: McpOAuthTokenSet,
) -> anyhow::Result<()> {
    store.save(&connection_id(provider_id), encode_tokens(tokens))
}

pub(crate) fn load_tokens(store: &ConnectionStore, provider_id: &str) -> Option<McpOAuthTokenSet> {
    store
        .get(&connection_id(provider_id))
        .and_then(|connection| decode_tokens(connection).ok())
}

fn encode_tokens(tokens: McpOAuthTokenSet) -> StoredConnection {
    let mut fields = BTreeMap::new();
    fields.insert(KIND_FIELD.to_string(), KIND_OAUTH.to_string());
    fields.insert(ACCESS_TOKEN_FIELD.to_string(), tokens.access_token);
    if let Some(value) = tokens.refresh_token {
        fields.insert(REFRESH_TOKEN_FIELD.to_string(), value);
    }
    fields.insert(TOKEN_TYPE_FIELD.to_string(), tokens.token_type);
    if let Some(value) = tokens.expires_at {
        fields.insert(EXPIRES_AT_FIELD.to_string(), value.to_rfc3339());
    }
    if let Some(value) = tokens.scope {
        fields.insert(SCOPE_FIELD.to_string(), value);
    }
    if let Some(value) = tokens.token_endpoint {
        fields.insert(TOKEN_ENDPOINT_FIELD.to_string(), value);
    }
    if let Some(value) = tokens.client_id {
        fields.insert(CLIENT_ID_FIELD.to_string(), value);
    }
    if let Some(value) = tokens.client_secret {
        fields.insert(CLIENT_SECRET_FIELD.to_string(), value);
    }
    StoredConnection {
        fields,
        metadata: None,
    }
}

fn decode_tokens(connection: StoredConnection) -> Result<McpOAuthTokenSet, ()> {
    if connection.fields.get(KIND_FIELD).map(String::as_str) != Some(KIND_OAUTH) {
        return Err(());
    }
    let non_empty = |field: &str| {
        connection
            .fields
            .get(field)
            .filter(|value| !value.trim().is_empty())
            .cloned()
    };
    let access_token = non_empty(ACCESS_TOKEN_FIELD).ok_or(())?;
    let expires_at = connection
        .fields
        .get(EXPIRES_AT_FIELD)
        .map(|raw| {
            chrono::DateTime::parse_from_rfc3339(raw).map(|dt| dt.with_timezone(&chrono::Utc))
        })
        .transpose()
        .map_err(|_| ())?;
    Ok(McpOAuthTokenSet {
        access_token,
        refresh_token: non_empty(REFRESH_TOKEN_FIELD),
        token_type: non_empty(TOKEN_TYPE_FIELD).unwrap_or_else(|| "Bearer".to_string()),
        expires_at,
        scope: non_empty(SCOPE_FIELD),
        token_endpoint: non_empty(TOKEN_ENDPOINT_FIELD),
        client_id: non_empty(CLIENT_ID_FIELD),
        client_secret: non_empty(CLIENT_SECRET_FIELD),
    })
}

/// Adapts Yolop's synchronous connection file to the shared OAuth token store.
#[derive(Clone)]
pub(crate) struct ConnectionTokenStore {
    store: Arc<ConnectionStore>,
}

impl ConnectionTokenStore {
    pub(crate) fn new(store: Arc<ConnectionStore>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl McpTokenStore for ConnectionTokenStore {
    async fn load(&self, server_name: &str) -> anyhow::Result<Option<McpOAuthTokenSet>> {
        Ok(load_tokens(&self.store, server_name))
    }

    async fn save(&self, server_name: &str, tokens: &McpOAuthTokenSet) -> anyhow::Result<()> {
        save_tokens(&self.store, server_name, tokens.clone())
    }
}

/// The shared OAuth client requires DNS pinning for discovered public endpoints.
/// Literal loopback URLs are the one local-CLI exception: users explicitly
/// configure them and the upstream URL validator already limits plaintext HTTP
/// to loopback hosts.
struct YolopMcpOAuthEgress {
    direct: DirectEgressService,
}

impl Default for YolopMcpOAuthEgress {
    fn default() -> Self {
        Self {
            direct: DirectEgressService::for_runtime_traffic_from_env(),
        }
    }
}

fn allow_configured_loopback(mut request: EgressRequest) -> EgressRequest {
    let is_loopback = reqwest::Url::parse(&request.url)
        .ok()
        .is_some_and(|url| matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "::1")));
    if is_loopback {
        request.dns_pinning_required = false;
    }
    request
}

#[async_trait]
impl EgressService for YolopMcpOAuthEgress {
    async fn send(&self, request: EgressRequest) -> EgressResult<EgressResponse> {
        self.direct.send(allow_configured_loopback(request)).await
    }

    async fn send_stream(&self, request: EgressRequest) -> EgressResult<EgressStreamResponse> {
        self.direct
            .send_stream(allow_configured_loopback(request))
            .await
    }
}

pub(crate) fn oauth_egress() -> Arc<dyn EgressService> {
    Arc::new(YolopMcpOAuthEgress::default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    fn sample(expires_at: Option<chrono::DateTime<Utc>>) -> McpOAuthTokenSet {
        McpOAuthTokenSet {
            access_token: "access".to_string(),
            refresh_token: Some("refresh".to_string()),
            token_type: "Bearer".to_string(),
            expires_at,
            scope: Some("search".to_string()),
            token_endpoint: Some("https://as.example/token".to_string()),
            client_id: Some("client-123".to_string()),
            client_secret: None,
        }
    }

    #[test]
    fn round_trips_upstream_oauth_tokens_with_refresh_metadata() {
        let tmp = tempfile::tempdir().expect("tmp");
        let store = ConnectionStore::open(tmp.path().join("connections.toml"));
        let expires_at = Utc.with_ymd_and_hms(2030, 1, 2, 3, 4, 5).unwrap();
        save_tokens(&store, "parallel", sample(Some(expires_at))).expect("save tokens");

        let loaded = load_tokens(&store, "parallel").expect("loaded tokens");
        assert_eq!(loaded.access_token, "access");
        assert_eq!(loaded.refresh_token.as_deref(), Some("refresh"));
        assert_eq!(loaded.expires_at, Some(expires_at));
        assert_eq!(loaded.authorization_value(), "Bearer access");
    }

    #[test]
    fn rejects_non_oauth_connections() {
        let connection = StoredConnection {
            fields: BTreeMap::from([("api_key".to_string(), "secret".to_string())]),
            metadata: None,
        };
        assert!(decode_tokens(connection).is_err());
    }
}

//! Persistence for user-scoped MCP OAuth tokens.
//!
//! Tokens minted by the interactive login flow (`crate::auth::mcp_oauth_login`) are
//! stored in the shared [`ConnectionStore`] under a stable `mcp-oauth:<id>`
//! connection id, alongside just enough of the authorization-server contract
//! (`token_endpoint`, `client_id`, optional `client_secret`) to refresh the
//! access token later without re-reading MCP config or re-running discovery.
//! [`crate::runtime::StoredMcpAuthProvider`] reads them back per request.

use crate::connectors::{ConnectionStore, StoredConnection};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct McpOAuthTokenSet {
    pub(crate) access_token: String,
    pub(crate) refresh_token: Option<String>,
    #[serde(default = "default_token_type")]
    pub(crate) token_type: String,
    pub(crate) expires_at: Option<DateTime<Utc>>,
    pub(crate) scope: Option<String>,
    /// Authorization-server token endpoint, retained so a refresh is
    /// self-contained (no config re-read / re-discovery).
    pub(crate) token_endpoint: Option<String>,
    /// OAuth client id used at login (from dynamic registration or config).
    pub(crate) client_id: Option<String>,
    /// Client secret, only for confidential clients that were issued one.
    pub(crate) client_secret: Option<String>,
}

impl McpOAuthTokenSet {
    /// True once `expires_at` has arrived. A `now` at or past the expiry is
    /// treated as expired so we refresh before the token is rejected.
    pub(crate) fn is_expired(&self, now: DateTime<Utc>) -> bool {
        self.expires_at.is_some_and(|expires_at| expires_at <= now)
    }

    /// The `Authorization` header value this token set contributes.
    pub(crate) fn authorization_value(&self) -> String {
        format!("{} {}", self.token_type, self.access_token)
    }
}

fn default_token_type() -> String {
    "Bearer".to_string()
}

pub(crate) fn connection_id(provider_id: &str) -> String {
    format!("mcp-oauth:{provider_id}")
}

pub(crate) fn save_tokens(
    store: &ConnectionStore,
    provider_id: &str,
    tokens: McpOAuthTokenSet,
) -> anyhow::Result<()> {
    store.save(&connection_id(provider_id), tokens.into())
}

pub(crate) fn load_tokens(store: &ConnectionStore, provider_id: &str) -> Option<McpOAuthTokenSet> {
    store
        .get(&connection_id(provider_id))
        .and_then(|connection| connection.try_into().ok())
}

impl From<McpOAuthTokenSet> for StoredConnection {
    fn from(tokens: McpOAuthTokenSet) -> Self {
        let mut fields = BTreeMap::new();
        fields.insert(KIND_FIELD.to_string(), KIND_OAUTH.to_string());
        fields.insert(ACCESS_TOKEN_FIELD.to_string(), tokens.access_token);
        if let Some(refresh_token) = tokens.refresh_token {
            fields.insert(REFRESH_TOKEN_FIELD.to_string(), refresh_token);
        }
        fields.insert(TOKEN_TYPE_FIELD.to_string(), tokens.token_type);
        if let Some(expires_at) = tokens.expires_at {
            fields.insert(EXPIRES_AT_FIELD.to_string(), expires_at.to_rfc3339());
        }
        if let Some(scope) = tokens.scope {
            fields.insert(SCOPE_FIELD.to_string(), scope);
        }
        if let Some(token_endpoint) = tokens.token_endpoint {
            fields.insert(TOKEN_ENDPOINT_FIELD.to_string(), token_endpoint);
        }
        if let Some(client_id) = tokens.client_id {
            fields.insert(CLIENT_ID_FIELD.to_string(), client_id);
        }
        if let Some(client_secret) = tokens.client_secret {
            fields.insert(CLIENT_SECRET_FIELD.to_string(), client_secret);
        }
        StoredConnection {
            fields,
            metadata: None,
        }
    }
}

impl TryFrom<StoredConnection> for McpOAuthTokenSet {
    type Error = ();

    fn try_from(connection: StoredConnection) -> Result<Self, Self::Error> {
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
        let token_type = non_empty(TOKEN_TYPE_FIELD).unwrap_or_else(default_token_type);
        let expires_at = connection
            .fields
            .get(EXPIRES_AT_FIELD)
            .map(|raw| DateTime::parse_from_rfc3339(raw).map(|dt| dt.with_timezone(&Utc)))
            .transpose()
            .map_err(|_| ())?;
        Ok(Self {
            access_token,
            refresh_token: non_empty(REFRESH_TOKEN_FIELD),
            token_type,
            expires_at,
            scope: non_empty(SCOPE_FIELD),
            token_endpoint: non_empty(TOKEN_ENDPOINT_FIELD),
            client_id: non_empty(CLIENT_ID_FIELD),
            client_secret: non_empty(CLIENT_SECRET_FIELD),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn sample(expires_at: Option<DateTime<Utc>>) -> McpOAuthTokenSet {
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
    fn round_trips_oauth_tokens_with_refresh_metadata() {
        let tmp = tempfile::tempdir().expect("tmp");
        let store = ConnectionStore::open(tmp.path().join("connections.toml"));
        let expires_at = Utc.with_ymd_and_hms(2030, 1, 2, 3, 4, 5).unwrap();
        save_tokens(&store, "parallel", sample(Some(expires_at))).expect("save tokens");

        let raw = store.get("mcp-oauth:parallel").expect("stored raw");
        assert_eq!(raw.fields.get("kind").map(String::as_str), Some("oauth"));
        assert_eq!(
            raw.fields.get("token_endpoint").map(String::as_str),
            Some("https://as.example/token")
        );

        let loaded = load_tokens(&store, "parallel").expect("loaded tokens");
        assert_eq!(loaded.access_token, "access");
        assert_eq!(loaded.refresh_token.as_deref(), Some("refresh"));
        assert_eq!(loaded.expires_at, Some(expires_at));
        assert_eq!(loaded.scope.as_deref(), Some("search"));
        assert_eq!(loaded.client_id.as_deref(), Some("client-123"));
        assert_eq!(loaded.authorization_value(), "Bearer access");
    }

    #[test]
    fn rejects_non_oauth_connections() {
        let connection = StoredConnection {
            fields: BTreeMap::from([("api_key".to_string(), "secret".to_string())]),
            metadata: None,
        };
        assert!(McpOAuthTokenSet::try_from(connection).is_err());
    }

    #[test]
    fn detects_expired_tokens_at_boundary() {
        let expires_at = Utc.with_ymd_and_hms(2030, 1, 2, 3, 4, 5).unwrap();
        let tokens = sample(Some(expires_at));
        assert!(!tokens.is_expired(expires_at - chrono::TimeDelta::seconds(1)));
        assert!(tokens.is_expired(expires_at));
    }
}

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

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct McpOAuthTokenSet {
    pub(crate) access_token: String,
    pub(crate) refresh_token: Option<String>,
    #[serde(default = "default_token_type")]
    pub(crate) token_type: String,
    pub(crate) expires_at: Option<DateTime<Utc>>,
    pub(crate) scope: Option<String>,
}

impl McpOAuthTokenSet {
    #[allow(dead_code)]
    pub(crate) fn is_expired(&self, now: DateTime<Utc>) -> bool {
        self.expires_at.is_some_and(|expires_at| expires_at <= now)
    }
}

fn default_token_type() -> String {
    "Bearer".to_string()
}

#[allow(dead_code)]
pub(crate) fn connection_id(provider_id: &str) -> String {
    format!("mcp-oauth:{provider_id}")
}

#[allow(dead_code)]
pub(crate) fn save_tokens(
    store: &ConnectionStore,
    provider_id: &str,
    tokens: McpOAuthTokenSet,
) -> anyhow::Result<()> {
    store.save(&connection_id(provider_id), tokens.into())
}

#[allow(dead_code)]
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
        let access_token = connection
            .fields
            .get(ACCESS_TOKEN_FIELD)
            .filter(|token| !token.trim().is_empty())
            .cloned()
            .ok_or(())?;
        let refresh_token = connection
            .fields
            .get(REFRESH_TOKEN_FIELD)
            .filter(|token| !token.trim().is_empty())
            .cloned();
        let token_type = connection
            .fields
            .get(TOKEN_TYPE_FIELD)
            .filter(|token_type| !token_type.trim().is_empty())
            .cloned()
            .unwrap_or_else(default_token_type);
        let expires_at = connection
            .fields
            .get(EXPIRES_AT_FIELD)
            .map(|raw| DateTime::parse_from_rfc3339(raw).map(|dt| dt.with_timezone(&Utc)))
            .transpose()
            .map_err(|_| ())?;
        let scope = connection
            .fields
            .get(SCOPE_FIELD)
            .filter(|scope| !scope.trim().is_empty())
            .cloned();
        Ok(Self {
            access_token,
            refresh_token,
            token_type,
            expires_at,
            scope,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn stores_oauth_tokens_under_provider_connection_id() {
        let tmp = tempfile::tempdir().expect("tmp");
        let store = ConnectionStore::open(tmp.path().join("connections.toml"));
        let expires_at = Utc.with_ymd_and_hms(2030, 1, 2, 3, 4, 5).unwrap();
        save_tokens(
            &store,
            "parallel",
            McpOAuthTokenSet {
                access_token: "access".to_string(),
                refresh_token: Some("refresh".to_string()),
                token_type: "Bearer".to_string(),
                expires_at: Some(expires_at),
                scope: Some("search".to_string()),
            },
        )
        .expect("save tokens");

        let raw = store.get("mcp-oauth:parallel").expect("stored raw");
        assert_eq!(raw.fields.get("kind").map(String::as_str), Some("oauth"));
        assert_eq!(
            raw.fields.get("refresh_token").map(String::as_str),
            Some("refresh")
        );
        let loaded = load_tokens(&store, "parallel").expect("loaded tokens");
        assert_eq!(loaded.access_token, "access");
        assert_eq!(loaded.refresh_token.as_deref(), Some("refresh"));
        assert_eq!(loaded.expires_at, Some(expires_at));
        assert_eq!(loaded.scope.as_deref(), Some("search"));
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
        let tokens = McpOAuthTokenSet {
            access_token: "access".to_string(),
            refresh_token: None,
            token_type: "Bearer".to_string(),
            expires_at: Some(expires_at),
            scope: None,
        };
        assert!(!tokens.is_expired(expires_at - chrono::TimeDelta::seconds(1)));
        assert!(tokens.is_expired(expires_at));
    }
}

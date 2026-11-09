//! Yolop persistence and transport adapters for Everruns' shared MCP OAuth.

use crate::connectors::{ConnectionStore, StoredConnection};
use async_trait::async_trait;
use everruns_core::EgressRequestKind;
use everruns_core::host::DirectEgressService;
use everruns_core::mcp::oauth::{DEFAULT_REFRESH_SKEW_SECONDS, OAuthClient};
use everruns_core::mcp::{McpAuthProvider, McpAuthRequest, McpCredential};
use everruns_core::{
    EgressRequest, EgressResponse, EgressResult, EgressService, EgressStreamResponse,
};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::sync::Arc;

pub(crate) type McpOAuthTokenSet = everruns_core::mcp::oauth::TokenSet;

const KIND_FIELD: &str = "kind";
const KIND_OAUTH: &str = "oauth";
const ACCESS_TOKEN_FIELD: &str = "access_token";
const REFRESH_TOKEN_FIELD: &str = "refresh_token";
const TOKEN_TYPE_FIELD: &str = "token_type";
const EXPIRES_AT_FIELD: &str = "expires_at";
const SCOPE_FIELD: &str = "scope";
const TOKEN_ENDPOINT_FIELD: &str = "token_endpoint";
const RESOURCE_FIELD: &str = "resource";
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

#[cfg(test)]
pub(crate) fn load_tokens(store: &ConnectionStore, provider_id: &str) -> Option<McpOAuthTokenSet> {
    store
        .get(&connection_id(provider_id))
        .and_then(|connection| decode_tokens(connection).ok())
}

fn encode_tokens(tokens: McpOAuthTokenSet) -> StoredConnection {
    let mut fields = BTreeMap::new();
    // Distinguish a replacement login even when the issuer returns identical tokens.
    fields.insert("revision".into(), everruns_core::mcp::oauth::random_state());
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
    if let Some(value) = tokens.resource {
        fields.insert(RESOURCE_FIELD.to_string(), value);
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
        resource: non_empty(RESOURCE_FIELD),
        client_id: non_empty(CLIENT_ID_FIELD),
        client_secret: non_empty(CLIENT_SECRET_FIELD),
    })
}

/// The lease covers the entire read/refresh/conditional-write cycle across
/// processes. Logout and new login do not wait for network I/O; the comparison
/// rejects their stale refresh result instead.
pub(crate) struct CoordinatedOAuthProvider {
    store: Arc<ConnectionStore>,
    egress: Arc<dyn EgressService>,
}

impl CoordinatedOAuthProvider {
    pub(crate) fn new(store: Arc<ConnectionStore>, egress: Arc<dyn EgressService>) -> Self {
        Self { store, egress }
    }

    async fn refresh_lease(&self, provider_id: &str) -> anyhow::Result<std::fs::File> {
        // This public identifier selects a filesystem-safe lock name, not a credential hash.
        let digest = Sha256::digest(provider_id.as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let path = self
            .store
            .path()
            .with_extension(format!("mcp-{digest}.lock"));
        let file = crate::connectors::private_lock_file(&path)?;
        crate::connectors::cancellable_lock(file).await
    }
}

#[async_trait]
impl McpAuthProvider for CoordinatedOAuthProvider {
    async fn authorization(
        &self,
        request: &McpAuthRequest<'_>,
    ) -> anyhow::Result<Option<McpCredential>> {
        let provider_id = request.oauth_provider_id.unwrap_or(request.server_name);
        let id = connection_id(provider_id);
        // Readers with a usable credential do not queue behind network I/O.
        // Refreshers still re-read after acquiring the cross-process lease.
        let Some(snapshot) = self.store.get_checked(&id)? else {
            return Ok(None);
        };
        let tokens = decode_tokens(snapshot)
            .map_err(|_| anyhow::anyhow!("Invalid MCP OAuth credentials; sign in again"))?;
        if !tokens.needs_refresh(chrono::Utc::now(), DEFAULT_REFRESH_SKEW_SECONDS)
            || tokens.refresh_token.is_none()
        {
            return Ok(Some(McpCredential::authorization(
                tokens.authorization_value(),
            )));
        }
        let _lease = self.refresh_lease(provider_id).await?;
        let Some(expected) = self.store.get_checked(&id)? else {
            return Ok(None);
        };
        let current = decode_tokens(expected.clone())
            .map_err(|_| anyhow::anyhow!("Invalid MCP OAuth credentials; sign in again"))?;
        if !current.needs_refresh(chrono::Utc::now(), DEFAULT_REFRESH_SKEW_SECONDS)
            || current.refresh_token.is_none()
        {
            return Ok(Some(McpCredential::authorization(
                current.authorization_value(),
            )));
        }
        let renewed = OAuthClient::new(self.egress.as_ref(), EgressRequestKind::Mcp)
            .refresh(&current)
            .await?;
        anyhow::ensure!(
            self.store
                .compare_and_save(&id, &expected, encode_tokens(renewed.clone()))
                .await?,
            "MCP OAuth credentials changed during refresh; retry the request"
        );
        Ok(Some(McpCredential::authorization(
            renewed.authorization_value(),
        )))
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
            resource: Some("https://mcp.example/resource".to_string()),
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

        drop(store);
        let store = ConnectionStore::open(tmp.path().join("connections.toml"));
        let loaded = load_tokens(&store, "parallel").expect("loaded tokens");
        assert_eq!(
            loaded.resource.as_deref(),
            Some("https://mcp.example/resource")
        );
        assert_eq!(loaded.access_token, "access");
        assert_eq!(loaded.refresh_token.as_deref(), Some("refresh"));
        assert_eq!(loaded.expires_at, Some(expires_at));
        assert_eq!(loaded.authorization_value(), "Bearer access");
    }

    #[derive(Default)]
    struct RefreshEgress(std::sync::Mutex<Vec<EgressRequest>>);

    #[async_trait]
    impl EgressService for RefreshEgress {
        async fn send(&self, request: EgressRequest) -> EgressResult<EgressResponse> {
            self.0.lock().unwrap().push(request);
            Ok(EgressResponse {
                status: 200,
                headers: BTreeMap::new(),
                body: br#"{"access_token":"renewed","refresh_token":"rotated","expires_in":0}"#
                    .to_vec(),
            })
        }
        async fn send_stream(&self, _: EgressRequest) -> EgressResult<EgressStreamResponse> {
            panic!("OAuth refresh must not stream")
        }
    }

    #[tokio::test]
    async fn persisted_resource_survives_rotation_and_repeated_process_restart() {
        use everruns_core::mcp::{McpAuthProvider, McpAuthRequest};
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("connections.toml");
        let original = sample(Some(Utc::now() - chrono::Duration::seconds(1)));
        save_tokens(
            &ConnectionStore::open(path.clone()),
            "server",
            original.clone(),
        )
        .unwrap();
        let egress = Arc::new(RefreshEgress::default());
        for _ in 0..2 {
            let store = Arc::new(ConnectionStore::open(path.clone()));
            let provider = CoordinatedOAuthProvider::new(store.clone(), egress.clone());
            provider
                .authorization(&McpAuthRequest {
                    server_name: "server",
                    auth_mode: everruns_core::McpServerAuthMode::OAuth,
                    oauth_provider_id: None,
                })
                .await
                .unwrap()
                .unwrap();
            let stored = load_tokens(&store, "server").unwrap();
            assert_eq!(stored.resource, original.resource);
            assert_eq!(stored.refresh_token.as_deref(), Some("rotated"));
        }
        let requests = egress.0.lock().unwrap();
        assert_eq!(requests.len(), 2);
        for (i, request) in requests.iter().enumerate() {
            assert_eq!(request.url, "https://as.example/token");
            let body = String::from_utf8(request.body.clone()).unwrap();
            assert!(body.contains("resource=https%3A%2F%2Fmcp.example%2Fresource"));
            assert!(body.contains(if i == 0 {
                "refresh_token=refresh"
            } else {
                "refresh_token=rotated"
            }));
        }
    }

    #[tokio::test]
    async fn legacy_file_requires_login_before_any_refresh_request() {
        use everruns_core::mcp::{McpAuthProvider, McpAuthRequest};
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("connections.toml");
        let mut legacy = encode_tokens(sample(Some(Utc::now() - chrono::Duration::seconds(1))));
        legacy.fields.remove(RESOURCE_FIELD);
        ConnectionStore::open(path.clone())
            .save(&connection_id("server"), legacy)
            .unwrap();
        let egress = Arc::new(RefreshEgress::default());
        let provider =
            CoordinatedOAuthProvider::new(Arc::new(ConnectionStore::open(path)), egress.clone());
        let error = provider
            .authorization(&McpAuthRequest {
                server_name: "server",
                auth_mode: everruns_core::McpServerAuthMode::OAuth,
                oauth_provider_id: None,
            })
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("sign in again"));
        assert!(egress.0.lock().unwrap().is_empty());
    }

    struct ProcessEgress(std::path::PathBuf);

    #[async_trait]
    impl EgressService for ProcessEgress {
        async fn send(&self, request: EgressRequest) -> EgressResult<EgressResponse> {
            use std::io::Write;
            let body = String::from_utf8(request.body).unwrap();
            assert!(body.contains("resource=https%3A%2F%2Fmcp.example%2Fresource"));
            assert!(body.contains("refresh_token=refresh"));
            let mut log = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(self.0.join("requests"))
                .unwrap();
            writeln!(log, "request").unwrap();
            while !self.0.join("release").exists() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            Ok(EgressResponse {
                status: if self.0.join("fail").exists() {
                    500
                } else {
                    200
                },
                headers: BTreeMap::new(),
                body: br#"{"access_token":"renewed","refresh_token":"rotated","expires_in":3600}"#
                    .to_vec(),
            })
        }
        async fn send_stream(&self, _: EgressRequest) -> EgressResult<EgressStreamResponse> {
            panic!("OAuth must not stream")
        }
    }

    #[tokio::test]
    async fn refresh_process_child() {
        let Some(dir) = std::env::var_os("YOLOP_TEST_REFRESH_DIR") else {
            return;
        };
        let dir = std::path::PathBuf::from(dir);
        let provider = CoordinatedOAuthProvider::new(
            Arc::new(ConnectionStore::open(dir.join("connections.toml"))),
            Arc::new(ProcessEgress(dir)),
        );
        if std::env::var_os("YOLOP_TEST_REFRESH_CONTENDED").is_some() {
            assert!(
                tokio::time::timeout(
                    std::time::Duration::from_millis(50),
                    provider.authorization(&McpAuthRequest {
                        server_name: "server",
                        auth_mode: everruns_core::McpServerAuthMode::OAuth,
                        oauth_provider_id: None,
                    })
                )
                .await
                .is_err()
            );
            std::fs::write(provider.store.path().with_file_name("contended"), "").unwrap();
        }
        let result = provider
            .authorization(&everruns_core::mcp::McpAuthRequest {
                server_name: "server",
                auth_mode: everruns_core::McpServerAuthMode::OAuth,
                oauth_provider_id: None,
            })
            .await;
        if std::env::var_os("YOLOP_TEST_REFRESH_ERROR").is_some() {
            assert!(result.is_err());
        } else {
            assert!(result.unwrap().is_some());
        }
    }

    struct TestChild(std::process::Child);

    impl TestChild {
        fn kill(&mut self) -> std::io::Result<()> {
            self.0.kill()
        }
        fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
            loop {
                if let Some(status) = self.0.try_wait()? {
                    return Ok(status);
                }
                if std::time::Instant::now() >= deadline {
                    self.0.kill()?;
                    self.0.wait()?;
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "test child timed out",
                    ));
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
    }

    impl Drop for TestChild {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    fn child(dir: &std::path::Path, error: bool) -> TestChild {
        spawn_child(dir, error, false)
    }

    fn spawn_child(dir: &std::path::Path, error: bool, contend: bool) -> TestChild {
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "auth::mcp_oauth::tests::refresh_process_child",
                "--nocapture",
            ])
            .env("YOLOP_TEST_REFRESH_DIR", dir);
        if error {
            command.env("YOLOP_TEST_REFRESH_ERROR", "1");
        }
        if contend {
            command.env("YOLOP_TEST_REFRESH_CONTENDED", "1");
        }
        TestChild(command.spawn().unwrap())
    }

    fn wait_for_request(dir: &std::path::Path) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !dir.join("requests").exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "child refresh did not start"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    fn seed(dir: &std::path::Path) -> ConnectionStore {
        let store = ConnectionStore::open(dir.join("connections.toml"));
        save_tokens(
            &store,
            "server",
            sample(Some(Utc::now() - chrono::Duration::seconds(1))),
        )
        .unwrap();
        store
    }

    #[test]
    fn separate_processes_share_one_rotation() {
        let dir = tempfile::tempdir().unwrap();
        let store = seed(dir.path());
        let mut first = child(dir.path(), false);
        wait_for_request(dir.path());
        let mut second = spawn_child(dir.path(), false, true);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !dir.path().join("contended").exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "second process did not contend"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        std::fs::write(dir.path().join("release"), "").unwrap();
        assert!(first.wait().unwrap().success());
        assert!(second.wait().unwrap().success());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("requests")).unwrap(),
            "request\n"
        );
        assert_eq!(
            load_tokens(&store, "server")
                .unwrap()
                .refresh_token
                .as_deref(),
            Some("rotated")
        );
    }

    #[test]
    fn logout_or_identical_new_login_wins_over_in_flight_process_refresh() {
        for login_again in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let store = seed(dir.path());
            let original = load_tokens(&store, "server").unwrap();
            let mut process = child(dir.path(), true);
            wait_for_request(dir.path());
            store.clear(&connection_id("server")).unwrap();
            if login_again {
                save_tokens(&store, "server", original).unwrap();
            }
            std::fs::write(dir.path().join("release"), "").unwrap();
            assert!(process.wait().unwrap().success());
            let current = load_tokens(&store, "server");
            if login_again {
                assert_eq!(current.unwrap().access_token, "access");
            } else {
                assert!(current.is_none());
            }
        }
    }

    #[test]
    fn failed_refresh_or_persistence_does_not_replace_credentials() {
        for failure in ["http", "parse", "write"] {
            let dir = tempfile::tempdir().unwrap();
            let store = seed(dir.path());
            let before = std::fs::read(store.path()).unwrap();
            let mut process = child(dir.path(), true);
            wait_for_request(dir.path());
            match failure {
                "parse" => std::fs::write(store.path(), "invalid = = toml").unwrap(),
                "write" => std::fs::create_dir(
                    dir.path()
                        .join(format!(".connections.toml.tmp.{}", process.0.id())),
                )
                .unwrap(),
                _ => std::fs::write(dir.path().join("fail"), "").unwrap(),
            }
            std::fs::write(dir.path().join("release"), "").unwrap();
            assert!(process.wait().unwrap().success());
            if failure == "parse" {
                assert_eq!(
                    std::fs::read_to_string(store.path()).unwrap(),
                    "invalid = = toml"
                );
            } else {
                assert_eq!(std::fs::read(store.path()).unwrap(), before);
            }
        }
    }

    #[test]
    fn killed_refresh_process_releases_lease_without_persisting() {
        let dir = tempfile::tempdir().unwrap();
        let store = seed(dir.path());
        let mut first = child(dir.path(), false);
        wait_for_request(dir.path());
        first.kill().unwrap();
        first.wait().unwrap();
        assert_eq!(
            load_tokens(&store, "server").unwrap().access_token,
            "access"
        );
        std::fs::write(dir.path().join("release"), "").unwrap();
        assert!(child(dir.path(), false).wait().unwrap().success());
        assert_eq!(
            load_tokens(&store, "server").unwrap().access_token,
            "renewed"
        );
    }

    #[tokio::test]
    async fn replacement_login_is_readable_while_old_refresh_holds_lease() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(seed(dir.path()));
        let provider =
            CoordinatedOAuthProvider::new(store.clone(), Arc::new(RefreshEgress::default()));
        let _lease = provider.refresh_lease("server").await.unwrap();
        save_tokens(
            &store,
            "server",
            sample(Some(Utc::now() + chrono::Duration::hours(1))),
        )
        .unwrap();
        let credential = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            provider.authorization(&McpAuthRequest {
                server_name: "server",
                auth_mode: everruns_core::McpServerAuthMode::OAuth,
                oauth_provider_id: None,
            }),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(credential.is_some());
    }

    #[tokio::test]
    async fn cancelling_persistence_wait_never_writes_later() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(seed(dir.path()));
        let before = std::fs::read(store.path()).unwrap();
        let egress = Arc::new(RefreshEgress::default());
        let provider = CoordinatedOAuthProvider::new(store.clone(), egress.clone());
        let write_lease = crate::connectors::private_lock_file(
            &store.path().with_extension("connections-write.lock"),
        )
        .unwrap();
        write_lease.lock().unwrap();
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(50),
            provider.authorization(&McpAuthRequest {
                server_name: "server",
                auth_mode: everruns_core::McpServerAuthMode::OAuth,
                oauth_provider_id: None,
            }),
        )
        .await;
        assert!(result.is_err());
        assert_eq!(egress.0.lock().unwrap().len(), 1);
        assert_eq!(std::fs::read(store.path()).unwrap(), before);
        drop(write_lease);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(std::fs::read(store.path()).unwrap(), before);
    }

    #[tokio::test]
    async fn waiting_for_lease_is_cancellable_and_malformed_storage_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(seed(dir.path()));
        let provider =
            CoordinatedOAuthProvider::new(store.clone(), Arc::new(RefreshEgress::default()));
        let lease = provider.refresh_lease("server").await.unwrap();
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(50),
                provider.refresh_lease("server")
            )
            .await
            .is_err()
        );
        drop(lease);
        let _lease = provider.refresh_lease("server").await.unwrap();
        std::fs::write(store.path(), "invalid = = toml").unwrap();
        assert!(store.get_checked(&connection_id("server")).is_err());
    }

    #[test]
    fn legacy_records_keep_missing_resource_for_reauthorization() {
        let mut connection = encode_tokens(sample(None));
        connection.fields.remove(RESOURCE_FIELD);
        let loaded = decode_tokens(connection).unwrap();
        assert_eq!(loaded.resource, None);
        assert_eq!(loaded.refresh_token.as_deref(), Some("refresh"));
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

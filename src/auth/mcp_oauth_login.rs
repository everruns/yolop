//! Interactive loopback host for Everruns' shared MCP OAuth client.

use crate::auth::mcp_oauth::McpOAuthTokenSet;
use anyhow::{Context, Result, anyhow};
use everruns_core::mcp::oauth::{LoginOptions, RegisteredClient};
use std::collections::HashMap;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const CALLBACK_PATH: &str = "/callback";
const CLIENT_METADATA_URL_ENV: &str = "YOLOP_MCP_CLIENT_METADATA_URL";

pub(crate) struct PreparedLogin {
    pub authorize_url: String,
    listener: TcpListener,
    oauth: everruns_core::mcp::oauth::PreparedLogin,
}

#[allow(dead_code)]
pub(crate) async fn login(
    mcp_url: &str,
    configured_client_id: Option<&str>,
    scope: Option<&str>,
) -> Result<McpOAuthTokenSet> {
    let prepared = prepare_login(mcp_url, configured_client_id, scope).await?;
    crate::auth::oauth_flow::open_browser(prepared.authorize_url.as_str())?;
    complete_login(prepared).await
}

pub(crate) async fn prepare_login(
    mcp_url: &str,
    configured_client_id: Option<&str>,
    scope: Option<&str>,
) -> Result<PreparedLogin> {
    // A distributor can publish its own metadata without teaching the client to
    // fetch arbitrary client documents. No default URL until one is hosted.
    let metadata_url = match std::env::var(CLIENT_METADATA_URL_ENV) {
        Ok(value) => Some(value),
        Err(std::env::VarError::NotPresent) => None,
        Err(error) => return Err(error).context(CLIENT_METADATA_URL_ENV),
    };
    let egress = crate::auth::mcp_oauth::oauth_egress();
    prepare_login_with_egress(
        egress.as_ref(),
        mcp_url,
        configured_client_id,
        scope,
        metadata_url.as_deref(),
    )
    .await
}

async fn prepare_login_with_egress(
    egress: &dyn everruns_core::EgressService,
    mcp_url: &str,
    configured_client_id: Option<&str>,
    scope: Option<&str>,
    metadata_url: Option<&str>,
) -> Result<PreparedLogin> {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .context("bind loopback OAuth callback")?;
    let port = listener
        .local_addr()
        .context("resolve callback port")?
        .port();
    let redirect_uri = format!("http://127.0.0.1:{port}{CALLBACK_PATH}");
    let configured_client = configured_client_id.map(|client_id| RegisteredClient {
        client_id: client_id.to_string(),
        client_secret: None,
    });
    let oauth = everruns_core::mcp::oauth::prepare_login_with_options(
        egress,
        mcp_url,
        &redirect_uri,
        LoginOptions {
            client_name: "yolop",
            client: configured_client,
            client_metadata_url: metadata_url,
            scope,
        },
    )
    .await?;
    Ok(PreparedLogin {
        authorize_url: oauth.authorization_url.clone(),
        listener,
        oauth,
    })
}

pub(crate) async fn complete_login(prepared: PreparedLogin) -> Result<McpOAuthTokenSet> {
    let server_url = prepared.oauth.resource.clone();
    // The browser callback stays open until the token exchange finishes, so
    // the page only claims success once the connection is actually usable.
    let (
        mut socket,
        Callback {
            code,
            state,
            issuer,
        },
    ) = wait_for_callback(prepared.listener, &prepared.oauth.state, &server_url).await?;
    let egress = crate::auth::mcp_oauth::oauth_egress();
    let tokens = everruns_core::mcp::oauth::complete_login(
        egress.as_ref(),
        &prepared.oauth,
        &code,
        &state,
        issuer.as_deref(),
    )
    .await;
    respond_after_exchange(&mut socket, &server_url, issuer.as_deref(), tokens).await
}

/// Writes the browser result page once the token exchange has settled: the
/// success page only when tokens exist, an error page otherwise. Split out
/// so both branches stay drivable without a network exchange.
async fn respond_after_exchange(
    socket: &mut TcpStream,
    server_url: &str,
    issuer: Option<&str>,
    tokens: Result<McpOAuthTokenSet>,
) -> Result<McpOAuthTokenSet> {
    match tokens {
        Ok(tokens) => {
            // The token response carries no user identity, so the page shows
            // what the exchange granted: scope and expiry.
            let expiry = tokens
                .expires_at
                .map(|at| at.format("%Y-%m-%d %H:%M UTC").to_string());
            let details = connection_details(
                server_url,
                issuer,
                tokens.scope.as_deref(),
                expiry.as_deref(),
            );
            let details: Vec<(&str, &str)> = details
                .iter()
                .map(|(label, value)| (label.as_str(), value.as_str()))
                .collect();
            write_response(
                socket,
                "200 OK",
                "MCP login complete. You can return to the client.",
                &details,
            )
            .await?;
            Ok(tokens)
        }
        Err(error) => {
            let details = connection_details(server_url, issuer, None, None);
            let details: Vec<(&str, &str)> = details
                .iter()
                .map(|(label, value)| (label.as_str(), value.as_str()))
                .collect();
            // Best effort: the CLI still reports the real error below.
            let _ = write_response(
                socket,
                "500 Login failed",
                "Login failed while requesting access. Check the client output and retry.",
                &details,
            )
            .await;
            Err(error)
        }
    }
}

/// Detail rows for the login result page: server identity plus what the token
/// exchange granted. Pure so the issuer/scope/expiry rules stay testable
/// without a network callback.
fn connection_details(
    server_url: &str,
    callback_issuer: Option<&str>,
    scope: Option<&str>,
    expiry: Option<&str>,
) -> Vec<(String, String)> {
    let mut details = Vec::new();
    if !server_url.is_empty() {
        details.push(("Server".to_string(), server_url.to_string()));
    }
    if let Some(issuer) = callback_issuer.filter(|issuer| !issuer.is_empty())
        && issuer != server_url
    {
        details.push(("Authorization server".to_string(), issuer.to_string()));
    }
    if let Some(scope) = scope.filter(|scope| !scope.is_empty()) {
        details.push(("Access scope".to_string(), scope.to_string()));
    }
    if let Some(expiry) = expiry.filter(|expiry| !expiry.is_empty()) {
        details.push(("Credentials expire".to_string(), expiry.to_string()));
    }
    details
}

struct Callback {
    code: String,
    state: String,
    issuer: Option<String>,
}

async fn wait_for_callback(
    listener: TcpListener,
    expected_state: &str,
    server_url: &str,
) -> Result<(TcpStream, Callback)> {
    let server_details: &[(&str, &str)] = if server_url.is_empty() {
        &[]
    } else {
        &[("Server", server_url)]
    };
    loop {
        let (mut socket, _) = listener.accept().await.context("accept OAuth callback")?;
        let request = read_request_line(&mut socket).await?;
        let path = request
            .split_whitespace()
            .nth(1)
            .ok_or_else(|| anyhow!("invalid OAuth callback request"))?;
        let parsed = reqwest::Url::parse(&format!("http://127.0.0.1{path}"))
            .context("parse OAuth callback URL")?;
        let params: HashMap<_, _> = parsed.query_pairs().into_owned().collect();

        if parsed.path() != CALLBACK_PATH {
            write_response(
                &mut socket,
                "404 Not Found",
                "Unexpected callback path.",
                server_details,
            )
            .await?;
            continue;
        }
        let state = params.get("state").cloned().unwrap_or_default();
        if state != expected_state {
            write_response(
                &mut socket,
                "400 Bad Request",
                "Login rejected: the state did not match.",
                server_details,
            )
            .await?;
            return Err(anyhow!("OAuth callback state mismatch"));
        }
        if let Some(error) = params.get("error") {
            let message = format!("Authorization failed: {error}.");
            write_response(&mut socket, "400 Bad Request", &message, server_details).await?;
            return Err(anyhow!("authorization server returned error: {error}"));
        }
        if let Some(code) = params.get("code") {
            // The success page waits until complete_login finishes the token
            // exchange, so the browser only sees success once tokens exist.
            return Ok((
                socket,
                Callback {
                    code: code.clone(),
                    state,
                    issuer: params.get("iss").cloned(),
                },
            ));
        }
        write_response(
            &mut socket,
            "400 Bad Request",
            "Callback had no authorization code.",
            server_details,
        )
        .await?;
        return Err(anyhow!("OAuth callback missing authorization code"));
    }
}

async fn read_request_line(socket: &mut TcpStream) -> Result<String> {
    let mut buffer = vec![0u8; 8192];
    let n = socket
        .read(&mut buffer)
        .await
        .context("read OAuth callback")?;
    let request = String::from_utf8_lossy(&buffer[..n]);
    Ok(request.lines().next().unwrap_or_default().to_string())
}

async fn write_response(
    socket: &mut TcpStream,
    status: &str,
    message: &str,
    details: &[(&str, &str)],
) -> Result<()> {
    let body =
        crate::auth::oauth_flow::callback_page_with_details(status, message, "MCP server", details);
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    socket
        .write_all(response.as_bytes())
        .await
        .context("write OAuth callback response")
}

#[cfg(test)]
pub(crate) mod test_support {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    pub(crate) struct MockOAuthServer {
        pub base: String,
    }

    impl MockOAuthServer {
        pub(crate) async fn start() -> Self {
            let listener = TcpListener::bind(("127.0.0.1", 0))
                .await
                .expect("bind mock");
            let port = listener.local_addr().expect("addr").port();
            let base = format!("http://127.0.0.1:{port}");
            let base_for_task = base.clone();
            tokio::spawn(async move {
                loop {
                    let Ok((mut socket, _)) = listener.accept().await else {
                        break;
                    };
                    let base = base_for_task.clone();
                    tokio::spawn(async move {
                        let mut buf = vec![0u8; 8192];
                        let Ok(n) = socket.read(&mut buf).await else {
                            return;
                        };
                        let request = String::from_utf8_lossy(&buf[..n]).to_string();
                        let first = request.lines().next().unwrap_or_default();
                        let mut it = first.split_whitespace();
                        let method = it.next().unwrap_or_default();
                        let path = it.next().unwrap_or_default();
                        let body = request
                            .split_once("\r\n\r\n")
                            .map(|(_, body)| body.to_string())
                            .unwrap_or_default();
                        let json = route(method, path, &body, &base);
                        let response = match json {
                            Some(body) => format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                                body.len()
                            ),
                            None => "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string(),
                        };
                        let _ = socket.write_all(response.as_bytes()).await;
                    });
                }
            });
            Self { base }
        }
    }

    fn route(method: &str, path: &str, body: &str, base: &str) -> Option<String> {
        match (method, path) {
            ("GET", "/.well-known/oauth-protected-resource") => Some(format!(
                r#"{{"resource":"{base}/mcp","authorization_servers":["{base}"]}}"#
            )),
            ("GET", "/.well-known/oauth-authorization-server") => Some(format!(
                r#"{{"issuer":"{base}","authorization_endpoint":"{base}/authorize","token_endpoint":"{base}/token","registration_endpoint":"{base}/register","scopes_supported":["read"]}}"#
            )),
            ("POST", "/register") => Some(r#"{"client_id":"dcr-client-1"}"#.to_string()),
            ("POST", "/token") => {
                if body.contains("grant_type=refresh_token") {
                    Some(r#"{"access_token":"access-2","refresh_token":"refresh-2","token_type":"Bearer","expires_in":3600}"#.to_string())
                } else {
                    Some(r#"{"access_token":"access-1","refresh_token":"refresh-1","token_type":"Bearer","expires_in":3600,"scope":"read"}"#.to_string())
                }
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::MockOAuthServer;
    use super::*;

    struct NegotiationEgress {
        cimd: bool,
        sent: std::sync::Mutex<Vec<everruns_core::EgressRequest>>,
    }

    #[async_trait::async_trait]
    impl everruns_core::EgressService for NegotiationEgress {
        async fn send(
            &self,
            request: everruns_core::EgressRequest,
        ) -> everruns_core::EgressResult<everruns_core::EgressResponse> {
            let body = match request.url.as_str() {
                "https://mcp.example/.well-known/oauth-protected-resource" => serde_json::json!({
                    "resource": "https://mcp.example/", "authorization_servers": ["https://auth.example"]
                }),
                "https://auth.example/.well-known/oauth-authorization-server" => {
                    serde_json::json!({
                        "issuer": "https://auth.example", "authorization_endpoint": "https://auth.example/authorize",
                        "token_endpoint": "https://auth.example/token", "registration_endpoint": "https://auth.example/register",
                        "client_id_metadata_document_supported": self.cimd
                    })
                }
                "https://auth.example/register" => serde_json::json!({"client_id": "registered"}),
                other => panic!("unexpected OAuth request: {other}"),
            };
            self.sent.lock().unwrap().push(request);
            Ok(everruns_core::EgressResponse {
                status: 200,
                headers: Default::default(),
                body: serde_json::to_vec(&body).unwrap(),
            })
        }

        async fn send_stream(
            &self,
            _: everruns_core::EgressRequest,
        ) -> everruns_core::EgressResult<everruns_core::EgressStreamResponse> {
            panic!("OAuth negotiation does not stream")
        }
    }

    #[tokio::test]
    async fn metadata_negotiation_keeps_the_native_callback_and_configured_client() {
        const METADATA_URL: &str = "https://client.example/yolop.json";
        for (support, configured, metadata_url, expected) in [
            (true, None, Some(METADATA_URL), METADATA_URL),
            (false, None, Some(METADATA_URL), "registered"),
            (true, None, None, "registered"),
            (true, Some("configured"), Some(METADATA_URL), "configured"),
        ] {
            let egress = NegotiationEgress {
                cimd: support,
                sent: Default::default(),
            };
            let prepared = prepare_login_with_egress(
                &egress,
                "https://mcp.example/",
                configured,
                Some("read"),
                metadata_url,
            )
            .await
            .unwrap();
            let authorize = reqwest::Url::parse(&prepared.authorize_url).unwrap();
            let query: HashMap<_, _> = authorize.query_pairs().into_owned().collect();
            assert_eq!(query["client_id"], expected);
            assert_eq!(query["code_challenge_method"], "S256");
            assert_eq!(query["scope"], "read");
            assert_eq!(
                query["redirect_uri"],
                format!(
                    "http://127.0.0.1:{}/callback",
                    prepared.listener.local_addr().unwrap().port()
                )
            );
            assert_eq!(prepared.oauth.issuer, "https://auth.example");
            let sent = egress.sent.lock().unwrap();
            assert_eq!(
                sent.iter().filter(|r| r.method == "POST").count(),
                usize::from(expected == "registered")
            );
            assert!(!sent.iter().any(|r| r.url == METADATA_URL));
        }
    }

    #[tokio::test]
    async fn callback_response_serves_branded_page() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            write_response(
                &mut socket,
                "200 OK",
                "MCP login complete. You can return to the client.",
                &[("Server", "https://mcp.example.com/mcp")],
            )
            .await
            .unwrap();
        });

        let mut browser = TcpStream::connect(address).await.unwrap();
        server.await.unwrap();
        let mut response = String::new();
        browser.read_to_string(&mut response).await.unwrap();
        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(response.contains("MCP server is connected."));
        assert!(response.contains("https://mcp.example.com/mcp"));
    }

    #[tokio::test]
    async fn success_page_lists_granted_scope_and_expiry() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let tokens = McpOAuthTokenSet {
                access_token: "access-1".to_string(),
                refresh_token: None,
                token_type: "Bearer".to_string(),
                expires_at: Some(
                    chrono::DateTime::parse_from_rfc3339("2026-10-16T12:00:00Z")
                        .unwrap()
                        .with_timezone(&chrono::Utc),
                ),
                scope: Some("read write".to_string()),
                token_endpoint: None,
                client_id: Some("client-1".to_string()),
                client_secret: None,
            };
            let stored = respond_after_exchange(
                &mut socket,
                "https://mcp.example.com/mcp",
                Some("https://auth.example.com"),
                Ok(tokens),
            )
            .await;
            assert!(stored.is_ok());
        });

        let mut browser = TcpStream::connect(address).await.unwrap();
        server.await.unwrap();
        let mut response = String::new();
        browser.read_to_string(&mut response).await.unwrap();
        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(response.contains("https://mcp.example.com/mcp"));
        assert!(response.contains("https://auth.example.com"));
        assert!(response.contains("read write"));
        assert!(response.contains("2026-10-16 12:00 UTC"));
    }

    #[tokio::test]
    async fn exchange_failure_serves_error_page() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let stored = respond_after_exchange(
                &mut socket,
                "https://mcp.example.com/mcp",
                None,
                Err(anyhow::anyhow!("token exchange failed")),
            )
            .await;
            assert!(stored.is_err());
        });

        let mut browser = TcpStream::connect(address).await.unwrap();
        server.await.unwrap();
        let mut response = String::new();
        browser.read_to_string(&mut response).await.unwrap();
        assert!(response.starts_with("HTTP/1.1 500"));
        assert!(response.contains("Almost there."));
        assert!(response.contains("No changes were made."));
        assert!(response.contains("https://mcp.example.com/mcp"));
    }

    #[test]
    fn connection_details_shows_issuer_only_when_it_differs_from_the_server() {
        let server = "https://mcp.example.com/mcp";
        let details =
            connection_details(server, Some("https://auth.example.com"), Some("read"), None);
        let rows: Vec<(&str, &str)> = details
            .iter()
            .map(|(label, value)| (label.as_str(), value.as_str()))
            .collect();
        assert!(rows.contains(&("Server", server)));
        assert!(rows.contains(&("Authorization server", "https://auth.example.com")));
        assert!(rows.contains(&("Access scope", "read")));

        let same_issuer = connection_details(server, Some(server), Some("read"), None);
        assert!(
            same_issuer
                .iter()
                .all(|(label, _)| label != "Authorization server"),
            "issuer matching the server adds no row"
        );

        let no_issuer = connection_details(server, None, None, Some("2026-10-16 12:00 UTC"));
        assert!(
            no_issuer
                .iter()
                .all(|(label, _)| label != "Authorization server"),
            "absent issuer adds no row"
        );
        assert!(
            no_issuer.contains(&(
                "Credentials expire".to_string(),
                "2026-10-16 12:00 UTC".to_string()
            )),
            "expiry is shown when known"
        );
    }

    // The MCP host binds the OAuth resource to the MCP server origin
    // and requires that origin to be HTTPS, so an authorization server cannot
    // mint a token for an endpoint it does not control. The loopback *redirect*
    // below is still plain HTTP — that is the native-app pattern and unchanged.
    //
    // The consequence for this file: `prepare_login` can no longer be driven
    // end-to-end against a local plain-HTTP mock, so the discovery →
    // registration → resource-binding assertions moved upstream, where
    // core's MCP tests exercise them over https through a fake egress.
    // What stays here is the boundary yolop owns: a plain-HTTP MCP endpoint is
    // refused before any token is requested.
    #[tokio::test]
    async fn plain_http_mcp_endpoints_are_refused_before_requesting_a_token() {
        let server = MockOAuthServer::start().await;
        // `PreparedLogin` holds a live listener and is not `Debug`, so unwrap
        // the error side by hand rather than via `expect_err`.
        let Err(error) = prepare_login(&format!("{}/mcp", server.base), None, Some("read")).await
        else {
            panic!("plain-HTTP MCP endpoint must not reach a token request");
        };
        assert!(
            error.to_string().contains("HTTPS"),
            "expected an HTTPS requirement error, got: {error}"
        );
    }
}

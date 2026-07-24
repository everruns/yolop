//! Interactive OAuth login for remote (HTTP) MCP servers.
//!
//! Implements the MCP authorization handshake against a server's own
//! authorization server, discovered at runtime rather than hand-configured:
//!
//! 1. **Protected-resource metadata** (RFC 9728) at the server origin points to
//!    its authorization server(s); we fall back to treating the server origin
//!    as the issuer when the document is absent.
//! 2. **Authorization-server metadata** (RFC 8414 / OpenID discovery) yields the
//!    `authorization`/`token`/`registration` endpoints.
//! 3. **Dynamic client registration** (RFC 7591) mints a public client when the
//!    server advertises a registration endpoint and no `client_id` is
//!    configured.
//! 4. **Authorization code + PKCE** (RFC 7636) runs through the browser with a
//!    loopback redirect (RFC 8252), and the code is exchanged for tokens.
//!
//! The resulting [`McpOAuthTokenSet`] carries the token endpoint and client id
//! so [`refresh`] (and the runtime auth provider) can renew the access token
//! without re-discovering anything.

use crate::auth::mcp_oauth::McpOAuthTokenSet;
use anyhow::{Context, Result, anyhow};
use chrono::{Duration, Utc};
use reqwest::Url;
use serde::Deserialize;
use std::collections::HashMap;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const CALLBACK_PATH: &str = "/callback";
const CLIENT_NAME: &str = "yolop";
/// Refresh a little before the hard expiry so a token is renewed before the
/// server would reject it.
const REFRESH_SKEW: Duration = Duration::seconds(60);

/// Authorization-server endpoints resolved via discovery.
#[derive(Debug, Clone)]
pub(crate) struct OAuthEndpoints {
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub registration_endpoint: Option<String>,
    pub scopes_supported: Option<Vec<String>>,
}

/// The OAuth client used for a login — either configured or dynamically
/// registered.
#[derive(Debug, Clone)]
struct OAuthClient {
    client_id: String,
    client_secret: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ProtectedResourceMetadata {
    #[serde(default)]
    authorization_servers: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct AuthServerMetadata {
    authorization_endpoint: String,
    token_endpoint: String,
    #[serde(default)]
    registration_endpoint: Option<String>,
    #[serde(default)]
    scopes_supported: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct RegistrationResponse {
    client_id: String,
    #[serde(default)]
    client_secret: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    token_type: Option<String>,
    #[serde(default)]
    expires_in: Option<i64>,
    #[serde(default)]
    scope: Option<String>,
}

fn http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .context("build OAuth HTTP client")
}

/// Guard against pointing the browser / token calls at a plaintext or
/// non-HTTP(S) endpoint. Loopback stays allowed over HTTP so a locally hosted
/// authorization server (and the test suite) works without TLS.
fn require_secure(url: &str, what: &str) -> Result<Url> {
    let parsed = Url::parse(url).with_context(|| format!("parse {what} URL: {url}"))?;
    let is_loopback = matches!(parsed.host_str(), Some("localhost" | "127.0.0.1" | "::1"));
    match parsed.scheme() {
        "https" => Ok(parsed),
        "http" if is_loopback => Ok(parsed),
        other => Err(anyhow!(
            "{what} must use https (got {other} for a non-loopback host): {url}"
        )),
    }
}

/// Prepared interactive login: discovery + DCR + PKCE are done, the authorize
/// URL is ready to show the user, and [`complete_login`] waits on the loopback
/// callback then exchanges the code. Splitting prepare from complete lets the
/// TUI print the URL (and keep the event loop alive) in both fullscreen and
/// inline modes before blocking on the browser.
pub(crate) struct PreparedLogin {
    pub authorize_url: String,
    listener: TcpListener,
    state: String,
    verifier: String,
    redirect_uri: String,
    endpoints: OAuthEndpoints,
    oauth_client: OAuthClient,
    scope: Option<String>,
}

/// Run the full interactive login for `mcp_url`, returning tokens ready to
/// persist. Convenience wrapper around [`prepare_login`] + open browser +
/// [`complete_login`] for callers that do not need to surface the authorize
/// URL first.
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

/// Discover, register, and build the authorize URL without opening a browser or
/// waiting on the callback. The caller should show [`PreparedLogin::authorize_url`]
/// then call [`complete_login`].
pub(crate) async fn prepare_login(
    mcp_url: &str,
    configured_client_id: Option<&str>,
    scope: Option<&str>,
) -> Result<PreparedLogin> {
    let client = http_client()?;
    let endpoints = discover_endpoints(&client, mcp_url).await?;

    // Bind the loopback callback first so its port is part of the redirect_uri
    // we register and send to the authorization endpoint.
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .context("bind loopback OAuth callback")?;
    let port = listener
        .local_addr()
        .context("resolve callback port")?
        .port();
    let redirect_uri = format!("http://127.0.0.1:{port}{CALLBACK_PATH}");

    let oauth_client = match configured_client_id {
        Some(client_id) => OAuthClient {
            client_id: client_id.to_string(),
            client_secret: None,
        },
        None => register_client(&client, &endpoints, &redirect_uri).await?,
    };

    let verifier = crate::auth::oauth_flow::random_pkce_verifier();
    let challenge = crate::auth::oauth_flow::pkce_challenge(&verifier);
    let state = crate::auth::oauth_flow::random_token(32);
    let scope = scope
        .map(str::to_string)
        .or_else(|| endpoints.scopes_supported.as_ref().map(|s| s.join(" ")));

    let auth_url = authorize_url(
        &endpoints,
        &oauth_client.client_id,
        &redirect_uri,
        &challenge,
        &state,
        scope.as_deref(),
    )?;
    Ok(PreparedLogin {
        authorize_url: auth_url.to_string(),
        listener,
        state,
        verifier,
        redirect_uri,
        endpoints,
        oauth_client,
        scope,
    })
}

/// Wait for the authorize redirect on the prepared loopback listener and
/// exchange the code for tokens.
pub(crate) async fn complete_login(prepared: PreparedLogin) -> Result<McpOAuthTokenSet> {
    let PreparedLogin {
        listener,
        state,
        verifier,
        redirect_uri,
        endpoints,
        oauth_client,
        scope,
        ..
    } = prepared;
    let client = http_client()?;
    let code = wait_for_callback(listener, &state).await?;
    exchange_code(
        &client,
        &endpoints,
        &oauth_client,
        &code,
        &verifier,
        &redirect_uri,
        scope.as_deref(),
    )
    .await
}

/// Discover the authorization-server endpoints for an MCP server URL.
pub(crate) async fn discover_endpoints(
    client: &reqwest::Client,
    mcp_url: &str,
) -> Result<OAuthEndpoints> {
    let server = require_secure(mcp_url, "MCP server")?;
    let origin = origin_of(&server);

    // RFC 9728: protected-resource metadata advertises the issuer(s).
    let issuer = match fetch_json::<ProtectedResourceMetadata>(
        client,
        &join_well_known(&origin, "oauth-protected-resource"),
    )
    .await?
    {
        Some(prm) => prm
            .authorization_servers
            .into_iter()
            .next()
            .unwrap_or_else(|| origin.clone()),
        None => origin.clone(),
    };
    let issuer = require_secure(&issuer, "authorization server")?;
    let issuer_origin = origin_of(&issuer);

    // RFC 8414 first, then OpenID Connect discovery as a fallback.
    for suffix in ["oauth-authorization-server", "openid-configuration"] {
        if let Some(meta) =
            fetch_json::<AuthServerMetadata>(client, &join_well_known(&issuer_origin, suffix))
                .await?
        {
            // Validate the endpoints before handing them to the browser / token
            // exchange so discovery can't downgrade us to plaintext.
            require_secure(&meta.authorization_endpoint, "authorization endpoint")?;
            require_secure(&meta.token_endpoint, "token endpoint")?;
            if let Some(reg) = &meta.registration_endpoint {
                require_secure(reg, "registration endpoint")?;
            }
            return Ok(OAuthEndpoints {
                authorization_endpoint: meta.authorization_endpoint,
                token_endpoint: meta.token_endpoint,
                registration_endpoint: meta.registration_endpoint,
                scopes_supported: meta.scopes_supported,
            });
        }
    }

    Err(anyhow!(
        "no OAuth authorization-server metadata found for {issuer_origin} \
         (looked under /.well-known/oauth-authorization-server and /openid-configuration)"
    ))
}

/// Register a public client via RFC 7591 dynamic client registration.
async fn register_client(
    client: &reqwest::Client,
    endpoints: &OAuthEndpoints,
    redirect_uri: &str,
) -> Result<OAuthClient> {
    let registration_endpoint = endpoints.registration_endpoint.as_deref().ok_or_else(|| {
        anyhow!(
            "server does not support dynamic client registration; \
             configure `oauth_client_id` for this MCP server"
        )
    })?;
    let body = serde_json::json!({
        "client_name": CLIENT_NAME,
        "redirect_uris": [redirect_uri],
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": "none",
    });
    let response = client
        .post(registration_endpoint)
        .json(&body)
        .send()
        .await
        .context("register OAuth client")?;
    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        return Err(anyhow!(
            "dynamic client registration failed ({status}): {text}"
        ));
    }
    let parsed: RegistrationResponse = response
        .json()
        .await
        .context("parse client registration response")?;
    Ok(OAuthClient {
        client_id: parsed.client_id,
        client_secret: parsed.client_secret,
    })
}

fn authorize_url(
    endpoints: &OAuthEndpoints,
    client_id: &str,
    redirect_uri: &str,
    challenge: &str,
    state: &str,
    scope: Option<&str>,
) -> Result<Url> {
    let mut url =
        Url::parse(&endpoints.authorization_endpoint).context("parse authorization endpoint")?;
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", client_id)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("code_challenge", challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", state);
    if let Some(scope) = scope {
        url.query_pairs_mut().append_pair("scope", scope);
    }
    Ok(url)
}

/// Exchange an authorization code for tokens, folding in the refresh metadata.
async fn exchange_code(
    client: &reqwest::Client,
    endpoints: &OAuthEndpoints,
    oauth_client: &OAuthClient,
    code: &str,
    verifier: &str,
    redirect_uri: &str,
    scope: Option<&str>,
) -> Result<McpOAuthTokenSet> {
    let mut form = vec![
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", redirect_uri),
        ("client_id", oauth_client.client_id.as_str()),
        ("code_verifier", verifier),
    ];
    if let Some(secret) = &oauth_client.client_secret {
        form.push(("client_secret", secret.as_str()));
    }
    let token: TokenResponse = post_token(client, &endpoints.token_endpoint, &form).await?;
    Ok(token_set(
        token,
        &endpoints.token_endpoint,
        oauth_client,
        None,
        scope,
    ))
}

/// Renew an access token from a stored token set. Preserves the client id,
/// token endpoint, and (when the server omits a rotated one) the refresh token.
pub(crate) async fn refresh(tokens: &McpOAuthTokenSet) -> Result<McpOAuthTokenSet> {
    let token_endpoint = tokens
        .token_endpoint
        .as_deref()
        .ok_or_else(|| anyhow!("stored token has no token endpoint to refresh against"))?;
    let refresh_token = tokens
        .refresh_token
        .as_deref()
        .ok_or_else(|| anyhow!("stored token has no refresh token"))?;
    let client_id = tokens
        .client_id
        .as_deref()
        .ok_or_else(|| anyhow!("stored token has no client id to refresh with"))?;
    require_secure(token_endpoint, "token endpoint")?;

    let client = http_client()?;
    let mut form = vec![
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", client_id),
    ];
    if let Some(secret) = tokens.client_secret.as_deref() {
        form.push(("client_secret", secret));
    }
    let token: TokenResponse = post_token(&client, token_endpoint, &form).await?;
    let oauth_client = OAuthClient {
        client_id: client_id.to_string(),
        client_secret: tokens.client_secret.clone(),
    };
    Ok(token_set(
        token,
        token_endpoint,
        &oauth_client,
        tokens.refresh_token.clone(),
        tokens.scope.as_deref(),
    ))
}

/// Whether a token set should be refreshed before use (expired or within the
/// refresh skew of expiry). A set without a refresh token is never eligible.
pub(crate) fn needs_refresh(tokens: &McpOAuthTokenSet) -> bool {
    tokens.refresh_token.is_some() && tokens.is_expired(Utc::now() + REFRESH_SKEW)
}

async fn post_token(
    client: &reqwest::Client,
    token_endpoint: &str,
    form: &[(&str, &str)],
) -> Result<TokenResponse> {
    let response = client
        .post(token_endpoint)
        .form(form)
        .send()
        .await
        .context("call token endpoint")?;
    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        return Err(anyhow!("token endpoint returned {status}: {text}"));
    }
    response.json().await.context("parse token response")
}

fn token_set(
    token: TokenResponse,
    token_endpoint: &str,
    oauth_client: &OAuthClient,
    fallback_refresh: Option<String>,
    fallback_scope: Option<&str>,
) -> McpOAuthTokenSet {
    let expires_at = token
        .expires_in
        .filter(|seconds| *seconds > 0)
        .map(|seconds| Utc::now() + Duration::seconds(seconds));
    McpOAuthTokenSet {
        access_token: token.access_token,
        refresh_token: token.refresh_token.or(fallback_refresh),
        token_type: token.token_type.unwrap_or_else(|| "Bearer".to_string()),
        expires_at,
        scope: token.scope.or_else(|| fallback_scope.map(str::to_string)),
        token_endpoint: Some(token_endpoint.to_string()),
        client_id: Some(oauth_client.client_id.clone()),
        client_secret: oauth_client.client_secret.clone(),
    }
}

/// `scheme://host[:port]` of a URL (no path), used as the well-known base.
fn origin_of(url: &Url) -> String {
    let scheme = url.scheme();
    let host = url.host_str().unwrap_or_default();
    match url.port() {
        Some(port) => format!("{scheme}://{host}:{port}"),
        None => format!("{scheme}://{host}"),
    }
}

fn join_well_known(origin: &str, suffix: &str) -> String {
    format!("{origin}/.well-known/{suffix}")
}

/// GET a JSON document, returning `None` on 404 (metadata simply absent) and an
/// error on transport failures or malformed JSON.
async fn fetch_json<T: for<'de> Deserialize<'de>>(
    client: &reqwest::Client,
    url: &str,
) -> Result<Option<T>> {
    let response = client
        .get(url)
        .header("Accept", "application/json")
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !response.status().is_success() {
        return Err(anyhow!("GET {url} returned {}", response.status()));
    }
    let parsed = response
        .json::<T>()
        .await
        .with_context(|| format!("parse JSON from {url}"))?;
    Ok(Some(parsed))
}

/// Serve the loopback redirect until the authorization server redirects back
/// with a matching `state`, returning the authorization code. Rejects
/// mismatched state and surfaces `error` responses.
async fn wait_for_callback(listener: TcpListener, expected_state: &str) -> Result<String> {
    loop {
        let (mut socket, _) = listener.accept().await.context("accept OAuth callback")?;
        let request = read_request_line(&mut socket).await?;
        let path = request
            .split_whitespace()
            .nth(1)
            .ok_or_else(|| anyhow!("invalid OAuth callback request"))?;
        let parsed =
            Url::parse(&format!("http://127.0.0.1{path}")).context("parse OAuth callback URL")?;
        let params: HashMap<_, _> = parsed.query_pairs().into_owned().collect();

        if parsed.path() != CALLBACK_PATH {
            write_response(&mut socket, "404 Not Found", "Unexpected callback path.").await?;
            continue;
        }
        if params.get("state").map(String::as_str) != Some(expected_state) {
            write_response(
                &mut socket,
                "400 Bad Request",
                "Login rejected: the state did not match.",
            )
            .await?;
            return Err(anyhow!("OAuth callback state mismatch"));
        }
        if let Some(error) = params.get("error") {
            let message = format!("Authorization failed: {error}.");
            write_response(&mut socket, "400 Bad Request", &message).await?;
            return Err(anyhow!("authorization server returned error: {error}"));
        }
        if let Some(code) = params.get("code") {
            write_response(
                &mut socket,
                "200 OK",
                "MCP login complete. You can return to the terminal.",
            )
            .await?;
            return Ok(code.clone());
        }
        write_response(
            &mut socket,
            "400 Bad Request",
            "Callback had no authorization code.",
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

async fn write_response(socket: &mut TcpStream, status: &str, message: &str) -> Result<()> {
    let body = crate::auth::oauth_flow::callback_page(status, message, "MCP server");
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    socket
        .write_all(response.as_bytes())
        .await
        .context("write OAuth callback response")
}

/// A hand-rolled loopback OAuth authorization server for tests: serves the
/// discovery documents, dynamic client registration, and token endpoints so the
/// non-browser parts of the login flow (and the runtime auth provider) can be
/// exercised end-to-end without a real provider. Shared with `runtime`'s
/// provider tests.
#[cfg(test)]
pub(crate) mod test_support {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    pub(crate) struct MockOAuthServer {
        pub base: String,
    }

    impl MockOAuthServer {
        /// Start the server on an ephemeral loopback port and spawn its accept
        /// loop. The task runs until the process exits (fine for tests).
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
                            .map(|(_, b)| b.to_string())
                            .unwrap_or_default();
                        let json = route(method, path, &body, &base);
                        let response = match json {
                            Some(body) => format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                                body.len()
                            ),
                            None => {
                                "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                                    .to_string()
                            }
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
            ("GET", "/.well-known/oauth-protected-resource") => {
                Some(format!(r#"{{"authorization_servers":["{base}"]}}"#))
            }
            ("GET", "/.well-known/oauth-authorization-server") => Some(format!(
                r#"{{"authorization_endpoint":"{base}/authorize","token_endpoint":"{base}/token","registration_endpoint":"{base}/register","scopes_supported":["read"]}}"#
            )),
            ("POST", "/register") => Some(r#"{"client_id":"dcr-client-1"}"#.to_string()),
            ("POST", "/token") => {
                if body.contains("grant_type=refresh_token") {
                    Some(
                        r#"{"access_token":"access-2","refresh_token":"refresh-2","token_type":"Bearer","expires_in":3600}"#
                            .to_string(),
                    )
                } else {
                    Some(
                        r#"{"access_token":"access-1","refresh_token":"refresh-1","token_type":"Bearer","expires_in":3600,"scope":"read"}"#
                            .to_string(),
                    )
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

    #[tokio::test]
    async fn callback_response_serves_branded_page() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            write_response(
                &mut socket,
                "200 OK",
                "MCP login complete. You can return to the terminal.",
            )
            .await
            .unwrap();
        });

        let mut browser = TcpStream::connect(address).await.unwrap();
        server.await.unwrap();
        let mut response = String::new();
        browser.read_to_string(&mut response).await.unwrap();

        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(response.contains("Content-Type: text/html; charset=utf-8"));
        assert!(response.contains("<svg"));
        assert!(response.contains("MCP server is connected."));
        assert!(response.contains("Your terminal is already warming up the keyboard."));
    }

    #[tokio::test]
    async fn discovers_endpoints_via_protected_resource_metadata() {
        let server = MockOAuthServer::start().await;
        let client = http_client().unwrap();
        let endpoints = discover_endpoints(&client, &format!("{}/mcp", server.base))
            .await
            .expect("discover");
        assert_eq!(
            endpoints.authorization_endpoint,
            format!("{}/authorize", server.base)
        );
        assert_eq!(endpoints.token_endpoint, format!("{}/token", server.base));
        assert_eq!(
            endpoints.registration_endpoint.as_deref(),
            Some(format!("{}/register", server.base).as_str())
        );
        assert_eq!(
            endpoints.scopes_supported.as_deref(),
            Some(["read".to_string()].as_slice())
        );
    }

    #[tokio::test]
    async fn full_flow_minus_browser_registers_and_exchanges() {
        let server = MockOAuthServer::start().await;
        let client = http_client().unwrap();
        let endpoints = discover_endpoints(&client, &format!("{}/mcp", server.base))
            .await
            .expect("discover");
        let oauth_client = register_client(&client, &endpoints, "http://127.0.0.1:9/callback")
            .await
            .expect("register");
        assert_eq!(oauth_client.client_id, "dcr-client-1");

        let tokens = exchange_code(
            &client,
            &endpoints,
            &oauth_client,
            "auth-code",
            "verifier",
            "http://127.0.0.1:9/callback",
            Some("read"),
        )
        .await
        .expect("exchange");
        assert_eq!(tokens.access_token, "access-1");
        assert_eq!(tokens.refresh_token.as_deref(), Some("refresh-1"));
        assert_eq!(
            tokens.token_endpoint.as_deref(),
            Some(format!("{}/token", server.base).as_str())
        );
        assert_eq!(tokens.client_id.as_deref(), Some("dcr-client-1"));
        assert!(tokens.expires_at.is_some(), "expires_in yields an expiry");
    }

    #[tokio::test]
    async fn refresh_mints_new_access_token_and_preserves_metadata() {
        let server = MockOAuthServer::start().await;
        let tokens = McpOAuthTokenSet {
            access_token: "access-1".to_string(),
            refresh_token: Some("refresh-1".to_string()),
            token_type: "Bearer".to_string(),
            expires_at: Some(Utc::now() - Duration::seconds(1)),
            scope: Some("read".to_string()),
            token_endpoint: Some(format!("{}/token", server.base)),
            client_id: Some("dcr-client-1".to_string()),
            client_secret: None,
        };
        let refreshed = refresh(&tokens).await.expect("refresh");
        assert_eq!(refreshed.access_token, "access-2");
        assert_eq!(refreshed.refresh_token.as_deref(), Some("refresh-2"));
        assert_eq!(refreshed.client_id.as_deref(), Some("dcr-client-1"));
        assert_eq!(
            refreshed.token_endpoint.as_deref(),
            Some(format!("{}/token", server.base).as_str())
        );
    }

    #[test]
    fn require_secure_allows_https_and_loopback_http() {
        assert!(require_secure("https://mcp.example.com/mcp", "s").is_ok());
        assert!(require_secure("http://127.0.0.1:8080/mcp", "s").is_ok());
        assert!(require_secure("http://localhost/mcp", "s").is_ok());
        assert!(require_secure("http://mcp.example.com/mcp", "s").is_err());
        assert!(require_secure("ftp://mcp.example.com", "s").is_err());
    }

    #[test]
    fn origin_strips_path_and_keeps_port() {
        let url = Url::parse("https://as.example.com:8443/tenant/authorize").unwrap();
        assert_eq!(origin_of(&url), "https://as.example.com:8443");
        let url = Url::parse("https://as.example.com/authorize").unwrap();
        assert_eq!(origin_of(&url), "https://as.example.com");
    }

    #[test]
    fn authorize_url_carries_pkce_and_state() {
        let endpoints = OAuthEndpoints {
            authorization_endpoint: "https://as.example.com/authorize".to_string(),
            token_endpoint: "https://as.example.com/token".to_string(),
            registration_endpoint: None,
            scopes_supported: None,
        };
        let url = authorize_url(
            &endpoints,
            "client-1",
            "http://127.0.0.1:9999/callback",
            "challenge-xyz",
            "state-abc",
            Some("read write"),
        )
        .unwrap();
        let pairs: HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(pairs.get("response_type").map(String::as_str), Some("code"));
        assert_eq!(pairs.get("client_id").map(String::as_str), Some("client-1"));
        assert_eq!(
            pairs.get("code_challenge_method").map(String::as_str),
            Some("S256")
        );
        assert_eq!(
            pairs.get("code_challenge").map(String::as_str),
            Some("challenge-xyz")
        );
        assert_eq!(pairs.get("state").map(String::as_str), Some("state-abc"));
        assert_eq!(pairs.get("scope").map(String::as_str), Some("read write"));
    }

    #[test]
    fn needs_refresh_only_when_expiring_with_refresh_token() {
        let mut tokens = McpOAuthTokenSet {
            access_token: "a".to_string(),
            refresh_token: Some("r".to_string()),
            token_type: "Bearer".to_string(),
            expires_at: Some(Utc::now() + Duration::seconds(300)),
            scope: None,
            token_endpoint: Some("https://as.example/token".to_string()),
            client_id: Some("c".to_string()),
            client_secret: None,
        };
        assert!(!needs_refresh(&tokens), "fresh token is not refreshed");
        tokens.expires_at = Some(Utc::now() + Duration::seconds(10));
        assert!(needs_refresh(&tokens), "near-expiry token is refreshed");
        tokens.refresh_token = None;
        assert!(
            !needs_refresh(&tokens),
            "no refresh token -> cannot refresh"
        );
    }
}

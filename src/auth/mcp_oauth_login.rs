//! Interactive loopback host for Everruns' shared MCP OAuth client.

use crate::auth::mcp_oauth::McpOAuthTokenSet;
use anyhow::{Context, Result, anyhow};
use everruns_mcp::oauth::RegisteredClient;
use std::collections::HashMap;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const CALLBACK_PATH: &str = "/callback";

pub(crate) struct PreparedLogin {
    pub authorize_url: String,
    listener: TcpListener,
    oauth: everruns_mcp::oauth::PreparedLogin,
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
    let egress = crate::auth::mcp_oauth::oauth_egress();
    let oauth = everruns_mcp::oauth::prepare_login(
        egress.as_ref(),
        mcp_url,
        &redirect_uri,
        "yolop",
        configured_client,
        scope,
    )
    .await?;
    Ok(PreparedLogin {
        authorize_url: oauth.authorization_url.clone(),
        listener,
        oauth,
    })
}

pub(crate) async fn complete_login(prepared: PreparedLogin) -> Result<McpOAuthTokenSet> {
    let Callback {
        code,
        state,
        issuer,
    } = wait_for_callback(prepared.listener, &prepared.oauth.state).await?;
    let egress = crate::auth::mcp_oauth::oauth_egress();
    everruns_mcp::oauth::complete_login(
        egress.as_ref(),
        &prepared.oauth,
        &code,
        &state,
        issuer.as_deref(),
    )
    .await
}

struct Callback {
    code: String,
    state: String,
    issuer: Option<String>,
}

async fn wait_for_callback(listener: TcpListener, expected_state: &str) -> Result<Callback> {
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
            write_response(&mut socket, "404 Not Found", "Unexpected callback path.").await?;
            continue;
        }
        let state = params.get("state").cloned().unwrap_or_default();
        if state != expected_state {
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
            return Ok(Callback {
                code: code.clone(),
                state,
                issuer: params.get("iss").cloned(),
            });
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
        assert!(response.contains("MCP server is connected."));
    }

    // everruns-mcp 0.17.24 binds the OAuth resource to the MCP server origin
    // and requires that origin to be HTTPS, so an authorization server cannot
    // mint a token for an endpoint it does not control. The loopback *redirect*
    // below is still plain HTTP — that is the native-app pattern and unchanged.
    //
    // The consequence for this file: `prepare_login` can no longer be driven
    // end-to-end against a local plain-HTTP mock, so the discovery →
    // registration → resource-binding assertions moved upstream, where
    // everruns-mcp's own tests exercise them over https through a fake egress.
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

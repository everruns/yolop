//! OpenRouter PKCE login.
//!
//! OpenRouter's OAuth is not a refreshable access-token grant: there is no
//! client id or secret. The user authorizes at `openrouter.ai/auth`, the
//! loopback callback receives a short-lived code, and `POST /api/v1/auth/keys`
//! exchanges that code for a user-controlled API key. Yolop stores the key as
//! `tokens.openrouter` (same as a pasted `OPENROUTER_API_KEY`).

use anyhow::{Context, Result, anyhow};
use reqwest::Url;
use serde::Deserialize;
use std::collections::HashMap;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const AUTHORIZE_URL: &str = "https://openrouter.ai/auth";
const KEYS_URL: &str = "https://openrouter.ai/api/v1/auth/keys";
const CALLBACK_PATH: &str = "/auth/callback";
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
const USER_AUTH_TIMEOUT: Duration = Duration::from_secs(10 * 60);

#[derive(Debug, Deserialize)]
struct KeyResponse {
    key: String,
}

pub(crate) struct PreparedLogin {
    pub authorize_url: String,
    listener: TcpListener,
    verifier: String,
    keys_url: String,
}

/// Interactive browser login against the production OpenRouter endpoints.
pub async fn login_with_browser() -> Result<String> {
    let prepared = prepare_login().await?;
    crate::auth::oauth_flow::open_browser(&prepared.authorize_url)?;
    complete_login(prepared).await
}

pub(crate) async fn prepare_login() -> Result<PreparedLogin> {
    prepare_login_at(AUTHORIZE_URL, KEYS_URL).await
}

pub(crate) async fn prepare_login_at(authorize_url: &str, keys_url: &str) -> Result<PreparedLogin> {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .context("bind OpenRouter OAuth callback")?;
    let port = listener
        .local_addr()
        .context("resolve OpenRouter callback port")?
        .port();
    let redirect_uri = format!("http://127.0.0.1:{port}{CALLBACK_PATH}");
    let verifier = crate::auth::oauth_flow::random_pkce_verifier();
    let challenge = crate::auth::oauth_flow::pkce_challenge(&verifier);
    let mut url = Url::parse(authorize_url).context("parse OpenRouter authorize URL")?;
    url.query_pairs_mut()
        .append_pair("callback_url", &redirect_uri)
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256");
    Ok(PreparedLogin {
        authorize_url: url.to_string(),
        listener,
        verifier,
        keys_url: keys_url.to_string(),
    })
}

pub(crate) async fn complete_login(prepared: PreparedLogin) -> Result<String> {
    let code = tokio::time::timeout(USER_AUTH_TIMEOUT, wait_for_callback(prepared.listener))
        .await
        .map_err(|_| anyhow!("OpenRouter browser login timed out"))??;
    exchange_code_at(&prepared.keys_url, &code, &prepared.verifier).await
}

async fn exchange_code_at(keys_url: &str, code: &str, verifier: &str) -> Result<String> {
    let client = reqwest::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .build()
        .context("build OpenRouter auth HTTP client")?;
    let response = client
        .post(keys_url)
        .json(&serde_json::json!({
            "code": code,
            "code_verifier": verifier,
            "code_challenge_method": "S256",
        }))
        .send()
        .await
        .context("exchange OpenRouter OAuth code")?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(anyhow!(
            "OpenRouter OAuth exchange failed ({status}): {body}"
        ));
    }
    let parsed: KeyResponse = response
        .json()
        .await
        .context("parse OpenRouter API key response")?;
    if parsed.key.is_empty() {
        return Err(anyhow!("OpenRouter OAuth exchange returned an empty key"));
    }
    Ok(parsed.key)
}

async fn wait_for_callback(listener: TcpListener) -> Result<String> {
    loop {
        let (mut socket, _) = listener
            .accept()
            .await
            .context("accept OpenRouter OAuth callback")?;
        let request = read_request_line(&mut socket).await?;
        let path = request
            .split_whitespace()
            .nth(1)
            .ok_or_else(|| anyhow!("invalid OpenRouter OAuth callback request"))?;
        let parsed = Url::parse(&format!("http://127.0.0.1{path}"))
            .context("parse OpenRouter OAuth callback URL")?;
        let params: HashMap<_, _> = parsed.query_pairs().into_owned().collect();

        if parsed.path() != CALLBACK_PATH {
            write_response(&mut socket, "404 Not Found", "Unexpected callback path.").await?;
            continue;
        }
        if let Some(error) = params.get("error") {
            let message = format!("Authorization failed: {error}.");
            write_response(&mut socket, "400 Bad Request", &message).await?;
            return Err(anyhow!(
                "OpenRouter authorization server returned error: {error}"
            ));
        }
        if let Some(code) = params.get("code") {
            if code.is_empty() {
                write_response(
                    &mut socket,
                    "400 Bad Request",
                    "Callback had an empty authorization code.",
                )
                .await?;
                return Err(anyhow!("OpenRouter OAuth callback had an empty code"));
            }
            write_response(
                &mut socket,
                "200 OK",
                "OpenRouter login complete. You can return to the terminal.",
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
    }
}

async fn read_request_line(socket: &mut TcpStream) -> Result<String> {
    let mut buffer = vec![0u8; 8192];
    let n = socket
        .read(&mut buffer)
        .await
        .context("read OpenRouter OAuth callback")?;
    let request = String::from_utf8_lossy(&buffer[..n]);
    Ok(request.lines().next().unwrap_or_default().to_string())
}

async fn write_response(socket: &mut TcpStream, status: &str, message: &str) -> Result<()> {
    let body = crate::auth::oauth_flow::callback_page(status, message, "OpenRouter");
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    socket
        .write_all(response.as_bytes())
        .await
        .context("write OpenRouter OAuth callback response")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    struct MockKeysServer {
        base: String,
        received: std::sync::Arc<tokio::sync::Mutex<Option<String>>>,
    }

    impl MockKeysServer {
        async fn start(status: u16, body: &str) -> Self {
            let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let base = format!("http://127.0.0.1:{port}");
            let received = std::sync::Arc::new(tokio::sync::Mutex::new(None));
            let received_for_task = received.clone();
            let body = body.to_string();
            tokio::spawn(async move {
                loop {
                    let Ok((mut socket, _)) = listener.accept().await else {
                        break;
                    };
                    let mut buf = vec![0u8; 8192];
                    let Ok(n) = socket.read(&mut buf).await else {
                        continue;
                    };
                    let request = String::from_utf8_lossy(&buf[..n]).to_string();
                    *received_for_task.lock().await = Some(request.clone());
                    let response = format!(
                        "HTTP/1.1 {status} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        if (200..300).contains(&status) {
                            "OK"
                        } else {
                            "Error"
                        },
                        body.len()
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                }
            });
            Self { base, received }
        }

        fn keys_url(&self) -> String {
            format!("{}/api/v1/auth/keys", self.base)
        }
    }

    fn callback_url(prepared: &PreparedLogin) -> Url {
        let authorize = Url::parse(&prepared.authorize_url).unwrap();
        let params: HashMap<_, _> = authorize.query_pairs().into_owned().collect();
        Url::parse(params.get("callback_url").expect("callback_url")).unwrap()
    }

    async fn get(url: &str) -> String {
        let parsed = Url::parse(url).unwrap();
        let mut stream = TcpStream::connect(format!(
            "{}:{}",
            parsed.host_str().unwrap(),
            parsed.port().unwrap()
        ))
        .await
        .unwrap();
        let path = match parsed.query() {
            Some(query) => format!("{}?{query}", parsed.path()),
            None => parsed.path().to_string(),
        };
        let request =
            format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n");
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).await.unwrap();
        response
    }

    #[tokio::test]
    async fn prepare_login_builds_pkce_authorize_url() {
        let prepared = prepare_login_at("https://openrouter.ai/auth", KEYS_URL)
            .await
            .unwrap();
        let url = Url::parse(&prepared.authorize_url).unwrap();
        let params: HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(url.scheme(), "https");
        assert_eq!(url.host_str(), Some("openrouter.ai"));
        assert_eq!(url.path(), "/auth");
        assert_eq!(
            params.get("code_challenge_method").map(String::as_str),
            Some("S256")
        );
        let expected_challenge = crate::auth::oauth_flow::pkce_challenge(&prepared.verifier);
        assert_eq!(
            params.get("code_challenge").map(String::as_str),
            Some(expected_challenge.as_str())
        );
        let callback = Url::parse(params.get("callback_url").expect("callback_url")).unwrap();
        assert_eq!(callback.host_str(), Some("127.0.0.1"));
        assert_eq!(callback.path(), CALLBACK_PATH);
    }

    #[tokio::test]
    async fn complete_login_exchanges_callback_code_for_api_key() {
        let server =
            MockKeysServer::start(200, r#"{"key":"sk-or-test-key","user_id":"user_123"}"#).await;
        let prepared = prepare_login_at("https://openrouter.ai/auth", &server.keys_url())
            .await
            .unwrap();
        let callback = callback_url(&prepared);
        let hit = format!("{callback}?code=auth-code-1");
        let browser = tokio::spawn(async move { get(&hit).await });
        let key = complete_login(prepared).await.expect("exchange");
        let page = browser.await.unwrap();
        assert_eq!(key, "sk-or-test-key");
        assert!(page.contains("HTTP/1.1 200 OK"));
        assert!(page.contains("OpenRouter is connected."));
        let request = server.received.lock().await.clone().expect("keys POST");
        assert!(request.starts_with("POST /api/v1/auth/keys"));
        assert!(request.contains("\"code\":\"auth-code-1\""));
        assert!(request.contains("\"code_challenge_method\":\"S256\""));
        assert!(request.contains("\"code_verifier\":"));
    }

    #[tokio::test]
    async fn complete_login_rejects_authorization_error() {
        let server = MockKeysServer::start(200, r#"{"key":"unused"}"#).await;
        let prepared = prepare_login_at("https://openrouter.ai/auth", &server.keys_url())
            .await
            .unwrap();
        let callback = callback_url(&prepared);
        let hit = format!("{callback}?error=access_denied");
        let browser = tokio::spawn(async move { get(&hit).await });
        let error = complete_login(prepared).await.expect_err("denied");
        let page = browser.await.unwrap();
        assert!(error.to_string().contains("access_denied"), "got: {error}");
        assert!(page.contains("HTTP/1.1 400 Bad Request"));
    }

    #[tokio::test]
    async fn exchange_surfaces_openrouter_error_body() {
        let err = exchange_code_at(
            &MockKeysServer::start(403, r#"{"error":"Invalid code or code_verifier"}"#)
                .await
                .keys_url(),
            "stale-code",
            "verifier",
        )
        .await
        .expect_err("403");
        let message = err.to_string();
        assert!(message.contains("403"), "got: {message}");
        assert!(
            message.contains("Invalid code or code_verifier"),
            "got: {message}"
        );
    }

    #[tokio::test]
    async fn callback_page_is_branded_for_openrouter() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            write_response(
                &mut socket,
                "200 OK",
                "OpenRouter login complete. You can return to the terminal.",
            )
            .await
            .unwrap();
        });

        let mut browser = TcpStream::connect(address).await.unwrap();
        server.await.unwrap();
        let mut response = String::new();
        browser.read_to_string(&mut response).await.unwrap();
        assert!(response.contains("OpenRouter is connected."));
        assert!(response.contains("<svg"));
    }
}

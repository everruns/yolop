use crate::oauth_flow;
use crate::settings::CodexAuth;
use anyhow::{Context, Result, anyhow};
use reqwest::Url;
use serde::Deserialize;
use std::collections::HashMap;
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub const CODEX_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub const CODEX_ORIGINATOR: &str = "yolop";
const AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const DEVICE_USER_CODE_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/usercode";
const DEVICE_TOKEN_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/token";
const DEVICE_REDIRECT_URI: &str = "https://auth.openai.com/deviceauth/callback";
const DEVICE_USER_URL: &str = "https://auth.openai.com/codex/device";
const SCOPE: &str = "openid profile email offline_access";
const CALLBACK_PORT: u16 = 1455;
const CALLBACK_PATH: &str = "/auth/callback";
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
const USER_AUTH_TIMEOUT: Duration = Duration::from_secs(10 * 60);

#[derive(Debug, Clone)]
pub struct DeviceLogin {
    pub user_code: String,
    pub verification_uri: String,
    device_auth_id: String,
    interval_secs: u64,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<i64>,
    #[serde(default)]
    id_token: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DeviceCodeResponse {
    device_auth_id: String,
    user_code: String,
    #[serde(default)]
    verification_uri: Option<String>,
    #[serde(default)]
    verification_url: Option<String>,
    #[serde(default)]
    interval: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct DeviceTokenResponse {
    authorization_code: String,
    code_verifier: String,
}

pub async fn login_with_browser() -> Result<CodexAuth> {
    let redirect_uri = format!("http://localhost:{CALLBACK_PORT}{CALLBACK_PATH}");
    let state = oauth_flow::random_token(32);
    let verifier = oauth_flow::random_pkce_verifier();
    let challenge = oauth_flow::pkce_challenge(&verifier);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", CALLBACK_PORT))
        .await
        .with_context(|| format!("bind Codex OAuth callback on localhost:{CALLBACK_PORT}"))?;

    let mut url = Url::parse(AUTHORIZE_URL)?;
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", CODEX_CLIENT_ID)
        .append_pair("redirect_uri", &redirect_uri)
        .append_pair("scope", SCOPE)
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", &state)
        .append_pair("id_token_add_organizations", "true")
        .append_pair("codex_cli_simplified_flow", "true")
        .append_pair("originator", CODEX_ORIGINATOR);

    open_browser(url.as_str()).await?;
    let code = tokio::time::timeout(USER_AUTH_TIMEOUT, wait_for_callback(listener, &state))
        .await
        .map_err(|_| anyhow!("Codex browser login timed out"))??;
    exchange_code(&code, &verifier, &redirect_uri).await
}

pub async fn start_device_login() -> Result<DeviceLogin> {
    let client = auth_http_client()?;
    let response = client
        .post(DEVICE_USER_CODE_URL)
        .json(&serde_json::json!({ "client_id": CODEX_CLIENT_ID }))
        .send()
        .await
        .context("start Codex device login")?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(anyhow!(
            "Codex device login start failed ({status}): {body}"
        ));
    }
    let parsed: DeviceCodeResponse = response.json().await.context("parse device login")?;
    Ok(DeviceLogin {
        user_code: parsed.user_code,
        verification_uri: parsed
            .verification_uri
            .or(parsed.verification_url)
            .unwrap_or_else(|| DEVICE_USER_URL.to_string()),
        device_auth_id: parsed.device_auth_id,
        interval_secs: parsed.interval.unwrap_or(5).max(1),
    })
}

pub async fn complete_device_login(login: DeviceLogin) -> Result<CodexAuth> {
    tokio::time::timeout(USER_AUTH_TIMEOUT, complete_device_login_inner(login))
        .await
        .map_err(|_| anyhow!("Codex device login timed out before authorization completed"))?
}

async fn complete_device_login_inner(login: DeviceLogin) -> Result<CodexAuth> {
    let client = auth_http_client()?;
    for _ in 0..120 {
        tokio::time::sleep(std::time::Duration::from_secs(login.interval_secs)).await;
        let response = client
            .post(DEVICE_TOKEN_URL)
            .json(&serde_json::json!({
                "device_auth_id": login.device_auth_id,
                "user_code": login.user_code,
            }))
            .send()
            .await
            .context("poll Codex device login")?;
        if response.status().is_success() {
            let token: DeviceTokenResponse =
                response.json().await.context("parse Codex device token")?;
            return exchange_code(
                &token.authorization_code,
                &token.code_verifier,
                DEVICE_REDIRECT_URI,
            )
            .await;
        }
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if matches!(status.as_u16(), 403 | 404) {
            continue;
        }
        return Err(anyhow!("Codex device login failed ({status}): {body}"));
    }
    Err(anyhow!(
        "Codex device login timed out before authorization completed"
    ))
}

pub async fn refresh_with_token(refresh_token: &str) -> Result<CodexAuth> {
    refresh_with_token_at(TOKEN_URL, refresh_token).await
}

/// Refresh against an arbitrary token endpoint (tests inject a local mock).
pub async fn refresh_with_token_at(token_url: &str, refresh_token: &str) -> Result<CodexAuth> {
    let client = auth_http_client()?;
    let response = client
        .post(token_url)
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", CODEX_CLIENT_ID),
        ])
        .send()
        .await
        .context("refresh Codex token")?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(anyhow!("Codex token refresh failed ({status}): {body}"));
    }
    let token: TokenResponse = response.json().await.context("parse Codex refresh token")?;
    auth_from_token_response(token)
}

/// OpenAI rotates Codex refresh tokens; reuse of an already-spent token returns
/// `refresh_token_reused` and requires either adopting a newer on-disk token or
/// signing in again.
pub fn is_refresh_token_reused(err: &anyhow::Error) -> bool {
    let message = format!("{err:#}");
    message.contains("refresh_token_reused")
}

pub fn auth_from_access_token(access_token: String) -> CodexAuth {
    CodexAuth {
        account_id: extract_account_id(&access_token),
        email: extract_email(&access_token),
        access_token,
        refresh_token: None,
        expires_at: None,
    }
}

pub fn should_refresh(expires_at: Option<i64>) -> bool {
    expires_at
        .map(|expires| expires <= now_epoch_millis() + 60_000)
        .unwrap_or(false)
}

pub fn now_epoch_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

pub fn extract_account_id(access_token: &str) -> Option<String> {
    oauth_flow::jwt_payload(access_token).and_then(|payload| {
        payload
            .get("https://api.openai.com/auth")
            .and_then(|auth| auth.get("chatgpt_account_id"))
            .and_then(|value| value.as_str())
            .or_else(|| {
                payload
                    .get("chatgpt_account_id")
                    .and_then(|value| value.as_str())
            })
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    })
}

fn extract_email(access_token: &str) -> Option<String> {
    oauth_flow::jwt_payload(access_token).and_then(|payload| {
        payload
            .get("https://api.openai.com/profile")
            .and_then(|profile| profile.get("email"))
            .and_then(|value| value.as_str())
            .or_else(|| payload.get("email").and_then(|value| value.as_str()))
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    })
}

fn auth_from_token_response(token: TokenResponse) -> Result<CodexAuth> {
    let expires_at = token
        .expires_in
        .filter(|seconds| *seconds > 0)
        .map(|seconds| now_epoch_millis() + seconds.saturating_mul(1000));
    let account_id = extract_account_id(&token.access_token)
        .or_else(|| token.id_token.as_deref().and_then(extract_account_id));
    let email = extract_email(&token.access_token)
        .or_else(|| token.id_token.as_deref().and_then(extract_email));
    Ok(CodexAuth {
        access_token: token.access_token,
        refresh_token: token.refresh_token,
        expires_at,
        account_id,
        email,
    })
}

async fn exchange_code(code: &str, verifier: &str, redirect_uri: &str) -> Result<CodexAuth> {
    let client = auth_http_client()?;
    let response = client
        .post(TOKEN_URL)
        .form(&[
            ("grant_type", "authorization_code"),
            ("client_id", CODEX_CLIENT_ID),
            ("code", code),
            ("code_verifier", verifier),
            ("redirect_uri", redirect_uri),
        ])
        .send()
        .await
        .context("exchange Codex OAuth code")?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(anyhow!("Codex OAuth exchange failed ({status}): {body}"));
    }
    let token: TokenResponse = response.json().await.context("parse Codex OAuth token")?;
    auth_from_token_response(token)
}

fn auth_http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .build()
        .context("build Codex auth HTTP client")
}

async fn wait_for_callback(
    listener: tokio::net::TcpListener,
    expected_state: &str,
) -> Result<String> {
    loop {
        let (mut socket, _) = listener.accept().await.context("accept OAuth callback")?;
        let mut buffer = vec![0u8; 8192];
        let n = socket
            .read(&mut buffer)
            .await
            .context("read OAuth callback")?;
        let request = String::from_utf8_lossy(&buffer[..n]);
        let first_line = request.lines().next().unwrap_or_default();
        let path = first_line
            .split_whitespace()
            .nth(1)
            .ok_or_else(|| anyhow!("invalid OAuth callback request"))?;
        let parsed =
            Url::parse(&format!("http://localhost{path}")).context("parse OAuth callback URL")?;
        let params: HashMap<_, _> = parsed.query_pairs().into_owned().collect();

        let (status, body) = if parsed.path() != CALLBACK_PATH {
            (
                "404 Not Found",
                "Yolop Codex login received an unexpected callback path.",
            )
        } else if params.get("state").map(String::as_str) != Some(expected_state) {
            (
                "400 Bad Request",
                "Yolop Codex login rejected this callback because the state did not match.",
            )
        } else if let Some(error) = params.get("error") {
            ("400 Bad Request", error.as_str())
        } else if let Some(code) = params.get("code") {
            write_callback_response(
                &mut socket,
                "200 OK",
                "Yolop Codex login complete. You can return to the terminal.",
            )
            .await?;
            return Ok(code.clone());
        } else {
            (
                "400 Bad Request",
                "Yolop Codex login callback had no authorization code.",
            )
        };
        write_callback_response(&mut socket, status, body).await?;
    }
}

async fn write_callback_response(
    socket: &mut tokio::net::TcpStream,
    status: &str,
    message: &str,
) -> Result<()> {
    let body = callback_page(status, message);
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    socket
        .write_all(response.as_bytes())
        .await
        .context("write OAuth callback response")
}

fn callback_page(status: &str, message: &str) -> String {
    let ok = status.starts_with("200");
    let title = if ok {
        "Yolop Codex login complete"
    } else {
        "Yolop Codex login needs attention"
    };
    let eyebrow = if ok { "Signed in" } else { "Login interrupted" };
    let fun = if ok {
        "Your terminal is already warming up the keyboard."
    } else {
        "The terminal kept your seat warm. Try the login flow again when ready."
    };
    let class = if ok { "success" } else { "error" };
    format!(
        r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>{title}</title>
  <style>
    :root {{
      color-scheme: light dark;
      --ink: #12131a;
      --muted: #626776;
      --panel: rgba(255, 255, 255, 0.86);
      --line: rgba(18, 19, 26, 0.12);
      --gold: #d4a43a;
      --navy: #0a1636;
      --bad: #b74747;
    }}
    * {{ box-sizing: border-box; }}
    html, body {{ min-height: 100%; }}
    body {{
      margin: 0;
      font: 16px/1.5 ui-sans-serif, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
      color: var(--ink);
      background:
        radial-gradient(circle at 18% 12%, rgba(212, 164, 58, 0.22), transparent 28rem),
        radial-gradient(circle at 85% 18%, rgba(10, 22, 54, 0.14), transparent 30rem),
        linear-gradient(135deg, #fbfaf7 0%, #eef1f7 100%);
      display: grid;
      place-items: center;
      padding: 32px;
    }}
    main {{
      width: min(720px, 100%);
      border: 1px solid var(--line);
      border-radius: 28px;
      padding: clamp(28px, 6vw, 56px);
      background: var(--panel);
      box-shadow: 0 24px 80px rgba(10, 22, 54, 0.16);
      backdrop-filter: blur(20px);
    }}
    .logo {{
      width: 88px;
      height: 88px;
      display: grid;
      place-items: center;
      border-radius: 24px;
      background: #fff;
      box-shadow: inset 0 0 0 1px var(--line), 0 12px 32px rgba(10, 22, 54, 0.12);
      margin-bottom: 28px;
    }}
    .logo svg {{ width: 62px; height: 62px; display: block; }}
    .eyebrow {{
      margin: 0 0 10px;
      color: {accent};
      font-weight: 700;
      letter-spacing: 0;
      text-transform: uppercase;
      font-size: 0.82rem;
    }}
    h1 {{
      margin: 0;
      font-size: clamp(2rem, 7vw, 4.25rem);
      line-height: 0.96;
      letter-spacing: 0;
    }}
    .message {{
      margin: 24px 0 0;
      color: var(--muted);
      font-size: clamp(1rem, 2.2vw, 1.2rem);
      max-width: 38rem;
    }}
    .next {{
      margin-top: 34px;
      padding: 18px 20px;
      border-radius: 16px;
      background: rgba(10, 22, 54, 0.06);
      color: var(--navy);
      font-weight: 650;
    }}
    .success .next {{ border-left: 5px solid var(--gold); }}
    .error .next {{ border-left: 5px solid var(--bad); }}
    @media (prefers-color-scheme: dark) {{
      :root {{
        --ink: #f4f5fb;
        --muted: #b8bdcc;
        --panel: rgba(24, 25, 34, 0.88);
        --line: rgba(255, 255, 255, 0.12);
        --navy: #f4f5fb;
      }}
      body {{
        background:
          radial-gradient(circle at 18% 12%, rgba(212, 164, 58, 0.18), transparent 28rem),
          radial-gradient(circle at 85% 18%, rgba(94, 116, 184, 0.18), transparent 30rem),
          linear-gradient(135deg, #101118 0%, #171927 100%);
      }}
      .logo {{ background: rgba(255, 255, 255, 0.08); }}
      .next {{ background: rgba(255, 255, 255, 0.08); }}
    }}
  </style>
</head>
<body>
  <main class="{class}">
    <div class="logo" aria-label="Yolop logo">{logo}</div>
    <p class="eyebrow">{eyebrow}</p>
    <h1>{headline}</h1>
    <p class="message">{message}</p>
    <div class="next">{fun}</div>
  </main>
</body>
</html>"#,
        title = oauth_flow::html_escape(title),
        accent = if ok { "var(--gold)" } else { "var(--bad)" },
        class = class,
        logo = include_str!("../logo.svg"),
        eyebrow = oauth_flow::html_escape(eyebrow),
        headline = if ok {
            "Codex is connected."
        } else {
            "Almost there."
        },
        message = oauth_flow::html_escape(message),
        fun = oauth_flow::html_escape(fun),
    )
}

async fn open_browser(url: &str) -> Result<()> {
    let mut command = if cfg!(target_os = "macos") {
        let mut command = tokio::process::Command::new("open");
        command.arg(url);
        command
    } else if cfg!(target_os = "windows") {
        let mut command = tokio::process::Command::new("cmd");
        command.args(["/C", "start", "", url]);
        command
    } else {
        let mut command = tokio::process::Command::new("xdg-open");
        command.arg(url);
        command
    };
    command.kill_on_drop(true);
    // Detach stdio so a chatty browser launcher can't print onto the TUI frame
    // (mirrors `crate::proc::detach_stdio`, which is for std `Command`).
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let status = command
        .status()
        .await
        .context("open browser for Codex login")?;
    if status.success() {
        Ok(())
    } else {
        Err(anyhow!("browser opener exited with {status}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;

    fn jwt_with_payload(payload: serde_json::Value) -> String {
        format!(
            "header.{}.sig",
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode(serde_json::to_vec(&payload).unwrap())
        )
    }

    #[test]
    fn extracts_account_id_from_codex_jwt_claim() {
        let token = jwt_with_payload(serde_json::json!({
            "https://api.openai.com/auth": {
                "chatgpt_account_id": "acc_123"
            }
        }));
        assert_eq!(extract_account_id(&token).as_deref(), Some("acc_123"));
    }

    #[test]
    fn should_refresh_only_near_expiry() {
        assert!(!should_refresh(None));
        assert!(should_refresh(Some(now_epoch_millis() + 1_000)));
        assert!(!should_refresh(Some(now_epoch_millis() + 300_000)));
    }

    #[test]
    fn detects_refresh_token_reused_error() {
        let err = anyhow!(
            "Codex token refresh failed (401 Unauthorized): {{\"error\":{{\"code\":\"refresh_token_reused\"}}}}"
        );
        assert!(is_refresh_token_reused(&err));
        assert!(!is_refresh_token_reused(&anyhow!("network down")));
    }

    #[test]
    fn callback_page_includes_logo_and_success_copy() {
        let page = callback_page("200 OK", "Yolop Codex login complete.");

        assert!(page.contains("<svg"));
        assert!(page.contains("Codex is connected."));
        assert!(page.contains("Your terminal is already warming up the keyboard."));
        assert!(page.contains("Yolop Codex login complete."));
    }
}

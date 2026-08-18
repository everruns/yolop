//! Loopback setup page, opt-in per run (`--acp-setup-page`) or per user (the
//! `acp_setup_page` setting).
//!
//! ACP has no secure way for an agent to ask for free text, let alone a secret:
//! the client owns every input surface, and the only agent-initiated ask is
//! `session/request_permission`, which is a fixed list of options. Providers
//! that need an API key, a base URL, or a custom model spec therefore have no
//! in-conversation path over ACP (see `knowledge/specs/acp.md`).
//!
//! What ACP *does* have is agent-handled authentication: `authenticate` hands
//! control to the agent, which is free to run any out-of-band flow. This module
//! is that flow, generalised past OAuth. It binds a loopback listener, serves a
//! small form, and the browser becomes the input surface the protocol lacks.
//! The secret travels over `127.0.0.1` into the settings store, never through
//! the ACP transcript or the session log.
//!
//! Same trust boundary as the OAuth callback listeners in `crate::auth`: bound
//! to loopback only, guarded by an unguessable one-page token, `Host` checked
//! so a hostile page cannot drive it by DNS rebinding, and torn down once a
//! provider is saved (or after [`PAGE_LIFETIME`]).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use reqwest::Url;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, watch};
use tokio::task::JoinHandle;

use crate::auth::oauth_flow::{callback_page, html_escape, open_browser, random_token};
use crate::capabilities::model_discovery::provider_is_usable;
use crate::config::SettingsStore;
use crate::runtime::SUPPORTED_PROVIDERS;

/// The only path the page answers on; everything else is a 404.
const PAGE_PATH: &str = "/setup";
/// How long an unused page stays open before the listener is dropped.
const PAGE_LIFETIME: Duration = Duration::from_secs(10 * 60);
/// Cap on a single request. The form is a handful of short fields; anything
/// larger is a client bug or an attempt to grow the process, not a submission.
const MAX_REQUEST_BYTES: usize = 32 * 1024;

/// Owns the process's current setup page, if any.
///
/// One page is shared by every ACP session: the settings it writes are global,
/// so a second listener would only add a second port to guard.
pub(crate) struct SetupPageService {
    settings: Arc<SettingsStore>,
    live: Mutex<Option<LivePage>>,
}

struct LivePage {
    url: String,
    /// `None` while the page is still open; `Some` once it saved a provider,
    /// failed, or timed out.
    outcome: watch::Receiver<Option<std::result::Result<String, String>>>,
    task: JoinHandle<()>,
}

impl SetupPageService {
    pub(crate) fn new(settings: Arc<SettingsStore>) -> Self {
        Self {
            settings,
            live: Mutex::new(None),
        }
    }

    /// URL of the live page, starting one if the last has finished. Callers
    /// hand this to the user; nothing happens until it is opened.
    pub(crate) async fn url(&self) -> Result<String> {
        let mut live = self.live.lock().await;
        if let Some(existing) = live.as_ref()
            && !existing.task.is_finished()
        {
            return Ok(existing.url.clone());
        }

        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .context("bind the yolop setup page")?;
        let port = listener
            .local_addr()
            .context("resolve the yolop setup page port")?
            .port();
        let token = random_token(24);
        let url = format!("http://127.0.0.1:{port}{PAGE_PATH}?t={token}");

        let (tx, rx) = watch::channel(None);
        let settings = self.settings.clone();
        let task = tokio::spawn(async move {
            let served =
                tokio::time::timeout(PAGE_LIFETIME, serve_until_saved(listener, token, settings))
                    .await;
            let outcome = match served {
                Ok(Ok(summary)) => Ok(summary),
                Ok(Err(err)) => Err(err.to_string()),
                Err(_) => Err("the setup page timed out".to_string()),
            };
            let _ = tx.send(Some(outcome));
        });

        *live = Some(LivePage {
            url: url.clone(),
            outcome: rx,
            task,
        });
        Ok(url)
    }

    /// Resolve once the live page saves a provider, fails, or times out.
    /// Errors when no page is running.
    pub(crate) async fn wait(&self) -> Result<String> {
        let mut outcome = {
            let live = self.live.lock().await;
            live.as_ref()
                .map(|page| page.outcome.clone())
                .ok_or_else(|| anyhow!("no setup page is running"))?
        };
        loop {
            let settled = outcome.borrow_and_update().clone();
            if let Some(result) = settled {
                return result.map_err(|message| anyhow!(message));
            }
            outcome
                .changed()
                .await
                .map_err(|_| anyhow!("the setup page stopped without a result"))?;
        }
    }

    /// The `authenticate` path: open the page and block until it is submitted,
    /// so the ACP client's own "authenticating" state tracks the real flow.
    pub(crate) async fn run_to_completion(&self) -> Result<String> {
        let url = self.url().await?;
        if let Err(err) = open_browser(&url) {
            // Headless hosts and remote clients cannot open a browser here; the
            // URL was already handed to the caller, so this is not fatal.
            tracing::debug!(%err, "acp: could not open the setup page automatically");
        }
        self.wait().await
    }
}

async fn serve_until_saved(
    listener: TcpListener,
    token: String,
    settings: Arc<SettingsStore>,
) -> Result<String> {
    loop {
        let (mut socket, _) = listener
            .accept()
            .await
            .context("accept a yolop setup page request")?;
        match handle_connection(&mut socket, &token, &settings).await {
            Ok(Some(summary)) => return Ok(summary),
            // A rejected or re-rendered request leaves the page open.
            Ok(None) => continue,
            Err(err) => {
                tracing::debug!(%err, "acp: setup page request failed");
                continue;
            }
        }
    }
}

struct HttpRequest {
    method: String,
    path: String,
    query: HashMap<String, String>,
    host: Option<String>,
    body: String,
}

/// Serve one request. `Ok(Some(summary))` means a provider was saved and the
/// page is done; `Ok(None)` means the page stays open.
async fn handle_connection(
    socket: &mut TcpStream,
    token: &str,
    settings: &Arc<SettingsStore>,
) -> Result<Option<String>> {
    let request = read_request(socket).await?;

    // A hostile page can make a browser POST to a known loopback port, but it
    // cannot read the token or forge the Host header, so both are checked.
    if !host_is_loopback(request.host.as_deref()) {
        write_page(
            socket,
            "403 Forbidden",
            &denied_page("unexpected Host header"),
        )
        .await?;
        return Ok(None);
    }
    if request.path != PAGE_PATH {
        write_page(socket, "404 Not Found", &denied_page("no such page")).await?;
        return Ok(None);
    }
    if request.query.get("t").map(String::as_str) != Some(token) {
        write_page(socket, "403 Forbidden", &denied_page("invalid setup link")).await?;
        return Ok(None);
    }

    let snapshot = settings.snapshot();
    match request.method.as_str() {
        "GET" => {
            let page = form_page(&snapshot, token, None);
            write_page(socket, "200 OK", &page).await?;
            Ok(None)
        }
        "POST" => {
            let form = parse_form(&request.body);
            match apply_setup(settings, &form) {
                Ok(summary) => {
                    let page = callback_page("200 OK", &summary, "provider setup");
                    write_page(socket, "200 OK", &page).await?;
                    Ok(Some(summary))
                }
                Err(message) => {
                    let page = form_page(&settings.snapshot(), token, Some(&message));
                    write_page(socket, "200 OK", &page).await?;
                    Ok(None)
                }
            }
        }
        _ => {
            write_page(
                socket,
                "405 Method Not Allowed",
                &denied_page("unsupported method"),
            )
            .await?;
            Ok(None)
        }
    }
}

/// Persist one submission. The returned summary is safe to echo over ACP: it
/// names the provider and model, never the credential.
fn apply_setup(
    settings: &Arc<SettingsStore>,
    form: &HashMap<String, String>,
) -> std::result::Result<String, String> {
    apply_setup_with(settings, form, provider_is_usable)
}

/// The body of [`apply_setup`], parameterised on the connectivity predicate so
/// tests can judge it without depending on which provider keys happen to be in
/// the process environment.
fn apply_setup_with(
    settings: &Arc<SettingsStore>,
    form: &HashMap<String, String>,
    is_usable: impl Fn(&crate::config::Settings, &str) -> bool,
) -> std::result::Result<String, String> {
    let field = |name: &str| form.get(name).map(|v| v.trim()).unwrap_or_default();
    let provider = field("provider");
    if !SUPPORTED_PROVIDERS.contains(&provider) {
        return Err(format!("unknown provider `{provider}`"));
    }
    let token = field("token");
    let base_url = field("base_url");
    let model = field("model");

    if !base_url.is_empty() {
        let parsed = Url::parse(base_url).map_err(|err| format!("base URL is not a URL: {err}"))?;
        if !matches!(parsed.scheme(), "http" | "https") {
            return Err("base URL must be http or https".to_string());
        }
        settings
            .set_base_url(provider.to_string(), base_url.to_string())
            .map_err(|err| format!("could not save the base URL: {err}"))?;
    }
    if !token.is_empty() {
        settings
            .set_token(provider.to_string(), token.to_string())
            .map_err(|err| format!("could not save the API key: {err}"))?;
    }
    if !model.is_empty() {
        settings
            .set_model(provider.to_string(), model.to_string())
            .map_err(|err| format!("could not save the model: {err}"))?;
    }

    // Judge connectivity through the same predicate ACP uses to build its model
    // list, so a page that reports success is a page whose provider will show up
    // as a usable model.
    if !is_usable(&settings.snapshot(), provider) {
        return Err(missing_credential_message(provider));
    }
    settings
        .set_default_provider(Some(provider.to_string()))
        .map_err(|err| format!("could not save the provider: {err}"))?;

    let model_note = if model.is_empty() {
        String::new()
    } else {
        format!(" model={model}")
    };
    Ok(format!("setup: provider={provider}{model_note}"))
}

fn missing_credential_message(provider: &str) -> String {
    match provider {
        "custom" | "ollama" => {
            format!("`{provider}` still needs a base URL (and a key if the endpoint requires one)")
        }
        "codex" => "`codex` needs a ChatGPT sign-in; use the Sign in with ChatGPT method instead"
            .to_string(),
        "local" => {
            "`local` needs the in-process engine compiled in and its weights downloaded".to_string()
        }
        _ => format!("`{provider}` still has no API key"),
    }
}

fn provider_label(provider: &str) -> &'static str {
    match provider {
        "openai" => "OpenAI",
        "codex" => "Codex subscription",
        "anthropic" => "Anthropic",
        "meta" => "Meta Model API",
        "google" => "Google Gemini",
        "openrouter" => "OpenRouter",
        "ollama" => "Ollama local",
        "local" => "Local (in-process)",
        "custom" => "Custom OpenAI-compatible endpoint",
        "llmsim" => "Offline demo mode",
        _ => "Provider",
    }
}

fn host_is_loopback(host: Option<&str>) -> bool {
    let Some(host) = host else {
        return false;
    };
    let host = host.trim();
    // Strip the port; IPv6 literals keep their brackets.
    let name = match host.rsplit_once(':') {
        Some((name, port)) if !port.contains(']') => name,
        _ => host,
    };
    matches!(name, "127.0.0.1" | "localhost" | "[::1]" | "::1")
}

fn parse_form(body: &str) -> HashMap<String, String> {
    // `Url` already owns percent- and `+`-decoding for query strings, which is
    // the same grammar as an urlencoded form body.
    match Url::parse(&format!("http://127.0.0.1/?{body}")) {
        Ok(url) => url.query_pairs().into_owned().collect(),
        Err(_) => HashMap::new(),
    }
}

async fn read_request(socket: &mut TcpStream) -> Result<HttpRequest> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        if let Some(end) = find_header_end(&buffer) {
            break end;
        }
        if buffer.len() > MAX_REQUEST_BYTES {
            return Err(anyhow!("setup page request headers were too large"));
        }
        let read = socket
            .read(&mut chunk)
            .await
            .context("read a setup page request")?;
        if read == 0 {
            return Err(anyhow!("setup page request ended before its headers"));
        }
        buffer.extend_from_slice(&chunk[..read]);
    };

    let head = String::from_utf8_lossy(&buffer[..header_end]).to_string();
    let mut lines = head.lines();
    let request_line = lines.next().unwrap_or_default().to_string();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let target = parts.next().unwrap_or_default().to_string();

    let mut host = None;
    let mut content_length = 0usize;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if name.eq_ignore_ascii_case("host") {
            host = Some(value.to_string());
        } else if name.eq_ignore_ascii_case("content-length") {
            content_length = value.parse().unwrap_or(0);
        }
    }
    if content_length > MAX_REQUEST_BYTES {
        return Err(anyhow!("setup page request body was too large"));
    }

    let mut body = buffer[header_end..].to_vec();
    while body.len() < content_length {
        let read = socket
            .read(&mut chunk)
            .await
            .context("read a setup page request body")?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..read]);
    }
    body.truncate(content_length);

    let parsed = Url::parse(&format!("http://127.0.0.1{target}"))
        .context("parse a setup page request target")?;
    Ok(HttpRequest {
        method,
        path: parsed.path().to_string(),
        query: parsed.query_pairs().into_owned().collect(),
        host,
        body: String::from_utf8_lossy(&body).to_string(),
    })
}

fn find_header_end(buffer: &[u8]) -> Option<usize> {
    buffer
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|at| at + 4)
        .or_else(|| {
            buffer
                .windows(2)
                .position(|window| window == b"\n\n")
                .map(|at| at + 2)
        })
}

async fn write_page(socket: &mut TcpStream, status: &str, body: &str) -> Result<()> {
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nReferrer-Policy: no-referrer\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    socket
        .write_all(response.as_bytes())
        .await
        .context("write a setup page response")
}

fn denied_page(reason: &str) -> String {
    callback_page("400 Bad Request", reason, "provider setup")
}

/// The form itself. Kept deliberately plain: one select, three fields, no
/// scripts, nothing fetched from the network.
fn form_page(settings: &crate::config::Settings, token: &str, error: Option<&str>) -> String {
    let options = SUPPORTED_PROVIDERS
        .iter()
        .map(|provider| {
            let connected = if provider_is_usable(settings, provider) {
                " (connected)"
            } else {
                ""
            };
            format!(
                r#"<option value="{provider}">{label}{connected}</option>"#,
                provider = html_escape(provider),
                label = html_escape(provider_label(provider)),
            )
        })
        .collect::<Vec<_>>()
        .join("\n        ");
    let banner = error
        .map(|message| format!(r#"<p class="error">{}</p>"#, html_escape(message)))
        .unwrap_or_default();
    let action = format!("{PAGE_PATH}?t={}", html_escape(token));

    format!(
        r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <meta name="referrer" content="no-referrer">
  <title>Yolop provider setup</title>
  <style>
    :root {{ color-scheme: light dark; --ink: #12131a; --muted: #626776; --line: rgba(18,19,26,.16); --bad: #b74747; }}
    * {{ box-sizing: border-box; }}
    body {{ margin: 0; min-height: 100vh; display: grid; place-items: center; padding: 32px;
      font: 16px/1.5 ui-sans-serif, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
      color: var(--ink); background: linear-gradient(135deg, #fbfaf7 0%, #eef1f7 100%); }}
    main {{ width: min(560px, 100%); background: rgba(255,255,255,.9); border: 1px solid var(--line);
      border-radius: 24px; padding: clamp(24px, 5vw, 44px); box-shadow: 0 24px 80px rgba(10,22,54,.16); }}
    h1 {{ font-size: 1.4rem; margin: 0 0 4px; }}
    p.lede {{ margin: 0 0 24px; color: var(--muted); }}
    label {{ display: block; font-weight: 600; margin: 16px 0 6px; }}
    input, select {{ width: 100%; padding: 10px 12px; border-radius: 12px; border: 1px solid var(--line);
      background: #fff; color: var(--ink); font: inherit; }}
    small {{ display: block; color: var(--muted); margin-top: 6px; }}
    button {{ margin-top: 24px; width: 100%; padding: 12px; border: 0; border-radius: 999px;
      background: #0a1636; color: #fff; font: inherit; font-weight: 600; cursor: pointer; }}
    .error {{ margin: 0 0 16px; padding: 12px 14px; border-radius: 12px; color: var(--bad);
      border: 1px solid var(--bad); background: rgba(183,71,71,.08); }}
  </style>
</head>
<body>
  <main>
    <h1>Connect a provider</h1>
    <p class="lede">Yolop is running as an editor agent. This page is served by that process on
      localhost, so the key you type never travels through the editor or the session log.</p>
    {banner}
    <form method="post" action="{action}" autocomplete="off">
      <label for="provider">Provider</label>
      <select id="provider" name="provider">
        {options}
      </select>
      <label for="token">API key</label>
      <input id="token" name="token" type="password" autocomplete="off" spellcheck="false"
        placeholder="leave empty to keep the saved key">
      <label for="base_url">Base URL</label>
      <input id="base_url" name="base_url" type="url" autocomplete="off" spellcheck="false"
        placeholder="optional; required for a custom endpoint">
      <label for="model">Model</label>
      <input id="model" name="model" type="text" autocomplete="off" spellcheck="false"
        placeholder="optional; defaults to this provider's model">
      <small>Saving also makes this the default provider. Pick the model in your editor afterwards.</small>
      <button type="submit">Save and connect</button>
    </form>
  </main>
</body>
</html>"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SettingsStore;

    fn store() -> (tempfile::TempDir, Arc<SettingsStore>) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Arc::new(SettingsStore::open(dir.path().join("settings.toml")));
        (dir, store)
    }

    #[test]
    fn form_lists_every_supported_provider_and_never_echoes_a_key() {
        let (_dir, store) = store();
        store
            .set_token("openai".to_string(), "sk-secret-value".to_string())
            .expect("save token");
        let page = form_page(&store.snapshot(), "page-token", None);
        for provider in SUPPORTED_PROVIDERS {
            assert!(
                page.contains(&format!(r#"value="{provider}""#)),
                "provider {provider} missing from the form"
            );
            assert_ne!(
                provider_label(provider),
                "Provider",
                "provider {provider} has no label"
            );
        }
        assert!(page.contains("OpenAI (connected)"));
        assert!(!page.contains("sk-secret-value"));
    }

    #[test]
    fn apply_setup_saves_a_key_and_selects_the_provider() {
        let (_dir, store) = store();
        let form = HashMap::from([
            ("provider".to_string(), "anthropic".to_string()),
            ("token".to_string(), "sk-ant-test".to_string()),
            ("model".to_string(), "claude-test".to_string()),
        ]);
        let summary = apply_setup_with(&store, &form, |_, _| true).expect("setup applies");
        assert_eq!(summary, "setup: provider=anthropic model=claude-test");
        assert!(!summary.contains("sk-ant-test"));
        let settings = store.snapshot();
        assert_eq!(settings.token_for("anthropic"), Some("sk-ant-test"));
        assert_eq!(settings.model_for("anthropic"), Some("claude-test"));
    }

    #[test]
    fn apply_setup_rejects_an_unknown_provider() {
        let (_dir, store) = store();
        let form = HashMap::from([("provider".to_string(), "not-a-provider".to_string())]);
        assert_eq!(
            apply_setup_with(&store, &form, |_, _| true).unwrap_err(),
            "unknown provider `not-a-provider`"
        );
    }

    #[test]
    fn apply_setup_reports_a_provider_that_is_still_unconnected() {
        let (_dir, store) = store();
        let form = HashMap::from([("provider".to_string(), "anthropic".to_string())]);
        let error = apply_setup_with(&store, &form, |_, _| false).unwrap_err();
        assert!(error.contains("still has no API key"), "got: {error}");
        assert_eq!(store.snapshot().default_provider, None);
    }

    #[test]
    fn apply_setup_rejects_a_base_url_that_is_not_http() {
        let (_dir, store) = store();
        let form = HashMap::from([
            ("provider".to_string(), "custom".to_string()),
            ("base_url".to_string(), "file:///etc/passwd".to_string()),
        ]);
        assert_eq!(
            apply_setup_with(&store, &form, |_, _| true).unwrap_err(),
            "base URL must be http or https"
        );
    }

    #[test]
    fn host_header_must_be_loopback() {
        assert!(host_is_loopback(Some("127.0.0.1:8123")));
        assert!(host_is_loopback(Some("localhost:8123")));
        assert!(host_is_loopback(Some("[::1]:8123")));
        assert!(!host_is_loopback(Some("evil.example.com:8123")));
        assert!(!host_is_loopback(None));
    }

    #[test]
    fn form_bodies_decode_percent_and_plus_escapes() {
        let form = parse_form("provider=custom&token=a%2Bb%20c&base_url=http%3A%2F%2Fx%2Fv1");
        assert_eq!(form.get("token").map(String::as_str), Some("a+b c"));
        assert_eq!(
            form.get("base_url").map(String::as_str),
            Some("http://x/v1")
        );
    }

    /// End to end over a real socket: the token gates the page, and a submitted
    /// form both persists the key and ends the page's life.
    #[tokio::test]
    async fn page_gates_on_the_token_and_saves_a_submission() {
        let (_dir, store) = store();
        let service = SetupPageService::new(store.clone());
        let url = service.url().await.expect("start the page");
        let base = url.split(PAGE_PATH).next().expect("base").to_string();
        let token = url.split("t=").nth(1).expect("token").to_string();
        let client = reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("http client");

        let denied = client
            .get(format!("{base}{PAGE_PATH}?t=wrong"))
            .send()
            .await
            .expect("send");
        assert_eq!(denied.status(), 403);

        let form = client
            .get(format!("{base}{PAGE_PATH}?t={token}"))
            .send()
            .await
            .expect("send");
        assert_eq!(form.status(), 200);
        assert!(
            form.text()
                .await
                .expect("body")
                .contains("Connect a provider")
        );

        let saved = client
            .post(format!("{base}{PAGE_PATH}?t={token}"))
            .form(&[("provider", "openai"), ("token", "sk-page-test")])
            .send()
            .await
            .expect("send");
        assert_eq!(saved.status(), 200);

        assert_eq!(
            service.wait().await.expect("outcome"),
            "setup: provider=openai"
        );
        assert_eq!(store.snapshot().token_for("openai"), Some("sk-page-test"));
    }
}

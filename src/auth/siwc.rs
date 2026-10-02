// Open-source "Sign in with ChatGPT" (SIWC): ChatGPT plan usage without an
// OpenAI-issued client ID.
//
// The Codex route (`crate::auth::codex`) signs in with a static client and
// runs turns on the private Codex backend. This route registers a client per
// user and workspace instead: the first sign-in sends
// `client_id=dynamic_agent_client` with an `agent_name_hint` and this host's
// stable `ext_agent_host_id`, and the callback returns the client OpenAI
// issued. That client is saved (`[chatgpt_registration]`), reused for every
// later sign-in, and recorded on the token set so refresh uses it. Turns then
// go to the public Responses API (`crate::drivers::chatgpt_plan`).
//
// Decisions:
// - The route is opt-in (`chatgpt_sign_in = "open-source"` or
//   `YOLOP_CHATGPT_SIGN_IN`); the Codex route stays the default until this one
//   is proven against a live account.
// - The ID token is verified against OpenAI's JWKS with `ring` (already in the
//   tree through rustls) instead of adding a JWT crate: RS256 is the only
//   algorithm the discovery document advertises.
// - One registration per host. An account picker (several registrations) is
//   not built yet; a different account needs the saved registration removed.
// - There is no documented device flow for this route, so it is browser-only.
//
// The flow as implemented, and what was inferred, is pinned in
// knowledge/specs/chatgpt-sign-in.md.
use crate::config::{ChatGptRegistration, CodexAuth, OpenSourceGrant, Settings, SettingsStore};
use anyhow::{Context, Result, anyhow, bail};
use rand::RngExt;
use reqwest::Url;
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Environment override for the sign-in route; wins over `chatgpt_sign_in`.
pub const SIGN_IN_ENV: &str = "YOLOP_CHATGPT_SIGN_IN";
/// First-registration entrypoint. Never saved and never used for a token call.
pub const DYNAMIC_CLIENT_ID: &str = "dynamic_agent_client";
/// The app name OpenAI shows on the consent screen. Sent only on the first
/// registration, and the same on every installation.
pub const AGENT_NAME_HINT: &str = "Yolop";
/// The scope that lets the token pay for inference with the ChatGPT plan.
pub const PLAN_SCOPE: &str = "chatgpt.tokens.use.direct";
/// Identity scopes plus the plan-usage scopes.
pub const SCOPE: &str =
    "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";
/// The `resource` (token audience) for every authorize, exchange and refresh.
pub const RESOURCE: &str = "https://api.openai.com/v1";
const PREFERRED_CALLBACK_PORT: u16 = 1455;
const CALLBACK_PATH: &str = "/auth/callback";
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
const USER_AUTH_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// Allowed clock skew when checking ID-token expiry.
const CLOCK_SKEW_SECS: i64 = 60;

/// Which route a new ChatGPT sign-in takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignInRoute {
    /// The Codex CLI's client and the Codex backend (the default).
    Codex,
    /// Dynamic registration and the public Responses API.
    OpenSource,
}

impl SignInRoute {
    pub fn as_str(self) -> &'static str {
        match self {
            SignInRoute::Codex => "codex",
            SignInRoute::OpenSource => "open-source",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "codex" | "default" => Some(Self::Codex),
            "open-source" | "opensource" | "oss" | "siwc" => Some(Self::OpenSource),
            _ => None,
        }
    }
}

/// The route a new sign-in takes: `YOLOP_CHATGPT_SIGN_IN`, then the
/// `chatgpt_sign_in` setting, then [`SignInRoute::Codex`].
pub fn configured_sign_in(settings: &Settings) -> Result<SignInRoute> {
    let env = std::env::var(SIGN_IN_ENV).ok();
    resolve_sign_in(env.as_deref(), settings.chatgpt_sign_in())
}

/// Pure precedence behind [`configured_sign_in`]. Blank counts as unset; an
/// unknown value is an error naming its source.
pub fn resolve_sign_in(env: Option<&str>, setting: Option<&str>) -> Result<SignInRoute> {
    for (source, value) in [(SIGN_IN_ENV, env), ("chatgpt_sign_in", setting)] {
        let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
            continue;
        };
        return SignInRoute::parse(value)
            .ok_or_else(|| anyhow!("invalid {source} `{value}`; expected codex or open-source"));
    }
    Ok(SignInRoute::Codex)
}

/// OpenAI's auth endpoints. Tests point these at a local mock.
#[derive(Debug, Clone)]
pub struct Endpoints {
    pub issuer: String,
    pub authorize: String,
    pub token: String,
    pub revoke: String,
    pub jwks: String,
}

impl Endpoints {
    /// Production values from
    /// `https://auth.openai.com/.well-known/openid-configuration`.
    pub fn production() -> Self {
        Self {
            issuer: "https://auth.openai.com".to_string(),
            authorize: "https://auth.openai.com/api/accounts/authorize".to_string(),
            token: "https://auth.openai.com/api/accounts/oauth/token".to_string(),
            revoke: "https://auth.openai.com/api/accounts/oauth/revoke".to_string(),
            jwks: "https://auth.openai.com/.well-known/jwks.json".to_string(),
        }
    }
}

/// A fresh `urn:uuid:` host ID (UUIDv4), one of the formats the docs accept.
pub fn new_host_id() -> String {
    let mut bytes = [0u8; 16];
    rand::rng().fill(&mut bytes);
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!(
        "urn:uuid:{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// Inputs for one authorization request.
#[derive(Debug, Clone)]
pub struct AuthorizeParams<'a> {
    pub redirect_uri: &'a str,
    pub host_id: &'a str,
    /// The saved registration, for a returning sign-in. `None` registers.
    pub registration: Option<&'a ChatGptRegistration>,
    /// The retained ID token of that registration's last sign-in.
    pub id_token_hint: Option<&'a str>,
    /// Ask for consent again, after the plan scope was declined.
    pub force_consent: bool,
    pub state: &'a str,
    pub nonce: &'a str,
    pub code_challenge: &'a str,
}

/// The browser URL for one attempt: a new registration with
/// `dynamic_agent_client` and `agent_name_hint`, or a returning sign-in with
/// the saved client and account hints.
pub fn authorize_url(endpoints: &Endpoints, params: &AuthorizeParams<'_>) -> Result<Url> {
    let mut url = Url::parse(&endpoints.authorize).context("parse authorize endpoint")?;
    {
        let mut query = url.query_pairs_mut();
        query.append_pair("response_type", "code");
        match params.registration {
            Some(registration) => {
                query.append_pair("client_id", &registration.client_id);
                if let Some(hint) = params.id_token_hint {
                    query.append_pair("id_token_hint", hint);
                }
                if let Some(email) = &registration.email {
                    query.append_pair("login_hint", email);
                }
            }
            None => {
                query.append_pair("client_id", DYNAMIC_CLIENT_ID);
                query.append_pair("agent_name_hint", AGENT_NAME_HINT);
            }
        }
        query
            .append_pair("ext_agent_host_id", params.host_id)
            .append_pair("redirect_uri", params.redirect_uri)
            .append_pair("scope", SCOPE)
            .append_pair("resource", RESOURCE)
            .append_pair("state", params.state)
            .append_pair("nonce", params.nonce)
            .append_pair("code_challenge", params.code_challenge)
            .append_pair("code_challenge_method", "S256");
        if params.force_consent {
            query.append_pair("prompt", "consent");
        }
    }
    Ok(url)
}

/// The client the code exchange uses. A new registration must come back with
/// an issued client; a returning sign-in may omit it, but must not change it.
pub fn issued_client_id(pending: Option<&str>, callback: Option<&str>) -> Result<String> {
    let callback = callback.map(str::trim).filter(|value| !value.is_empty());
    match (pending, callback) {
        (None, None) => bail!("ChatGPT registration did not return a client ID"),
        (None, Some(DYNAMIC_CLIENT_ID)) => {
            bail!("ChatGPT registration returned the registration entrypoint, not a client ID")
        }
        (None, Some(issued)) => {
            crate::auth::codex::validate_client_id(issued)
                .context("ChatGPT registration returned an invalid client ID")?;
            Ok(issued.to_string())
        }
        (Some(saved), Some(returned)) if returned != saved => bail!(
            "ChatGPT sign-in returned client `{returned}`, not the saved `{saved}`; refusing to mix registrations"
        ),
        (Some(saved), _) => Ok(saved.to_string()),
    }
}

/// The token endpoint's response for this route.
#[derive(Debug, Clone, Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub expires_in: Option<i64>,
    #[serde(default)]
    pub id_token: Option<String>,
    #[serde(default)]
    pub scope: Option<String>,
}

impl TokenResponse {
    pub fn scopes(&self) -> Vec<String> {
        split_scopes(self.scope.as_deref())
    }
}

fn split_scopes(scope: Option<&str>) -> Vec<String> {
    scope
        .unwrap_or_default()
        .split_whitespace()
        .map(str::to_string)
        .collect()
}

pub fn grants_plan_usage(scopes: &[String]) -> bool {
    scopes.iter().any(|scope| scope == PLAN_SCOPE)
}

async fn post_token_form(
    token_url: &str,
    form: &[(&str, &str)],
    what: &str,
) -> Result<TokenResponse> {
    let response = http_client()?
        .post(token_url)
        .form(form)
        .send()
        .await
        .with_context(|| format!("{what} ChatGPT token"))?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        bail!("ChatGPT token {what} failed ({status}): {body}");
    }
    response
        .json()
        .await
        .with_context(|| format!("parse ChatGPT {what} response"))
}

/// Exchange an authorization code with the issued client (never
/// `dynamic_agent_client`), the same redirect URI, and the same resource.
pub async fn exchange_code(
    token_url: &str,
    client_id: &str,
    code: &str,
    verifier: &str,
    redirect_uri: &str,
) -> Result<TokenResponse> {
    post_token_form(
        token_url,
        &[
            ("grant_type", "authorization_code"),
            ("client_id", client_id),
            ("code", code),
            ("code_verifier", verifier),
            ("redirect_uri", redirect_uri),
            ("resource", RESOURCE),
        ],
        "exchange",
    )
    .await
}

/// Refresh with the client saved on the token set. `scope` is omitted so the
/// grant is kept as it is.
pub async fn refresh_with_token_at(
    token_url: &str,
    client_id: &str,
    refresh_token: &str,
) -> Result<CodexAuth> {
    let token = post_token_form(
        token_url,
        &[
            ("grant_type", "refresh_token"),
            ("client_id", client_id),
            ("refresh_token", refresh_token),
            ("resource", RESOURCE),
        ],
        "refresh",
    )
    .await?;
    let scopes = token.scopes();
    Ok(CodexAuth {
        expires_at: expires_at(token.expires_in),
        account_id: None,
        email: None,
        client_id: Some(client_id.to_string()),
        open_source: Some(OpenSourceGrant {
            id_token: token.id_token,
            scopes,
            subject: None,
        }),
        access_token: token.access_token,
        refresh_token: token.refresh_token,
    })
}

/// End the renewable session at sign-out. An empty `200` is success, also for
/// a token that was already invalid.
pub async fn revoke_refresh_token(client_id: &str, refresh_token: &str) -> Result<()> {
    revoke_refresh_token_at(&Endpoints::production().revoke, client_id, refresh_token).await
}

pub async fn revoke_refresh_token_at(
    revoke_url: &str,
    client_id: &str,
    refresh_token: &str,
) -> Result<()> {
    let response = http_client()?
        .post(revoke_url)
        .form(&[
            ("token", refresh_token),
            ("token_type_hint", "refresh_token"),
            ("client_id", client_id),
        ])
        .send()
        .await
        .context("revoke ChatGPT session")?;
    if response.status().is_success() {
        return Ok(());
    }
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    bail!("ChatGPT session revocation failed ({status}): {body}")
}

/// Clear the saved ChatGPT login, and when it came from this route, revoke
/// its refresh token in the background. Returns whether a login existed. The
/// registration and host ID stay for the next sign-in.
pub fn clear_login(settings: &SettingsStore) -> Result<bool> {
    let revocable = settings.snapshot().codex_auth().and_then(|auth| {
        auth.open_source.as_ref()?;
        Some((auth.client_id.clone()?, auth.refresh_token.clone()?))
    });
    let existed = settings.clear_codex_auth()?;
    if let Some((client_id, refresh_token)) = revocable
        && let Ok(runtime) = tokio::runtime::Handle::try_current()
    {
        runtime.spawn(async move {
            if let Err(error) = revoke_refresh_token(&client_id, &refresh_token).await {
                // The tokens are already gone locally; the user can still
                // disconnect the app in ChatGPT settings.
                tracing::warn!(error = %error, "ChatGPT session revocation not confirmed");
            }
        });
    }
    Ok(existed)
}

fn expires_at(expires_in: Option<i64>) -> Option<i64> {
    expires_in
        .filter(|seconds| *seconds > 0)
        .map(|seconds| crate::auth::codex::now_epoch_millis() + seconds.saturating_mul(1000))
}

// ---------------------------------------------------------------------------
// ID-token validation
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct Jwks {
    pub keys: Vec<Jwk>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Jwk {
    #[serde(default)]
    pub kid: Option<String>,
    pub kty: String,
    #[serde(default)]
    pub n: Option<String>,
    #[serde(default)]
    pub e: Option<String>,
}

pub async fn fetch_jwks(jwks_url: &str) -> Result<Jwks> {
    let response = http_client()?
        .get(jwks_url)
        .send()
        .await
        .context("fetch OpenAI signing keys")?;
    if !response.status().is_success() {
        bail!(
            "fetching OpenAI signing keys failed ({})",
            response.status()
        );
    }
    response.json().await.context("parse OpenAI signing keys")
}

/// The identity a validated ID token carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub subject: String,
    pub email: Option<String>,
}

/// Verify an ID token: RS256 signature against `jwks`, then issuer, audience
/// (the issued client), expiry, and the nonce of this attempt.
pub fn validate_id_token(
    id_token: &str,
    jwks: &Jwks,
    issuer: &str,
    client_id: &str,
    nonce: &str,
    now_secs: i64,
) -> Result<Identity> {
    let mut parts = id_token.split('.');
    let (Some(header_b64), Some(payload_b64), Some(signature_b64), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        bail!("ID token is not a JWS compact token");
    };
    let decode = |segment: &str| {
        base64::Engine::decode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, segment)
            .context("ID token segment is not base64url")
    };
    let header: Value = serde_json::from_slice(&decode(header_b64)?).context("ID token header")?;
    if header.get("alg").and_then(Value::as_str) != Some("RS256") {
        bail!("ID token is not signed with RS256");
    }
    let kid = header.get("kid").and_then(Value::as_str);
    let key = jwks
        .keys
        .iter()
        .filter(|key| key.kty == "RSA")
        .find(|key| kid.is_none() || key.kid.as_deref() == kid)
        .ok_or_else(|| anyhow!("no OpenAI signing key matches the ID token"))?;
    let (Some(n), Some(e)) = (key.n.as_deref(), key.e.as_deref()) else {
        bail!("OpenAI signing key is missing its modulus or exponent");
    };
    let public_key = ring::signature::RsaPublicKeyComponents {
        n: decode(n)?,
        e: decode(e)?,
    };
    let signed = format!("{header_b64}.{payload_b64}");
    public_key
        .verify(
            &ring::signature::RSA_PKCS1_2048_8192_SHA256,
            signed.as_bytes(),
            &decode(signature_b64)?,
        )
        .map_err(|_| anyhow!("ID token signature does not verify"))?;

    let claims: Value = serde_json::from_slice(&decode(payload_b64)?).context("ID token claims")?;
    if claims.get("iss").and_then(Value::as_str) != Some(issuer) {
        bail!("ID token issuer is not {issuer}");
    }
    let audience_ok = match claims.get("aud") {
        Some(Value::String(aud)) => aud == client_id,
        Some(Value::Array(auds)) => auds.iter().any(|aud| aud.as_str() == Some(client_id)),
        _ => false,
    };
    if !audience_ok {
        bail!("ID token was not issued to client {client_id}");
    }
    let exp = claims
        .get("exp")
        .and_then(Value::as_i64)
        .ok_or_else(|| anyhow!("ID token has no expiry"))?;
    if exp + CLOCK_SKEW_SECS < now_secs {
        bail!("ID token has expired");
    }
    if claims.get("nonce").and_then(Value::as_str) != Some(nonce) {
        bail!("ID token nonce does not match this sign-in");
    }
    let subject = claims
        .get("sub")
        .and_then(Value::as_str)
        .filter(|sub| !sub.is_empty())
        .ok_or_else(|| anyhow!("ID token has no subject"))?
        .to_string();
    let email = claims
        .get("email")
        .and_then(Value::as_str)
        .or_else(|| {
            claims
                .get("https://api.openai.com/profile")
                .and_then(|profile| profile.get("email"))
                .and_then(Value::as_str)
        })
        .filter(|email| !email.is_empty())
        .map(str::to_string);
    Ok(Identity { subject, email })
}

// ---------------------------------------------------------------------------
// Browser sign-in
// ---------------------------------------------------------------------------

/// Sign in through the open-source route in the system browser, save the
/// registration, and return the token set (the caller saves it like any
/// other ChatGPT login).
pub async fn login_with_browser(settings: &SettingsStore) -> Result<CodexAuth> {
    login_with(&Endpoints::production(), settings, |url| {
        crate::auth::oauth_flow::open_browser(url)
    })
    .await
}

/// [`login_with_browser`] against arbitrary endpoints, with the browser
/// launch injected so tests can drive the callback.
pub async fn login_with<F>(
    endpoints: &Endpoints,
    settings: &SettingsStore,
    open: F,
) -> Result<CodexAuth>
where
    F: FnOnce(&str) -> Result<()>,
{
    let host_id = settings.ensure_chatgpt_host_id(new_host_id)?;
    let snapshot = settings.snapshot();
    let registration = snapshot.chatgpt_registration().cloned();
    // Only a hint saved by the same registration identifies the right account.
    let id_token_hint = snapshot
        .codex_auth()
        .filter(|auth| {
            registration
                .as_ref()
                .is_some_and(|reg| auth.client_id.as_deref() == Some(reg.client_id.as_str()))
        })
        .and_then(|auth| auth.open_source.as_ref())
        .and_then(|grant| grant.id_token.clone());
    let force_consent = registration.as_ref().is_some_and(|_| {
        // A login without the plan scope means it was declined last time.
        snapshot
            .codex_auth()
            .and_then(|auth| auth.open_source.as_ref())
            .is_some_and(|grant| !grants_plan_usage(&grant.scopes))
    });

    let listener = bind_callback_listener().await?;
    let port = listener.local_addr().context("callback address")?.port();
    let redirect_uri = format!("http://127.0.0.1:{port}{CALLBACK_PATH}");
    let state = crate::auth::oauth_flow::random_token(32);
    let nonce = crate::auth::oauth_flow::random_token(32);
    let verifier = crate::auth::oauth_flow::random_pkce_verifier();
    let challenge = crate::auth::oauth_flow::pkce_challenge(&verifier);
    let url = authorize_url(
        endpoints,
        &AuthorizeParams {
            redirect_uri: &redirect_uri,
            host_id: &host_id,
            registration: registration.as_ref(),
            id_token_hint: id_token_hint.as_deref(),
            force_consent,
            state: &state,
            nonce: &nonce,
            code_challenge: &challenge,
        },
    )?;
    open(url.as_str())?;
    let params = tokio::time::timeout(USER_AUTH_TIMEOUT, wait_for_callback(listener, &state))
        .await
        .map_err(|_| anyhow!("ChatGPT sign-in timed out"))??;
    if let Some(error) = params.get("error") {
        if error == "access_denied" {
            bail!("ChatGPT sign-in was declined; nothing was saved");
        }
        bail!("ChatGPT sign-in failed: {error}");
    }
    let code = params
        .get("code")
        .ok_or_else(|| anyhow!("ChatGPT sign-in callback had no authorization code"))?;
    let client_id = issued_client_id(
        registration.as_ref().map(|reg| reg.client_id.as_str()),
        params.get("client_id").map(String::as_str),
    )?;

    let token = exchange_code(&endpoints.token, &client_id, code, &verifier, &redirect_uri).await?;
    let id_token = token
        .id_token
        .clone()
        .ok_or_else(|| anyhow!("ChatGPT sign-in returned no ID token"))?;
    let jwks = fetch_jwks(&endpoints.jwks).await?;
    let now_secs = crate::auth::codex::now_epoch_millis() / 1000;
    let identity = validate_id_token(
        &id_token,
        &jwks,
        &endpoints.issuer,
        &client_id,
        &nonce,
        now_secs,
    )?;
    if let Some(registration) = &registration
        && registration.subject != identity.subject
    {
        bail!(
            "ChatGPT signed in a different account than the saved registration; refusing to replace its credentials"
        );
    }

    // Save the registration as soon as identity is proven, so a declined
    // plan scope still reuses this client on the next attempt.
    settings.set_chatgpt_registration(Some(ChatGptRegistration {
        client_id: client_id.clone(),
        subject: identity.subject.clone(),
        email: identity
            .email
            .clone()
            .or_else(|| registration.as_ref().and_then(|reg| reg.email.clone())),
    }))?;

    let scopes = token.scopes();
    let auth = CodexAuth {
        access_token: token.access_token,
        refresh_token: token.refresh_token,
        expires_at: expires_at(token.expires_in),
        account_id: None,
        email: identity.email,
        client_id: Some(client_id),
        open_source: Some(OpenSourceGrant {
            id_token: Some(id_token),
            scopes: scopes.clone(),
            subject: Some(identity.subject),
        }),
    };
    if !grants_plan_usage(&scopes) {
        // Keep the sign-in so the retry asks for consent with this client,
        // but do not hand back a login no turn can use.
        settings.set_codex_auth(auth)?;
        bail!(
            "ChatGPT plan use was not granted ({PLAN_SCOPE} missing). Sign in again to allow it, \
             or choose another provider such as an OpenAI API key."
        );
    }
    Ok(auth)
}

async fn bind_callback_listener() -> Result<tokio::net::TcpListener> {
    // Loopback on 127.0.0.1 only; the port may vary between sign-ins.
    match tokio::net::TcpListener::bind(("127.0.0.1", PREFERRED_CALLBACK_PORT)).await {
        Ok(listener) => Ok(listener),
        Err(_) => tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .context("bind ChatGPT sign-in callback on 127.0.0.1"),
    }
}

/// Wait for the callback that carries this attempt's `state`, answer it, and
/// return its query parameters (a `code` or an OAuth `error`).
async fn wait_for_callback(
    listener: tokio::net::TcpListener,
    expected_state: &str,
) -> Result<HashMap<String, String>> {
    loop {
        let (mut socket, _) = listener.accept().await.context("accept sign-in callback")?;
        let mut buffer = vec![0u8; 16 * 1024];
        let n = socket
            .read(&mut buffer)
            .await
            .context("read sign-in callback")?;
        let request = String::from_utf8_lossy(&buffer[..n]);
        let path = request
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .unwrap_or("/");
        let parsed =
            Url::parse(&format!("http://127.0.0.1{path}")).context("parse callback URL")?;
        let params: HashMap<_, _> = parsed.query_pairs().into_owned().collect();
        let (status, message, done) = if parsed.path() != CALLBACK_PATH {
            ("404 Not Found", "Unexpected callback path.", false)
        } else if params.get("state").map(String::as_str) != Some(expected_state) {
            ("400 Bad Request", "The sign-in state did not match.", false)
        } else if params.contains_key("error") {
            ("400 Bad Request", "ChatGPT sign-in did not complete.", true)
        } else {
            (
                "200 OK",
                "Yolop ChatGPT sign-in received. You can return to the terminal.",
                true,
            )
        };
        let body = crate::auth::oauth_flow::callback_page(status, message, "ChatGPT");
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        socket
            .write_all(response.as_bytes())
            .await
            .context("answer sign-in callback")?;
        if done {
            return Ok(params);
        }
    }
}

fn http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .build()
        .context("build ChatGPT auth HTTP client")
}

#[cfg(test)]
pub(crate) mod test_support {
    //! An RSA test key and a signer, so tests mint ID tokens the validator
    //! accepts. The key exists only for these tests.
    use base64::Engine as _;

    pub const TEST_KID: &str = "yolop-test-key";
    const TEST_KEY_PKCS1: &str = include_str!("../../tests/fixtures/siwc_test_rsa_pkcs1.b64");

    fn b64(bytes: &[u8]) -> String {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
    }

    fn key_pair() -> ring::rsa::KeyPair {
        let der = base64::engine::general_purpose::STANDARD
            .decode(TEST_KEY_PKCS1.split_whitespace().collect::<String>())
            .expect("fixture base64");
        ring::rsa::KeyPair::from_der(&der).expect("fixture key")
    }

    /// The JWKS document publishing the test key.
    pub fn jwks_json() -> String {
        use ring::signature::KeyPair as _;
        let pair = key_pair();
        let public = pair.public_key();
        let components: ring::rsa::PublicKeyComponents<Vec<u8>> = public.into();
        serde_json::json!({
            "keys": [{
                "kty": "RSA",
                "kid": TEST_KID,
                "alg": "RS256",
                "use": "sig",
                "n": b64(&components.n),
                "e": b64(&components.e),
            }]
        })
        .to_string()
    }

    /// Sign `claims` as an RS256 JWT with the test key.
    pub fn sign(claims: &serde_json::Value) -> String {
        let header = serde_json::json!({ "alg": "RS256", "kid": TEST_KID, "typ": "JWT" });
        let signed = format!(
            "{}.{}",
            b64(header.to_string().as_bytes()),
            b64(claims.to_string().as_bytes())
        );
        let pair = key_pair();
        let mut signature = vec![0u8; pair.public().modulus_len()];
        pair.sign(
            &ring::signature::RSA_PKCS1_SHA256,
            &ring::rand::SystemRandom::new(),
            signed.as_bytes(),
            &mut signature,
        )
        .expect("sign");
        format!("{signed}.{}", b64(&signature))
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{jwks_json, sign};
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    const ISSUER: &str = "https://auth.openai.com";

    fn jwks() -> Jwks {
        serde_json::from_str(&jwks_json()).unwrap()
    }

    fn claims(client: &str, nonce: &str, exp: i64) -> Value {
        serde_json::json!({
            "iss": ISSUER,
            "aud": [client],
            "sub": "user-sub-1",
            "email": "user@example.com",
            "exp": exp,
            "iat": exp - 3600,
            "nonce": nonce,
        })
    }

    fn query(url: &Url) -> HashMap<String, String> {
        url.query_pairs().into_owned().collect()
    }

    #[test]
    fn sign_in_route_defaults_to_codex_and_env_wins() {
        assert_eq!(resolve_sign_in(None, None).unwrap(), SignInRoute::Codex);
        assert_eq!(
            resolve_sign_in(Some(" "), Some("")).unwrap(),
            SignInRoute::Codex
        );
        assert_eq!(
            resolve_sign_in(None, Some("open-source")).unwrap(),
            SignInRoute::OpenSource
        );
        assert_eq!(
            resolve_sign_in(Some("codex"), Some("open-source")).unwrap(),
            SignInRoute::Codex
        );
        let err = resolve_sign_in(Some("chatgpt"), None).unwrap_err();
        assert!(format!("{err:#}").contains(SIGN_IN_ENV), "{err:#}");
    }

    #[test]
    fn host_id_is_a_uuid_v4_urn() {
        let id = new_host_id();
        let uuid = id.strip_prefix("urn:uuid:").expect("urn prefix");
        let groups: Vec<_> = uuid.split('-').map(str::len).collect();
        assert_eq!(groups, vec![8, 4, 4, 4, 12]);
        assert_eq!(&uuid[14..15], "4", "version nibble: {id}");
        assert!(
            matches!(&uuid[19..20], "8" | "9" | "a" | "b"),
            "variant: {id}"
        );
        assert_ne!(id, new_host_id());
    }

    #[test]
    fn first_sign_in_registers_with_the_dynamic_client() {
        let url = authorize_url(
            &Endpoints::production(),
            &AuthorizeParams {
                redirect_uri: "http://127.0.0.1:1455/auth/callback",
                host_id: "urn:uuid:host",
                registration: None,
                id_token_hint: Some("ignored-without-registration"),
                force_consent: false,
                state: "st",
                nonce: "no",
                code_challenge: "ch",
            },
        )
        .unwrap();
        assert!(
            url.as_str()
                .starts_with("https://auth.openai.com/api/accounts/authorize?")
        );
        let q = query(&url);
        assert_eq!(q["client_id"], DYNAMIC_CLIENT_ID);
        assert_eq!(q["agent_name_hint"], AGENT_NAME_HINT);
        assert_eq!(q["ext_agent_host_id"], "urn:uuid:host");
        assert_eq!(q["scope"], SCOPE);
        assert_eq!(q["resource"], RESOURCE);
        assert_eq!(q["response_type"], "code");
        assert_eq!(q["code_challenge_method"], "S256");
        assert_eq!(q["nonce"], "no");
        assert!(!q.contains_key("id_token_hint"));
        assert!(!q.contains_key("prompt"));
    }

    #[test]
    fn returning_sign_in_reuses_the_issued_client_without_name_hint() {
        let registration = ChatGptRegistration {
            client_id: "oaiapp_issued".to_string(),
            subject: "user-sub-1".to_string(),
            email: Some("user@example.com".to_string()),
        };
        let url = authorize_url(
            &Endpoints::production(),
            &AuthorizeParams {
                redirect_uri: "http://127.0.0.1:5555/auth/callback",
                host_id: "urn:uuid:host",
                registration: Some(&registration),
                id_token_hint: Some("old.id.token"),
                force_consent: true,
                state: "st",
                nonce: "no",
                code_challenge: "ch",
            },
        )
        .unwrap();
        let q = query(&url);
        assert_eq!(q["client_id"], "oaiapp_issued");
        assert!(!q.contains_key("agent_name_hint"));
        assert_eq!(q["ext_agent_host_id"], "urn:uuid:host");
        assert_eq!(q["id_token_hint"], "old.id.token");
        assert_eq!(q["login_hint"], "user@example.com");
        assert_eq!(q["prompt"], "consent");
    }

    #[test]
    fn callback_client_rules() {
        assert_eq!(
            issued_client_id(None, Some("oaiapp_new")).unwrap(),
            "oaiapp_new"
        );
        assert!(issued_client_id(None, None).is_err());
        assert!(issued_client_id(None, Some(DYNAMIC_CLIENT_ID)).is_err());
        assert!(issued_client_id(None, Some("bad id")).is_err());
        assert_eq!(
            issued_client_id(Some("oaiapp_saved"), None).unwrap(),
            "oaiapp_saved"
        );
        assert_eq!(
            issued_client_id(Some("oaiapp_saved"), Some("oaiapp_saved")).unwrap(),
            "oaiapp_saved"
        );
        assert!(issued_client_id(Some("oaiapp_saved"), Some("oaiapp_other")).is_err());
    }

    #[test]
    fn id_token_validation_accepts_a_good_token() {
        // A fresh nonce per run, the same way a real sign-in draws one.
        let nonce = crate::auth::oauth_flow::random_token(32);
        let token = sign(&claims("oaiapp_issued", &nonce, 2_000_000_000));
        let identity = validate_id_token(
            &token,
            &jwks(),
            ISSUER,
            "oaiapp_issued",
            &nonce,
            1_900_000_000,
        )
        .unwrap();
        assert_eq!(identity.subject, "user-sub-1");
        assert_eq!(identity.email.as_deref(), Some("user@example.com"));
    }

    #[test]
    fn id_token_validation_rejects_bad_claims_and_signatures() {
        let now = 1_900_000_000;
        let nonce = crate::auth::oauth_flow::random_token(32);
        let good = claims("oaiapp_issued", &nonce, 2_000_000_000);
        let check = |token: &str| {
            validate_id_token(token, &jwks(), ISSUER, "oaiapp_issued", &nonce, now)
                .map(|_| ())
                .unwrap_err()
                .to_string()
        };
        let mut wrong = good.clone();
        wrong["aud"] = serde_json::json!("oaiapp_other");
        assert!(check(&sign(&wrong)).contains("not issued to"));
        let mut wrong = good.clone();
        wrong["iss"] = serde_json::json!("https://evil.example");
        assert!(check(&sign(&wrong)).contains("issuer"));
        let mut wrong = good.clone();
        wrong["nonce"] = serde_json::json!("other");
        assert!(check(&sign(&wrong)).contains("nonce"));
        let mut wrong = good.clone();
        wrong["exp"] = serde_json::json!(now - 3600);
        assert!(check(&sign(&wrong)).contains("expired"));
        // A token whose payload was swapped after signing.
        let token = sign(&good);
        let mut parts: Vec<&str> = token.split('.').collect();
        let forged = base64::Engine::encode(
            &base64::engine::general_purpose::URL_SAFE_NO_PAD,
            serde_json::json!({"sub": "attacker"}).to_string(),
        );
        parts[1] = &forged;
        assert!(check(&parts.join(".")).contains("signature"));
        // An unsigned token.
        let unsigned = format!(
            "{}.{}.",
            base64::Engine::encode(
                &base64::engine::general_purpose::URL_SAFE_NO_PAD,
                r#"{"alg":"none"}"#
            ),
            parts[1]
        );
        assert!(check(&unsigned).contains("RS256"));
    }

    /// A one-thread HTTP mock: `handler` answers each request (method + path +
    /// body) and every raw request is recorded.
    fn serve<H>(handler: H) -> (String, Arc<Mutex<Vec<String>>>)
    where
        H: Fn(&str, &str) -> (u16, String) + Send + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen_clone = seen.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let mut request = Vec::new();
                let mut buffer = [0u8; 8192];
                let header_end = loop {
                    let n = stream.read(&mut buffer).unwrap_or(0);
                    if n == 0 {
                        break None;
                    }
                    request.extend_from_slice(&buffer[..n]);
                    if let Some(i) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                        break Some(i + 4);
                    }
                };
                let Some(header_end) = header_end else {
                    continue;
                };
                let head = String::from_utf8_lossy(&request[..header_end]).to_string();
                let length = head
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                while request.len() < header_end + length {
                    let n = stream.read(&mut buffer).unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    request.extend_from_slice(&buffer[..n]);
                }
                let text = String::from_utf8_lossy(&request).to_string();
                seen_clone.lock().unwrap().push(text.clone());
                let request_line = head.lines().next().unwrap_or_default().to_string();
                let body = &text[header_end.min(text.len())..];
                let (status, response_body) = handler(&request_line, body);
                let response = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response_body}",
                    response_body.len()
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });
        (base, seen)
    }

    fn form(body: &str) -> HashMap<String, String> {
        Url::parse(&format!("http://form/?{body}"))
            .map(|url| url.query_pairs().into_owned().collect())
            .unwrap_or_default()
    }

    #[tokio::test]
    async fn refresh_sends_the_saved_client_and_resource() {
        let (base, seen) = serve(|_, _| {
            (
                200,
                r#"{"access_token":"new","refresh_token":"next","expires_in":3600,"scope":"openid chatgpt.tokens.use.direct","id_token":"fresh.id.token"}"#.to_string(),
            )
        });
        let auth = refresh_with_token_at(&format!("{base}/token"), "oaiapp_issued", "old")
            .await
            .unwrap();
        let request = seen.lock().unwrap()[0].clone();
        let body = form(request.split("\r\n\r\n").nth(1).unwrap());
        assert_eq!(body["grant_type"], "refresh_token");
        assert_eq!(body["client_id"], "oaiapp_issued");
        assert_eq!(body["refresh_token"], "old");
        assert_eq!(body["resource"], RESOURCE);
        assert!(!body.contains_key("scope"), "refresh must keep the grant");
        assert_eq!(auth.client_id.as_deref(), Some("oaiapp_issued"));
        assert_eq!(auth.refresh_token.as_deref(), Some("next"));
        let grant = auth.open_source.expect("open-source grant");
        assert!(grants_plan_usage(&grant.scopes));
        assert_eq!(grant.id_token.as_deref(), Some("fresh.id.token"));
    }

    #[tokio::test]
    async fn refresh_failure_keeps_the_error_code() {
        let (base, _) = serve(|_, _| (400, r#"{"error":"refresh_token_reused"}"#.to_string()));
        let err = refresh_with_token_at(&format!("{base}/token"), "oaiapp_issued", "old")
            .await
            .unwrap_err();
        assert!(crate::auth::codex::is_refresh_token_reused(&err), "{err:#}");
    }

    #[tokio::test]
    async fn revoke_posts_the_refresh_token_with_its_client() {
        let (base, seen) = serve(|_, _| (200, String::new()));
        revoke_refresh_token_at(&format!("{base}/revoke"), "oaiapp_issued", "rt")
            .await
            .unwrap();
        let request = seen.lock().unwrap()[0].clone();
        let body = form(request.split("\r\n\r\n").nth(1).unwrap());
        assert_eq!(body["token"], "rt");
        assert_eq!(body["token_type_hint"], "refresh_token");
        assert_eq!(body["client_id"], "oaiapp_issued");

        let (base, _) = serve(|_, _| (503, "{}".to_string()));
        assert!(
            revoke_refresh_token_at(&format!("{base}/revoke"), "oaiapp_issued", "rt")
                .await
                .is_err()
        );
    }

    /// The mock auth server: a token endpoint that mints an ID token for the
    /// nonce in the pending authorize URL, and the JWKS endpoint.
    fn mock_auth_server(
        nonce: Arc<Mutex<String>>,
        scope: &'static str,
        subject: &'static str,
    ) -> (Endpoints, Arc<Mutex<Vec<String>>>) {
        let (base, seen) = serve(move |line, body| {
            if line.starts_with("GET /jwks") {
                return (200, jwks_json());
            }
            let form = form(body);
            let client = form.get("client_id").cloned().unwrap_or_default();
            let mut claims = claims(&client, &nonce.lock().unwrap(), 4_000_000_000);
            claims["sub"] = serde_json::json!(subject);
            let id_token = sign(&claims);
            (
                200,
                serde_json::json!({
                    "access_token": "access-1",
                    "refresh_token": "refresh-1",
                    "expires_in": 3600,
                    "token_type": "Bearer",
                    "scope": scope,
                    "id_token": id_token,
                })
                .to_string(),
            )
        });
        (
            Endpoints {
                issuer: ISSUER.to_string(),
                authorize: "https://auth.openai.com/api/accounts/authorize".to_string(),
                token: format!("{base}/token"),
                revoke: format!("{base}/revoke"),
                jwks: format!("{base}/jwks"),
            },
            seen,
        )
    }

    /// A fake browser: read the authorize URL, remember its nonce, and hit the
    /// loopback callback the way OpenAI's redirect would.
    fn browser(
        nonce: Arc<Mutex<String>>,
        issued_client: Option<&'static str>,
        seen_urls: Arc<Mutex<Vec<String>>>,
    ) -> impl FnOnce(&str) -> Result<()> {
        move |url: &str| {
            let parsed = Url::parse(url).unwrap();
            let q = query(&parsed);
            *nonce.lock().unwrap() = q["nonce"].clone();
            seen_urls.lock().unwrap().push(url.to_string());
            let mut callback = Url::parse(&q["redirect_uri"]).unwrap();
            {
                let mut pairs = callback.query_pairs_mut();
                pairs.append_pair("code", "auth-code");
                pairs.append_pair("state", &q["state"]);
                if let Some(client) = issued_client {
                    pairs.append_pair("client_id", client);
                }
            }
            tokio::spawn(async move {
                let _ = reqwest::get(callback.as_str()).await;
            });
            Ok(())
        }
    }

    #[tokio::test]
    async fn first_sign_in_registers_exchanges_with_issued_client_and_saves_registration() {
        let tmp = tempfile::tempdir().unwrap();
        let settings = SettingsStore::open(tmp.path().join("settings.toml"));
        let nonce = Arc::new(Mutex::new(String::new()));
        let (endpoints, seen) = mock_auth_server(
            nonce.clone(),
            "openid email chatgpt.tokens.use.direct",
            "user-sub-1",
        );
        let urls = Arc::new(Mutex::new(Vec::new()));

        let auth = login_with(
            &endpoints,
            &settings,
            browser(nonce.clone(), Some("oaiapp_issued"), urls.clone()),
        )
        .await
        .unwrap();

        assert_eq!(auth.client_id.as_deref(), Some("oaiapp_issued"));
        let grant = auth.open_source.as_ref().unwrap();
        assert_eq!(grant.subject.as_deref(), Some("user-sub-1"));
        assert!(grant.id_token.is_some());
        let exchange = seen
            .lock()
            .unwrap()
            .iter()
            .find(|r| r.starts_with("POST /token"))
            .cloned()
            .unwrap();
        let body = form(exchange.split("\r\n\r\n").nth(1).unwrap());
        assert_eq!(body["grant_type"], "authorization_code");
        assert_eq!(body["client_id"], "oaiapp_issued");
        assert_eq!(body["code"], "auth-code");
        assert_eq!(body["resource"], RESOURCE);
        assert!(body["redirect_uri"].starts_with("http://127.0.0.1:"));
        let authorize = query(&Url::parse(&urls.lock().unwrap()[0]).unwrap());
        assert_eq!(authorize["client_id"], DYNAMIC_CLIENT_ID);
        assert_eq!(body["redirect_uri"], authorize["redirect_uri"]);

        let snapshot = settings.snapshot();
        let registration = snapshot.chatgpt_registration().unwrap();
        assert_eq!(registration.client_id, "oaiapp_issued");
        assert_eq!(registration.subject, "user-sub-1");
        let host_id = snapshot.chatgpt_host_id.clone().unwrap();
        assert_eq!(authorize["ext_agent_host_id"], host_id);

        // The next sign-in reuses the registration and the host ID, and the
        // callback may omit the client.
        settings.set_codex_auth(auth).unwrap();
        let urls = Arc::new(Mutex::new(Vec::new()));
        let again = login_with(&endpoints, &settings, browser(nonce, None, urls.clone()))
            .await
            .unwrap();
        assert_eq!(again.client_id.as_deref(), Some("oaiapp_issued"));
        let authorize = query(&Url::parse(&urls.lock().unwrap()[0]).unwrap());
        assert_eq!(authorize["client_id"], "oaiapp_issued");
        assert!(!authorize.contains_key("agent_name_hint"));
        assert!(authorize.contains_key("id_token_hint"));
        assert_eq!(authorize["ext_agent_host_id"], host_id);
    }

    #[tokio::test]
    async fn sign_in_without_plan_scope_is_refused_but_keeps_the_registration() {
        let tmp = tempfile::tempdir().unwrap();
        let settings = SettingsStore::open(tmp.path().join("settings.toml"));
        let nonce = Arc::new(Mutex::new(String::new()));
        let (endpoints, _) = mock_auth_server(nonce.clone(), "openid email", "user-sub-1");
        let err = login_with(
            &endpoints,
            &settings,
            browser(nonce.clone(), Some("oaiapp_issued"), Arc::default()),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains(PLAN_SCOPE), "{err:#}");
        assert_eq!(
            settings
                .snapshot()
                .chatgpt_registration()
                .unwrap()
                .client_id,
            "oaiapp_issued"
        );

        // The retry asks for consent again with the saved client.
        let urls = Arc::new(Mutex::new(Vec::new()));
        let _ = login_with(&endpoints, &settings, browser(nonce, None, urls.clone())).await;
        let authorize = query(&Url::parse(&urls.lock().unwrap()[0]).unwrap());
        assert_eq!(authorize["prompt"], "consent");
        assert_eq!(authorize["client_id"], "oaiapp_issued");
    }

    #[tokio::test]
    async fn returning_sign_in_rejects_a_different_account() {
        let tmp = tempfile::tempdir().unwrap();
        let settings = SettingsStore::open(tmp.path().join("settings.toml"));
        settings
            .set_chatgpt_registration(Some(ChatGptRegistration {
                client_id: "oaiapp_issued".to_string(),
                subject: "user-sub-1".to_string(),
                email: None,
            }))
            .unwrap();
        let nonce = Arc::new(Mutex::new(String::new()));
        let (endpoints, _) =
            mock_auth_server(nonce.clone(), "chatgpt.tokens.use.direct", "someone-else");
        let err = login_with(&endpoints, &settings, browser(nonce, None, Arc::default()))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("different account"), "{err:#}");
        assert!(settings.snapshot().codex_auth().is_none());
    }
}

//! Host adapter for Everruns' open-source ChatGPT login.
use crate::config::{CodexAuth, Settings, SettingsStore};
use anyhow::{Result, anyhow, bail};
use everruns_drivers::chatgpt::login::LoginAttempt;
pub use everruns_drivers::chatgpt::oauth::{Endpoints, grants_plan_usage, new_host_id};
pub const SIGN_IN_ENV: &str = "YOLOP_CHATGPT_SIGN_IN";

/// Which route a new ChatGPT sign-in takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignInRoute {
    /// The Codex CLI's client and the Codex backend.
    Codex,
    /// Dynamic registration and the public Responses API (the default).
    OpenSource,
}

/// The route a new sign-in takes when neither the env var nor the setting
/// names one.
pub const DEFAULT_SIGN_IN: SignInRoute = SignInRoute::OpenSource;

impl SignInRoute {
    pub fn as_str(self) -> &'static str {
        match self {
            SignInRoute::Codex => "codex",
            SignInRoute::OpenSource => "open-source",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "default" => Some(DEFAULT_SIGN_IN),
            "codex" => Some(Self::Codex),
            "open-source" | "opensource" | "oss" | "siwc" => Some(Self::OpenSource),
            _ => None,
        }
    }
}

/// The route a new sign-in takes: `YOLOP_CHATGPT_SIGN_IN`, then the
/// `chatgpt_sign_in` setting, then [`DEFAULT_SIGN_IN`].
pub fn configured_sign_in(settings: &Settings) -> Result<SignInRoute> {
    Ok(explicit_sign_in(settings)?.unwrap_or(DEFAULT_SIGN_IN))
}

/// The route the env var or the setting names, or `None` when neither does.
pub fn explicit_sign_in(settings: &Settings) -> Result<Option<SignInRoute>> {
    let env = std::env::var(SIGN_IN_ENV).ok();
    resolve_explicit_sign_in(env.as_deref(), settings.chatgpt_sign_in())
}

/// Pure precedence behind [`configured_sign_in`], for tests.
#[cfg(test)]
fn resolve_sign_in(env: Option<&str>, setting: Option<&str>) -> Result<SignInRoute> {
    Ok(resolve_explicit_sign_in(env, setting)?.unwrap_or(DEFAULT_SIGN_IN))
}

/// Pure precedence behind [`explicit_sign_in`]. Blank counts as unset; an
/// unknown value is an error naming its source.
fn resolve_explicit_sign_in(
    env: Option<&str>,
    setting: Option<&str>,
) -> Result<Option<SignInRoute>> {
    for (source, value) in [(SIGN_IN_ENV, env), ("chatgpt_sign_in", setting)] {
        let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
            continue;
        };
        return SignInRoute::parse(value)
            .map(Some)
            .ok_or_else(|| anyhow!("invalid {source} `{value}`; expected codex or open-source"));
    }
    Ok(None)
}

pub async fn login_with_browser(settings: &SettingsStore) -> Result<CodexAuth> {
    login_with(&Endpoints::production(), settings, |url| async move {
        crate::auth::codex::open_browser(&url).await
    })
    .await
}
pub async fn login_with<F, Fut>(
    endpoints: &Endpoints,
    settings: &SettingsStore,
    open: F,
) -> Result<CodexAuth>
where
    F: FnOnce(String) -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
{
    let host_id = settings.ensure_chatgpt_host_id(new_host_id)?;
    let snapshot = settings.snapshot();
    let registration = snapshot.chatgpt_registration().cloned();
    let grant = snapshot
        .codex_auth()
        .filter(|auth| {
            registration
                .as_ref()
                .is_some_and(|r| auth.client_id.as_deref() == Some(r.client_id.as_str()))
        })
        .and_then(|auth| auth.open_source.as_ref());
    let attempt = LoginAttempt::start(
        endpoints.clone(),
        "yolop",
        &host_id,
        registration.clone(),
        grant.and_then(|g| g.id_token.as_deref()),
        grant.is_some_and(|g| !grants_plan_usage(&g.scopes)),
    )
    .await?;
    open(attempt.authorize_url.clone()).await?;
    let auth = attempt.finish().await?;
    let grant = auth
        .open_source
        .as_ref()
        .ok_or_else(|| anyhow!("Missing plan grant"))?;
    settings.save_chatgpt_login(registration.as_ref(), auth.clone())?;
    if !grants_plan_usage(&grant.scopes) {
        bail!(
            "ChatGPT plan use was not granted. Sign in again to allow it, or choose an API provider."
        );
    }
    Ok(auth)
}
/// Confirm revocation before clearing the local login. Retain registration and host ID.
pub async fn clear_login(settings: &SettingsStore) -> Result<bool> {
    let _lease = crate::drivers::codex::settings_rotation_lock(settings.path()).await?;
    let Some(auth) = settings.refresh_codex_auth_from_disk_checked()? else {
        return Ok(false);
    };
    if auth.open_source.is_some() {
        let client = auth
            .client_id
            .as_deref()
            .ok_or_else(|| anyhow!("Missing revocation client"))?;
        let refresh = auth
            .refresh_token
            .as_deref()
            .ok_or_else(|| anyhow!("Missing revocation token"))?;
        everruns_drivers::chatgpt::oauth::revoke_refresh_token(client, refresh).await?;
    }
    settings.clear_codex_auth_under_lease()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn route_precedence() {
        assert_eq!(
            resolve_sign_in(None, None).unwrap(),
            SignInRoute::OpenSource
        );
        assert_eq!(
            resolve_sign_in(Some("codex"), Some("open-source")).unwrap(),
            SignInRoute::Codex
        );
        assert!(resolve_sign_in(Some("unknown"), None).is_err());
    }
}

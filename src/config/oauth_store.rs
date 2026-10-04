//! Cross-process write guards keep OAuth rotations coherent with other settings writes.
use super::*;
use std::{
    fs::File,
    ops::{Deref, DerefMut},
};

pub(crate) fn rotation_lock(path: &Path) -> Result<File> {
    private_lock(&path.with_extension("chatgpt.lock"))
}
pub(super) fn write_lock(path: &Path) -> Result<File> {
    private_lock(&path.with_extension("write.lock"))
}
fn private_lock(path: &Path) -> Result<File> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path)?;
    file.lock()?;
    Ok(file)
}
pub(super) struct UpdateGuard<'a> {
    pub(super) state: MutexGuard<'a, SettingsState>,
    pub(super) _file: File,
}
impl Deref for UpdateGuard<'_> {
    type Target = SettingsState;
    fn deref(&self) -> &Self::Target {
        &self.state
    }
}
impl DerefMut for UpdateGuard<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.state
    }
}

impl SettingsStore {
    /// Save registration and grant together; reject a callback whose saved
    /// identity changed while its browser sign-in was in flight.
    pub(crate) fn save_chatgpt_login(
        &self,
        expected: Option<&ChatGptRegistration>,
        auth: CodexAuth,
    ) -> Result<()> {
        let _lease = rotation_lock(self.path())?;
        let mut guard = self.lock_fresh_for_update()?;
        anyhow::ensure!(
            guard.base.chatgpt_registration.as_ref() == expected,
            "ChatGPT registration changed during sign-in. Try again."
        );
        let grant = auth.open_source.as_ref().context("Missing plan grant")?;
        let registration = ChatGptRegistration {
            client_id: auth.client_id.clone().context("Missing issuing client")?,
            subject: grant.subject.clone().context("Missing validated subject")?,
            email: auth.email.clone(),
        };
        guard.base.chatgpt_registration = Some(registration);
        guard.base.codex_auth = Some(auth);
        self.save_base_locked(&mut guard)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CodexAuth;
    fn auth(token: &str) -> CodexAuth {
        CodexAuth {
            access_token: token.into(),
            refresh_token: Some(format!("{token}-refresh")),
            expires_at: None,
            account_id: None,
            email: None,
            client_id: None,
            open_source: None,
        }
    }
    #[test]
    fn stale_refresh_cannot_overwrite_another_settings_instance_login() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");
        let first = SettingsStore::open(path.clone());
        first.set_codex_auth(auth("old")).unwrap();
        let second = SettingsStore::open(path);
        second.set_codex_auth(auth("new-login")).unwrap();
        let _lease = rotation_lock(first.path()).unwrap();
        assert!(
            !first
                .compare_and_set_codex_auth(&auth("old"), auth("stale-refresh"))
                .unwrap()
        );
        assert_eq!(
            first.refresh_codex_auth_from_disk_checked().unwrap(),
            Some(auth("new-login"))
        );
    }
    #[test]
    fn unrelated_settings_writer_preserves_the_rotated_pair() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");
        let first = SettingsStore::open(path.clone());
        first.set_codex_auth(auth("old")).unwrap();
        let second = SettingsStore::open(path);
        let _snapshot = second.snapshot();
        first.set_codex_auth(auth("rotated")).unwrap();
        second.set_chatgpt_registration(None).unwrap();
        assert_eq!(
            first.refresh_codex_auth_from_disk_checked().unwrap(),
            Some(auth("rotated"))
        );
    }

    #[test]
    fn concurrent_login_cannot_mix_another_registration_with_its_tokens() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");
        let first = SettingsStore::open(path.clone());
        let second = SettingsStore::open(path);
        let mut login = auth("first");
        login.client_id = Some("app_first".into());
        login.open_source = Some(OpenSourceGrant {
            id_token: None,
            scopes: vec![],
            subject: Some("subject_first".into()),
        });
        first.save_chatgpt_login(None, login.clone()).unwrap();
        let mut other = login.clone();
        other.client_id = Some("app_other".into());
        assert!(second.save_chatgpt_login(None, other.clone()).is_err());
        assert!(second.set_codex_auth(other).is_err());
        assert_eq!(
            first.refresh_codex_auth_from_disk_checked().unwrap(),
            Some(login)
        );
    }
}

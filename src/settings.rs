// Persistent yolop settings — preferred provider plus optional per-provider
// API tokens.
//
// Stored at `<config_dir>/yolop/settings.toml` (`~/.config/yolop/settings.toml`
// on Linux). The capabilities that own `/provider` and `/token` write
// through `SettingsStore` so user choices survive across runs.
//
// Tokens are written with 0o600 on Unix (owner-only) and are still less
// secure than a real secret manager — they sit in plain text on disk. The
// `/token <provider> clear` command removes a stored entry. Env vars
// (OPENAI_API_KEY, ANTHROPIC_API_KEY, …) continue to take precedence so a
// per-run override is always possible.

use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use toml::Table;
use toml::Value;

#[derive(Debug, Clone, Default)]
pub struct Settings {
    pub provider: Option<String>,
    pub tokens: BTreeMap<String, String>,
}

impl Settings {
    pub fn from_table(table: &Table) -> Self {
        let provider = table
            .get("provider")
            .and_then(Value::as_str)
            .map(str::to_string);
        let mut tokens = BTreeMap::new();
        if let Some(t) = table.get("tokens").and_then(Value::as_table) {
            for (k, v) in t {
                if let Some(s) = v.as_str() {
                    tokens.insert(k.clone(), s.to_string());
                }
            }
        }
        Self { provider, tokens }
    }

    pub fn to_table(&self) -> Table {
        let mut table = Table::new();
        if let Some(p) = &self.provider {
            table.insert("provider".to_string(), Value::String(p.clone()));
        }
        if !self.tokens.is_empty() {
            let mut tokens_table = Table::new();
            for (k, v) in &self.tokens {
                tokens_table.insert(k.clone(), Value::String(v.clone()));
            }
            table.insert("tokens".to_string(), Value::Table(tokens_table));
        }
        table
    }

    pub fn token_for(&self, provider: &str) -> Option<&str> {
        self.tokens.get(provider).map(String::as_str)
    }

    pub fn has_token(&self, provider: &str) -> bool {
        self.tokens.contains_key(provider)
    }
}

pub fn default_settings_path() -> Option<PathBuf> {
    dirs::config_dir().map(|p| p.join("yolop").join("settings.toml"))
}

pub fn load_from(path: &Path) -> Settings {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Settings::default();
    };
    let table: Table = toml::from_str(&text).unwrap_or_default();
    Settings::from_table(&table)
}

/// Write the settings file in place, truncating + rewriting. On Unix the
/// file is forced to mode 0o600 (owner-only) both at create time and
/// after open, so pre-existing files written with a broader default mode
/// are tightened on first save. The write is not crash-atomic — a power
/// loss mid-`write_all` can leave a truncated TOML file; load tolerates
/// that by falling back to defaults.
fn save_to(path: &Path, settings: &Settings) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("create settings dir {}", dir.display()))?;
    }
    let toml_text = toml::to_string(&settings.to_table()).context("serialize settings")?;

    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts
        .open(path)
        .with_context(|| format!("open settings {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // `mode()` on OpenOptions only applies when the file is created.
        // If the user (or an older yolop) wrote settings.toml with the
        // default 0o644, tighten it on every save so tokens never sit at
        // world-readable permissions.
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("tighten settings permissions {}", path.display()))?;
    }
    file.write_all(toml_text.as_bytes())
        .with_context(|| format!("write settings {}", path.display()))?;
    Ok(())
}

/// Thread-safe handle shared across capabilities. The cached `Settings` is
/// the source of truth in memory; mutations flush to disk synchronously.
/// The write is not crash-atomic — see `save_to` — but load tolerates a
/// truncated file by falling back to defaults.
pub struct SettingsStore {
    path: PathBuf,
    inner: Mutex<Settings>,
}

impl SettingsStore {
    pub fn open(path: PathBuf) -> Self {
        let settings = load_from(&path);
        Self {
            path,
            inner: Mutex::new(settings),
        }
    }

    pub fn snapshot(&self) -> Settings {
        self.inner.lock().expect("settings lock poisoned").clone()
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn set_provider(&self, provider: Option<String>) -> Result<()> {
        let mut guard = self.inner.lock().expect("settings lock poisoned");
        guard.provider = provider;
        save_to(&self.path, &guard)
    }

    pub fn set_token(&self, provider: String, token: String) -> Result<()> {
        let mut guard = self.inner.lock().expect("settings lock poisoned");
        guard.tokens.insert(provider, token);
        save_to(&self.path, &guard)
    }

    /// Returns whether a token was actually present before removal.
    pub fn clear_token(&self, provider: &str) -> Result<bool> {
        let mut guard = self.inner.lock().expect("settings lock poisoned");
        let existed = guard.tokens.remove(provider).is_some();
        save_to(&self.path, &guard)?;
        Ok(existed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_roundtrip_via_disk() {
        let tmp = tempfile::tempdir().expect("tmp");
        let path = tmp.path().join("nested").join("settings.toml");
        let store = SettingsStore::open(path.clone());
        assert!(store.snapshot().provider.is_none());
        store
            .set_provider(Some("anthropic".to_string()))
            .expect("save");

        let on_disk = std::fs::read_to_string(&path).expect("read");
        assert!(
            on_disk.contains("provider = \"anthropic\""),
            "expected TOML key/value, got: {on_disk}"
        );

        let reloaded = SettingsStore::open(path);
        assert_eq!(reloaded.snapshot().provider.as_deref(), Some("anthropic"));
    }

    #[test]
    fn token_roundtrip_via_disk() {
        let tmp = tempfile::tempdir().expect("tmp");
        let path = tmp.path().join("settings.toml");
        let store = SettingsStore::open(path.clone());
        store
            .set_token("openai".to_string(), "sk-test-abc".to_string())
            .expect("save");
        store
            .set_token("anthropic".to_string(), "anthropic-key".to_string())
            .expect("save");

        let on_disk = std::fs::read_to_string(&path).expect("read");
        assert!(
            on_disk.contains("[tokens]") && on_disk.contains("openai = \"sk-test-abc\""),
            "expected TOML tokens table, got: {on_disk}"
        );

        let reloaded = SettingsStore::open(path);
        assert_eq!(reloaded.snapshot().token_for("openai"), Some("sk-test-abc"));
        assert_eq!(
            reloaded.snapshot().token_for("anthropic"),
            Some("anthropic-key")
        );
        assert!(reloaded.snapshot().token_for("google").is_none());
    }

    #[test]
    fn clearing_token_removes_only_that_entry() {
        let tmp = tempfile::tempdir().expect("tmp");
        let path = tmp.path().join("settings.toml");
        let store = SettingsStore::open(path.clone());
        store
            .set_token("openai".to_string(), "sk-1".to_string())
            .expect("save");
        store
            .set_token("anthropic".to_string(), "anth-1".to_string())
            .expect("save");

        let removed = store.clear_token("openai").expect("clear");
        assert!(removed);

        let reloaded = SettingsStore::open(path);
        assert!(reloaded.snapshot().token_for("openai").is_none());
        assert_eq!(reloaded.snapshot().token_for("anthropic"), Some("anth-1"));
    }

    #[test]
    fn clearing_absent_token_reports_false_but_succeeds() {
        let tmp = tempfile::tempdir().expect("tmp");
        let path = tmp.path().join("settings.toml");
        let store = SettingsStore::open(path);
        let removed = store.clear_token("openai").expect("clear");
        assert!(!removed);
    }

    #[test]
    fn missing_file_yields_default() {
        let tmp = tempfile::tempdir().expect("tmp");
        let path = tmp.path().join("absent.toml");
        let store = SettingsStore::open(path);
        assert!(store.snapshot().provider.is_none());
        assert!(store.snapshot().tokens.is_empty());
    }

    #[test]
    fn malformed_file_yields_default() {
        let tmp = tempfile::tempdir().expect("tmp");
        let path = tmp.path().join("settings.toml");
        std::fs::write(&path, "this = is = not = toml").expect("write");
        let store = SettingsStore::open(path);
        assert!(store.snapshot().provider.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn save_tightens_permissions_on_preexisting_file() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().expect("tmp");
        let path = tmp.path().join("settings.toml");
        // Simulate a stale file from a previous yolop version that wrote
        // settings with the default 0o644.
        std::fs::write(&path, "provider = \"openai\"\n").expect("seed");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("chmod");

        let store = SettingsStore::open(path.clone());
        store
            .set_token("openai".to_string(), "sk-new".to_string())
            .expect("save");

        let mode = std::fs::metadata(&path).expect("meta").permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn save_writes_owner_only_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().expect("tmp");
        let path = tmp.path().join("settings.toml");
        let store = SettingsStore::open(path.clone());
        store
            .set_token("openai".to_string(), "sk-test".to_string())
            .expect("save");

        let mode = std::fs::metadata(&path).expect("meta").permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}

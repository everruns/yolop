// Persistent yolop settings — preferred provider, optional per-provider
// API tokens, per-provider model picks, custom endpoint base URLs, and
// small behavior toggles.
//
// Stored at `<config_dir>/yolop/settings.toml` (`~/.config/yolop/settings.toml`
// on Linux). The `/setup` capability writes through `SettingsStore` so user
// choices survive across runs.
//
// Tokens are written with 0o600 on Unix (owner-only) and are still less
// secure than a real secret manager — they sit in plain text on disk. The
// `/setup` command can remove a stored entry. Env vars
// (OPENAI_API_KEY, ANTHROPIC_API_KEY, …) continue to take precedence so a
// per-run override is always possible.

use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use toml::Table;
use toml::Value;

/// How aggressively yolop pauses for spoken ("soft") approval before
/// running critical actions. Soft approval is prompt-engineering, not a
/// hard gate: the chosen level is injected into the system prompt so the
/// model itself decides when to ask, batches safe calls, and the user
/// approves in plain text ("yes", "approved"). See `capabilities::approval`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ApprovalMode {
    /// Ask before any state-changing action — the lowest threshold.
    Protective,
    /// Ask only before clearly destructive, irreversible, or outward-facing
    /// actions. The default.
    #[default]
    Normal,
    /// Never pause for approval; act autonomously.
    Off,
}

impl ApprovalMode {
    /// Canonical lowercase name, as written to settings.toml and shown in
    /// the status bar.
    pub fn as_str(self) -> &'static str {
        match self {
            ApprovalMode::Protective => "protective",
            ApprovalMode::Normal => "normal",
            ApprovalMode::Off => "off",
        }
    }

    /// Parse a user- or config-supplied level. Accepts the canonical names
    /// plus a few intuitive aliases so natural-language requests ("be more
    /// paranoid", "yolo mode") and config files both resolve.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "protective" | "strict" | "paranoid" | "high" | "careful" => Some(Self::Protective),
            "normal" | "default" | "medium" | "standard" => Some(Self::Normal),
            "off" | "none" | "yolo" | "disabled" | "low" => Some(Self::Off),
            _ => None,
        }
    }
}

impl std::fmt::Display for ApprovalMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone)]
pub struct Settings {
    pub provider: Option<String>,
    pub tokens: BTreeMap<String, String>,
    /// Last model spec chosen per provider, in the provider-relative
    /// `model [effort]` form `/setup model` accepts. Lets a model pick
    /// survive restarts and provider switches.
    pub models: BTreeMap<String, String>,
    /// Endpoint base URLs keyed by provider. Today only `custom` (the
    /// generic OpenAI-compatible provider) writes here.
    pub base_urls: BTreeMap<String, String>,
    pub attribution: bool,
    /// Soft-approval paranoia level, injected into the system prompt each
    /// turn. Central, cross-session, surfaced in the status bar.
    pub approval_mode: ApprovalMode,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            provider: None,
            tokens: BTreeMap::new(),
            models: BTreeMap::new(),
            base_urls: BTreeMap::new(),
            attribution: true,
            approval_mode: ApprovalMode::Normal,
        }
    }
}

impl Settings {
    pub fn from_table(table: &Table) -> Self {
        let provider = table
            .get("provider")
            .and_then(Value::as_str)
            .map(str::to_string);
        let attribution = table
            .get("attribution")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        let approval_mode = table
            .get("approval_mode")
            .and_then(Value::as_str)
            .and_then(ApprovalMode::parse)
            .unwrap_or_default();
        let string_map = |key: &str| {
            let mut map = BTreeMap::new();
            if let Some(t) = table.get(key).and_then(Value::as_table) {
                for (k, v) in t {
                    if let Some(s) = v.as_str() {
                        map.insert(k.clone(), s.to_string());
                    }
                }
            }
            map
        };
        Self {
            provider,
            tokens: string_map("tokens"),
            models: string_map("models"),
            base_urls: string_map("base_urls"),
            attribution,
            approval_mode,
        }
    }

    pub fn to_table(&self) -> Table {
        let mut table = Table::new();
        if let Some(p) = &self.provider {
            table.insert("provider".to_string(), Value::String(p.clone()));
        }
        if !self.attribution {
            table.insert("attribution".to_string(), Value::Boolean(false));
        }
        // Only persist a non-default level so settings.toml stays sparse.
        if self.approval_mode != ApprovalMode::Normal {
            table.insert(
                "approval_mode".to_string(),
                Value::String(self.approval_mode.as_str().to_string()),
            );
        }
        let mut insert_map = |key: &str, map: &BTreeMap<String, String>| {
            if !map.is_empty() {
                let mut t = Table::new();
                for (k, v) in map {
                    t.insert(k.clone(), Value::String(v.clone()));
                }
                table.insert(key.to_string(), Value::Table(t));
            }
        };
        insert_map("tokens", &self.tokens);
        insert_map("models", &self.models);
        insert_map("base_urls", &self.base_urls);
        table
    }

    pub fn token_for(&self, provider: &str) -> Option<&str> {
        self.tokens.get(provider).map(String::as_str)
    }

    pub fn has_token(&self, provider: &str) -> bool {
        self.tokens.contains_key(provider)
    }

    pub fn model_for(&self, provider: &str) -> Option<&str> {
        self.models.get(provider).map(String::as_str)
    }

    pub fn base_url_for(&self, provider: &str) -> Option<&str> {
        self.base_urls.get(provider).map(String::as_str)
    }

    pub fn attribution_enabled(&self) -> bool {
        self.attribution
    }

    pub fn approval_mode(&self) -> ApprovalMode {
        self.approval_mode
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

/// Write the settings file atomically: stage into a sibling temp file,
/// `fsync`, then `rename` over the target. POSIX `rename` is atomic, so
/// concurrent readers and crashes see either the old contents or the
/// fully written new contents — never a truncated TOML. The temp file is
/// created with mode 0o600 on Unix, so the resulting file is owner-only
/// regardless of any prior file's permissions.
fn save_to(path: &Path, settings: &Settings) -> Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    std::fs::create_dir_all(&parent)
        .with_context(|| format!("create settings dir {}", parent.display()))?;
    let toml_text = toml::to_string(&settings.to_table()).context("serialize settings")?;

    let file_name = path
        .file_name()
        .with_context(|| format!("settings path has no file name: {}", path.display()))?;
    let mut tmp_name = std::ffi::OsString::from(".");
    tmp_name.push(file_name);
    tmp_name.push(format!(".tmp.{}", std::process::id()));
    let tmp_path = parent.join(tmp_name);

    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let write_result = (|| -> Result<()> {
        let mut file = opts
            .open(&tmp_path)
            .with_context(|| format!("open temp settings {}", tmp_path.display()))?;
        file.write_all(toml_text.as_bytes())
            .with_context(|| format!("write temp settings {}", tmp_path.display()))?;
        file.sync_all()
            .with_context(|| format!("sync temp settings {}", tmp_path.display()))?;
        Ok(())
    })();
    if let Err(e) = write_result {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(e);
    }
    std::fs::rename(&tmp_path, path)
        .with_context(|| format!("rename {} -> {}", tmp_path.display(), path.display()))?;
    Ok(())
}

/// Thread-safe handle shared across capabilities. The cached `Settings`
/// is the source of truth in memory; mutations flush to disk via an
/// atomic temp-file + rename (see `save_to`), so readers and crashes
/// never observe a partially written settings file.
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

    pub fn set_attribution(&self, enabled: bool) -> Result<()> {
        let mut guard = self.inner.lock().expect("settings lock poisoned");
        guard.attribution = enabled;
        save_to(&self.path, &guard)
    }

    pub fn set_approval_mode(&self, mode: ApprovalMode) -> Result<()> {
        let mut guard = self.inner.lock().expect("settings lock poisoned");
        guard.approval_mode = mode;
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

    pub fn set_model(&self, provider: String, spec: String) -> Result<()> {
        let mut guard = self.inner.lock().expect("settings lock poisoned");
        guard.models.insert(provider, spec);
        save_to(&self.path, &guard)
    }

    pub fn set_base_url(&self, provider: String, url: String) -> Result<()> {
        let mut guard = self.inner.lock().expect("settings lock poisoned");
        guard.base_urls.insert(provider, url);
        save_to(&self.path, &guard)
    }

    /// Returns whether a base URL was actually present before removal.
    pub fn clear_base_url(&self, provider: &str) -> Result<bool> {
        let mut guard = self.inner.lock().expect("settings lock poisoned");
        let existed = guard.base_urls.remove(provider).is_some();
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
    fn attribution_defaults_to_enabled() {
        let settings = Settings::from_table(&Table::new());

        assert!(settings.attribution_enabled());
        assert!(!settings.to_table().contains_key("attribution"));
    }

    #[test]
    fn attribution_can_be_disabled_via_disk() {
        let tmp = tempfile::tempdir().expect("tmp");
        let path = tmp.path().join("settings.toml");
        let store = SettingsStore::open(path.clone());
        store.set_attribution(false).expect("save");

        let on_disk = std::fs::read_to_string(&path).expect("read");
        assert!(
            on_disk.contains("attribution = false"),
            "expected attribution setting, got: {on_disk}"
        );

        let reloaded = SettingsStore::open(path);
        assert!(!reloaded.snapshot().attribution_enabled());
    }

    #[test]
    fn approval_mode_defaults_to_normal_and_is_omitted() {
        let settings = Settings::from_table(&Table::new());
        assert_eq!(settings.approval_mode(), ApprovalMode::Normal);
        // The default level is not written, keeping settings.toml sparse.
        assert!(!settings.to_table().contains_key("approval_mode"));
    }

    #[test]
    fn approval_mode_roundtrips_via_disk() {
        let tmp = tempfile::tempdir().expect("tmp");
        let path = tmp.path().join("settings.toml");
        let store = SettingsStore::open(path.clone());
        store
            .set_approval_mode(ApprovalMode::Protective)
            .expect("save");

        let on_disk = std::fs::read_to_string(&path).expect("read");
        assert!(
            on_disk.contains("approval_mode = \"protective\""),
            "expected approval level, got: {on_disk}"
        );

        let reloaded = SettingsStore::open(path);
        assert_eq!(
            reloaded.snapshot().approval_mode(),
            ApprovalMode::Protective
        );
    }

    #[test]
    fn approval_mode_parses_canonical_names_and_aliases() {
        assert_eq!(
            ApprovalMode::parse("protective"),
            Some(ApprovalMode::Protective)
        );
        assert_eq!(
            ApprovalMode::parse("Paranoid"),
            Some(ApprovalMode::Protective)
        );
        assert_eq!(ApprovalMode::parse(" normal "), Some(ApprovalMode::Normal));
        assert_eq!(ApprovalMode::parse("yolo"), Some(ApprovalMode::Off));
        assert_eq!(ApprovalMode::parse("off"), Some(ApprovalMode::Off));
        assert!(ApprovalMode::parse("sometimes").is_none());
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
    fn model_and_base_url_roundtrip_via_disk() {
        let tmp = tempfile::tempdir().expect("tmp");
        let path = tmp.path().join("settings.toml");
        let store = SettingsStore::open(path.clone());
        store
            .set_model("openai".to_string(), "gpt-5.5 high".to_string())
            .expect("save model");
        store
            .set_base_url("custom".to_string(), "http://localhost:8000/v1".to_string())
            .expect("save base url");

        let on_disk = std::fs::read_to_string(&path).expect("read");
        assert!(on_disk.contains("[models]"), "got: {on_disk}");
        assert!(on_disk.contains("[base_urls]"), "got: {on_disk}");

        let reloaded = SettingsStore::open(path);
        assert_eq!(
            reloaded.snapshot().model_for("openai"),
            Some("gpt-5.5 high")
        );
        assert_eq!(
            reloaded.snapshot().base_url_for("custom"),
            Some("http://localhost:8000/v1")
        );
        assert!(reloaded.snapshot().model_for("anthropic").is_none());
    }

    #[test]
    fn clearing_base_url_reports_presence() {
        let tmp = tempfile::tempdir().expect("tmp");
        let path = tmp.path().join("settings.toml");
        let store = SettingsStore::open(path);
        assert!(!store.clear_base_url("custom").expect("clear absent"));
        store
            .set_base_url("custom".to_string(), "http://localhost:1234/v1".to_string())
            .expect("save");
        assert!(store.clear_base_url("custom").expect("clear present"));
        assert!(store.snapshot().base_url_for("custom").is_none());
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

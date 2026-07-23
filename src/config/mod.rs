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

pub mod capability_settings;
pub mod hooks;
pub mod mcp;
pub mod schema;
pub mod service;

use crate::config::capability_settings::{
    CapabilityOverride, capabilities_to_toml, parse_capabilities_table,
};
use crate::config::mcp::McpSettings;
use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use toml::Table;
use toml::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexAuth {
    pub access_token: String,
    pub refresh_token: Option<String>,
    /// Unix epoch milliseconds. OAuth token responses are relative, but the
    /// driver needs an absolute expiry so it can refresh before a turn starts.
    pub expires_at: Option<i64>,
    pub account_id: Option<String>,
    pub email: Option<String>,
}

/// When yolop provisions an isolated git worktree for code changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WorktreesMode {
    /// Create a worktree when the user prompt looks like implementation work.
    #[default]
    Auto,
    /// Always use a worktree in git repositories.
    Always,
    /// Never create worktrees; operate in the resolved cwd.
    Off,
}

impl WorktreesMode {
    pub fn as_str(self) -> &'static str {
        match self {
            WorktreesMode::Auto => "auto",
            WorktreesMode::Always => "always",
            WorktreesMode::Off => "off",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "auto" | "default" => Some(Self::Auto),
            "always" | "on" => Some(Self::Always),
            "off" | "none" | "disabled" => Some(Self::Off),
            _ => None,
        }
    }
}

impl std::fmt::Display for WorktreesMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Filesystem and network containment used for arbitrary shell commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SandboxMode {
    /// Host reads plus private-temp writes; workspace writes and network denied.
    ReadOnly,
    /// Host reads plus workspace/private-temp writes; network denied.
    WorkspaceWrite,
    /// Explicitly run commands directly on the host. Dangerous.
    #[default]
    DangerFullAccess,
}

impl SandboxMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::WorkspaceWrite => "workspace-write",
            Self::DangerFullAccess => "danger-full-access",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "read-only" | "readonly" => Some(Self::ReadOnly),
            "workspace-write" | "workspace" | "native" | "on" | "default" => {
                Some(Self::WorkspaceWrite)
            }
            "danger-full-access" | "full-access" | "off" | "none" | "disabled" | "unsafe-host" => {
                Some(Self::DangerFullAccess)
            }
            _ => None,
        }
    }
}

impl std::str::FromStr for SandboxMode {
    type Err = String;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        Self::parse(raw).ok_or_else(|| {
            format!(
                "unknown sandbox mode `{raw}`; expected read-only, workspace-write, or danger-full-access"
            )
        })
    }
}

/// When a shell command must stop for explicit user approval.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ApprovalPolicy {
    /// Ask before commands outside Yolop's conservative read-only allowlist.
    Untrusted,
    /// Try inside the sandbox, then ask before retrying a sandbox denial with full access.
    OnFailure,
    /// Work inside the sandbox and ask only when the model requests full access.
    #[default]
    OnRequest,
    /// Never prompt or grant full-access escalation.
    Never,
}

impl ApprovalPolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Untrusted => "untrusted",
            Self::OnFailure => "on-failure",
            Self::OnRequest => "on-request",
            Self::Never => "never",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "untrusted" => Some(Self::Untrusted),
            "on-failure" | "on_failure" => Some(Self::OnFailure),
            "on-request" | "on_request" | "auto" | "default" => Some(Self::OnRequest),
            "never" => Some(Self::Never),
            _ => None,
        }
    }
}

impl std::fmt::Display for ApprovalPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

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
    /// The default provider, used when no `--provider` flag is given. A value
    /// here takes precedence over environment-credential auto-detection, which
    /// applies only when this is unset. Persisted as the `default_provider` key
    /// (the legacy `provider` key is still read for backward compatibility).
    pub default_provider: Option<String>,
    pub tokens: BTreeMap<String, String>,
    /// Last model spec chosen per provider, in the provider-relative
    /// `model [effort]` form `/setup model` accepts. Lets a model pick
    /// survive restarts and provider switches.
    pub models: BTreeMap<String, String>,
    /// Global fallback model spec applied to the active provider when no
    /// per-provider `[models]` entry exists. Provider-relative, same form as
    /// `[models]` (`gpt-5.5 high`). A per-provider pick always wins over this.
    pub default_model: Option<String>,
    /// Endpoint base URLs keyed by provider. Today only `custom` (the
    /// generic OpenAI-compatible provider) writes here.
    pub base_urls: BTreeMap<String, String>,
    /// ChatGPT/Codex OAuth credentials. Kept separate from `[tokens]` because
    /// subscription auth needs refresh/account metadata, not just a bearer key.
    pub codex_auth: Option<CodexAuth>,
    pub attribution: bool,
    /// Soft-approval paranoia level, injected into the system prompt each
    /// turn. Central, cross-session, surfaced in the status bar.
    pub approval_mode: ApprovalMode,
    /// Hard shell escalation policy, independent from soft approval.
    pub approval_policy: ApprovalPolicy,
    /// Whether the TUI auto-starts a turn when a background task finishes while
    /// idle (proactive wake). On by default; disable for a quieter session.
    pub proactive_wake: bool,
    /// Git worktree isolation for code changes (`auto`, `always`, `off`).
    pub worktrees: WorktreesMode,
    /// Sandbox boundary for arbitrary shell commands.
    pub sandbox: SandboxMode,
    /// Interactive-TUI color theme: `yolop` (yolop's own palette) or a bundled
    /// tuika preset name. `None` means the default (yolop's palette). The
    /// `--theme` flag overrides this for a single run.
    pub theme: Option<String>,
    /// Global MCP servers (`[mcp.servers.<name>]` in settings.toml). Repo `.mcp.json` entries override these by name.
    pub mcp: McpSettings,
    /// Ordered harness capability overrides (`[[capabilities]]` in settings.toml).
    pub capabilities: Vec<CapabilityOverride>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            default_provider: None,
            tokens: BTreeMap::new(),
            models: BTreeMap::new(),
            default_model: None,
            base_urls: BTreeMap::new(),
            codex_auth: None,
            attribution: true,
            approval_mode: ApprovalMode::Normal,
            approval_policy: ApprovalPolicy::OnRequest,
            proactive_wake: true,
            worktrees: WorktreesMode::Auto,
            sandbox: SandboxMode::WorkspaceWrite,
            theme: None,
            mcp: McpSettings::default(),
            capabilities: Vec::new(),
        }
    }
}

impl Settings {
    pub fn from_table(table: &Table) -> Self {
        // Read the current `default_provider` key, falling back to the legacy
        // `provider` key so pre-rename settings files keep working.
        let default_provider = table
            .get("default_provider")
            .or_else(|| table.get("provider"))
            .and_then(Value::as_str)
            .map(str::to_string);
        let default_model = table
            .get("default_model")
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
        let approval_policy = table
            .get("approval_policy")
            .and_then(Value::as_str)
            .and_then(ApprovalPolicy::parse)
            .unwrap_or_default();
        let proactive_wake = table
            .get("proactive_wake")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        let worktrees = table
            .get("worktrees")
            .and_then(Value::as_str)
            .and_then(WorktreesMode::parse)
            .unwrap_or_default();
        let sandbox = table
            .get("sandbox_mode")
            .or_else(|| table.get("sandbox"))
            .and_then(Value::as_str)
            .and_then(SandboxMode::parse)
            .unwrap_or_default();
        let theme = table
            .get("theme")
            .and_then(Value::as_str)
            .map(str::to_string);
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
        let codex_auth = table
            .get("codex_auth")
            .and_then(Value::as_table)
            .and_then(parse_codex_auth);
        Self {
            default_provider,
            tokens: string_map("tokens"),
            models: string_map("models"),
            default_model,
            base_urls: string_map("base_urls"),
            codex_auth,
            attribution,
            approval_mode,
            approval_policy,
            proactive_wake,
            worktrees,
            sandbox,
            theme,
            mcp: parse_mcp_settings(table),
            capabilities: parse_capabilities_table(table),
        }
    }

    pub fn to_table(&self) -> Table {
        let mut table = Table::new();
        if let Some(p) = &self.default_provider {
            table.insert("default_provider".to_string(), Value::String(p.clone()));
        }
        if let Some(m) = &self.default_model {
            table.insert("default_model".to_string(), Value::String(m.clone()));
        }
        if !self.attribution {
            table.insert("attribution".to_string(), Value::Boolean(false));
        }
        // Only persist when disabled so settings.toml stays sparse.
        if !self.proactive_wake {
            table.insert("proactive_wake".to_string(), Value::Boolean(false));
        }
        // Only persist a non-default level so settings.toml stays sparse.
        if self.approval_mode != ApprovalMode::Normal {
            table.insert(
                "approval_mode".to_string(),
                Value::String(self.approval_mode.as_str().to_string()),
            );
        }
        if self.approval_policy != ApprovalPolicy::OnRequest {
            table.insert(
                "approval_policy".to_string(),
                Value::String(self.approval_policy.as_str().to_string()),
            );
        }
        if self.worktrees != WorktreesMode::Auto {
            table.insert(
                "worktrees".to_string(),
                Value::String(self.worktrees.as_str().to_string()),
            );
        }
        if self.sandbox != SandboxMode::DangerFullAccess {
            table.insert(
                "sandbox_mode".to_string(),
                Value::String(self.sandbox.as_str().to_string()),
            );
        }
        // Only persist a real selection; `yolop`/unset both mean the default.
        if let Some(theme) = self
            .theme
            .as_deref()
            .filter(|t| !t.eq_ignore_ascii_case("yolop"))
        {
            table.insert("theme".to_string(), Value::String(theme.to_string()));
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
        if let Some(auth) = &self.codex_auth {
            table.insert(
                "codex_auth".to_string(),
                Value::Table(codex_auth_to_table(auth)),
            );
        }
        if !self.mcp.servers.is_empty() {
            table.insert(
                "mcp".to_string(),
                Value::Table(mcp_settings_to_table(&self.mcp)),
            );
        }
        let caps = capabilities_to_toml(&self.capabilities);
        if let Value::Array(items) = caps
            && !items.is_empty()
        {
            table.insert("capabilities".to_string(), Value::Array(items));
        }
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

    pub fn default_model(&self) -> Option<&str> {
        self.default_model.as_deref()
    }

    pub fn base_url_for(&self, provider: &str) -> Option<&str> {
        self.base_urls.get(provider).map(String::as_str)
    }

    pub fn codex_auth(&self) -> Option<&CodexAuth> {
        self.codex_auth.as_ref()
    }

    pub fn has_codex_auth(&self) -> bool {
        self.codex_auth
            .as_ref()
            .is_some_and(|auth| !auth.access_token.is_empty())
    }

    pub fn attribution_enabled(&self) -> bool {
        self.attribution
    }

    pub fn approval_mode(&self) -> ApprovalMode {
        self.approval_mode
    }

    pub fn approval_policy(&self) -> ApprovalPolicy {
        self.approval_policy
    }

    pub fn proactive_wake_enabled(&self) -> bool {
        self.proactive_wake
    }

    pub fn worktrees_mode(&self) -> WorktreesMode {
        self.worktrees
    }

    pub fn sandbox_mode(&self) -> SandboxMode {
        self.sandbox
    }

    /// The persisted TUI theme name, if the user selected a non-default one.
    /// `None` (and `yolop`) both mean yolop's own palette.
    pub fn theme(&self) -> Option<&str> {
        self.theme
            .as_deref()
            .filter(|t| !t.eq_ignore_ascii_case("yolop"))
    }

    pub fn capability_overrides_for(&self, id: &str) -> Vec<(usize, &CapabilityOverride)> {
        self.capabilities
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.capability_ref == id)
            .collect()
    }
}

fn parse_mcp_settings(table: &Table) -> McpSettings {
    table
        .get("mcp")
        .and_then(|value| value.clone().try_into::<McpSettings>().ok())
        .unwrap_or_default()
}

fn mcp_settings_to_table(settings: &McpSettings) -> Table {
    toml::Value::try_from(settings)
        .ok()
        .and_then(|value| value.as_table().cloned())
        .unwrap_or_default()
}
fn parse_codex_auth(table: &Table) -> Option<CodexAuth> {
    let access_token = table
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())?
        .to_string();
    Some(CodexAuth {
        access_token,
        refresh_token: table
            .get("refresh_token")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_string),
        expires_at: table.get("expires_at").and_then(Value::as_integer),
        account_id: table
            .get("account_id")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_string),
        email: table
            .get("email")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_string),
    })
}

fn codex_auth_to_table(auth: &CodexAuth) -> Table {
    let mut table = Table::new();
    table.insert(
        "access_token".to_string(),
        Value::String(auth.access_token.clone()),
    );
    if let Some(refresh_token) = &auth.refresh_token {
        table.insert(
            "refresh_token".to_string(),
            Value::String(refresh_token.clone()),
        );
    }
    if let Some(expires_at) = auth.expires_at {
        table.insert("expires_at".to_string(), Value::Integer(expires_at));
    }
    if let Some(account_id) = &auth.account_id {
        table.insert("account_id".to_string(), Value::String(account_id.clone()));
    }
    if let Some(email) = &auth.email {
        table.insert("email".to_string(), Value::String(email.clone()));
    }
    table
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

    pub fn set_default_provider(&self, provider: Option<String>) -> Result<()> {
        let mut guard = self.inner.lock().expect("settings lock poisoned");
        guard.default_provider = provider;
        save_to(&self.path, &guard)
    }

    pub fn set_attribution(&self, enabled: bool) -> Result<()> {
        let mut guard = self.inner.lock().expect("settings lock poisoned");
        guard.attribution = enabled;
        save_to(&self.path, &guard)
    }

    pub fn set_theme(&self, theme: Option<String>) -> Result<()> {
        let mut guard = self.inner.lock().expect("settings lock poisoned");
        guard.theme = theme;
        save_to(&self.path, &guard)
    }

    pub fn set_proactive_wake(&self, enabled: bool) -> Result<()> {
        let mut guard = self.inner.lock().expect("settings lock poisoned");
        guard.proactive_wake = enabled;
        save_to(&self.path, &guard)
    }

    pub fn set_approval_mode(&self, mode: ApprovalMode) -> Result<()> {
        let mut guard = self.inner.lock().expect("settings lock poisoned");
        guard.approval_mode = mode;
        save_to(&self.path, &guard)
    }

    pub fn set_approval_policy(&self, policy: ApprovalPolicy) -> Result<()> {
        let mut guard = self.inner.lock().expect("settings lock poisoned");
        guard.approval_policy = policy;
        save_to(&self.path, &guard)
    }

    pub fn set_worktrees_mode(&self, mode: WorktreesMode) -> Result<()> {
        let mut guard = self.inner.lock().expect("settings lock poisoned");
        guard.worktrees = mode;
        save_to(&self.path, &guard)
    }

    pub fn set_sandbox_mode(&self, mode: SandboxMode) -> Result<()> {
        let mut guard = self.inner.lock().expect("settings lock poisoned");
        guard.sandbox = mode;
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

    /// Returns whether a per-provider model was actually present before removal.
    pub fn clear_model(&self, provider: &str) -> Result<bool> {
        let mut guard = self.inner.lock().expect("settings lock poisoned");
        let existed = guard.models.remove(provider).is_some();
        save_to(&self.path, &guard)?;
        Ok(existed)
    }

    /// Set or clear the global fallback model spec (`default_model`).
    pub fn set_default_model(&self, spec: Option<String>) -> Result<()> {
        let mut guard = self.inner.lock().expect("settings lock poisoned");
        guard.default_model = spec.filter(|s| !s.trim().is_empty());
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

    pub fn set_codex_auth(&self, auth: CodexAuth) -> Result<()> {
        let mut guard = self.inner.lock().expect("settings lock poisoned");
        guard.codex_auth = Some(auth);
        save_to(&self.path, &guard)
    }

    /// Re-read `[codex_auth]` from disk and adopt it into the in-memory cache.
    ///
    /// Used by the Codex driver before/after refresh so a concurrent yolop
    /// process that already rotated the refresh token is not overwritten by
    /// stale in-memory credentials (OpenAI returns `refresh_token_reused`).
    pub fn refresh_codex_auth_from_disk(&self) -> Option<CodexAuth> {
        let from_disk = load_from(&self.path).codex_auth;
        let mut guard = self.inner.lock().expect("settings lock poisoned");
        guard.codex_auth = from_disk.clone();
        from_disk
    }

    /// Returns whether a Codex login was actually present before removal.
    pub fn clear_codex_auth(&self) -> Result<bool> {
        let mut guard = self.inner.lock().expect("settings lock poisoned");
        let existed = guard.codex_auth.take().is_some();
        save_to(&self.path, &guard)?;
        Ok(existed)
    }

    pub fn replace_mcp(&self, mcp: McpSettings) -> Result<()> {
        let mut guard = self.inner.lock().expect("settings lock poisoned");
        guard.mcp = mcp;
        save_to(&self.path, &guard)
    }

    /// Append a harness capability override to the ordered settings list.
    pub fn append_capability_override(&self, entry: CapabilityOverride) -> Result<usize> {
        let mut guard = self.inner.lock().expect("settings lock poisoned");
        guard.capabilities.push(entry);
        let index = guard.capabilities.len() - 1;
        save_to(&self.path, &guard)?;
        Ok(index)
    }

    /// Drop every stored `[[capabilities]]` override.
    pub fn clear_capability_overrides(&self) -> Result<()> {
        let mut guard = self.inner.lock().expect("settings lock poisoned");
        guard.capabilities.clear();
        save_to(&self.path, &guard)
    }

    /// Enable or disable a capability by `ref`, idempotently. Enabling
    /// appends a plain `[[capabilities]] ref = "<id>"` unless one is already
    /// present; disabling drops every override with that `ref` (both the
    /// enabling entry and any config/remove entries). Returns whether the
    /// stored set changed. This is the write side of `/extensions
    /// enable|disable` — a targeted alternative to hand-editing the ordered
    /// override list.
    pub fn set_capability_enabled(&self, capability_ref: &str, enabled: bool) -> Result<bool> {
        let mut guard = self.inner.lock().expect("settings lock poisoned");
        let already: Vec<usize> = guard
            .capabilities
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.capability_ref == capability_ref)
            .map(|(index, _)| index)
            .collect();
        let changed = if enabled {
            if already.iter().any(|&i| !guard.capabilities[i].is_remove()) {
                false
            } else {
                guard
                    .capabilities
                    .push(CapabilityOverride::enable(capability_ref));
                true
            }
        } else if already.is_empty() {
            false
        } else {
            guard
                .capabilities
                .retain(|entry| entry.capability_ref != capability_ref);
            true
        };
        if changed {
            save_to(&self.path, &guard)?;
        }
        Ok(changed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use everruns_core::McpServerAuthMode;

    #[test]
    fn default_provider_roundtrip_via_disk() {
        let tmp = tempfile::tempdir().expect("tmp");
        let path = tmp.path().join("nested").join("settings.toml");
        let store = SettingsStore::open(path.clone());
        assert!(store.snapshot().default_provider.is_none());
        store
            .set_default_provider(Some("anthropic".to_string()))
            .expect("save");

        let on_disk = std::fs::read_to_string(&path).expect("read");
        assert!(
            on_disk.contains("default_provider = \"anthropic\""),
            "expected TOML key/value, got: {on_disk}"
        );

        let reloaded = SettingsStore::open(path);
        assert_eq!(
            reloaded.snapshot().default_provider.as_deref(),
            Some("anthropic")
        );
    }

    #[test]
    fn theme_roundtrip_via_disk() {
        let tmp = tempfile::tempdir().expect("tmp");
        let path = tmp.path().join("settings.toml");
        let store = SettingsStore::open(path.clone());
        assert_eq!(store.snapshot().theme(), None);

        store
            .set_theme(Some("gruvbox-dark".to_string()))
            .expect("save");
        let on_disk = std::fs::read_to_string(&path).expect("read");
        assert!(
            on_disk.contains("theme = \"gruvbox-dark\""),
            "expected TOML key/value, got: {on_disk}"
        );
        assert_eq!(
            SettingsStore::open(path.clone()).snapshot().theme(),
            Some("gruvbox-dark")
        );

        // `yolop` and clearing both mean the default and stay out of the file.
        store.set_theme(Some("yolop".to_string())).expect("save");
        assert_eq!(store.snapshot().theme(), None);
        let on_disk = std::fs::read_to_string(&path).expect("read");
        assert!(
            !on_disk.contains("theme ="),
            "yolop should not be persisted, got: {on_disk}"
        );
    }

    #[test]
    fn legacy_provider_key_is_still_read() {
        let tmp = tempfile::tempdir().expect("tmp");
        let path = tmp.path().join("settings.toml");
        // A settings file written before the rename uses the bare `provider` key.
        std::fs::write(&path, "provider = \"anthropic\"\n").expect("seed");
        let store = SettingsStore::open(path);
        assert_eq!(
            store.snapshot().default_provider.as_deref(),
            Some("anthropic")
        );
    }

    #[test]
    fn attribution_defaults_to_enabled() {
        let settings = Settings::from_table(&Table::new());

        assert!(settings.attribution_enabled());
        assert!(!settings.to_table().contains_key("attribution"));
    }

    #[test]
    fn proactive_wake_defaults_on_and_round_trips_when_disabled() {
        let settings = Settings::from_table(&Table::new());
        assert!(settings.proactive_wake_enabled());
        // Default stays out of the file to keep it sparse.
        assert!(!settings.to_table().contains_key("proactive_wake"));

        let tmp = tempfile::tempdir().expect("tmp");
        let path = tmp.path().join("settings.toml");
        let store = SettingsStore::open(path.clone());
        store.set_proactive_wake(false).expect("save");
        assert!(!store.snapshot().proactive_wake_enabled());
        // Survives a reopen.
        assert!(
            !SettingsStore::open(path)
                .snapshot()
                .proactive_wake_enabled()
        );
    }

    #[test]
    fn sandbox_defaults_to_full_access_and_workspace_write_round_trips() {
        let settings = Settings::from_table(&Table::new());
        assert_eq!(settings.sandbox_mode(), SandboxMode::DangerFullAccess);
        assert!(!settings.to_table().contains_key("sandbox_mode"));

        let tmp = tempfile::tempdir().expect("tmp");
        let path = tmp.path().join("settings.toml");
        let store = SettingsStore::open(path.clone());
        store
            .set_sandbox_mode(SandboxMode::WorkspaceWrite)
            .expect("save");
        assert!(
            std::fs::read_to_string(&path)
                .expect("read")
                .contains("sandbox_mode = \"workspace-write\"")
        );
        assert_eq!(
            SettingsStore::open(path).snapshot().sandbox_mode(),
            SandboxMode::WorkspaceWrite
        );
    }

    #[test]
    fn sandbox_and_approval_policy_parse_codex_matrix_and_legacy_aliases() {
        assert_eq!(SandboxMode::parse("read-only"), Some(SandboxMode::ReadOnly));
        assert_eq!(
            SandboxMode::parse("native"),
            Some(SandboxMode::WorkspaceWrite)
        );
        assert_eq!(
            SandboxMode::parse("off"),
            Some(SandboxMode::DangerFullAccess)
        );
        assert_eq!(
            ApprovalPolicy::parse("on-failure"),
            Some(ApprovalPolicy::OnFailure)
        );
        assert_eq!(ApprovalPolicy::parse("never"), Some(ApprovalPolicy::Never));
    }

    #[test]
    fn approval_policy_defaults_to_on_request_and_round_trips() {
        let settings = Settings::from_table(&Table::new());
        assert_eq!(settings.approval_policy(), ApprovalPolicy::OnRequest);
        assert!(!settings.to_table().contains_key("approval_policy"));

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("settings.toml");
        let store = SettingsStore::open(path.clone());
        store
            .set_approval_policy(ApprovalPolicy::OnFailure)
            .unwrap();
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("approval_policy = \"on-failure\"")
        );
        assert_eq!(
            SettingsStore::open(path).snapshot().approval_policy(),
            ApprovalPolicy::OnFailure
        );
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
    fn codex_auth_roundtrips_via_disk() {
        let tmp = tempfile::tempdir().expect("tmp");
        let path = tmp.path().join("settings.toml");
        let store = SettingsStore::open(path.clone());
        store
            .set_codex_auth(CodexAuth {
                access_token: "access-token".to_string(),
                refresh_token: Some("refresh-token".to_string()),
                expires_at: Some(1_771_000_000_000),
                account_id: Some("acc_123".to_string()),
                email: Some("user@example.com".to_string()),
            })
            .expect("save codex auth");

        let on_disk = std::fs::read_to_string(&path).expect("read");
        assert!(on_disk.contains("[codex_auth]"), "got: {on_disk}");
        assert!(
            on_disk.contains("refresh_token = \"refresh-token\""),
            "got: {on_disk}"
        );

        let reloaded = SettingsStore::open(path);
        let snapshot = reloaded.snapshot();
        let auth = snapshot.codex_auth().expect("codex auth");
        assert_eq!(auth.access_token, "access-token");
        assert_eq!(auth.account_id.as_deref(), Some("acc_123"));
        assert!(snapshot.has_codex_auth());
    }

    #[test]
    fn refresh_codex_auth_from_disk_picks_up_external_writes() {
        let tmp = tempfile::tempdir().expect("tmp");
        let path = tmp.path().join("settings.toml");
        let store = SettingsStore::open(path.clone());
        store
            .set_codex_auth(CodexAuth {
                access_token: "access-old".to_string(),
                refresh_token: Some("refresh-old".to_string()),
                expires_at: Some(1),
                account_id: None,
                email: None,
            })
            .expect("save");

        // Simulate another process rotating tokens on the same file.
        let other = SettingsStore::open(path);
        other
            .set_codex_auth(CodexAuth {
                access_token: "access-new".to_string(),
                refresh_token: Some("refresh-new".to_string()),
                expires_at: Some(9),
                account_id: Some("acc".to_string()),
                email: Some("a@b.c".to_string()),
            })
            .expect("external write");

        let adopted = store.refresh_codex_auth_from_disk().expect("disk auth");
        assert_eq!(adopted.access_token, "access-new");
        assert_eq!(adopted.refresh_token.as_deref(), Some("refresh-new"));
        assert_eq!(
            store
                .snapshot()
                .codex_auth()
                .map(|a| a.access_token.as_str()),
            Some("access-new")
        );
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
    fn global_mcp_servers_roundtrip_via_settings_toml() {
        let tmp = tempfile::tempdir().expect("tmp");
        let path = tmp.path().join("settings.toml");
        std::fs::write(
            &path,
            r#"
[mcp.servers.linear]
type = "http"
url = "https://mcp.linear.app/sse"
auth_mode = "oauth"
oauth_provider_id = "linear"

[mcp.servers.linear.headers]
Authorization = "Bearer ${LINEAR_API_KEY}"
"#,
        )
        .expect("write");

        let store = SettingsStore::open(path.clone());
        let snapshot = store.snapshot();
        let linear = snapshot.mcp.servers.get("linear").expect("linear server");
        assert!(linear.enabled);
        assert_eq!(linear.server.url, "https://mcp.linear.app/sse");
        assert_eq!(linear.server.auth_mode, McpServerAuthMode::OAuth);
        assert_eq!(linear.server.oauth_provider_id.as_deref(), Some("linear"));
        assert_eq!(
            linear
                .server
                .headers
                .get("Authorization")
                .map(String::as_str),
            Some("Bearer ${LINEAR_API_KEY}")
        );

        save_to(&path, &snapshot).expect("save");
        let on_disk = std::fs::read_to_string(&path).expect("read");
        assert!(on_disk.contains("[mcp.servers.linear]"), "got: {on_disk}");
        assert!(
            on_disk.contains(r#"auth_mode = "o_auth""#),
            "got: {on_disk}"
        );
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
    fn default_model_roundtrips_and_clears() {
        let tmp = tempfile::tempdir().expect("tmp");
        let path = tmp.path().join("settings.toml");
        let store = SettingsStore::open(path.clone());
        assert!(store.snapshot().default_model().is_none());

        store
            .set_default_model(Some("claude-sonnet-4-5".to_string()))
            .expect("save default model");
        let on_disk = std::fs::read_to_string(&path).expect("read");
        assert!(
            on_disk.contains("default_model = \"claude-sonnet-4-5\""),
            "expected default_model key, got: {on_disk}"
        );

        let reloaded = SettingsStore::open(path.clone());
        assert_eq!(
            reloaded.snapshot().default_model(),
            Some("claude-sonnet-4-5")
        );

        // Empty/whitespace clears, and the key drops out of the TOML.
        reloaded
            .set_default_model(Some("  ".to_string()))
            .expect("clear");
        assert!(reloaded.snapshot().default_model().is_none());
        let on_disk = std::fs::read_to_string(&path).expect("read");
        assert!(!on_disk.contains("default_model"), "got: {on_disk}");
    }

    #[test]
    fn clearing_model_reports_presence() {
        let tmp = tempfile::tempdir().expect("tmp");
        let path = tmp.path().join("settings.toml");
        let store = SettingsStore::open(path);
        assert!(!store.clear_model("openai").expect("clear absent"));
        store
            .set_model("openai".to_string(), "gpt-5.5 high".to_string())
            .expect("save");
        assert!(store.clear_model("openai").expect("clear present"));
        assert!(store.snapshot().model_for("openai").is_none());
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
        assert!(store.snapshot().default_provider.is_none());
        assert!(store.snapshot().tokens.is_empty());
    }

    #[test]
    fn malformed_file_yields_default() {
        let tmp = tempfile::tempdir().expect("tmp");
        let path = tmp.path().join("settings.toml");
        std::fs::write(&path, "this = is = not = toml").expect("write");
        let store = SettingsStore::open(path);
        assert!(store.snapshot().default_provider.is_none());
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

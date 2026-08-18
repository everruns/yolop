use super::mcp::McpSettings;
use super::{
    ApprovalMode, ApprovalPolicy, CapabilityOverride, SandboxMode, Settings, WorktreesMode,
    capabilities_to_toml, mcp_settings_to_table, parse_capabilities_table, parse_mcp_settings,
};
use crate::runtime::SUPPORTED_PROVIDERS;
use anyhow::{Context, Result, bail};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use toml::{Table, Value};

const PROFILEABLE_KEYS: &[&str] = &[
    "default_provider",
    "models",
    "base_urls",
    "approval_mode",
    "approval_policy",
    "worktrees",
    "sandbox_mode",
    "capabilities",
    "capabilities_mode",
    "mcp",
    "mcp_mode",
    "instructions",
    "instructions_file",
    "skills_dir",
];

/// The subset of [`PROFILEABLE_KEYS`] that [`SettingsOverlay::to_table`]
/// regenerates from parsed state. Everything else recognized (the instruction
/// and skills pointers) is carried through verbatim, alongside keys yolop does
/// not know, so a write never rewrites a path or inlines a file's contents.
const REWRITTEN_KEYS: &[&str] = &[
    "default_provider",
    "models",
    "base_urls",
    "approval_mode",
    "approval_policy",
    "worktrees",
    "sandbox_mode",
    "capabilities",
    "capabilities_mode",
    "mcp",
    "mcp_mode",
];

const GLOBAL_ONLY_KEYS: &[&str] = &[
    "tokens",
    "codex_auth",
    "theme",
    "attribution",
    "proactive_wake",
    "acp_setup_page",
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProfileName(String);

impl ProfileName {
    pub fn parse(raw: &str) -> Result<Self> {
        let valid = !raw.is_empty()
            && raw.len() <= 64
            && raw.bytes().enumerate().all(|(index, byte)| match byte {
                b'a'..=b'z' | b'0'..=b'9' => true,
                b'-' | b'_' => index > 0,
                _ => false,
            });
        if !valid {
            bail!(
                "invalid profile name `{raw}`; expected 1-64 lowercase letters, numbers, hyphens, or underscores, starting with a letter or number"
            );
        }
        Ok(Self(raw.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// How a profile's structural list combines with the global one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ListMode {
    /// Profile entries are layered on top of the global ones.
    #[default]
    Merge,
    /// The profile's entries are the whole set; global ones are ignored.
    Replace,
}

impl ListMode {
    /// `append`/`merge` and `replace` both read naturally depending on whether
    /// the key is an ordered list (`capabilities`) or a map (`mcp`), so accept
    /// either spelling for either key.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "merge" | "append" | "layer" => Some(Self::Merge),
            "replace" | "only" | "exclusive" => Some(Self::Replace),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Merge => "merge",
            Self::Replace => "replace",
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct SettingsOverlay {
    pub default_provider: Option<Option<String>>,
    pub models: BTreeMap<String, Option<String>>,
    pub base_urls: BTreeMap<String, Option<String>>,
    pub approval_mode: Option<ApprovalMode>,
    pub approval_policy: Option<ApprovalPolicy>,
    pub worktrees: Option<WorktreesMode>,
    pub sandbox: Option<SandboxMode>,
    /// Harness capability overrides applied after the global ones (or instead
    /// of them under [`ListMode::Replace`]). This is also how a profile enables
    /// or disables an installed extension, whose enablement *is* a
    /// `[[capabilities]]` entry.
    pub capabilities: Vec<CapabilityOverride>,
    pub capabilities_mode: ListMode,
    /// MCP servers merged by name over the global ones (or instead of them
    /// under [`ListMode::Replace`]). Workspace `.mcp.json` still overlays the
    /// result, so a repo can still override a profile server by name.
    pub mcp: McpSettings,
    pub mcp_mode: ListMode,
    /// Extra system-prompt text appended after `system.md` and the capability
    /// blocks, for a profile that gives the agent a standing job.
    pub instructions: Option<String>,
    /// Extra skills scope, resolved to a host directory. Ranks between the
    /// workspace and global scopes.
    pub skills_dir: Option<PathBuf>,
    unknown: Table,
}

impl SettingsOverlay {
    pub fn apply_to(&self, settings: &mut Settings) {
        if let Some(provider) = &self.default_provider {
            settings.default_provider.clone_from(provider);
        }
        apply_map(&mut settings.models, &self.models);
        apply_map(&mut settings.base_urls, &self.base_urls);
        if let Some(mode) = self.approval_mode {
            settings.approval_mode = mode;
        }
        if let Some(policy) = self.approval_policy {
            settings.approval_policy = policy;
        }
        if let Some(mode) = self.worktrees {
            settings.worktrees = mode;
        }
        if let Some(mode) = self.sandbox {
            settings.sandbox = mode;
        }
        if self.capabilities_mode == ListMode::Replace {
            settings.capabilities.clear();
        }
        settings
            .capabilities
            .extend(self.capabilities.iter().cloned());
        if self.mcp_mode == ListMode::Replace {
            settings.mcp.servers.clear();
        }
        for (name, entry) in &self.mcp.servers {
            settings.mcp.servers.insert(name.clone(), entry.clone());
        }
        if let Some(instructions) = &self.instructions {
            settings.instructions = Some(instructions.clone());
        }
        if let Some(dir) = &self.skills_dir {
            settings.skills_dir = Some(dir.clone());
        }
    }

    /// Parse a profile document. `base_dir` is the directory holding the
    /// profile file; `instructions_file` and `skills_dir` resolve against it.
    pub fn from_table(table: &Table, base_dir: &Path) -> Result<(Self, Vec<String>)> {
        for key in GLOBAL_ONLY_KEYS {
            if table.contains_key(*key) {
                bail!("`{key}` is global-only and cannot be set in a profile");
            }
        }

        let warnings = table
            .keys()
            .filter(|key| !PROFILEABLE_KEYS.contains(&key.as_str()))
            .map(|key| format!("ignoring unknown profile key `{key}`"))
            .collect();
        let unknown = table
            .iter()
            .filter(|(key, _)| !REWRITTEN_KEYS.contains(&key.as_str()))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();

        let default_provider = optional_string(table, "default_provider")?.map(Some);
        let models = optional_string_map(table, "models")?;
        let base_urls = optional_string_map(table, "base_urls")?;
        if let Some(Some(provider)) = &default_provider
            && !SUPPORTED_PROVIDERS.contains(&provider.as_str())
        {
            bail!(
                "`default_provider` expects one of {}; got `{provider}`",
                SUPPORTED_PROVIDERS.join(", ")
            );
        }
        for provider in models.keys().chain(base_urls.keys()) {
            if !SUPPORTED_PROVIDERS.contains(&provider.as_str()) {
                bail!(
                    "unknown provider `{provider}` in profile table; expected one of {}",
                    SUPPORTED_PROVIDERS.join(", ")
                );
            }
        }
        let approval_mode = optional_parsed(
            table,
            "approval_mode",
            ApprovalMode::parse,
            "protective, normal, or off",
        )?;
        let approval_policy = optional_parsed(
            table,
            "approval_policy",
            ApprovalPolicy::parse,
            "untrusted, on-failure, on-request, or never",
        )?;
        let worktrees = optional_parsed(
            table,
            "worktrees",
            WorktreesMode::parse,
            "auto, always, or off",
        )?;
        let sandbox = optional_parsed(
            table,
            "sandbox_mode",
            SandboxMode::parse,
            "read-only, workspace-write, or danger-full-access",
        )?;
        let capabilities = parse_capabilities_table(table);
        if table.contains_key("capabilities") && capabilities.is_empty() {
            bail!("`capabilities` must be an array of tables, each with a `ref`");
        }
        let capabilities_mode = optional_parsed(
            table,
            "capabilities_mode",
            ListMode::parse,
            "merge (default) or replace",
        )?
        .unwrap_or_default();
        let mcp = parse_mcp_settings(table);
        if table.contains_key("mcp") && mcp.servers.is_empty() {
            bail!("`mcp` must be a table of `[mcp.servers.<name>]` entries");
        }
        let mcp_mode = optional_parsed(
            table,
            "mcp_mode",
            ListMode::parse,
            "merge (default) or replace",
        )?
        .unwrap_or_default();
        let instructions = load_instructions(table, base_dir)?;
        let skills_dir = optional_string(table, "skills_dir")?.map(|raw| resolve(base_dir, &raw));

        Ok((
            Self {
                default_provider,
                models,
                base_urls,
                approval_mode,
                approval_policy,
                worktrees,
                sandbox,
                capabilities,
                capabilities_mode,
                mcp,
                mcp_mode,
                instructions,
                skills_dir,
                unknown,
            },
            warnings,
        ))
    }

    pub fn to_table(&self) -> Table {
        let mut table = self.unknown.clone();
        insert_optional_string(&mut table, "default_provider", &self.default_provider);
        insert_map(&mut table, "models", &self.models);
        insert_map(&mut table, "base_urls", &self.base_urls);
        if let Some(mode) = self.approval_mode {
            table.insert(
                "approval_mode".to_string(),
                Value::String(mode.as_str().to_string()),
            );
        }
        if let Some(policy) = self.approval_policy {
            table.insert(
                "approval_policy".to_string(),
                Value::String(policy.as_str().to_string()),
            );
        }
        if let Some(mode) = self.worktrees {
            table.insert(
                "worktrees".to_string(),
                Value::String(mode.as_str().to_string()),
            );
        }
        if let Some(mode) = self.sandbox {
            table.insert(
                "sandbox_mode".to_string(),
                Value::String(mode.as_str().to_string()),
            );
        }
        if let Value::Array(items) = capabilities_to_toml(&self.capabilities)
            && !items.is_empty()
        {
            table.insert("capabilities".to_string(), Value::Array(items));
        }
        if self.capabilities_mode != ListMode::default() {
            table.insert(
                "capabilities_mode".to_string(),
                Value::String(self.capabilities_mode.as_str().to_string()),
            );
        }
        if !self.mcp.servers.is_empty() {
            table.insert(
                "mcp".to_string(),
                Value::Table(mcp_settings_to_table(&self.mcp)),
            );
        }
        if self.mcp_mode != ListMode::default() {
            table.insert(
                "mcp_mode".to_string(),
                Value::String(self.mcp_mode.as_str().to_string()),
            );
        }
        table
    }
}

#[derive(Clone, Debug)]
pub struct ActiveProfile {
    pub name: ProfileName,
    pub path: PathBuf,
    pub overlay: SettingsOverlay,
    pub warnings: Vec<String>,
}

impl ActiveProfile {
    pub fn load(settings_path: &Path, raw_name: &str) -> Result<Self> {
        let name = ProfileName::parse(raw_name)?;
        let parent = settings_path.parent().unwrap_or_else(|| Path::new("."));
        let path = parent
            .join("profiles")
            .join(format!("{}.toml", name.as_str()));
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("read profile `{}` from {}", name.as_str(), path.display()))?;
        let table: Table = toml::from_str(&text)
            .with_context(|| format!("parse profile `{}` at {}", name.as_str(), path.display()))?;
        let base_dir = parent.join("profiles");
        let (mut overlay, warnings) =
            SettingsOverlay::from_table(&table, &base_dir).with_context(|| {
                format!("invalid profile `{}` at {}", name.as_str(), path.display())
            })?;
        // Convention over configuration: `profiles/<name>/skills/` is the
        // profile's skills scope when it exists, so a profile directory needs
        // no `skills_dir` key to carry skills.
        if overlay.skills_dir.is_none() {
            let conventional = base_dir.join(name.as_str()).join("skills");
            if conventional.is_dir() {
                overlay.skills_dir = Some(conventional);
            }
        }
        Ok(Self {
            name,
            path,
            overlay,
            warnings,
        })
    }
}

/// Resolve a profile-relative path against the profiles directory; absolute
/// paths (and `~`) are taken as given.
fn resolve(base_dir: &Path, raw: &str) -> PathBuf {
    if let Some(rest) = raw.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest);
    }
    let path = PathBuf::from(raw);
    if path.is_absolute() {
        path
    } else {
        base_dir.join(path)
    }
}

/// Inline `instructions` plus the contents of `instructions_file`, in that
/// order. A named file that cannot be read fails the profile rather than
/// silently dropping the standing job it describes.
fn load_instructions(table: &Table, base_dir: &Path) -> Result<Option<String>> {
    let mut blocks = Vec::new();
    if let Some(inline) = optional_string(table, "instructions")? {
        blocks.push(inline.trim_end().to_string());
    }
    if let Some(raw) = optional_string(table, "instructions_file")? {
        let path = resolve(base_dir, &raw);
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("read `instructions_file` {}", path.display()))?;
        blocks.push(text.trim_end().to_string());
    }
    let joined = blocks
        .into_iter()
        .filter(|block| !block.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    Ok((!joined.is_empty()).then_some(joined))
}

fn optional_string(table: &Table, key: &str) -> Result<Option<String>> {
    table
        .get(key)
        .map(|value| {
            value
                .as_str()
                .filter(|value| !value.trim().is_empty())
                .map(str::to_string)
                .ok_or_else(|| anyhow::anyhow!("`{key}` must be a non-empty string"))
        })
        .transpose()
}

fn optional_string_map(table: &Table, key: &str) -> Result<BTreeMap<String, Option<String>>> {
    let Some(value) = table.get(key) else {
        return Ok(BTreeMap::new());
    };
    let entries = value
        .as_table()
        .ok_or_else(|| anyhow::anyhow!("`{key}` must be a table"))?;
    entries
        .iter()
        .map(|(entry_key, value)| {
            let value = value
                .as_str()
                .filter(|value| !value.trim().is_empty())
                .map(str::to_string)
                .ok_or_else(|| anyhow::anyhow!("`{key}.{entry_key}` must be a non-empty string"))?;
            Ok((entry_key.clone(), Some(value)))
        })
        .collect()
}

fn optional_parsed<T: Copy>(
    table: &Table,
    key: &str,
    parse: impl Fn(&str) -> Option<T>,
    expected: &str,
) -> Result<Option<T>> {
    let Some(value) = table.get(key) else {
        return Ok(None);
    };
    let raw = value
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("`{key}` must be a string"))?;
    parse(raw)
        .map(Some)
        .ok_or_else(|| anyhow::anyhow!("`{key}` expects {expected}; got `{raw}`"))
}

fn apply_map(target: &mut BTreeMap<String, String>, overlay: &BTreeMap<String, Option<String>>) {
    for (key, value) in overlay {
        match value {
            Some(value) => {
                target.insert(key.clone(), value.clone());
            }
            None => {
                target.remove(key);
            }
        }
    }
}

fn insert_optional_string(table: &mut Table, key: &str, value: &Option<Option<String>>) {
    if let Some(Some(value)) = value {
        table.insert(key.to_string(), Value::String(value.clone()));
    }
}

fn insert_map(table: &mut Table, key: &str, values: &BTreeMap<String, Option<String>>) {
    let entries = values
        .iter()
        .filter_map(|(key, value)| value.as_ref().map(|value| (key.clone(), value.clone())))
        .map(|(key, value)| (key, Value::String(value)))
        .collect::<Table>();
    if !entries.is_empty() {
        table.insert(key.to_string(), Value::Table(entries));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_name_rejects_paths_and_case_ambiguity() {
        for invalid in ["", "../fast", "Fast", "-fast", "fast.toml", "fast profile"] {
            assert!(ProfileName::parse(invalid).is_err(), "accepted {invalid:?}");
        }
        assert_eq!(
            ProfileName::parse("deep-review_2").unwrap().as_str(),
            "deep-review_2"
        );
    }

    #[test]
    fn profile_is_sparse_and_rejects_global_credentials() {
        let table: Table = toml::from_str(
            "default_provider = 'openai'\napproval_policy = 'never'\nfuture_setting = 'preserved'\n[models]\nopenai = 'gpt-5.6 high'\n",
        )
        .unwrap();
        let (overlay, warnings) =
            SettingsOverlay::from_table(&table, Path::new("/nonexistent")).unwrap();
        assert_eq!(warnings, ["ignoring unknown profile key `future_setting`"]);
        assert_eq!(overlay.default_provider, Some(Some("openai".to_string())));
        assert_eq!(overlay.approval_policy, Some(ApprovalPolicy::Never));
        assert_eq!(overlay.models["openai"].as_deref(), Some("gpt-5.6 high"));
        assert!(overlay.to_table().get("sandbox_mode").is_none());
        assert_eq!(
            overlay.to_table()["future_setting"].as_str(),
            Some("preserved")
        );

        let credentials: Table = toml::from_str("[tokens]\nopenai = 'secret'\n").unwrap();
        assert!(SettingsOverlay::from_table(&credentials, Path::new("/nonexistent")).is_err());

        // Host-local settings are global-only too: the ACP setup page binds a
        // listener on this machine, so it is not part of an agent's job.
        let host_local: Table = toml::from_str("acp_setup_page = true\n").unwrap();
        let error = SettingsOverlay::from_table(&host_local, Path::new("/nonexistent"))
            .expect_err("acp_setup_page is global-only");
        assert!(error.to_string().contains("acp_setup_page"), "got: {error}");
    }

    #[test]
    fn overlay_replaces_scalars_and_merges_provider_maps() {
        let mut settings = Settings {
            default_provider: Some("anthropic".to_string()),
            models: BTreeMap::from([
                ("anthropic".to_string(), "claude-sonnet".to_string()),
                ("openai".to_string(), "gpt-base".to_string()),
            ]),
            ..Settings::default()
        };
        let overlay = SettingsOverlay {
            default_provider: Some(Some("openai".to_string())),
            models: BTreeMap::from([("openai".to_string(), Some("gpt-profile".to_string()))]),
            ..SettingsOverlay::default()
        };
        overlay.apply_to(&mut settings);
        assert_eq!(settings.default_provider.as_deref(), Some("openai"));
        assert_eq!(settings.models["openai"], "gpt-profile");
        assert_eq!(settings.models["anthropic"], "claude-sonnet");
    }

    #[test]
    fn capabilities_layer_by_default_and_replace_on_request() {
        let table: Table = toml::from_str(
            "[[capabilities]]\nref = 'ext:triage'\n\n[[capabilities]]\nref = 'yolop_lsp'\nenabled = false\n",
        )
        .unwrap();
        let (overlay, warnings) =
            SettingsOverlay::from_table(&table, Path::new("/nonexistent")).unwrap();
        assert!(warnings.is_empty());
        let mut settings = Settings {
            capabilities: vec![CapabilityOverride::enable("ext:notes")],
            ..Settings::default()
        };
        overlay.apply_to(&mut settings);
        let refs: Vec<_> = settings
            .capabilities
            .iter()
            .map(|entry| entry.capability_ref.as_str())
            .collect();
        assert_eq!(refs, ["ext:notes", "ext:triage", "yolop_lsp"]);
        assert!(settings.capabilities[2].is_remove());

        let replacing: Table =
            toml::from_str("capabilities_mode = 'replace'\n[[capabilities]]\nref = 'ext:triage'\n")
                .unwrap();
        let (overlay, _) =
            SettingsOverlay::from_table(&replacing, Path::new("/nonexistent")).unwrap();
        let mut settings = Settings {
            capabilities: vec![CapabilityOverride::enable("ext:notes")],
            ..Settings::default()
        };
        overlay.apply_to(&mut settings);
        assert_eq!(settings.capabilities.len(), 1);
        assert_eq!(settings.capabilities[0].capability_ref, "ext:triage");
        // Both survive a write back to disk.
        let round_tripped = overlay.to_table();
        assert_eq!(round_tripped["capabilities_mode"].as_str(), Some("replace"));
        assert!(round_tripped["capabilities"].as_array().is_some());
    }

    #[test]
    fn mcp_servers_merge_by_name_or_replace_the_global_set() {
        let table: Table = toml::from_str(
            "[mcp.servers.linear]\ntype = 'http'\nurl = 'https://profile.example/mcp'\n",
        )
        .unwrap();
        let (overlay, _) = SettingsOverlay::from_table(&table, Path::new("/nonexistent")).unwrap();
        let mut settings = Settings::default();
        settings.mcp.servers.insert(
            "notes".to_string(),
            toml::from_str::<Table>("type = 'http'\nurl = 'https://global.example/mcp'\n")
                .unwrap()
                .try_into()
                .unwrap(),
        );
        overlay.apply_to(&mut settings);
        assert_eq!(
            settings.mcp.servers.keys().collect::<Vec<_>>(),
            ["linear", "notes"]
        );

        let replacing: Table = toml::from_str(
            "mcp_mode = 'replace'\n[mcp.servers.linear]\ntype = 'http'\nurl = 'https://profile.example/mcp'\n",
        )
        .unwrap();
        let (overlay, _) =
            SettingsOverlay::from_table(&replacing, Path::new("/nonexistent")).unwrap();
        overlay.apply_to(&mut settings);
        assert_eq!(settings.mcp.servers.keys().collect::<Vec<_>>(), ["linear"]);
    }

    #[test]
    fn instructions_and_skills_resolve_against_the_profiles_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path();
        std::fs::write(base.join("triage.md"), "Route incoming work.\n").unwrap();
        let table: Table = toml::from_str(
            "instructions = 'You are the triage agent.'\ninstructions_file = 'triage.md'\nskills_dir = 'triage/skills'\n",
        )
        .unwrap();
        let (overlay, warnings) = SettingsOverlay::from_table(&table, base).unwrap();
        assert!(warnings.is_empty());
        assert_eq!(
            overlay.instructions.as_deref(),
            Some("You are the triage agent.\n\nRoute incoming work.")
        );
        assert_eq!(
            overlay.skills_dir.as_deref(),
            Some(base.join("triage").join("skills").as_path())
        );
        // Pointers are carried through verbatim, never rewritten or inlined.
        let written = overlay.to_table();
        assert_eq!(written["instructions_file"].as_str(), Some("triage.md"));
        assert_eq!(written["skills_dir"].as_str(), Some("triage/skills"));
        assert!(written["instructions"].as_str().is_some());

        let missing: Table = toml::from_str("instructions_file = 'absent.md'\n").unwrap();
        assert!(SettingsOverlay::from_table(&missing, base).is_err());
    }

    #[test]
    fn profile_directory_supplies_skills_without_a_key() {
        let tmp = tempfile::tempdir().unwrap();
        let config = tmp.path();
        let profiles = config.join("profiles");
        std::fs::create_dir_all(profiles.join("triage").join("skills")).unwrap();
        std::fs::write(profiles.join("triage.toml"), "approval_policy = 'never'\n").unwrap();
        let active = ActiveProfile::load(&config.join("settings.toml"), "triage").unwrap();
        assert_eq!(
            active.overlay.skills_dir.as_deref(),
            Some(profiles.join("triage").join("skills").as_path())
        );
    }
}

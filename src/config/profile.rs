use super::{ApprovalMode, ApprovalPolicy, SandboxMode, Settings, WorktreesMode};
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
];

const GLOBAL_ONLY_KEYS: &[&str] = &[
    "tokens",
    "codex_auth",
    "theme",
    "attribution",
    "proactive_wake",
    "mcp",
    "capabilities",
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

#[derive(Clone, Debug, Default)]
pub struct SettingsOverlay {
    pub default_provider: Option<Option<String>>,
    pub models: BTreeMap<String, Option<String>>,
    pub base_urls: BTreeMap<String, Option<String>>,
    pub approval_mode: Option<ApprovalMode>,
    pub approval_policy: Option<ApprovalPolicy>,
    pub worktrees: Option<WorktreesMode>,
    pub sandbox: Option<SandboxMode>,
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
    }

    pub fn from_table(table: &Table) -> Result<(Self, Vec<String>)> {
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
            .filter(|(key, _)| !PROFILEABLE_KEYS.contains(&key.as_str()))
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

        Ok((
            Self {
                default_provider,
                models,
                base_urls,
                approval_mode,
                approval_policy,
                worktrees,
                sandbox,
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
        let (overlay, warnings) = SettingsOverlay::from_table(&table).with_context(|| {
            format!("invalid profile `{}` at {}", name.as_str(), path.display())
        })?;
        Ok(Self {
            name,
            path,
            overlay,
            warnings,
        })
    }
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
        let (overlay, warnings) = SettingsOverlay::from_table(&table).unwrap();
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
        assert!(SettingsOverlay::from_table(&credentials).is_err());
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
}

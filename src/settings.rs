// Persistent yolop settings — currently just the preferred provider name.
//
// Stored at `<config_dir>/yolop/settings.json` (`~/.config/yolop/settings.json`
// on Linux). The capability that owns `/provider` writes through
// `SettingsStore` so the choice survives across runs.

use anyhow::{Context, Result};
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

#[derive(Debug, Clone, Default)]
pub struct Settings {
    pub provider: Option<String>,
}

impl Settings {
    pub fn from_json(value: &Value) -> Self {
        Self {
            provider: value
                .get("provider")
                .and_then(Value::as_str)
                .map(str::to_string),
        }
    }

    pub fn to_json(&self) -> Value {
        let mut obj = Map::new();
        if let Some(p) = &self.provider {
            obj.insert("provider".to_string(), Value::String(p.clone()));
        }
        Value::Object(obj)
    }
}

pub fn default_settings_path() -> Option<PathBuf> {
    dirs::config_dir().map(|p| p.join("yolop").join("settings.json"))
}

pub fn load_from(path: &Path) -> Settings {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Settings::default();
    };
    let value: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    Settings::from_json(&value)
}

fn save_to(path: &Path, settings: &Settings) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("create settings dir {}", dir.display()))?;
    }
    let json = serde_json::to_string_pretty(&settings.to_json()).context("serialize settings")?;
    std::fs::write(path, json).with_context(|| format!("write settings {}", path.display()))?;
    Ok(())
}

/// Thread-safe handle shared across capabilities. The cached `Settings` is
/// the source of truth in memory; mutations flush to disk synchronously so
/// a crash mid-session leaves the on-disk file consistent.
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_via_disk() {
        let tmp = tempfile::tempdir().expect("tmp");
        let path = tmp.path().join("nested").join("settings.json");
        let store = SettingsStore::open(path.clone());
        assert!(store.snapshot().provider.is_none());
        store
            .set_provider(Some("anthropic".to_string()))
            .expect("save");

        let reloaded = SettingsStore::open(path);
        assert_eq!(reloaded.snapshot().provider.as_deref(), Some("anthropic"));
    }

    #[test]
    fn missing_file_yields_default() {
        let tmp = tempfile::tempdir().expect("tmp");
        let path = tmp.path().join("absent.json");
        let store = SettingsStore::open(path);
        assert!(store.snapshot().provider.is_none());
    }

    #[test]
    fn malformed_file_yields_default() {
        let tmp = tempfile::tempdir().expect("tmp");
        let path = tmp.path().join("settings.json");
        std::fs::write(&path, "not json").expect("write");
        let store = SettingsStore::open(path);
        assert!(store.snapshot().provider.is_none());
    }

    #[test]
    fn clearing_provider_persists_empty_object() {
        let tmp = tempfile::tempdir().expect("tmp");
        let path = tmp.path().join("settings.json");
        let store = SettingsStore::open(path.clone());
        store
            .set_provider(Some("openai".to_string()))
            .expect("save");
        store.set_provider(None).expect("save");
        let reloaded = SettingsStore::open(path);
        assert!(reloaded.snapshot().provider.is_none());
    }
}

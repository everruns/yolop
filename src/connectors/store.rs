//! Persistent storage for connector credentials.
//!
//! Credentials live in `<config_dir>/yolop/connections.toml`, beside
//! `settings.toml`. Values are written owner-only on Unix, matching the
//! settings token policy.

use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use toml::Table;
use toml::Value as TomlValue;

/// One saved connector credential bundle.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StoredConnection {
    /// Form fields submitted by the user (e.g. `api_key`).
    pub fields: BTreeMap<String, String>,
    /// Optional provider metadata returned from validation.
    pub metadata: Option<Value>,
}

#[derive(Debug, Clone, Default)]
struct ConnectionsFile {
    connections: BTreeMap<String, StoredConnection>,
}

impl ConnectionsFile {
    fn from_table(table: &Table) -> Self {
        let mut connections = BTreeMap::new();
        for (provider, value) in table {
            let Some(entry) = value.as_table() else {
                continue;
            };
            let mut fields = BTreeMap::new();
            let mut metadata = None;
            for (key, val) in entry {
                if key == "metadata" {
                    metadata = val
                        .as_str()
                        .map(|s| Value::String(s.to_string()))
                        .or_else(|| serde_json::to_value(val).ok());
                    continue;
                }
                if let Some(text) = val.as_str() {
                    fields.insert(key.clone(), text.to_string());
                }
            }
            if !fields.is_empty() {
                connections.insert(provider.clone(), StoredConnection { fields, metadata });
            }
        }
        Self { connections }
    }

    fn to_table(&self) -> Table {
        let mut root = Table::new();
        for (provider, conn) in &self.connections {
            let mut entry = Table::new();
            for (key, value) in &conn.fields {
                entry.insert(key.clone(), TomlValue::String(value.clone()));
            }
            if let Some(metadata) = &conn.metadata
                && let Ok(val) = toml::Value::try_from(metadata.clone())
            {
                entry.insert("metadata".to_string(), val);
            }
            root.insert(provider.clone(), TomlValue::Table(entry));
        }
        root
    }
}

pub fn default_connections_path() -> Option<PathBuf> {
    crate::settings::default_settings_path().map(|p| {
        p.parent()
            .map(|dir| dir.join("connections.toml"))
            .unwrap_or_else(|| PathBuf::from("connections.toml"))
    })
}

fn load_from(path: &Path) -> ConnectionsFile {
    let Ok(text) = std::fs::read_to_string(path) else {
        return ConnectionsFile::default();
    };
    let table: Table = toml::from_str(&text).unwrap_or_default();
    ConnectionsFile::from_table(&table)
}

fn save_to(path: &Path, file: &ConnectionsFile) -> Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    std::fs::create_dir_all(&parent)
        .with_context(|| format!("create connections dir {}", parent.display()))?;
    let toml_text = toml::to_string(&file.to_table()).context("serialize connections")?;

    let file_name = path
        .file_name()
        .with_context(|| format!("connections path has no file name: {}", path.display()))?;
    let mut tmp_name = std::ffi::OsString::from(".");
    tmp_name.push(file_name);
    tmp_name.push(format!(".tmp.{}", std::process::id()));
    let tmp_path = parent.join(tmp_name);

    let write_result = (|| -> Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            let file = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp_path)
                .with_context(|| format!("open temp connections {}", tmp_path.display()))?;
            let mut file = file;
            file.write_all(toml_text.as_bytes())
                .with_context(|| format!("write temp connections {}", tmp_path.display()))?;
            file.sync_all()
                .with_context(|| format!("sync temp connections {}", tmp_path.display()))?;
            Ok(())
        }
        #[cfg(not(unix))]
        {
            std::fs::write(&tmp_path, toml_text.as_bytes())
                .with_context(|| format!("write temp connections {}", tmp_path.display()))?;
            Ok(())
        }
    })();
    if let Err(e) = write_result {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(e);
    }
    std::fs::rename(&tmp_path, path)
        .with_context(|| format!("rename {} -> {}", tmp_path.display(), path.display()))?;
    Ok(())
}

/// Thread-safe handle to persisted connector credentials.
pub struct ConnectionStore {
    path: PathBuf,
    inner: Mutex<ConnectionsFile>,
}

impl ConnectionStore {
    pub fn open(path: PathBuf) -> Self {
        let file = load_from(&path);
        Self {
            path,
            inner: Mutex::new(file),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn get(&self, provider: &str) -> Option<StoredConnection> {
        self.inner
            .lock()
            .expect("connections lock poisoned")
            .connections
            .get(provider)
            .cloned()
    }

    pub fn is_connected(&self, provider: &str) -> bool {
        self.get(provider)
            .map(|c| c.fields.values().any(|v| !v.trim().is_empty()))
            .unwrap_or(false)
    }

    pub fn save(&self, provider: &str, connection: StoredConnection) -> Result<()> {
        let mut guard = self.inner.lock().expect("connections lock poisoned");
        guard.connections.insert(provider.to_string(), connection);
        save_to(&self.path, &guard)
    }

    pub fn clear(&self, provider: &str) -> Result<bool> {
        let mut guard = self.inner.lock().expect("connections lock poisoned");
        let existed = guard.connections.remove(provider).is_some();
        save_to(&self.path, &guard)?;
        Ok(existed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_via_disk() {
        let tmp = tempfile::tempdir().expect("tmp");
        let path = tmp.path().join("connections.toml");
        let store = ConnectionStore::open(path.clone());

        let mut fields = BTreeMap::new();
        fields.insert("api_key".to_string(), "daytona-secret".to_string());
        store
            .save(
                "daytona",
                StoredConnection {
                    fields,
                    metadata: None,
                },
            )
            .expect("save");

        let reloaded = ConnectionStore::open(path);
        let conn = reloaded.get("daytona").expect("daytona");
        assert_eq!(
            conn.fields.get("api_key").map(String::as_str),
            Some("daytona-secret")
        );
    }

    #[test]
    fn clear_removes_provider() {
        let tmp = tempfile::tempdir().expect("tmp");
        let store = ConnectionStore::open(tmp.path().join("connections.toml"));
        store
            .save(
                "daytona",
                StoredConnection {
                    fields: BTreeMap::from([("api_key".to_string(), "x".to_string())]),
                    metadata: None,
                },
            )
            .expect("save");
        assert!(store.clear("daytona").expect("clear"));
        assert!(!store.is_connected("daytona"));
    }
}

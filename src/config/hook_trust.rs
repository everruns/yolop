//! Trust records for workspace hook files.
//!
//! Decision: a `<workspace>/.agents/hooks.json` arrives with a clone, so it is
//! repository content, not the user's choice. Its hooks run shell commands on
//! the host with the full environment, at session start and on every tool
//! event, so loading them unasked would let a tracked file execute a command
//! (the rule workspace stdio MCP servers already follow, see `config/mcp.rs`).
//! A workspace hooks file therefore loads only after the user trusts it from
//! their own terminal (`yolop config hooks trust`).
//!
//! A record is keyed by the canonical workspace path and pins the SHA-256 of
//! the file bytes, so any edit, by a `git pull` or by the agent, revokes trust
//! until the user reviews the file again. The records live in yolop's config
//! dir, beside the global `hooks.json`, outside every workspace: a sandboxed
//! session cannot write there, and no repository can ship one.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const HOOK_TRUST_FILE_NAME: &str = "hook-trust.json";

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default)]
struct TrustFile {
    workspaces: BTreeMap<String, TrustEntry>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct TrustEntry {
    sha256: String,
    trusted_at: String,
}

#[derive(Debug, Clone)]
pub struct HookTrustStore {
    path: PathBuf,
}

impl HookTrustStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// True when `content` is exactly the bytes the user trusted for this
    /// workspace. A missing or unreadable trust file trusts nothing.
    pub fn is_trusted(&self, workspace_root: &Path, content: &[u8]) -> bool {
        let Ok(file) = self.read() else {
            return false;
        };
        file.workspaces
            .get(&workspace_key(workspace_root))
            .is_some_and(|entry| entry.sha256 == content_digest(content))
    }

    /// Record `content` as trusted for this workspace, replacing any earlier
    /// record. Returns the pinned digest.
    pub fn trust(&self, workspace_root: &Path, content: &[u8]) -> Result<String> {
        let mut file = self.read()?;
        let digest = content_digest(content);
        file.workspaces.insert(
            workspace_key(workspace_root),
            TrustEntry {
                sha256: digest.clone(),
                trusted_at: chrono::Utc::now().to_rfc3339(),
            },
        );
        self.write(&file)?;
        Ok(digest)
    }

    /// Drop this workspace's record. Returns whether one existed.
    pub fn revoke(&self, workspace_root: &Path) -> Result<bool> {
        let mut file = self.read()?;
        let removed = file
            .workspaces
            .remove(&workspace_key(workspace_root))
            .is_some();
        if removed {
            self.write(&file)?;
        }
        Ok(removed)
    }

    fn read(&self) -> Result<TrustFile> {
        match std::fs::read(&self.path) {
            Ok(raw) => serde_json::from_slice(&raw)
                .with_context(|| format!("parsing {}", self.path.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(TrustFile::default()),
            Err(error) => Err(error).with_context(|| format!("reading {}", self.path.display())),
        }
    }

    fn write(&self, file: &TrustFile) -> Result<()> {
        let mut content = serde_json::to_vec_pretty(file).context("serialize hook trust")?;
        content.push(b'\n');
        super::hooks::write_atomic(&self.path, &content)
    }
}

/// `sha256:<hex>` of the raw file bytes.
pub fn content_digest(content: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(content);
    let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("sha256:{hex}")
}

/// Canonical path when it resolves, so `./repo` and `/abs/repo` (or a symlink
/// to it) share one record; the path as given otherwise.
fn workspace_key(workspace_root: &Path) -> String {
    std::fs::canonicalize(workspace_root)
        .unwrap_or_else(|_| workspace_root.to_path_buf())
        .display()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trust_pins_content_and_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        let ws_a = tmp.path().join("a");
        let ws_b = tmp.path().join("b");
        std::fs::create_dir_all(&ws_a).unwrap();
        std::fs::create_dir_all(&ws_b).unwrap();
        let store = HookTrustStore::new(tmp.path().join("cfg").join(HOOK_TRUST_FILE_NAME));

        assert!(!store.is_trusted(&ws_a, b"{}"));
        store.trust(&ws_a, b"{\"hooks\":[]}").unwrap();
        assert!(store.is_trusted(&ws_a, b"{\"hooks\":[]}"));
        assert!(!store.is_trusted(&ws_a, b"{\"hooks\":[1]}"), "edit revokes");
        assert!(!store.is_trusted(&ws_b, b"{\"hooks\":[]}"), "per workspace");
        assert!(
            store.is_trusted(&ws_a.join("."), b"{\"hooks\":[]}"),
            "keyed by canonical path"
        );

        assert!(store.revoke(&ws_a).unwrap());
        assert!(!store.is_trusted(&ws_a, b"{\"hooks\":[]}"));
        assert!(!store.revoke(&ws_a).unwrap());
    }

    #[test]
    fn malformed_trust_file_trusts_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(HOOK_TRUST_FILE_NAME);
        std::fs::write(&path, "not json").unwrap();
        let store = HookTrustStore::new(path);
        assert!(!store.is_trusted(tmp.path(), b"{}"));
    }
}

//! Bounded fingerprints of Git-visible files for check freshness, never a commit.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::Path;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct WorkspaceEvidence {
    pub head: String,
    pub fingerprint: String,
}

pub(crate) async fn capture(root: &Path) -> Result<WorkspaceEvidence> {
    tokio::time::timeout(std::time::Duration::from_secs(5), capture_inner(root))
        .await
        .context("workspace evidence capture timed out")?
}

async fn capture_inner(root: &Path) -> Result<WorkspaceEvidence> {
    use super::readonly_git::git;
    let head = String::from_utf8(git(root, &["rev-parse", "--verify", "HEAD"]).await?)?
        .trim()
        .to_owned();
    anyhow::ensure!(
        head.len() >= 40 && head.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "invalid Git head"
    );
    let diff = git(
        root,
        &[
            "diff",
            "--relative",
            "--binary",
            "--no-ext-diff",
            "--no-textconv",
            "--no-renames",
            "--ignore-submodules=all",
            "HEAD",
            "--",
            ".",
        ],
    )
    .await?;
    let untracked = git(
        root,
        &[
            "ls-files",
            "--others",
            "--exclude-standard",
            "-z",
            "--",
            ".",
        ],
    )
    .await?;
    let root = root.to_path_buf();
    let fingerprint = tokio::task::spawn_blocking(move || -> Result<String> {
        let mut hash = Sha256::new();
        hash.update(&diff);
        hash.update(&untracked);
        let mut total = 0usize;
        for (count, path) in untracked
            .split(|byte| *byte == 0)
            .filter(|path| !path.is_empty())
            .enumerate()
        {
            anyhow::ensure!(
                count < 256,
                "workspace evidence exceeds 256 untracked files"
            );
            let path = std::str::from_utf8(path).context("unsupported untracked path encoding")?;
            let file = super::workspace_file::open(&root, path)?;
            let remaining = (2 * 1024 * 1024usize).saturating_sub(total);
            let mut bytes = Vec::new();
            file.take(remaining as u64 + 1).read_to_end(&mut bytes)?;
            anyhow::ensure!(
                bytes.len() <= remaining,
                "workspace evidence exceeds 2 MiB of untracked content"
            );
            total += bytes.len();
            hash.update((bytes.len() as u64).to_le_bytes());
            hash.update(bytes);
        }
        Ok(hash
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect())
    })
    .await??;
    Ok(WorkspaceEvidence { head, fingerprint })
}

/// An unavailable stamp carries its bounded diagnostic rather than implying
/// that a historical result was verified against the current workspace.
pub(crate) async fn record(root: &Path) -> serde_json::Value {
    match capture(root).await {
        Ok(stamp) => serde_json::to_value(stamp).unwrap_or_default(),
        Err(error) => {
            serde_json::json!({"unavailable":format!("{error:#}").chars().take(1024).collect::<String>()})
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn git(root: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    #[tokio::test]
    async fn capture_detects_tracked_untracked_and_head_changes_and_refuses_incomplete_evidence() {
        let root = tempfile::tempdir().unwrap();
        git(root.path(), &["init", "-q"]);
        git(root.path(), &["config", "user.name", "Test User"]);
        git(root.path(), &["config", "user.email", "test@example.com"]);
        std::fs::write(root.path().join("a"), "before").unwrap();
        git(root.path(), &["add", "a"]);
        git(root.path(), &["commit", "-qm", "baseline"]);
        let before = capture(root.path()).await.unwrap();
        assert_eq!(before, capture(root.path()).await.unwrap());
        std::fs::write(root.path().join("a"), "after").unwrap();
        assert_ne!(before, capture(root.path()).await.unwrap());
        std::fs::write(root.path().join("a"), "before").unwrap();
        std::fs::write(root.path().join("new"), "new").unwrap();
        let new = capture(root.path()).await.unwrap();
        assert_ne!(before, new);
        std::fs::write(root.path().join("new"), "edit").unwrap();
        assert_ne!(new, capture(root.path()).await.unwrap());
        std::fs::write(root.path().join("new"), vec![0; 2 * 1024 * 1024 + 1]).unwrap();
        assert!(capture(root.path()).await.is_err());
        assert!(
            record(root.path()).await["unavailable"]
                .as_str()
                .unwrap()
                .contains("2 MiB")
        );
    }
}

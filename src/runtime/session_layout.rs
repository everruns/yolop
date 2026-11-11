//! Move earlier per-session artifact paths while the session lease is held.
use anyhow::{Context, Result, ensure};
use std::path::Path;

pub(super) fn migrate_background(directory: &Path) -> Result<()> {
    let old = directory.join(".background");
    if !old.exists() {
        return Ok(());
    }
    merge_directory(&old, &directory.join("background"))
}

fn merge_directory(source: &Path, target: &Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(source)?;
    ensure!(
        metadata.is_dir(),
        "background migration source must be a directory"
    );
    if !target.exists() {
        std::fs::rename(source, target).context("move background artifacts")?;
    } else {
        ensure!(
            std::fs::symlink_metadata(target)?.is_dir(),
            "background migration target must be a directory"
        );
        for entry in std::fs::read_dir(source)? {
            let entry = entry?;
            let destination = target.join(entry.file_name());
            if !destination.exists() {
                ensure!(
                    !entry.file_type()?.is_symlink(),
                    "background artifacts must not be symlinks"
                );
                std::fs::rename(entry.path(), destination)?;
            } else {
                ensure!(
                    entry.file_type()?.is_dir(),
                    "conflicting background artifact {}",
                    destination.display()
                );
                merge_directory(&entry.path(), &destination)?;
            }
        }
        std::fs::remove_dir(source)?;
    }
    #[cfg(unix)]
    std::fs::File::open(target.parent().expect("background directory parent"))?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merges_old_background_runs_with_existing_logs_and_retries() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join(".background/run_1")).unwrap();
        std::fs::write(
            root.path().join(".background/run_1/output.log"),
            "retained output",
        )
        .unwrap();
        std::fs::create_dir_all(root.path().join("background")).unwrap();
        std::fs::write(root.path().join("background/index.json"), "{}").unwrap();
        migrate_background(root.path()).unwrap();
        migrate_background(root.path()).unwrap();
        assert_eq!(
            std::fs::read_to_string(root.path().join("background/run_1/output.log")).unwrap(),
            "retained output"
        );
        assert!(root.path().join("background/index.json").exists());
        assert!(!root.path().join(".background").exists());
    }

    #[test]
    fn conflicting_background_evidence_is_not_replaced() {
        let root = tempfile::tempdir().unwrap();
        for (folder, contents) in [(".background", "old"), ("background", "current")] {
            std::fs::create_dir_all(root.path().join(folder)).unwrap();
            std::fs::write(root.path().join(folder).join("result.json"), contents).unwrap();
        }
        assert!(migrate_background(root.path()).is_err());
        assert_eq!(
            std::fs::read_to_string(root.path().join(".background/result.json")).unwrap(),
            "old"
        );
        assert_eq!(
            std::fs::read_to_string(root.path().join("background/result.json")).unwrap(),
            "current"
        );
    }
}

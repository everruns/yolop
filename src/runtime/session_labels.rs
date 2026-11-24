//! Private presentation preferences, separate from canonical session history.

use anyhow::{Context, Result};
use everruns_contracts::typed_id::SessionId;
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::path::Path;

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub(crate) struct SessionLabels {
    pub title: Option<String>,
    pub pinned: bool,
    pub archived: bool,
}

pub(crate) fn read(root: &Path, session: SessionId) -> Result<SessionLabels> {
    let directory = root.join(session.to_string());
    if !directory.exists() {
        return Ok(SessionLabels::default());
    }
    anyhow::ensure!(
        !std::fs::symlink_metadata(&directory)?
            .file_type()
            .is_symlink(),
        "session directory is a symlink"
    );
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = match options.open(directory.join("ui.json")) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(SessionLabels::default());
        }
        Err(error) => return Err(error.into()),
    };
    anyhow::ensure!(
        file.metadata()?.is_file(),
        "session labels are not a regular file"
    );
    let mut bytes = Vec::new();
    file.take(4097).read_to_end(&mut bytes)?;
    anyhow::ensure!(bytes.len() <= 4096, "session labels exceed 4 KiB");
    let value: SessionLabels = serde_json::from_slice(&bytes).context("invalid session labels")?;
    validate(&value)?;
    Ok(value)
}

pub(crate) fn write(root: &Path, session: SessionId, labels: &SessionLabels) -> Result<()> {
    validate(labels)?;
    let directory = root.join(session.to_string());
    let metadata = std::fs::symlink_metadata(&directory)?;
    anyhow::ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "session directory is not a regular directory"
    );
    let mut file = tempfile::NamedTempFile::new_in(&directory)?;
    file.write_all(&serde_json::to_vec(labels)?)?;
    file.as_file().sync_all()?;
    file.persist(directory.join("ui.json"))?;
    Ok(())
}

fn validate(labels: &SessionLabels) -> Result<()> {
    if let Some(title) = &labels.title {
        anyhow::ensure!(
            title.len() <= 512 && !title.chars().any(char::is_control),
            "session title must be at most 512 bytes without control characters"
        );
    }
    Ok(())
}

pub(crate) fn active_elsewhere(root: &Path, session: SessionId) -> Option<bool> {
    let path = root.join(".locks").join(session.to_string());
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Some(false),
        Err(_) => return None,
    };
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    match file.try_lock() {
        Ok(()) => Some(false),
        Err(std::fs::TryLockError::WouldBlock) => Some(true),
        Err(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn labels_are_atomic_validated_private_and_lease_status_is_live() {
        let root = tempfile::tempdir().unwrap();
        let id = SessionId::new();
        std::fs::create_dir(root.path().join(id.to_string())).unwrap();
        assert!(!read(root.path(), id).unwrap().pinned);
        let labels = SessionLabels {
            title: Some("Fix terminal focus".into()),
            pinned: true,
            archived: true,
        };
        write(root.path(), id, &labels).unwrap();
        let readback = read(root.path(), id).unwrap();
        assert_eq!(readback.title, labels.title);
        assert!(readback.pinned && readback.archived);
        let bad = SessionLabels {
            title: Some("bad\nname".into()),
            ..Default::default()
        };
        assert!(write(root.path(), id, &bad).is_err());
        assert_eq!(active_elsewhere(root.path(), id), Some(false));
        std::fs::create_dir(root.path().join(".locks")).unwrap();
        let lease = std::fs::File::create(root.path().join(".locks").join(id.to_string())).unwrap();
        lease.lock().unwrap();
        assert_eq!(active_elsewhere(root.path(), id), Some(true));
        drop(lease);
        assert_eq!(active_elsewhere(root.path(), id), Some(false));
        #[cfg(unix)]
        {
            use std::os::unix::fs::{PermissionsExt, symlink};
            let path = root.path().join(id.to_string()).join("ui.json");
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            std::fs::remove_file(&path).unwrap();
            symlink("/etc/passwd", &path).unwrap();
            assert!(read(root.path(), id).is_err());
        }
    }
}

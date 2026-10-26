//! Stable local key encryption key, kept outside the authority database.
use anyhow::{Context, Result, ensure};
use base64::Engine;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;

pub fn open_or_create(directory: &Path) -> Result<String> {
    let mut lock_options = OpenOptions::new();
    lock_options
        .read(true)
        .write(true)
        .create(true)
        .truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        lock_options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let lock = lock_options.open(directory.join("engine.key.lock"))?;
    validate(&lock)?;
    lock.lock().context("lock Engine key initialization")?;
    let _lock = KeyLock(lock);
    let path = directory.join("engine.key");
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = match options.open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            ensure!(
                !directory.join("engine.sqlite").exists(),
                "Engine encryption key is missing; restore engine.key before resuming sessions"
            );
            let mut create = OpenOptions::new();
            create.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                create.mode(0o600);
            }
            match create.open(&path) {
                Ok(mut file) => {
                    let key = format!(
                        "local-v1:{}",
                        base64::engine::general_purpose::STANDARD
                            .encode(rand::random::<[u8; 32]>())
                    );
                    file.write_all(key.as_bytes())
                        .context("write Engine encryption key")?;
                    file.sync_all().context("sync Engine encryption key")?;
                    File::open(directory)?.sync_all()?;
                    return Ok(key);
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    options.open(&path)?
                }
                Err(error) => return Err(error).context("create Engine encryption key"),
            }
        }
        Err(error) => return Err(error).context("open Engine encryption key"),
    };
    validate(&file)?;
    let mut key = String::new();
    file.take(129).read_to_string(&mut key)?;
    ensure!(key.len() <= 128, "invalid Engine encryption key file");
    Ok(key)
}

struct KeyLock(File);
impl Drop for KeyLock {
    fn drop(&mut self) {
        if let Err(error) = self.0.unlock() {
            tracing::warn!(%error, "release Engine key initialization lock");
        }
    }
}

fn validate(file: &File) -> Result<()> {
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file() && metadata.len() <= 128,
        "invalid Engine encryption key file"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        ensure!(
            metadata.nlink() == 1
                && metadata.mode() & 0o077 == 0
                && metadata.uid() == unsafe { libc::getuid() },
            "Engine encryption key must be private and owned by the current user"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concurrent_initialization_and_restart_keep_the_same_key() {
        let dir = tempfile::tempdir().unwrap();
        let keys = std::thread::scope(|scope| {
            let workers: Vec<_> = (0..8)
                .map(|_| scope.spawn(|| open_or_create(dir.path()).unwrap()))
                .collect();
            workers
                .into_iter()
                .map(|worker| worker.join().unwrap())
                .collect::<Vec<_>>()
        });
        assert!(keys.iter().all(|key| key == &keys[0]));
        assert_eq!(open_or_create(dir.path()).unwrap(), keys[0]);
        std::fs::write(dir.path().join("engine.sqlite"), "").unwrap();
        std::fs::remove_file(dir.path().join("engine.key")).unwrap();
        assert!(
            open_or_create(dir.path())
                .unwrap_err()
                .to_string()
                .contains("missing")
        );
    }

    #[cfg(unix)]
    #[test]
    fn refuses_aliased_or_public_key_files() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let dir = tempfile::tempdir().unwrap();
        open_or_create(dir.path()).unwrap();
        let key = dir.path().join("engine.key");
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(open_or_create(dir.path()).is_err());
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
        let other = dir.path().join("other");
        std::fs::hard_link(&key, &other).unwrap();
        assert!(open_or_create(dir.path()).is_err());
        std::fs::remove_file(&key).unwrap();
        symlink(&other, &key).unwrap();
        assert!(open_or_create(dir.path()).is_err());
    }
}

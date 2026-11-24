//! Descriptor-relative, non-following access to regular workspace files.

use anyhow::{Result, ensure};
use std::path::{Component, Path};

pub(crate) fn open(root: &Path, path: &str) -> Result<std::fs::File> {
    let relative = Path::new(path);
    ensure!(
        !relative.is_absolute(),
        "attach a path relative to the workspace"
    );
    let components = relative
        .components()
        .map(|part| match part {
            Component::Normal(name) => Ok(name.to_os_string()),
            _ => anyhow::bail!("attachment paths cannot contain traversal or prefixes"),
        })
        .collect::<Result<Vec<_>>>()?;
    ensure!(!components.is_empty(), "attachment path is required");
    #[cfg(unix)]
    let file = {
        use rustix::fs::{Mode, OFlags, open, openat};
        let mut directory = open(
            root,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        for part in &components[..components.len() - 1] {
            directory = openat(
                &directory,
                part,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )?;
        }
        std::fs::File::from(openat(
            &directory,
            components.last().unwrap(),
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        )?)
    };
    #[cfg(not(unix))]
    let file = {
        let mut full = root.to_path_buf();
        for part in &components {
            full.push(part);
            ensure!(
                !std::fs::symlink_metadata(&full)?.file_type().is_symlink(),
                "attachment symlinks are not followed"
            );
        }
        std::fs::File::open(full)?
    };
    ensure!(
        file.metadata()?.is_file(),
        "only regular files can be attached"
    );
    Ok(file)
}

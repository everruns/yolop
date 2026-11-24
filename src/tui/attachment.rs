//! Bounded, workspace-contained snapshots for explicit composer attachments.

use anyhow::{Context, Result, ensure};
use std::io::Read;
use std::path::{Component, Path};

pub(super) const MAX_ATTACHMENT_BYTES: usize = 512 * 1024;

pub(super) fn read(root: &Path, path: &str) -> Result<Vec<u8>> {
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
    let mut bytes = Vec::new();
    file.take(MAX_ATTACHMENT_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= MAX_ATTACHMENT_BYTES,
        "attachment exceeds the 512 KiB limit"
    );
    Ok(bytes)
}

pub(super) fn text(root: &Path, reference: &str) -> Result<(String, String)> {
    let (path, range) = match reference.rsplit_once('#') {
        Some((path, range)) => (path, Some(range)),
        None => (reference, None),
    };
    let content = String::from_utf8(read(root, path)?).context("attachment is not UTF-8 text")?;
    ensure!(
        !content.contains('\0'),
        "binary file cannot be attached as text"
    );
    let selected = if let Some(range) = range {
        let (start, end) = range.split_once('-').unwrap_or((range, range));
        let start = start
            .parse::<usize>()
            .context("line numbers must be positive integers")?;
        let end = end
            .parse::<usize>()
            .context("line numbers must be positive integers")?;
        let lines = content.lines().collect::<Vec<_>>();
        ensure!(
            start > 0 && end >= start && end <= lines.len(),
            "line range is outside the file"
        );
        lines[start - 1..end].join("\n")
    } else {
        content
    };
    Ok((
        format!("File {}", super::review::file_display(reference)),
        selected,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn file_ranges_are_explicit_bounded_and_inside_the_workspace() {
        let repo = tempfile::tempdir().unwrap();
        std::fs::write(repo.path().join("code.rs"), "one\ntwo\nthree\n").unwrap();
        assert_eq!(text(repo.path(), "code.rs#2-3").unwrap().1, "two\nthree");
        for path in [
            "../code.rs",
            "code.rs#0",
            "code.rs#3-2",
            "code.rs#2-9",
            "/etc/passwd",
        ] {
            assert!(text(repo.path(), path).is_err(), "{path}");
        }
        std::fs::File::create(repo.path().join("large"))
            .unwrap()
            .set_len(MAX_ATTACHMENT_BYTES as u64 + 1)
            .unwrap();
        assert!(read(repo.path(), "large").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn attachments_never_follow_file_or_directory_symlinks() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret"), "private").unwrap();
        std::os::unix::fs::symlink(outside.path().join("secret"), root.path().join("file"))
            .unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("directory")).unwrap();
        assert!(read(root.path(), "file").is_err());
        assert!(read(root.path(), "directory/secret").is_err());
    }
}

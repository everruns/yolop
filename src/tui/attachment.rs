//! Bounded, workspace-contained snapshots for explicit composer attachments.

use anyhow::{Context, Result, ensure};
use std::io::Read;
use std::path::Path;

pub(super) const MAX_ATTACHMENT_BYTES: usize = 512 * 1024;

pub(super) use crate::exec::workspace_file::open;

pub(super) fn read(root: &Path, path: &str) -> Result<Vec<u8>> {
    let file = open(root, path)?;
    let mut bytes = Vec::new();
    file.take(MAX_ATTACHMENT_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= MAX_ATTACHMENT_BYTES,
        "attachment exceeds the 512 KiB limit"
    );
    Ok(bytes)
}

pub(super) fn range_suffix(reference: &str) -> &str {
    let Some((_, range)) = reference.rsplit_once('#') else {
        return "";
    };
    let (start, end) = range.split_once('-').unwrap_or((range, range));
    if [start, end].iter().all(|part| {
        !part.is_empty()
            && part.bytes().all(|byte| byte.is_ascii_digit())
            && part.parse::<usize>().is_ok()
    }) {
        &reference[reference.len() - range.len() - 1..]
    } else {
        ""
    }
}

pub(super) fn text(root: &Path, reference: &str) -> Result<(String, String)> {
    let suffix = range_suffix(reference);
    let (path, range) = if suffix.is_empty() {
        (reference, None)
    } else {
        let range = &suffix[1..];
        let (start, end) = range.split_once('-').unwrap_or((range, range));
        (
            &reference[..reference.len() - suffix.len()],
            Some((start.parse::<usize>()?, end.parse::<usize>()?)),
        )
    };
    let content = String::from_utf8(read(root, path)?).context("attachment is not UTF-8 text")?;
    ensure!(
        !content.contains('\0'),
        "binary file cannot be attached as text"
    );
    let selected = if let Some((start, end)) = range {
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
        std::fs::write(repo.path().join("code.rs#1-2-3"), "literal").unwrap();
        assert_eq!(text(repo.path(), "code.rs#1-2-3").unwrap().1, "literal");
        std::fs::write(repo.path().join("a#b.txt"), "one\ntwo\nthree\n").unwrap();
        assert_eq!(text(repo.path(), "a#b.txt").unwrap().1, "one\ntwo\nthree\n");
        assert_eq!(text(repo.path(), "a#b.txt#2-3").unwrap().1, "two\nthree");
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

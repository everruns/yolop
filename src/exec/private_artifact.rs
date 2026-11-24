//! Bounded text previews of session-owned output, never workspace paths.
use anyhow::{Context, Result};
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

pub(crate) fn read(root: &Path, path: &str) -> Result<String> {
    let relative = if let Ok(path) = Path::new(path).strip_prefix(root) {
        path.to_str().context("artifact path encoding")?
    } else {
        path.trim_start_matches('/')
    };
    let relative = relative
        .strip_prefix(".background/")
        .map(|suffix| format!("background/{suffix}"))
        .unwrap_or_else(|| relative.to_owned());
    anyhow::ensure!(
        ["background/", "outputs/", ".tasks/"]
            .iter()
            .any(|prefix| relative.starts_with(prefix)),
        "artifact is outside the private session roots"
    );
    let mut file = super::workspace_file::open(root, &relative)?;
    let skip = file.metadata()?.len().saturating_sub(64 * 1024);
    file.seek(SeekFrom::Start(skip))?;
    let mut bytes = Vec::new();
    file.take(64 * 1024).read_to_end(&mut bytes)?;
    Ok(format!(
        "{}{}",
        if skip > 0 {
            "[Showing last 64 KiB]\n"
        } else {
            ""
        },
        String::from_utf8_lossy(&bytes)
    ))
}

/// Only follow the runtime's explicit output references. Limit recursion and
/// count independently of the size of a recorded JSON result.
pub(crate) fn references(value: &serde_json::Value) -> Vec<String> {
    fn visit(value: &serde_json::Value, depth: usize, paths: &mut Vec<String>) {
        if depth > 8 || paths.len() >= 8 {
            return;
        }
        match value {
            serde_json::Value::Object(fields) => {
                for (key, value) in fields {
                    if matches!(
                        key.as_str(),
                        "full_output" | "output_files" | "stdout_file" | "stderr_file" | "path"
                    ) {
                        for value in
                            std::iter::once(value).chain(value.as_array().into_iter().flatten())
                        {
                            if let Some(path) = value.as_str()
                                && path.len() <= 4096
                                && paths.len() < 8
                                && !paths.iter().any(|existing| existing == path)
                            {
                                paths.push(path.into());
                            }
                        }
                    }
                    visit(value, depth + 1, paths);
                }
            }
            serde_json::Value::Array(values) => {
                for value in values.iter().take(64) {
                    visit(value, depth + 1, paths);
                }
            }
            _ => {}
        }
    }
    let mut paths = Vec::new();
    visit(value, 0, &mut paths);
    paths
}

pub(crate) fn previews(root: &Path, paths: Vec<String>) -> String {
    let mut seen = std::collections::HashSet::new();
    let mut pending = std::collections::VecDeque::from(paths);
    let mut output = String::new();
    while let Some(path) = pending.pop_front() {
        if seen.len() >= 8 {
            break;
        }
        if !seen.insert(path.clone()) {
            continue;
        }
        match read(root, &path) {
            Ok(text) => {
                if let Ok(value) = serde_json::from_str(&text) {
                    pending.extend(references(&value));
                }
                output.push_str(&format!("\n\nArtifact {path}:\n{text}"));
            }
            Err(error) => output.push_str(&format!("\n\nArtifact {path} unavailable: {error:#}")),
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bounded_private_logs_follow_explicit_links_and_reject_escapes() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("background/a")).unwrap();
        std::fs::create_dir_all(root.path().join("outputs")).unwrap();
        std::fs::write(
            root.path().join("background/a/result.json"),
            r#"{"full_output":"/outputs/a.stdout"}"#,
        )
        .unwrap();
        std::fs::write(root.path().join("outputs/a.stdout"), "x".repeat(100_000)).unwrap();
        let text = previews(root.path(), vec!["/.background/a/result.json".into()]);
        assert!(text.contains("Showing last 64 KiB"));
        assert!(text.len() < 66_000);
        assert!(read(root.path(), "/outputs/../../secret").is_err());
        assert!(read(root.path(), "/workspace/file").is_err());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("/etc/passwd", root.path().join("outputs/link")).unwrap();
            assert!(read(root.path(), "/outputs/link").is_err());
        }
    }
}

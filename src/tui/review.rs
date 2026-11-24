//! Read-only Git projections for the changes panel. Git configuration must not
//! launch an external diff, pager, text converter, or filesystem monitor.

use anyhow::{Context, Result, bail};
use std::path::Path;
use std::process::Stdio;
use tokio::io::AsyncReadExt;

pub(super) const MAX_REVIEW_BYTES: usize = 512 * 1024;
const MAX_FILES: usize = 500;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum ReviewScope {
    #[default]
    WorkingTree,
    LastTurn,
    Session,
    Base,
}

impl ReviewScope {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::WorkingTree => "Working tree",
            Self::LastTurn => "Last turn",
            Self::Session => "Session",
            Self::Base => "Versus base",
        }
    }

    pub(super) fn next(self) -> Self {
        match self {
            Self::WorkingTree => Self::LastTurn,
            Self::LastTurn => Self::Session,
            Self::Session => Self::Base,
            Self::Base => Self::WorkingTree,
        }
    }
}

#[derive(Clone, Debug)]
pub(super) struct ChangedFile {
    pub path: String,
    pub summary: String,
    snapshot_blob: Option<String>,
}

#[derive(Clone, Debug)]
pub(super) struct ReviewSnapshot {
    pub files: Vec<ChangedFile>,
    pub reference: String,
    pub notice: String,
}

pub(super) async fn git(root: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let mut child = tokio::process::Command::new("git")
        .arg("--no-pager")
        .args(["-c", "core.fsmonitor=false", "-c", "core.quotePath=true"])
        .arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("start Git")?;
    let mut stdout = child
        .stdout
        .take()
        .expect("piped stdout")
        .take(MAX_REVIEW_BYTES as u64 + 1);
    let mut stderr = child.stderr.take().expect("piped stderr").take(8193);
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let mut out = Vec::new();
        let mut err = Vec::new();
        tokio::try_join!(stdout.read_to_end(&mut out), stderr.read_to_end(&mut err))?;
        // A full pipe must never deadlock the wait below.
        if out.len() > MAX_REVIEW_BYTES || err.len() > 8192 {
            bail!("Git output exceeds the review limit; narrow the comparison");
        }
        let status = child.wait().await?;
        let differences = args.contains(&"--no-index") && status.code() == Some(1);
        if !status.success() && !differences {
            bail!("{}", String::from_utf8_lossy(&err).trim());
        }
        Ok(out)
    })
    .await
    .context("Git review timed out")?;
    if result.is_err() {
        let _ = child.kill().await;
    }
    result
}

pub(super) async fn resolve(root: &Path, reference: &str) -> Result<String> {
    let bytes = git(
        root,
        &["rev-parse", "--verify", "--end-of-options", reference],
    )
    .await?;
    let oid = String::from_utf8(bytes)?.trim().to_string();
    anyhow::ensure!(
        oid.len() >= 40 && oid.bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid Git object"
    );
    Ok(oid)
}

pub(super) async fn collect(root: &Path, reference: &str) -> Result<ReviewSnapshot> {
    let reference = resolve(root, reference).await?;
    let bytes = git(
        root,
        &[
            "diff",
            "--relative",
            "--no-ext-diff",
            "--no-textconv",
            "--numstat",
            "-z",
            &reference,
            "--",
        ],
    )
    .await?;
    let mut files = Vec::new();
    let mut entries = bytes.split(|b| *b == 0);
    while let Some(entry) = entries.next().filter(|entry| !entry.is_empty()) {
        let mut fields = entry.splitn(3, |b| *b == b'\t');
        let added = fields.next().unwrap_or_default();
        let removed = fields.next().unwrap_or_default();
        let mut path = fields.next().unwrap_or_default();
        if path.is_empty() {
            let _old_path = entries.next();
            path = entries.next().unwrap_or_default();
        }
        files.push(ChangedFile {
            path: String::from_utf8(path.to_vec()).context("non-UTF-8 path cannot be reviewed")?,
            summary: format!(
                "+{} -{}",
                String::from_utf8_lossy(added),
                String::from_utf8_lossy(removed)
            ),
            snapshot_blob: None,
        });
        if files.len() == MAX_FILES {
            break;
        }
    }
    let untracked = git(root, &["ls-files", "--others", "--exclude-standard", "-z"]).await?;
    let untracked = untracked
        .split(|b| *b == 0)
        .filter(|path| !path.is_empty())
        .map(|path| {
            String::from_utf8(path.to_vec()).context("non-UTF-8 untracked path cannot be reviewed")
        })
        .collect::<Result<Vec<_>>>()?;
    let captured = if untracked.is_empty() {
        Vec::new()
    } else {
        let mut args = vec![
            "--literal-pathspecs",
            "ls-tree",
            "-r",
            "-z",
            &reference,
            "--",
        ];
        args.extend(untracked.iter().map(String::as_str));
        git(root, &args).await?
    };
    let captured = captured
        .split(|byte| *byte == 0)
        .filter_map(|entry| {
            let tab = entry.iter().position(|byte| *byte == b'\t')?;
            let metadata = std::str::from_utf8(&entry[..tab]).ok()?;
            let blob = metadata.split_whitespace().last()?;
            Some((&entry[tab + 1..], blob))
        })
        .collect::<std::collections::HashMap<_, _>>();
    for path in untracked {
        if files.len() == MAX_FILES {
            break;
        }
        if let Some(blob) = captured.get(path.as_bytes()) {
            // Git's normal diff sees an index-untracked path as deleted even
            // when the checkpoint captured it. Compare its actual bytes.
            files.retain(|file| file.path != path);
            let old = git(root, &["cat-file", "blob", blob]).await;
            let new = read_working_file(root, &path).await;
            let summary = match (old, new) {
                (Ok(old), Ok(new)) if old == new => continue,
                (Ok(_), Ok(_)) => "changed since snapshot",
                _ => "snapshot file, comparison unavailable",
            };
            files.push(ChangedFile {
                path,
                summary: summary.into(),
                snapshot_blob: Some((*blob).into()),
            });
            continue;
        }
        files.push(ChangedFile {
            path,
            summary: "untracked".into(),
            snapshot_blob: None,
        });
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(ReviewSnapshot {
        notice: if files.len() == MAX_FILES {
            "Showing the first 500 changed files".into()
        } else {
            format!("{} changed files", files.len())
        },
        files,
        reference,
    })
}

pub(super) async fn file_diff(root: &Path, reference: &str, file: &ChangedFile) -> Result<String> {
    if let Some(blob) = &file.snapshot_blob {
        let old = git(root, &["cat-file", "blob", blob]).await?;
        let new = read_working_file(root, &file.path).await?;
        if old.contains(&0) || new.contains(&0) {
            return Ok("Binary file changed, text preview unavailable".into());
        }
        let temporary = tempfile::tempdir()?;
        let before = temporary.path().join("before");
        let after = temporary.path().join("after");
        tokio::fs::write(&before, old).await?;
        tokio::fs::write(&after, new).await?;
        let bytes = git(
            root,
            &[
                "diff",
                "--no-index",
                "--no-ext-diff",
                "--no-textconv",
                "--no-color",
                "--unified=3",
                "--",
                before.to_str().context("temporary path is not UTF-8")?,
                after.to_str().context("temporary path is not UTF-8")?,
            ],
        )
        .await?;
        let path = file_display(&file.path);
        let mut header = true;
        return Ok(String::from_utf8_lossy(&bytes)
            .lines()
            .map(|line| {
                if line.starts_with("@@ ") {
                    header = false;
                }
                if header && line.starts_with("diff --git ") {
                    format!("diff --git a/{path} b/{path}")
                } else if header && line.starts_with("--- ") {
                    format!("--- a/{path}")
                } else if header && line.starts_with("+++ ") {
                    format!("+++ b/{path}")
                } else {
                    line.to_owned()
                }
            })
            .collect::<Vec<_>>()
            .join("\n"));
    }
    if file.summary == "untracked" {
        let bytes = read_working_file(root, &file.path).await?;
        if bytes.contains(&0) {
            return Ok("Binary file, text preview unavailable".into());
        }
        let text = String::from_utf8(bytes).context("file is not UTF-8 text")?;
        return Ok(format!(
            "--- /dev/null\n+++ {}\n{}",
            file_display(&file.path),
            text.lines()
                .map(|line| format!("+{line}"))
                .collect::<Vec<_>>()
                .join("\n")
        ));
    }
    let bytes = git(
        root,
        &[
            "--literal-pathspecs",
            "diff",
            "--relative",
            "--no-ext-diff",
            "--no-textconv",
            "--no-color",
            "--unified=3",
            reference,
            "--",
            &file.path,
        ],
    )
    .await?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

async fn read_working_file(root: &Path, path: &str) -> Result<Vec<u8>> {
    let root = root.to_path_buf();
    let path = path.to_string();
    tokio::task::spawn_blocking(move || super::attachment::read(&root, &path)).await?
}

pub(super) fn file_display(path: &str) -> String {
    let mut visible = String::with_capacity(path.len());
    for ch in path.chars() {
        if ch.is_control() {
            visible.extend(ch.escape_default());
        } else {
            visible.push(ch);
        }
    }
    visible
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command(root: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .args(["-c", "core.fsmonitor=false"])
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
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    #[tokio::test]
    async fn review_reads_staged_unstaged_untracked_and_baseline_changes_without_external_helpers()
    {
        let repo = tempfile::tempdir().unwrap();
        let root = repo.path();
        command(root, &["init", "-q"]);
        command(root, &["config", "user.name", "Test User"]);
        command(root, &["config", "user.email", "test@example.com"]);
        std::fs::write(root.join("tracked.txt"), "before\n").unwrap();
        command(root, &["add", "tracked.txt"]);
        command(root, &["commit", "-qm", "baseline"]);
        command(root, &["config", "diff.external", "touch SHOULD_NOT_RUN"]);
        command(root, &["config", "core.fsmonitor", "touch SHOULD_NOT_RUN"]);
        std::fs::write(root.join("tracked.txt"), "staged\n").unwrap();
        command(root, &["-c", "core.fsmonitor=false", "add", "tracked.txt"]);
        std::fs::write(root.join("tracked.txt"), "unstaged\n").unwrap();
        std::fs::write(root.join("new.txt"), "untracked\n").unwrap();
        let snapshot = collect(root, "HEAD").await.unwrap();
        assert_eq!(snapshot.files.len(), 2);
        let tracked = snapshot
            .files
            .iter()
            .find(|file| file.path == "tracked.txt")
            .unwrap();
        let diff = file_diff(root, &snapshot.reference, tracked).await.unwrap();
        assert!(diff.contains("-before"));
        assert!(diff.contains("+unstaged"));
        let new = snapshot
            .files
            .iter()
            .find(|file| file.path == "new.txt")
            .unwrap();
        assert!(
            file_diff(root, &snapshot.reference, new)
                .await
                .unwrap()
                .contains("+untracked")
        );
        assert!(!root.join("SHOULD_NOT_RUN").exists());
        command(
            root,
            &[
                "-c",
                "core.fsmonitor=false",
                "add",
                "new.txt",
                "tracked.txt",
            ],
        );
        let tree = command(root, &["write-tree"]);
        command(root, &["-c", "core.fsmonitor=false", "reset", "-q", "HEAD"]);
        assert!(
            collect(root, &tree).await.unwrap().files.is_empty(),
            "captured untracked files must not be reported twice or as new"
        );
        std::fs::write(
            root.join("new.txt"),
            "changed after checkpoint\n++ keep source\n",
        )
        .unwrap();
        let snapshot = collect(root, &tree).await.unwrap();
        assert_eq!(snapshot.files.len(), 1);
        let diff = file_diff(root, &tree, &snapshot.files[0]).await.unwrap();
        assert!(diff.contains("-untracked"));
        assert!(diff.contains("+++ keep source"));
        assert!(diff.contains("+++ b/new.txt"));
        assert!(!diff.contains("/before"));
    }

    #[tokio::test]
    async fn references_and_literal_pathspecs_do_not_become_options_or_globs() {
        let repo = tempfile::tempdir().unwrap();
        let root = repo.path();
        command(root, &["init", "-q"]);
        command(root, &["config", "user.name", "Test User"]);
        command(root, &["config", "user.email", "test@example.com"]);
        std::fs::write(root.join("a[1].txt"), "old\n").unwrap();
        command(root, &["add", "--all"]);
        command(root, &["commit", "-qm", "baseline"]);
        std::fs::write(root.join("a[1].txt"), "new\n").unwrap();
        let snapshot = collect(root, "HEAD").await.unwrap();
        assert!(
            file_diff(root, &snapshot.reference, &snapshot.files[0])
                .await
                .unwrap()
                .contains("+new")
        );
        assert!(resolve(root, "--help").await.is_err());
    }

    #[tokio::test]
    async fn comparisons_from_a_subdirectory_stay_inside_the_workspace() {
        let repo = tempfile::tempdir().unwrap();
        let root = repo.path();
        let workspace = root.join("app");
        std::fs::create_dir(&workspace).unwrap();
        command(root, &["init", "-q"]);
        command(root, &["config", "user.name", "Test User"]);
        command(root, &["config", "user.email", "test@example.com"]);
        std::fs::write(root.join("outside.txt"), "private before\n").unwrap();
        std::fs::write(workspace.join("code.rs"), "before\n").unwrap();
        command(root, &["add", "."]);
        command(root, &["commit", "-qm", "baseline"]);
        std::fs::write(root.join("outside.txt"), "private after\n").unwrap();
        std::fs::write(workspace.join("code.rs"), "after\n").unwrap();
        std::fs::write(workspace.join("new.rs"), "new source\n").unwrap();
        let snapshot = collect(&workspace, "HEAD").await.unwrap();
        assert_eq!(
            snapshot
                .files
                .iter()
                .map(|file| file.path.as_str())
                .collect::<Vec<_>>(),
            ["code.rs", "new.rs"]
        );
        let diff = file_diff(&workspace, &snapshot.reference, &snapshot.files[0])
            .await
            .unwrap();
        assert!(diff.contains("-before"));
        assert!(diff.contains("+after"));
        assert!(!diff.contains("private"));
        command(root, &["add", "app"]);
        let tree = command(root, &["write-tree"]);
        command(root, &["reset", "-q", "HEAD"]);
        assert!(collect(&workspace, &tree).await.unwrap().files.is_empty());
    }
}

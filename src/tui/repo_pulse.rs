use crate::tui::presentation::RepositoryPulse;
use std::path::{Path, PathBuf};
use std::process::Command;
use tokio::sync::mpsc;

pub(crate) fn spawn(workspace: PathBuf) -> mpsc::UnboundedReceiver<Option<RepositoryPulse>> {
    spawn_with(workspace, inspect)
}

fn spawn_with<F>(workspace: PathBuf, inspect: F) -> mpsc::UnboundedReceiver<Option<RepositoryPulse>>
where
    F: FnOnce(&Path) -> Option<RepositoryPulse> + Send + 'static,
{
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::task::spawn_blocking(move || {
        let _ = tx.send(inspect(&workspace));
    });
    rx
}

fn inspect(workspace: &Path) -> Option<RepositoryPulse> {
    let root = git_text(workspace, &["rev-parse", "--show-toplevel"])?;
    let root = PathBuf::from(root);
    let branch = git_text(&root, &["branch", "--show-current"])
        .filter(|branch| !branch.is_empty())
        .or_else(|| {
            git_text(&root, &["rev-parse", "--short", "HEAD"])
                .map(|commit| format!("detached@{commit}"))
        })?;
    let name = git_text(&root, &["remote", "get-url", "origin"])
        .and_then(|remote| repository_name_from_remote(&remote))
        .or_else(|| {
            root.file_name()
                .and_then(|name| name.to_str())
                .map(str::to_string)
        })?;
    let changed_paths = porcelain_path_count(&git_bytes(
        &root,
        &["status", "--porcelain=v1", "-z", "--untracked-files=normal"],
    )?);
    let last_commit =
        git_text(&root, &["log", "-1", "--pretty=%s"]).map(|subject| truncate_chars(&subject, 100));

    Some(RepositoryPulse {
        name,
        branch,
        changed_paths,
        last_commit,
    })
}

fn git_text(workspace: &Path, args: &[&str]) -> Option<String> {
    let bytes = git_bytes(workspace, args)?;
    let text = String::from_utf8_lossy(&bytes).trim().to_string();
    (!text.is_empty()).then_some(text)
}

fn git_bytes(workspace: &Path, args: &[&str]) -> Option<Vec<u8>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(workspace)
        .args(args)
        .output()
        .ok()?;
    output.status.success().then_some(output.stdout)
}

fn repository_name_from_remote(remote: &str) -> Option<String> {
    let remote = remote.trim().trim_end_matches('/').trim_end_matches(".git");
    let path = if let Some((_, rest)) = remote.split_once("://") {
        rest.split_once('/').map(|(_, path)| path)?
    } else if let Some((_, path)) = remote.split_once(':') {
        path
    } else {
        remote
    };
    let path = path.trim_matches('/');
    (!path.is_empty()).then(|| path.to_string())
}

fn porcelain_path_count(status: &[u8]) -> usize {
    let entries = status
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
        .collect::<Vec<_>>();
    let mut count = 0;
    let mut index = 0;
    while let Some(entry) = entries.get(index) {
        count += 1;
        let renamed_or_copied = entry
            .get(..2)
            .is_some_and(|status| status.iter().any(|byte| matches!(byte, b'R' | b'C')));
        index += if renamed_or_copied { 2 } else { 1 };
    }
    count
}

fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut truncated = text.chars().take(max.saturating_sub(1)).collect::<String>();
    truncated.push('…');
    truncated
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repository_name_handles_https_ssh_and_local_remotes() {
        assert_eq!(
            repository_name_from_remote("https://github.com/everruns/yolop.git").as_deref(),
            Some("everruns/yolop")
        );
        assert_eq!(
            repository_name_from_remote("git@github.com:everruns/yolop.git").as_deref(),
            Some("everruns/yolop")
        );
        assert_eq!(
            repository_name_from_remote("/srv/git/yolop.git").as_deref(),
            Some("srv/git/yolop")
        );
    }

    #[test]
    fn porcelain_count_treats_rename_source_as_part_of_one_change() {
        assert_eq!(
            porcelain_path_count(b" M src/main.rs\0R  new.rs\0old.rs\0?? notes.txt\0"),
            3
        );
    }

    #[test]
    fn inspect_reads_repository_pulse() {
        let repo = tempfile::tempdir().expect("temp repo");
        git(repo.path(), &["init", "-q"]);
        git(repo.path(), &["config", "user.name", "Test User"]);
        git(repo.path(), &["config", "user.email", "test@example.com"]);
        std::fs::write(repo.path().join("README.md"), "hello\n").expect("write fixture");
        git(repo.path(), &["add", "README.md"]);
        git(repo.path(), &["commit", "-qm", "initial pulse"]);
        git(
            repo.path(),
            &[
                "remote",
                "add",
                "origin",
                "git@github.com:everruns/yolop.git",
            ],
        );
        std::fs::write(repo.path().join("README.md"), "changed\n").expect("change fixture");

        let pulse = inspect(repo.path()).expect("repository pulse");

        assert_eq!(pulse.name, "everruns/yolop");
        assert_eq!(pulse.changed_paths, 1);
        assert_eq!(pulse.last_commit.as_deref(), Some("initial pulse"));
        assert!(!pulse.branch.is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn spawn_returns_while_repository_inspection_is_still_blocked() {
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let mut result_rx = spawn_with(PathBuf::from("/unused"), move |_| {
            release_rx.recv().expect("release inspector");
            None
        });

        assert!(matches!(
            result_rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        release_tx.send(()).expect("release worker");
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(2), result_rx.recv())
                .await
                .expect("worker completion")
                .is_some()
        );
    }

    fn git(root: &Path, args: &[&str]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .expect("run git fixture command");
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

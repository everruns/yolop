// Git worktree management for yolop sessions.
//
// Each session can get an isolated worktree on a dedicated branch, branched
// from `origin/main`, so the user's primary checkout stays untouched. Worktrees
// live outside the repo (tmp dir, falling back to `~/.yolop/worktrees/`).

use crate::session_log::{SessionWorkspaceMetadata, WorktreeMetadata, write_session_workspace};
use crate::settings::WorktreesMode;
use anyhow::{Context, Result, bail};
use everruns_core::typed_id::SessionId;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};

const IMPLEMENT_VERBS: &[&str] = &[
    "fix",
    "implement",
    "add",
    "create",
    "build",
    "write",
    "update",
    "change",
    "refactor",
    "ship",
    "debug",
    "patch",
    "remove",
    "delete",
    "migrate",
    "replace",
    "introduce",
    "integrate",
    "wire",
    "hook",
    "land",
    "port",
    "rewrite",
    "repair",
    "correct",
    "extend",
    "modify",
];

const STOP_WORDS: &[&str] = &[
    "a", "an", "the", "to", "for", "in", "on", "at", "of", "and", "or", "with", "from", "this",
    "that", "please", "can", "you", "me", "my", "i", "we", "it", "is", "are", "be", "do", "does",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeInfo {
    pub path: PathBuf,
    pub branch: String,
    pub base_ref: String,
    pub slug: String,
}

/// Session-scoped worktree coordinator. Thread-safe for concurrent reads; writes
/// are serialized through the inner `RwLock`s.
pub struct WorktreeManager {
    mode: WorktreesMode,
    repo_root: Option<PathBuf>,
    active_root: Arc<RwLock<PathBuf>>,
    worktree: RwLock<Option<WorktreeInfo>>,
    session_id: SessionId,
    session_dir: PathBuf,
    auto_disabled: AtomicBool,
}

impl WorktreeManager {
    pub fn new(
        mode: WorktreesMode,
        repo_root: Option<PathBuf>,
        initial_active_root: PathBuf,
        session_id: SessionId,
        session_dir: PathBuf,
        restored: Option<WorktreeInfo>,
    ) -> Self {
        let active = restored
            .as_ref()
            .map(|w| w.path.clone())
            .unwrap_or(initial_active_root);
        Self {
            mode,
            repo_root,
            active_root: Arc::new(RwLock::new(active)),
            worktree: RwLock::new(restored),
            session_id,
            session_dir,
            auto_disabled: AtomicBool::new(false),
        }
    }

    pub fn shared_active_root(&self) -> Arc<RwLock<PathBuf>> {
        self.active_root.clone()
    }

    pub fn active_root(&self) -> PathBuf {
        self.active_root
            .read()
            .map(|p| p.clone())
            .unwrap_or_else(|_| PathBuf::from("."))
    }

    pub fn worktree_info(&self) -> Option<WorktreeInfo> {
        self.worktree.read().ok().and_then(|g| g.clone())
    }

    /// Returns `true` when the active root changed (worktree was created or restored).
    pub fn ensure_before_turn(&self, prompt: &str) -> Result<bool> {
        if self.worktree.read().map(|g| g.is_some()).unwrap_or(false) {
            return Ok(false);
        }
        if !self.should_activate(prompt) {
            return Ok(false);
        }
        self.activate(Some(prompt))?;
        Ok(true)
    }

    pub fn ensure_always(&self) -> Result<bool> {
        if self.worktree.read().map(|g| g.is_some()).unwrap_or(false) {
            return Ok(false);
        }
        if !self.can_use_worktrees() {
            return Ok(false);
        }
        self.activate(None)?;
        Ok(true)
    }

    fn should_activate(&self, prompt: &str) -> bool {
        if self.auto_disabled.load(Ordering::SeqCst) {
            return false;
        }
        match self.mode {
            WorktreesMode::Off => false,
            WorktreesMode::Always => self.can_use_worktrees(),
            WorktreesMode::Auto => {
                self.can_use_worktrees() && looks_like_implementation_intent(prompt)
            }
        }
    }

    fn can_use_worktrees(&self) -> bool {
        self.repo_root.is_some() && self.mode != WorktreesMode::Off
    }

    fn activate(&self, prompt: Option<&str>) -> Result<()> {
        let repo_root = self
            .repo_root
            .as_ref()
            .context("worktree activation requires a git repository")?;
        let slug = prompt
            .map(slug_from_prompt)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "session".to_string());
        let branch = branch_name(&slug, self.session_id);
        let base_ref = resolve_base_ref(repo_root);
        let path = worktree_path(repo_root, &self.session_id.to_string())?;

        let info = if path.exists() && worktree_contains_path(repo_root, &path) {
            WorktreeInfo {
                path: path.clone(),
                branch: branch.clone(),
                base_ref: base_ref.clone(),
                slug: slug.clone(),
            }
        } else {
            create_worktree(repo_root, &path, &branch, &base_ref)?;
            WorktreeInfo {
                path: path.clone(),
                branch,
                base_ref,
                slug,
            }
        };

        {
            let mut active = self
                .active_root
                .write()
                .map_err(|_| anyhow::anyhow!("active workspace lock poisoned"))?;
            *active = info.path.clone();
        }
        {
            let mut guard = self
                .worktree
                .write()
                .map_err(|_| anyhow::anyhow!("worktree lock poisoned"))?;
            *guard = Some(info.clone());
        }
        self.persist_metadata(&info)?;
        Ok(())
    }

    pub fn restore_from_metadata(&self, metadata: &SessionWorkspaceMetadata) -> Result<()> {
        let Some(worktree) = metadata.worktree.clone() else {
            return Ok(());
        };
        let repo_root = metadata
            .repo_root
            .as_ref()
            .or(self.repo_root.as_ref())
            .context("resume worktree metadata missing repo_root")?;

        let info = if worktree.path.exists() && worktree_contains_path(repo_root, &worktree.path) {
            WorktreeInfo {
                path: worktree.path.clone(),
                branch: worktree.branch.clone(),
                base_ref: worktree.base_ref.clone(),
                slug: worktree.slug.clone(),
            }
        } else {
            recreate_worktree(repo_root, &worktree)?
        };

        {
            let mut active = self
                .active_root
                .write()
                .map_err(|_| anyhow::anyhow!("active workspace lock poisoned"))?;
            *active = info.path.clone();
        }
        {
            let mut guard = self
                .worktree
                .write()
                .map_err(|_| anyhow::anyhow!("worktree lock poisoned"))?;
            *guard = Some(info.clone());
        }
        self.persist_metadata(&info)?;
        Ok(())
    }

    fn persist_metadata(&self, info: &WorktreeInfo) -> Result<()> {
        let metadata = SessionWorkspaceMetadata {
            active_root: info.path.clone(),
            repo_root: self.repo_root.clone(),
            worktree: Some(WorktreeMetadata {
                path: info.path.clone(),
                branch: info.branch.clone(),
                base_ref: info.base_ref.clone(),
                slug: info.slug.clone(),
            }),
        };
        write_session_workspace(&self.session_dir, &metadata).map_err(|e| anyhow::anyhow!("{e}"))
    }

    pub fn status_line(&self) -> Option<String> {
        let info = self.worktree_info()?;
        Some(format!(
            "worktree: {} @ {}",
            info.branch,
            info.path.display()
        ))
    }
}

pub fn detect_repo_root(path: &Path) -> Option<PathBuf> {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if text.is_empty() {
        None
    } else {
        std::fs::canonicalize(&text).ok()
    }
}

pub fn looks_like_implementation_intent(prompt: &str) -> bool {
    let trimmed = prompt.trim();
    if trimmed.is_empty() {
        return false;
    }
    if trimmed.starts_with("/ship") {
        return true;
    }
    let lower = format!(" {lower} ", lower = trimmed.to_ascii_lowercase());
    IMPLEMENT_VERBS
        .iter()
        .any(|verb| lower.contains(&format!(" {verb} ")))
}

pub fn slug_from_prompt(prompt: &str) -> String {
    let words: Vec<String> = prompt
        .to_ascii_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
        .filter(|w| STOP_WORDS.iter().all(|stop| stop != w))
        .take(5)
        .map(str::to_string)
        .collect();
    if words.is_empty() {
        "session".to_string()
    } else {
        words.join("-")
    }
}

pub fn branch_name(slug: &str, session_id: SessionId) -> String {
    let suffix = session_suffix(session_id);
    let base = slug.trim_matches('-');
    if base.is_empty() {
        format!("session-{suffix}")
    } else {
        format!("{base}-{suffix}")
    }
}

fn session_suffix(session_id: SessionId) -> String {
    let compact: String = session_id
        .to_string()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect();
    let suffix: String = compact.chars().rev().take(8).collect::<String>();
    let suffix: String = suffix.chars().rev().collect();
    if suffix.is_empty() {
        "00000000".to_string()
    } else {
        suffix
    }
}

pub fn repo_id(repo_root: &Path) -> String {
    let mut hasher = DefaultHasher::new();
    repo_root.display().to_string().hash(&mut hasher);
    format!("{:08x}", hasher.finish())
}

pub fn worktree_parent_dir(repo_id: &str) -> PathBuf {
    let tmp_base = std::env::var_os("TMPDIR")
        .map(PathBuf::from)
        .or_else(|| Some(PathBuf::from("/tmp")))
        .unwrap();
    let tmp_candidate = tmp_base.join("yolop").join("worktrees").join(repo_id);
    if std::fs::create_dir_all(&tmp_candidate).is_ok() {
        return tmp_candidate;
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".yolop")
        .join("worktrees")
        .join(repo_id)
}

pub fn worktree_path(repo_root: &Path, session_id: &str) -> Result<PathBuf> {
    let parent = worktree_parent_dir(&repo_id(repo_root));
    std::fs::create_dir_all(&parent)
        .with_context(|| format!("create worktree parent {}", parent.display()))?;
    Ok(parent.join(session_id))
}

fn resolve_base_ref(repo_root: &Path) -> String {
    for candidate in ["origin/main", "origin/master", "main", "master"] {
        if git_verify_ref(repo_root, candidate) {
            return candidate.to_string();
        }
    }
    "HEAD".to_string()
}

fn git_verify_ref(repo_root: &Path, reference: &str) -> bool {
    git_output(repo_root, &["rev-parse", "--verify", reference]).is_some()
}

fn create_worktree(repo_root: &Path, path: &Path, branch: &str, base_ref: &str) -> Result<()> {
    if path.exists() {
        std::fs::remove_dir_all(path).with_context(|| {
            format!(
                "remove stale worktree path {} before recreate",
                path.display()
            )
        })?;
    }
    // Best-effort fetch so origin/main is current when used as the base.
    if base_ref.starts_with("origin/") {
        let remote_branch = base_ref.strip_prefix("origin/").unwrap_or(base_ref);
        let _ = Command::new("git")
            .arg("-C")
            .arg(repo_root)
            .args(["fetch", "origin", remote_branch])
            .status();
    }
    let status = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args([
            "worktree",
            "add",
            "-B",
            branch,
            &path.display().to_string(),
            base_ref,
        ])
        .status()
        .with_context(|| format!("git worktree add for {}", path.display()))?;
    if status.success() {
        return Ok(());
    }
    bail!(
        "git worktree add -B {branch} {} {base_ref} failed",
        path.display()
    );
}

fn recreate_worktree(repo_root: &Path, saved: &WorktreeMetadata) -> Result<WorktreeInfo> {
    let path = saved.path.clone();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create worktree parent {}", parent.display()))?;
    }
    if path.exists() {
        let _ = std::fs::remove_dir_all(&path);
    }
    if branch_exists(repo_root, &saved.branch) {
        let status = Command::new("git")
            .arg("-C")
            .arg(repo_root)
            .args([
                "worktree",
                "add",
                &path.display().to_string(),
                &saved.branch,
            ])
            .status()
            .with_context(|| format!("reattach worktree branch {}", saved.branch))?;
        if !status.success() {
            bail!("git worktree add {} {}", path.display(), saved.branch);
        }
    } else {
        create_worktree(repo_root, &path, &saved.branch, &saved.base_ref)?;
    }
    Ok(WorktreeInfo {
        path,
        branch: saved.branch.clone(),
        base_ref: saved.base_ref.clone(),
        slug: saved.slug.clone(),
    })
}

fn branch_exists(repo_root: &Path, branch: &str) -> bool {
    git_output(
        repo_root,
        &["show-ref", "--verify", &format!("refs/heads/{branch}")],
    )
    .is_some()
}

fn worktree_contains_path(repo_root: &Path, path: &Path) -> bool {
    let canonical = match std::fs::canonicalize(path) {
        Ok(p) => p,
        Err(_) => return false,
    };
    let listing = match Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["worktree", "list", "--porcelain"])
        .output()
    {
        Ok(output) if output.status.success() => {
            String::from_utf8_lossy(&output.stdout).into_owned()
        }
        _ => return false,
    };
    listing.lines().any(|line| {
        line.strip_prefix("worktree ")
            .and_then(|p| std::fs::canonicalize(p).ok())
            .is_some_and(|listed| listed == canonical)
    })
}

fn git_output(repo_root: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if text.is_empty() { None } else { Some(text) }
}

pub fn restore_worktree_from_metadata(metadata: &SessionWorkspaceMetadata) -> Option<WorktreeInfo> {
    let worktree = metadata.worktree.as_ref()?;
    let repo_root = metadata.repo_root.as_ref()?;
    if worktree.path.exists() && worktree_contains_path(repo_root, &worktree.path) {
        return Some(WorktreeInfo {
            path: worktree.path.clone(),
            branch: worktree.branch.clone(),
            base_ref: worktree.base_ref.clone(),
            slug: worktree.slug.clone(),
        });
    }
    recreate_worktree(repo_root, worktree).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_from_prompt_strips_stop_words() {
        assert_eq!(
            slug_from_prompt("Please fix the auth bug in login"),
            "fix-auth-bug-login"
        );
    }

    #[test]
    fn implementation_intent_matches_common_verbs() {
        assert!(looks_like_implementation_intent("fix the auth bug"));
        assert!(looks_like_implementation_intent("/ship this change"));
        assert!(!looks_like_implementation_intent("how does auth work?"));
        assert!(!looks_like_implementation_intent("explain the module"));
    }

    #[test]
    fn branch_name_uses_slug_and_session_suffix() {
        let id = SessionId::default();
        let branch = branch_name("fix-auth", id);
        assert!(branch.starts_with("fix-auth-"));
        assert!(branch.len() > "fix-auth-".len());
    }

    #[test]
    fn worktree_manager_creates_isolated_branch_on_implementation_prompt() {
        let repo = tempfile::tempdir().expect("repo");
        std::process::Command::new("git")
            .args(["init", "-b", "main"])
            .current_dir(repo.path())
            .status()
            .expect("git init");
        std::fs::write(repo.path().join("README.md"), "hello").expect("readme");
        std::process::Command::new("git")
            .args(["add", "README.md"])
            .current_dir(repo.path())
            .status()
            .expect("git add");
        std::process::Command::new("git")
            .args([
                "commit",
                "-m",
                "init",
                "--author",
                "test <test@example.com>",
            ])
            .env("GIT_COMMITTER_NAME", "test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .current_dir(repo.path())
            .status()
            .expect("git commit");

        let sessions = tempfile::tempdir().expect("sessions");
        let session_id = SessionId::from_seed(99);
        let session_dir = sessions.path().join(session_id.to_string());
        std::fs::create_dir_all(&session_dir).expect("session dir");

        let manager = WorktreeManager::new(
            WorktreesMode::Auto,
            detect_repo_root(repo.path()),
            repo.path().to_path_buf(),
            session_id,
            session_dir,
            None,
        );

        manager
            .ensure_before_turn("fix the readme typo")
            .expect("activate worktree");
        let info = manager.worktree_info().expect("worktree info");
        assert!(info.path.exists());
        assert!(info.branch.starts_with("fix-readme"));
        assert_ne!(info.path, repo.path());

        let main_branch =
            git_output(repo.path(), &["branch", "--show-current"]).expect("main branch");
        assert_eq!(main_branch, "main");
    }
}

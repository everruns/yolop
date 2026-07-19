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

/// Patterns naming git-ignored paths to copy into freshly-created worktrees,
/// `.gitignore`-style. Lives at the repo root.
const WORKTREE_INCLUDE_FILE: &str = ".worktreeinclude";

/// Copied into managed worktrees automatically, without needing a
/// `.worktreeinclude` entry (mirrors Codex). Only copied when git-ignored.
const IMPLICIT_INCLUDE: &str = "AGENTS.override.md";

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

    /// Return the isolated worktree only when it is distinct from the main
    /// checkout. Checkpoint restore uses this stricter ownership boundary so
    /// corrupt or hand-edited metadata cannot turn the primary checkout into a
    /// whole-workspace restore target.
    pub fn owned_worktree_info(&self) -> Option<WorktreeInfo> {
        let info = self.worktree_info()?;
        let repo_root = self.repo_root.as_ref()?;
        let worktree_path = std::fs::canonicalize(&info.path).unwrap_or_else(|_| info.path.clone());
        let main_path = std::fs::canonicalize(repo_root).unwrap_or_else(|_| repo_root.clone());
        (worktree_path != main_path).then_some(info)
    }

    pub fn disable_auto(&self) {
        self.auto_disabled.store(true, Ordering::SeqCst);
    }

    pub fn auto_disabled(&self) -> bool {
        self.auto_disabled.load(Ordering::SeqCst)
    }

    pub fn status_message(&self) -> String {
        if let Some(info) = self.worktree_info() {
            let repo = self
                .repo_root
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "unknown".to_string());
            return format!(
                "active worktree\nrepo: {repo}\nbranch: {}\npath: {}\nbase: {}",
                info.branch,
                info.path.display(),
                info.base_ref,
            );
        }
        if self.repo_root.is_none() {
            return "worktrees: not in a git repository".to_string();
        }
        if self.auto_disabled() {
            return "worktrees: auto activation disabled for this session (already-active \
                    worktrees are unchanged)"
                .to_string();
        }
        format!(
            "worktrees: {} (inactive — activates on implementation prompts)",
            self.mode.as_str()
        )
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
        let mut metadata = SessionWorkspaceMetadata::new(info.path.clone(), self.repo_root.clone());
        metadata.worktree = Some(WorktreeMetadata {
            path: info.path.clone(),
            branch: info.branch.clone(),
            base_ref: info.base_ref.clone(),
            slug: info.slug.clone(),
        });
        write_session_workspace(&self.session_dir, &metadata).map_err(|e| anyhow::anyhow!("{e}"))
    }

    /// Short slug for the compact session status bar.
    pub fn status_bar_compact(&self) -> Option<String> {
        self.worktree_info().map(|info| info.slug)
    }

    /// Branch and filesystem path for the expanded status bar subsection.
    pub fn status_bar_expanded(&self) -> Option<(String, String)> {
        self.worktree_info().map(|info| {
            (
                info.branch,
                truncate_status_path(&info.path.display().to_string(), 72),
            )
        })
    }

    /// Light transcript notice when a worktree is activated mid-session.
    pub fn switch_notice(&self) -> Option<String> {
        self.worktree_info()
            .map(|info| format!("Switched to worktree · {}", info.branch))
    }
}

fn truncate_status_path(path: &str, max_len: usize) -> String {
    if path.chars().count() <= max_len {
        return path.to_string();
    }
    let tail_len = max_len.saturating_sub(1);
    let tail: String = path.chars().rev().take(tail_len).collect::<String>();
    format!("…{}", tail.chars().rev().collect::<String>())
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
        copy_worktree_includes(repo_root, path);
        return Ok(());
    }
    bail!(
        "git worktree add -B {branch} {} {base_ref} failed",
        path.display()
    );
}

/// Copy git-ignored files matching `.worktreeinclude` (plus an ignored
/// `AGENTS.override.md`) from the source checkout into a fresh worktree.
///
/// `git worktree add` only materializes tracked files, so intentionally-ignored
/// setup files (`.env`, local secrets, …) are otherwise missing. Best-effort:
/// failures are logged, never fatal. Skips symlinks and never overwrites a file
/// already present in the new checkout.
fn copy_worktree_includes(repo_root: &Path, worktree_path: &Path) {
    use ignore::gitignore::GitignoreBuilder;

    let mut builder = GitignoreBuilder::new(repo_root);
    let include_file = repo_root.join(WORKTREE_INCLUDE_FILE);
    if include_file.is_file()
        && let Some(err) = builder.add(&include_file)
    {
        tracing::warn!(error = %err, file = %include_file.display(), "ignoring malformed .worktreeinclude");
    }
    // Implicit, un-listed include — only copied if it is actually git-ignored.
    let _ = builder.add_line(None, IMPLICIT_INCLUDE);
    let matcher = match builder.build() {
        Ok(m) => m,
        Err(err) => {
            tracing::warn!(error = %err, "failed to build .worktreeinclude matcher");
            return;
        }
    };

    // Enumerate git-ignored files in the source checkout (relative paths).
    let output = match Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args([
            "ls-files",
            "-z",
            "--others",
            "--ignored",
            "--exclude-standard",
        ])
        .output()
    {
        Ok(out) if out.status.success() => out.stdout,
        Ok(out) => {
            tracing::warn!(
                status = ?out.status.code(),
                stderr = %String::from_utf8_lossy(&out.stderr).trim(),
                "git ls-files failed; skipping .worktreeinclude copy"
            );
            return;
        }
        Err(err) => {
            tracing::warn!(error = %err, "could not run git ls-files; skipping .worktreeinclude copy");
            return;
        }
    };

    // Resolve once so we can confirm copied sources stay inside the repo even if
    // a parent component is a symlink (git won't list through symlinked dirs,
    // but guard defensively against symlink escape).
    let repo_canon = std::fs::canonicalize(repo_root).unwrap_or_else(|_| repo_root.to_path_buf());

    let mut copied = 0usize;
    for rel in output.split(|&b| b == 0) {
        if rel.is_empty() {
            continue;
        }
        let rel = Path::new(std::str::from_utf8(rel).unwrap_or_default());
        if rel.as_os_str().is_empty() {
            continue;
        }
        if !matcher.matched_path_or_any_parents(rel, false).is_ignore() {
            continue;
        }
        let src = repo_root.join(rel);
        // Skip symlinks; copy regular files only.
        match std::fs::symlink_metadata(&src) {
            Ok(meta) if meta.file_type().is_symlink() => continue,
            Ok(meta) if !meta.is_file() => continue,
            Ok(_) => {}
            Err(_) => continue,
        }
        // Reject any source that resolves outside the repo (symlinked parent
        // escape). `std::fs::copy` follows symlinks, so this prevents reading
        // and re-materializing files from outside the checkout.
        match std::fs::canonicalize(&src) {
            Ok(canon) if canon.starts_with(&repo_canon) => {}
            _ => {
                tracing::warn!(src = %src.display(), "skipping worktree include resolving outside repo");
                continue;
            }
        }
        let dest = worktree_path.join(rel);
        if dest.exists() {
            continue;
        }
        if let Some(parent) = dest.parent()
            && let Err(err) = std::fs::create_dir_all(parent)
        {
            tracing::warn!(error = %err, dest = %dest.display(), "failed to create worktree include dir");
            continue;
        }
        match std::fs::copy(&src, &dest) {
            Ok(_) => copied += 1,
            Err(err) => {
                tracing::warn!(error = %err, src = %src.display(), "failed to copy worktree include");
            }
        }
    }
    if copied > 0 {
        tracing::info!(count = copied, worktree = %worktree_path.display(), "copied ignored files into worktree");
    }
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
        copy_worktree_includes(repo_root, &path);
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PruneReport {
    pub removed: Vec<PathBuf>,
    pub checkpoint_refs: Vec<String>,
    pub kept: usize,
    pub errors: Vec<String>,
}

/// Roots that may contain session worktrees (tmp + persistent fallback).
pub fn worktree_storage_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    let tmp_base = std::env::var_os("TMPDIR")
        .map(PathBuf::from)
        .or_else(|| Some(PathBuf::from("/tmp")))
        .unwrap();
    roots.push(tmp_base.join("yolop").join("worktrees"));
    roots.push(
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".yolop")
            .join("worktrees"),
    );
    roots
}

pub fn referenced_worktree_paths(
    sessions_dir: &Path,
) -> Result<std::collections::HashSet<PathBuf>> {
    use crate::session_log::{read_session_workspace_metadata, session_dir_path};
    use everruns_core::typed_id::SessionId;

    let mut referenced = std::collections::HashSet::new();
    if !sessions_dir.is_dir() {
        return Ok(referenced);
    }
    for entry in std::fs::read_dir(sessions_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Ok(session_id) = name.parse::<SessionId>() else {
            continue;
        };
        let session_dir = session_dir_path(sessions_dir, session_id);
        if let Some(metadata) = read_session_workspace_metadata(&session_dir)?
            && let Some(worktree) = metadata.worktree
        {
            if let Ok(path) = std::fs::canonicalize(&worktree.path) {
                referenced.insert(path);
            } else {
                referenced.insert(worktree.path);
            }
        }
    }
    Ok(referenced)
}

pub fn list_worktree_paths_on_disk() -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for root in worktree_storage_roots() {
        if !root.is_dir() {
            continue;
        }
        for repo_entry in std::fs::read_dir(&root)? {
            let repo_entry = repo_entry?;
            if !repo_entry.file_type()?.is_dir() {
                continue;
            }
            for session_entry in std::fs::read_dir(repo_entry.path())? {
                let session_entry = session_entry?;
                if session_entry.file_type()?.is_dir() {
                    paths.push(session_entry.path());
                }
            }
        }
    }
    paths.sort();
    Ok(paths)
}

fn main_repo_for_worktree(worktree_path: &Path) -> Option<PathBuf> {
    let git_common = git_output(worktree_path, &["rev-parse", "--git-common-dir"])?;
    let git_path = PathBuf::from(git_common);
    if git_path.is_absolute() {
        git_path.parent().map(|p| p.to_path_buf())
    } else {
        worktree_path
            .join(&git_path)
            .parent()
            .map(|p| p.to_path_buf())
    }
}

pub fn remove_worktree_path(path: &Path) -> Result<()> {
    if let Some(repo_root) = main_repo_for_worktree(path) {
        let status = Command::new("git")
            .arg("-C")
            .arg(&repo_root)
            .args(["worktree", "remove", "--force", &path.display().to_string()])
            .status()
            .with_context(|| format!("git worktree remove {}", path.display()))?;
        if status.success() {
            return Ok(());
        }
    }
    if path.exists() {
        std::fs::remove_dir_all(path)
            .with_context(|| format!("remove worktree directory {}", path.display()))?;
    }
    Ok(())
}

fn checkpoint_refs_for_worktree(path: &Path) -> Result<Vec<(PathBuf, String)>> {
    let Some(repo_root) = main_repo_for_worktree(path) else {
        return Ok(Vec::new());
    };
    let Some(session_id) = path.file_name().and_then(|name| name.to_str()) else {
        return Ok(Vec::new());
    };
    let prefix = format!("refs/yolop/checkpoints/{session_id}/");
    let output = Command::new("git")
        .arg("-C")
        .arg(&repo_root)
        .args(["for-each-ref", "--format=%(refname)", &prefix])
        .output()
        .context("list checkpoint refs")?;
    if !output.status.success() {
        bail!(
            "list checkpoint refs: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8(output.stdout)?
        .lines()
        .filter(|reference| !reference.is_empty())
        .map(|reference| (repo_root.clone(), reference.to_string()))
        .collect())
}

fn delete_checkpoint_ref(repo_root: &Path, reference: &str) -> Result<()> {
    let status = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["update-ref", "-d", reference])
        .status()
        .with_context(|| format!("delete checkpoint ref {reference}"))?;
    if !status.success() {
        bail!("delete checkpoint ref {reference}");
    }
    Ok(())
}

pub fn prune_orphan_worktrees(sessions_dir: &Path, dry_run: bool) -> Result<PruneReport> {
    let referenced = referenced_worktree_paths(sessions_dir)?;
    let mut report = PruneReport {
        removed: Vec::new(),
        checkpoint_refs: Vec::new(),
        kept: 0,
        errors: Vec::new(),
    };
    for path in list_worktree_paths_on_disk()? {
        let canonical = std::fs::canonicalize(&path).unwrap_or(path.clone());
        if referenced.contains(&canonical) || referenced.contains(&path) {
            report.kept += 1;
            continue;
        }
        if dry_run {
            match checkpoint_refs_for_worktree(&path) {
                Ok(refs) => report
                    .checkpoint_refs
                    .extend(refs.into_iter().map(|(_, reference)| reference)),
                Err(err) => report.errors.push(format!("{}: {err}", path.display())),
            }
            report.removed.push(path);
            continue;
        }
        let refs = match checkpoint_refs_for_worktree(&path) {
            Ok(refs) => refs,
            Err(err) => {
                report.errors.push(format!("{}: {err}", path.display()));
                continue;
            }
        };
        let mut ref_error = false;
        for (repo_root, reference) in refs {
            match delete_checkpoint_ref(&repo_root, &reference) {
                Ok(()) => report.checkpoint_refs.push(reference),
                Err(err) => {
                    report.errors.push(err.to_string());
                    ref_error = true;
                }
            }
        }
        if ref_error {
            continue;
        }
        match remove_worktree_path(&path) {
            Ok(()) => report.removed.push(path),
            Err(err) => report.errors.push(format!("{}: {err}", path.display())),
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn slug_from_prompt_strips_stop_words() {
        assert_eq!(
            slug_from_prompt("Please fix the auth bug in login"),
            "fix-auth-bug-login"
        );
    }

    #[test]
    fn primary_checkout_is_not_treated_as_an_owned_worktree() {
        let root = tempfile::tempdir().expect("root");
        let session = tempfile::tempdir().expect("session");
        let info = WorktreeInfo {
            path: root.path().to_path_buf(),
            branch: "main".to_string(),
            base_ref: "HEAD".to_string(),
            slug: "main".to_string(),
        };
        let manager = WorktreeManager::new(
            WorktreesMode::Always,
            Some(root.path().to_path_buf()),
            root.path().to_path_buf(),
            everruns_core::typed_id::SessionId::new(),
            session.path().to_path_buf(),
            Some(info),
        );
        assert!(manager.owned_worktree_info().is_none());
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
    fn truncate_status_path_keeps_tail() {
        let path = "/var/folders/rs/tcmw11q17s961ch7pb76czc0000gn/T/yolop/worktrees/b85662b381593c9f/session_019ee6c2";
        let truncated = truncate_status_path(path, 40);
        assert!(truncated.starts_with('…'));
        assert!(truncated.contains("session_019ee6c2"));
        assert!(truncated.chars().count() <= 40);
    }

    #[test]
    fn worktree_manager_creates_isolated_branch_on_implementation_prompt() {
        let _guard = crate::test_env::lock();
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
                "-c",
                "commit.gpgsign=false",
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

    #[test]
    fn copies_ignored_includes_into_worktree() {
        let repo = tempfile::tempdir().expect("repo");
        let run = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .arg("-c")
                .arg("commit.gpgsign=false")
                .args(args)
                .current_dir(repo.path())
                .env("GIT_COMMITTER_NAME", "test")
                .env("GIT_COMMITTER_EMAIL", "test@example.com")
                .status()
                .expect("git");
            assert!(status.success(), "git {args:?} failed");
        };
        run(&["init", "-b", "main"]);
        std::fs::write(
            repo.path().join(".gitignore"),
            ".env\nsecrets/\nAGENTS.override.md\n",
        )
        .expect("gitignore");
        std::fs::write(
            repo.path().join(".worktreeinclude"),
            ".env\nsecrets/secret.txt\n",
        )
        .expect("worktreeinclude");
        // Ignored files that should be copied.
        std::fs::write(repo.path().join(".env"), "TOKEN=abc").expect("env");
        std::fs::create_dir_all(repo.path().join("secrets")).expect("secrets dir");
        std::fs::write(repo.path().join("secrets/secret.txt"), "shh").expect("secret");
        // Ignored AGENTS.override.md is copied implicitly without being listed.
        std::fs::write(repo.path().join("AGENTS.override.md"), "override").expect("override");
        // Ignored but not matched by .worktreeinclude — must NOT be copied.
        std::fs::write(repo.path().join("secrets/other.txt"), "nope").expect("other");
        std::fs::write(repo.path().join("README.md"), "hello").expect("readme");
        run(&["add", "README.md", ".gitignore"]);
        run(&[
            "commit",
            "-m",
            "init",
            "--author",
            "test <test@example.com>",
        ]);

        // Unique, isolated worktree path so parallel tests can't collide.
        let wt_parent = tempfile::tempdir().expect("wt parent");
        let worktree = wt_parent.path().join("wt");
        create_worktree(repo.path(), &worktree, "copy-test", "HEAD").expect("create worktree");

        assert_eq!(
            std::fs::read_to_string(worktree.join(".env")).unwrap(),
            "TOKEN=abc"
        );
        assert_eq!(
            std::fs::read_to_string(worktree.join("secrets/secret.txt")).unwrap(),
            "shh"
        );
        assert_eq!(
            std::fs::read_to_string(worktree.join("AGENTS.override.md")).unwrap(),
            "override"
        );
        assert!(!worktree.join("secrets/other.txt").exists());

        let _ = std::process::Command::new("git")
            .arg("-C")
            .arg(repo.path())
            .args([
                "worktree",
                "remove",
                "--force",
                &worktree.display().to_string(),
            ])
            .status();
    }

    #[test]
    fn disable_auto_and_status_message() {
        let manager = WorktreeManager::new(
            WorktreesMode::Auto,
            Some(PathBuf::from("/tmp/repo")),
            PathBuf::from("/tmp/repo"),
            SessionId::from_seed(1),
            PathBuf::from("/tmp/session"),
            None,
        );
        assert!(manager.status_message().contains("auto"));
        manager.disable_auto();
        assert!(manager.auto_disabled());
        assert!(manager.status_message().contains("disabled"));
    }

    #[test]
    fn prune_removes_unreferenced_worktree_dirs() {
        let _guard = crate::test_env::lock();
        let sessions = tempfile::tempdir().expect("sessions");
        let storage = tempfile::tempdir().expect("storage");
        let storage_path = storage.path().to_path_buf();
        let previous_tmpdir = std::env::var_os("TMPDIR");
        // SAFETY: test-only TMPDIR override; restored before this test returns.
        unsafe {
            std::env::set_var("TMPDIR", &storage_path);
        }

        let orphan = storage_path
            .join("yolop")
            .join("worktrees")
            .join("abc12345")
            .join("orphan-session");
        let repo = tempfile::tempdir().expect("repo");
        let run = |args: &[&str]| {
            let output = Command::new("git")
                .arg("-C")
                .arg(repo.path())
                .args(args)
                .env("GIT_COMMITTER_NAME", "test")
                .env("GIT_COMMITTER_EMAIL", "test@example.com")
                .output()
                .expect("git");
            assert!(
                output.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        run(&["init", "-q"]);
        std::fs::write(repo.path().join("tracked.txt"), "base").expect("tracked");
        run(&["add", "tracked.txt"]);
        run(&[
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-qm",
            "base",
            "--author",
            "test <test@example.com>",
        ]);
        std::fs::create_dir_all(orphan.parent().expect("orphan parent")).expect("worktree parent");
        run(&[
            "worktree",
            "add",
            "-q",
            "-b",
            "orphan-session",
            orphan.to_str().expect("utf8 path"),
            "HEAD",
        ]);
        let checkpoint_ref = "refs/yolop/checkpoints/orphan-session/cp_1";
        run(&["update-ref", checkpoint_ref, "HEAD"]);

        let report = prune_orphan_worktrees(sessions.path(), false).expect("prune");
        assert_eq!(report.kept, 0);
        assert_eq!(report.removed.len(), 1);
        assert_eq!(report.checkpoint_refs, vec![checkpoint_ref]);
        assert!(!orphan.exists());
        assert!(git_output(repo.path(), &["show-ref", checkpoint_ref]).is_none());

        // SAFETY: restore prior TMPDIR so later tests see a valid temp root.
        unsafe {
            match &previous_tmpdir {
                Some(value) => std::env::set_var("TMPDIR", value),
                None => std::env::remove_var("TMPDIR"),
            }
        }
    }
}

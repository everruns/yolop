// Shared workspace host for yolop tools that shell out against disk.
//
// Everruns centralizes path presentation and containment in `MountFs` and
// `RealDiskFileStore`. File tools and host-backed capabilities share one
// repointable disk handle synced from the worktree's active-root lock.

use anyhow::{Context, Result, bail};
use everruns_runtime::RealDiskFileStore;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

/// Session-mutable workspace root + repointable host disk (EVE-660).
pub struct WorkspaceHost {
    active_root: Arc<RwLock<PathBuf>>,
    disk: Arc<RealDiskFileStore>,
    applied_root: Mutex<PathBuf>,
}

impl WorkspaceHost {
    pub fn new(active_root: Arc<RwLock<PathBuf>>, initial: PathBuf) -> everruns_core::Result<Self> {
        Ok(Self {
            disk: Arc::new(RealDiskFileStore::new(initial.clone())?),
            applied_root: Mutex::new(initial),
            active_root,
        })
    }

    pub fn disk(&self) -> Arc<RealDiskFileStore> {
        self.disk.clone()
    }

    /// Repoint the host disk when the worktree active root changed.
    pub fn sync(&self) -> everruns_core::Result<()> {
        use everruns_core::error::AgentLoopError;

        let current = self
            .active_root
            .read()
            .map_err(|_| AgentLoopError::config("workspace lock poisoned"))?
            .clone();
        let mut applied = self
            .applied_root
            .lock()
            .map_err(|_| AgentLoopError::config("workspace root lock poisoned"))?;
        if *applied != current {
            if current.is_dir() {
                self.disk.set_host_root(current.clone())?;
            }
            *applied = current;
        }
        Ok(())
    }

    pub fn host_root(&self) -> everruns_core::Result<PathBuf> {
        self.sync()?;
        Ok(self.disk.root())
    }

    pub fn spawn_cwd(&self) -> Result<PathBuf, String> {
        let current = self
            .active_root
            .read()
            .map(|guard| guard.clone())
            .map_err(|_| "workspace lock poisoned".to_string())?;
        if !current.is_dir() {
            return Err(format!(
                "workspace directory does not exist: {}",
                current.display()
            ));
        }
        self.sync().map_err(|e| e.to_string())?;
        Ok(self.disk.root())
    }
}

/// The model-facing display alias for the workspace root. everruns' `MountFs`
/// routes `/workspace/...` to the host root for the file tools (read/grep/edit);
/// `repo_map` / `repo_symbols` resolve host paths here directly, bypassing
/// `MountFs`, so they must honor the same alias.
const WORKSPACE_ALIAS: &str = "/workspace";

/// Map a model-facing path to a canonical host directory under `root`.
pub fn resolve_host_scope(root: &Path, path: Option<&str>) -> Result<PathBuf> {
    let Some(path) = path else {
        return Ok(root.to_path_buf());
    };
    let trimmed = path.trim();

    // The model frequently addresses scopes through the `/workspace` alias — a
    // strong cloud-agent prior, and the route everruns' `MountFs` already
    // accepts for the file tools. Strip it to a workspace-relative path so
    // `repo_map`/`repo_symbols` don't 404 with "path not found: /workspace" on a
    // scope that read/grep/edit resolve fine. Mirrors the alias handling in
    // `everruns_core::session_path::to_session_path`.
    if let Some(rest) = strip_workspace_alias(trimmed) {
        return resolve_relative_scope(root, rest.trim_start_matches('/'), path);
    }

    let candidate = Path::new(trimmed);
    if candidate.is_absolute() {
        let canonical = candidate
            .canonicalize()
            .with_context(|| format!("path not found: {path}"))?;
        if !canonical.starts_with(root) {
            bail!("`path` must stay inside the workspace");
        }
        return Ok(canonical);
    }

    resolve_relative_scope(root, trimmed, path)
}

/// Resolve a workspace-relative scope (already stripped of any `/workspace`
/// alias) against `root`, rejecting traversal and enforcing containment.
/// `original` is the caller-supplied spelling, used only for error messages.
fn resolve_relative_scope(root: &Path, relative: &str, original: &str) -> Result<PathBuf> {
    if relative.is_empty() || relative == "." {
        return Ok(root.to_path_buf());
    }
    let candidate = Path::new(relative);
    if candidate.components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        bail!("`path` must stay inside the workspace");
    }
    let scope = root.join(candidate);
    let canonical = scope
        .canonicalize()
        .with_context(|| format!("path not found: {original}"))?;
    if !canonical.starts_with(root) {
        bail!("`path` must stay inside the workspace");
    }
    Ok(canonical)
}

/// Strip the `/workspace` display alias, returning the remainder for an exact
/// match (`""`) or a `/workspace/<sub>` prefix. Returns `None` for unrelated
/// paths, including near-misses like `/workspacefoo` that only share the prefix
/// without the segment boundary.
fn strip_workspace_alias(path: &str) -> Option<&str> {
    if path == WORKSPACE_ALIAS {
        return Some("");
    }
    path.strip_prefix("/workspace/")
}

/// Validate that `root` contains no traversal components (for tests/helpers).
#[allow(dead_code)]
pub fn reject_escape(relative: &str) -> Result<()> {
    if Path::new(relative).components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        bail!("`path` must stay inside the workspace");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn resolve_host_scope_accepts_host_and_relative_paths() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::create_dir_all(dir.path().join("pkg")).expect("pkg dir");
        let root = fs::canonicalize(dir.path()).expect("canonical root");

        let host_path = root.join("pkg");
        let scope = resolve_host_scope(&root, host_path.to_str()).expect("host subpath");
        assert_eq!(scope, root.join("pkg"));

        let root_scope = resolve_host_scope(&root, root.to_str()).expect("host root");
        assert_eq!(root_scope, root);

        let relative = resolve_host_scope(&root, Some("pkg")).expect("relative subpath");
        assert_eq!(relative, root.join("pkg"));
    }

    #[test]
    fn resolve_host_scope_accepts_workspace_alias() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::create_dir_all(dir.path().join("pkg")).expect("pkg dir");
        let root = fs::canonicalize(dir.path()).expect("canonical root");

        // `/workspace` is the model-facing alias for the root; the file tools
        // accept it via MountFs, so repo_map/repo_symbols must too.
        assert_eq!(
            resolve_host_scope(&root, Some("/workspace")).expect("alias root"),
            root
        );
        assert_eq!(
            resolve_host_scope(&root, Some("/workspace/pkg")).expect("alias subpath"),
            root.join("pkg")
        );
        // Trailing/duplicate slashes still land on the alias target.
        assert_eq!(
            resolve_host_scope(&root, Some("/workspace/")).expect("alias root slash"),
            root
        );
    }

    #[test]
    fn resolve_host_scope_alias_rejects_escape_and_near_miss() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = fs::canonicalize(dir.path()).expect("canonical root");

        // Traversal dressed up with the alias prefix must not escape the root.
        let err =
            resolve_host_scope(&root, Some("/workspace/../outside")).expect_err("alias escape");
        assert!(err.to_string().contains("inside the workspace"));

        // `/workspacefoo` shares the prefix but is not the `/workspace` segment,
        // so it is treated as a real absolute path (which does not exist here).
        let err = resolve_host_scope(&root, Some("/workspacefoo")).expect_err("near miss");
        assert!(err.to_string().contains("path not found"));
    }

    #[test]
    fn resolve_host_scope_rejects_escape() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = fs::canonicalize(dir.path()).expect("canonical root");
        let err = resolve_host_scope(&root, Some("../outside")).expect_err("escape");
        assert!(err.to_string().contains("inside the workspace"));
    }

    #[test]
    fn workspace_host_repoints_on_active_root_change() {
        let first = tempfile::tempdir().expect("first");
        let second = tempfile::tempdir().expect("second");
        let active = Arc::new(RwLock::new(first.path().to_path_buf()));
        let host = WorkspaceHost::new(active.clone(), first.path().to_path_buf()).expect("host");

        assert_eq!(host.host_root().expect("root"), host.disk().root());

        *active.write().expect("lock") = second.path().to_path_buf();
        host.sync().expect("sync");
        assert_eq!(
            host.disk().root(),
            fs::canonicalize(second.path()).expect("canonical second")
        );
    }

    #[test]
    fn spawn_cwd_requires_existing_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let active = Arc::new(RwLock::new(dir.path().to_path_buf()));
        let host = WorkspaceHost::new(active, dir.path().to_path_buf()).expect("host");
        assert_eq!(host.spawn_cwd().expect("existing dir"), host.disk().root());

        let missing = dir.path().join("removed");
        *host.active_root.write().expect("lock") = missing.clone();
        host.sync().expect("sync");
        let err = host.spawn_cwd().expect_err("missing dir");
        assert!(err.contains("does not exist"));
    }
}

// Shared workspace host for yolop tools that shell out against disk.
//
// Everruns centralizes path presentation and containment in `RealDiskFileStore`.
// File tools and host-backed capabilities share one
// repointable disk handle synced from the worktree's active-root lock.

use anyhow::{Context, Result, bail};
use everruns_host::RealDiskFileStore;
use std::ops::Deref;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

/// Session-mutable workspace root + repointable host disk (EVE-660).
pub struct WorkspaceHost {
    active_root: Arc<RwLock<PathBuf>>,
    disk: Arc<RealDiskFileStore>,
    applied_root: Mutex<PathBuf>,
}

impl WorkspaceHost {
    pub fn new(
        active_root: Arc<RwLock<PathBuf>>,
        initial: PathBuf,
    ) -> everruns_provider::error::Result<Self> {
        Ok(Self {
            disk: Arc::new(RealDiskFileStore::new(initial.clone())?),
            applied_root: Mutex::new(initial),
            active_root,
        })
    }

    pub fn disk(&self) -> Arc<RealDiskFileStore> {
        self.disk.clone()
    }

    pub fn active_root(&self) -> Result<PathBuf, String> {
        self.active_root
            .read()
            .map(|root| root.clone())
            .map_err(|_| "workspace lock poisoned".to_string())
    }

    /// Repoint the host disk when the worktree active root changed.
    pub fn sync(&self) -> everruns_provider::error::Result<()> {
        use everruns_provider::AgentLoopError;

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

    pub fn host_root(&self) -> everruns_provider::error::Result<PathBuf> {
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

/// A disk path proven to live inside the workspace root.
///
/// The only way to obtain one from model-supplied input is [`resolve_host_scope`],
/// which resolves repository-relative or real absolute paths and enforces containment. The
/// disk-scanning capabilities (`repo_map`, `repo_symbols`, `ast_grep`, `lsp`)
/// resolve model paths through it, so a resolved scope is a *distinct type* from
/// a bare `PathBuf` a tool might build by hand — a new tool that skips
/// normalization can't produce a `HostPath` without going through the resolver.
/// `Deref<Target = Path>` keeps it ergonomic at the call sites.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostPath(PathBuf);

impl HostPath {
    /// Borrow the contained, containment-checked host path.
    pub fn as_path(&self) -> &Path {
        &self.0
    }

    /// Consume into the owned host path.
    pub fn into_path_buf(self) -> PathBuf {
        self.0
    }
}

impl Deref for HostPath {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.0
    }
}

impl AsRef<Path> for HostPath {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

/// Map a model-facing path to a canonical host directory under `root`.
///
/// Real absolute paths are accepted only when they resolve inside the repository.
/// Relative paths resolve from the repository root. Other absolute paths are not
/// aliases and are rejected.
pub fn resolve_host_scope(root: &Path, path: Option<&str>) -> Result<HostPath> {
    let Some(path) = path else {
        return Ok(HostPath(root.to_path_buf()));
    };
    let trimmed = path.trim();

    let candidate = Path::new(trimmed);
    if candidate.is_absolute() {
        let canonical = candidate
            .canonicalize()
            .with_context(|| format!("path not found: {path}"))?;
        if canonical.starts_with(root) {
            return Ok(HostPath(canonical));
        }
        bail!("`path` must stay inside the workspace");
    }

    resolve_relative_scope(root, trimmed, path).map(HostPath)
}

/// Resolve a workspace-relative scope (already normalized to a session path and
/// stripped of its leading slash) against `root`, rejecting traversal and
/// enforcing containment. `original` is the caller-supplied spelling, used only
/// for error messages.
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
        assert_eq!(scope, HostPath(root.join("pkg")));

        let root_scope = resolve_host_scope(&root, root.to_str()).expect("host root");
        assert_eq!(root_scope, HostPath(root.clone()));

        let relative = resolve_host_scope(&root, Some("pkg")).expect("relative subpath");
        assert_eq!(relative, HostPath(root.join("pkg")));
    }

    #[test]
    fn resolve_host_scope_rejects_virtual_and_unrelated_absolute_paths() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::create_dir_all(dir.path().join("pkg")).expect("pkg dir");
        let root = fs::canonicalize(dir.path()).expect("canonical root");

        assert_eq!(
            resolve_host_scope(&root, Some("pkg")).expect("relative path"),
            HostPath(root.join("pkg"))
        );
        assert!(resolve_host_scope(&root, Some("/workspace/pkg")).is_err());
        assert!(resolve_host_scope(&root, Some("/pkg")).is_err());
        assert!(resolve_host_scope(&root, Some("../outside")).is_err());
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

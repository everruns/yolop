// Shared workspace host for yolop tools that shell out against disk.
//
// Everruns centralizes path presentation and containment in `MountFs` and
// `RealDiskFileStore`. File tools and host-backed capabilities share one
// repointable disk handle synced from the worktree's active-root lock.

use anyhow::{Context, Result, bail};
use everruns_core::session_path::to_session_path;
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
/// which normalizes the `/workspace` display alias and enforces containment. The
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
/// yolop presents the model its **real** host checkout path (#258), yet models
/// still emit the `/workspace` alias from cloud-agent priors — so both spellings
/// must resolve, on every path surface, forever. A real host-absolute path is
/// honored directly (with a containment check); everything else — the
/// `/workspace` alias, a session-absolute `/src`, a bare relative `src` — is
/// funneled through everruns' `to_session_path`, the *same* normalizer the file
/// tools reach via `MountFs`. That shared normalizer is the point: `repo_map` /
/// `repo_symbols` / `ast_grep` / `lsp` now accept exactly what `read` / `grep` /
/// `edit` accept, and the two can't drift because there is one algorithm.
pub fn resolve_host_scope(root: &Path, path: Option<&str>) -> Result<HostPath> {
    let Some(path) = path else {
        return Ok(HostPath(root.to_path_buf()));
    };
    let trimmed = path.trim();

    // A real host-absolute path (what yolop actually shows the model): accept it
    // when it resolves inside the workspace, reject it when it resolves outside.
    // If it does not resolve as a real path it is almost certainly a session
    // spelling (`/workspace/...`, `/src`), so fall through to the normalizer.
    let candidate = Path::new(trimmed);
    if candidate.is_absolute()
        && let Ok(canonical) = candidate.canonicalize()
    {
        if canonical.starts_with(root) {
            return Ok(HostPath(canonical));
        }
        bail!("`path` must stay inside the workspace");
    }

    // Alias / session-absolute / relative all normalize through the one shared
    // normalizer (which strips `/workspace`, collapses slashes, and canonicalizes
    // the spelling), then resolve relative to the real root.
    let session = to_session_path(trimmed);
    resolve_relative_scope(root, session.trim_start_matches('/'), path).map(HostPath)
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

    // Every spelling a model might reach for — the `/workspace` alias, a
    // session-absolute `/pkg`, the real host-absolute path, a bare relative, and
    // their slash/​trailing-slash variants — must resolve to the *same* host path
    // as the file tools' MountFs route. This table is the cross-cutting guard the
    // per-tool suites never had: it fails the moment any spelling drifts.
    #[test]
    fn resolve_host_scope_spelling_table_is_consistent() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::create_dir_all(dir.path().join("pkg")).expect("pkg dir");
        let root = fs::canonicalize(dir.path()).expect("canonical root");

        let root_spellings = [
            None,
            Some(".".to_string()),
            Some("/workspace".to_string()),
            Some("/workspace/".to_string()),
            Some("//workspace//".to_string()),
            Some(root.to_str().expect("root utf8").to_string()),
        ];
        for spelling in root_spellings {
            let scope = resolve_host_scope(&root, spelling.as_deref())
                .unwrap_or_else(|e| panic!("root spelling {spelling:?}: {e}"));
            assert_eq!(scope.as_path(), root, "root spelling {spelling:?}");
        }

        let pkg = root.join("pkg");
        let pkg_spellings = [
            "pkg",
            "/pkg",                          // session-absolute
            "/workspace/pkg",                // display alias
            "//workspace//pkg",              // alias with redundant slashes
            pkg.to_str().expect("pkg utf8"), // real host-absolute
        ];
        for spelling in pkg_spellings {
            let scope = resolve_host_scope(&root, Some(spelling))
                .unwrap_or_else(|e| panic!("pkg spelling {spelling:?}: {e}"));
            assert_eq!(scope.as_path(), pkg, "pkg spelling {spelling:?}");
        }

        // Escapes are rejected no matter how they are dressed up.
        for escape in ["/workspace/../outside", "../outside", "//workspace//../x"] {
            assert!(
                resolve_host_scope(&root, Some(escape)).is_err(),
                "escape {escape:?} must be rejected"
            );
        }

        // `/workspacefoo` shares the prefix but is not the `/workspace` segment,
        // so it is a real absolute path that does not exist here — not the alias.
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

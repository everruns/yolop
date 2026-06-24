// Shared workspace path helpers.
//
// Yolop exposes two path namespaces to models:
// - **VFS** (`/workspace`, `/src/foo.rs`, `/outputs/...`) for file tools
// - **Host** (real filesystem paths) for bash, environment context, and spawn cwd
//
// Models trained on cloud-agent layouts often pass `/workspace/...` to every
// tool. File tools and repo_map normalize that alias; bash runs on the host and
// must use a directory that actually exists on disk.

use anyhow::{Context, Result, bail};
use std::path::{Component, Path, PathBuf};

/// Shared, session-mutable workspace root (worktree switches update this).
pub type SharedWorkspaceRoot = std::sync::Arc<std::sync::RwLock<PathBuf>>;

/// Strip the `/workspace` VFS root alias that file tools expose to models.
///
/// Accepts `/workspace`, `/workspace/`, `workspace`, and `/workspace/<subpath>`.
pub fn strip_workspace_vfs_prefix(path: &str) -> &str {
    let trimmed = path.trim().trim_start_matches('/');
    trimmed
        .strip_prefix("workspace/")
        .unwrap_or(if trimmed == "workspace" { "" } else { trimmed })
}

/// Resolve an optional path argument against a canonical workspace root.
///
/// Mirrors file-store normalization: `/workspace/foo` scopes to `foo` under
/// `root`, not a literal `root/workspace/foo` host path.
pub fn resolve_workspace_scope(root: &Path, path: Option<&str>) -> Result<PathBuf> {
    let Some(path) = path else {
        return Ok(root.to_path_buf());
    };
    let relative = strip_workspace_vfs_prefix(path);
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
        .with_context(|| format!("path not found: {path}"))?;
    if !canonical.starts_with(root) {
        bail!("`path` must stay inside the workspace");
    }
    Ok(canonical)
}

/// Return a directory suitable for `Command::current_dir`.
///
/// When the workspace directory was removed (e.g. `git worktree remove .`), spawn
/// would otherwise fail with the opaque `spawn failed: No such file or directory`.
pub fn resolve_spawn_cwd(root: &Path) -> Result<PathBuf, String> {
    if root.is_dir() {
        return Ok(root.to_path_buf());
    }
    Err(format!(
        "workspace directory does not exist: {}",
        root.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn strip_workspace_vfs_prefix_handles_root_aliases() {
        assert_eq!(strip_workspace_vfs_prefix("/workspace"), "");
        assert_eq!(strip_workspace_vfs_prefix("/workspace/"), "");
        assert_eq!(strip_workspace_vfs_prefix("workspace"), "");
        assert_eq!(strip_workspace_vfs_prefix("/workspace/src"), "src");
        assert_eq!(strip_workspace_vfs_prefix("workspace/pkg"), "pkg");
        assert_eq!(strip_workspace_vfs_prefix("/src/lib.rs"), "src/lib.rs");
    }

    #[test]
    fn resolve_workspace_scope_accepts_vfs_alias() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::create_dir_all(dir.path().join("pkg")).expect("pkg dir");
        let root = fs::canonicalize(dir.path()).expect("canonical root");

        let scope = resolve_workspace_scope(&root, Some("/workspace/pkg")).expect("vfs subpath");
        assert_eq!(scope, root.join("pkg"));

        let root_scope = resolve_workspace_scope(&root, Some("/workspace")).expect("vfs root");
        assert_eq!(root_scope, root);
    }

    #[test]
    fn resolve_workspace_scope_rejects_escape() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = fs::canonicalize(dir.path()).expect("canonical root");
        let err =
            resolve_workspace_scope(&root, Some("/workspace/../outside")).expect_err("escape");
        assert!(err.to_string().contains("inside the workspace"));
    }

    #[test]
    fn resolve_spawn_cwd_requires_existing_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(
            resolve_spawn_cwd(dir.path()).expect("existing dir"),
            dir.path()
        );

        let missing = dir.path().join("removed");
        let err = resolve_spawn_cwd(&missing).expect_err("missing dir");
        assert!(err.contains("does not exist"));
    }
}

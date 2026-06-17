// Yolop skills wiring for the upstream `ScopedSkillsCapability`.
//
// The skills *capability* (discovery, precedence, `list_skills` /
// `activate_skill` / `read_skill` / `write_skill`, validation, substitution)
// now lives in `everruns_core` as `ScopedSkillsCapability` — yolop no longer
// vendors it. This module keeps only the yolop-specific glue the core
// capability cannot own:
//
//   * the three scopes and where each maps on the *host* disk,
//   * a `SkillDirResolver` so `${SKILL_DIR}` expands to a real host path the
//     `bash` tool can read (the core default keeps it in the VFS),
//   * the system skills pre-packed in the binary and materialized once.
//
// The capability discovers/reads/writes strictly through the session
// `SessionFileSystem`. yolop's file store (`CodingCliSessionFileStore`) maps the
// three scope VFS roots onto the real directories below, so the capability never
// touches a host path directly — the host mapping lives behind the file store,
// not in the capability's configuration.
//
// Scopes (precedence: workspace > global > system; the core capability de-dups
// by skill directory name, so a nearer scope shadows a farther one):
//   * workspace — `<workspace>/.agents/skills`           (writable)
//   * global    — `<config_dir>/yolop/skills`            (writable; override: YOLOP_GLOBAL_SKILLS_DIR)
//   * system    — pre-packed, materialized once          (read-only; override: YOLOP_SYSTEM_SKILLS_DIR)

use everruns_core::capabilities::{SkillDirResolver, SkillScope, SkillsConfig};
use include_dir::{Dir, include_dir};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Env override for the global skills directory.
const GLOBAL_SKILLS_DIR_ENV: &str = "YOLOP_GLOBAL_SKILLS_DIR";
/// Env override for the system skills directory (skips materialization).
const SYSTEM_SKILLS_DIR_ENV: &str = "YOLOP_SYSTEM_SKILLS_DIR";

/// VFS root for the workspace scope. Routes through the workspace file store to
/// `<workspace>/.agents/skills` like any other workspace path.
pub const WORKSPACE_SKILLS_VFS: &str = "/.agents/skills";
/// Synthetic VFS root for the global scope, routed by the file store to the
/// user's global skills directory (outside the workspace).
pub const GLOBAL_SKILLS_VFS: &str = "/.yolop/global-skills";
/// Synthetic VFS root for the system scope, routed by the file store to the
/// materialized system skills directory.
pub const SYSTEM_SKILLS_VFS: &str = "/.yolop/system-skills";

/// System skills shipped inside the binary. The crate-root `skills/` directory is
/// embedded at compile time so a `cargo install` / Homebrew build carries them.
static SYSTEM_SKILLS: Dir<'static> = include_dir!("$CARGO_MANIFEST_DIR/skills");

/// The host directories backing each skill scope for a session.
#[derive(Clone, Debug)]
pub struct SkillDirs {
    /// `<workspace>/.agents/skills` — always present (created on demand).
    pub workspace: PathBuf,
    /// Global skills directory, or `None` when no platform config dir exists.
    pub global: Option<PathBuf>,
    /// Materialized system skills directory, or `None` when unavailable.
    pub system: Option<PathBuf>,
}

impl SkillDirs {
    /// Resolve the workspace/global/system directories for `workspace_root`.
    /// Materializes the embedded system skills as a side effect (idempotent).
    pub fn resolve(workspace_root: &Path) -> Self {
        Self {
            workspace: workspace_root.join(".agents").join("skills"),
            global: global_skills_dir(),
            system: system_skills_dir(),
        }
    }
}

/// Strip a VFS root prefix, returning the remainder as an absolute path under
/// that root (`/` for the root itself). Shared with the file-store router.
pub fn relative_under(path: &str, root: &str) -> Option<String> {
    if path == root {
        return Some("/".to_string());
    }
    path.strip_prefix(&format!("{root}/"))
        .map(|rest| format!("/{rest}"))
}

/// Build the `ScopedSkillsCapability` configuration for these directories.
/// Only scopes whose directory resolved are included; the system scope is
/// read-only, the others writable. `${SKILL_DIR}` and display paths resolve to
/// real host paths via [`HostSkillDirResolver`].
pub fn skills_config(dirs: &SkillDirs) -> SkillsConfig {
    let mut scopes = vec![SkillScope::new("workspace", WORKSPACE_SKILLS_VFS, true)];
    if dirs.global.is_some() {
        scopes.push(SkillScope::new("global", GLOBAL_SKILLS_VFS, true));
    }
    if dirs.system.is_some() {
        scopes.push(SkillScope::new("system", SYSTEM_SKILLS_VFS, false));
    }
    SkillsConfig {
        scopes,
        resolver: Arc::new(HostSkillDirResolver { dirs: dirs.clone() }),
        manage_tools: true,
    }
}

/// Resolves `${SKILL_DIR}` and display paths to real host paths so the host
/// `bash` tool can read a skill's bundled files. yolop's shell runs on the host,
/// not in the VFS, so the VFS-default resolver would hand the model unreachable
/// paths.
struct HostSkillDirResolver {
    dirs: SkillDirs,
}

impl HostSkillDirResolver {
    fn base_for(&self, label: &str) -> PathBuf {
        match label {
            "global" => self.dirs.global.clone(),
            "system" => self.dirs.system.clone(),
            _ => Some(self.dirs.workspace.clone()),
        }
        .unwrap_or_else(|| self.dirs.workspace.clone())
    }
}

impl SkillDirResolver for HostSkillDirResolver {
    fn skill_dir(&self, scope: &SkillScope, name: &str) -> String {
        self.base_for(&scope.label).join(name).display().to_string()
    }

    fn display_dir(&self, scope: &SkillScope, name: &str) -> String {
        // Show the real path too — it's what the agent passes to `bash`.
        self.skill_dir(scope, name)
    }
}

/// Global skills directory, or `None` when no platform config directory exists.
/// Honors `YOLOP_GLOBAL_SKILLS_DIR`; otherwise `<config_dir>/yolop/skills`.
/// The path is returned even when absent so newly installed global skills become
/// available without restarting the process.
pub fn global_skills_dir() -> Option<PathBuf> {
    Some(match std::env::var(GLOBAL_SKILLS_DIR_ENV) {
        Ok(value) if !value.is_empty() => PathBuf::from(value),
        _ => dirs::config_dir()?.join("yolop").join("skills"),
    })
}

/// System skills directory, materializing the embedded skills first.
///
/// Honors `YOLOP_SYSTEM_SKILLS_DIR` (used verbatim). Otherwise the embedded
/// `skills/` tree is written to `<data_dir>/yolop/system-skills` and that path is
/// returned. Materialization is idempotent and concurrency-safe (atomic per-file
/// writes, skipping files already present with identical bytes), so parallel
/// processes/tests do not race. Any failure is non-fatal: it logs and returns
/// `None`, leaving the system scope unavailable.
pub fn system_skills_dir() -> Option<PathBuf> {
    if let Ok(value) = std::env::var(SYSTEM_SKILLS_DIR_ENV)
        && !value.is_empty()
    {
        let dir = PathBuf::from(value);
        return dir.is_dir().then_some(dir);
    }

    if SYSTEM_SKILLS.entries().is_empty() {
        return None;
    }

    let dest = dirs::data_dir()?.join("yolop").join("system-skills");
    match materialize_system_skills(&dest) {
        Ok(()) => Some(dest),
        Err(e) => {
            tracing::warn!(error = %e, dest = %dest.display(), "failed to materialize system skills");
            None
        }
    }
}

/// Write the embedded system skills into `dest` if absent or changed.
fn materialize_system_skills(dest: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dest)?;
    extract_dir(&SYSTEM_SKILLS, dest)
}

/// Recursively write an embedded `Dir` under `dest`. `include_dir` entry paths
/// are relative to the embed root, so they map directly onto `dest`.
fn extract_dir(dir: &Dir<'_>, dest: &Path) -> std::io::Result<()> {
    for entry in dir.entries() {
        let target = dest.join(entry.path());
        match entry {
            include_dir::DirEntry::Dir(subdir) => {
                std::fs::create_dir_all(&target)?;
                extract_dir(subdir, dest)?;
            }
            include_dir::DirEntry::File(file) => {
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                write_if_changed(&target, file.contents())?;
            }
        }
    }
    Ok(())
}

/// Atomically write `contents` to `target`, skipping the write when the file is
/// already present with identical bytes. The atomic temp-then-rename keeps
/// concurrent writers from observing a partial file.
fn write_if_changed(target: &Path, contents: &[u8]) -> std::io::Result<()> {
    if let Ok(existing) = std::fs::read(target)
        && existing == contents
    {
        return Ok(());
    }
    let parent = target.parent().unwrap_or_else(|| Path::new("."));
    // The temp name must be unique per *call*, not just per process: parallel
    // materializations in the same process (e.g. concurrent tests) would
    // otherwise derive the same temp path and clobber each other's rename. A
    // process-wide counter disambiguates same-pid, same-target writers.
    static TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = parent.join(format!(
        ".{}.tmp-{}-{}",
        target
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("skill"),
        std::process::id(),
        seq
    ));
    std::fs::write(&tmp, contents)?;
    std::fs::rename(&tmp, target)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_under_strips_vfs_roots() {
        assert_eq!(
            relative_under("/.yolop/global-skills/foo/SKILL.md", GLOBAL_SKILLS_VFS),
            Some("/foo/SKILL.md".to_string())
        );
        assert_eq!(
            relative_under("/.yolop/system-skills", SYSTEM_SKILLS_VFS),
            Some("/".to_string())
        );
        assert_eq!(relative_under("/src/main.rs", GLOBAL_SKILLS_VFS), None);
        // A different root must not match.
        assert_eq!(
            relative_under("/.agents/skills/foo", GLOBAL_SKILLS_VFS),
            None
        );
    }

    #[test]
    fn config_includes_only_resolved_scopes() {
        let dirs = SkillDirs {
            workspace: PathBuf::from("/ws/.agents/skills"),
            global: None,
            system: Some(PathBuf::from("/data/sys")),
        };
        let cfg = skills_config(&dirs);
        let labels: Vec<&str> = cfg.scopes.iter().map(|s| s.label.as_str()).collect();
        assert_eq!(labels, vec!["workspace", "system"]);
        assert!(cfg.manage_tools);
        // System scope is read-only; workspace is writable.
        assert!(
            !cfg.scopes
                .iter()
                .find(|s| s.label == "system")
                .unwrap()
                .writable
        );
        assert!(
            cfg.scopes
                .iter()
                .find(|s| s.label == "workspace")
                .unwrap()
                .writable
        );
    }

    #[test]
    fn resolver_returns_real_host_paths() {
        let dirs = SkillDirs {
            workspace: PathBuf::from("/ws/.agents/skills"),
            global: Some(PathBuf::from("/cfg/yolop/skills")),
            system: Some(PathBuf::from("/data/sys")),
        };
        let r = HostSkillDirResolver { dirs };
        // Compare as paths so separators are platform-correct.
        assert_eq!(
            PathBuf::from(r.skill_dir(&SkillScope::new("global", GLOBAL_SKILLS_VFS, true), "foo")),
            PathBuf::from("/cfg/yolop/skills").join("foo")
        );
        assert_eq!(
            PathBuf::from(r.skill_dir(
                &SkillScope::new("workspace", WORKSPACE_SKILLS_VFS, true),
                "bar"
            )),
            PathBuf::from("/ws/.agents/skills").join("bar")
        );
    }
}

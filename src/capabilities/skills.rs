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

use async_trait::async_trait;
use everruns_core::capabilities::{
    Capability, CapabilityStatus, SkillDirResolver, SkillScope, SkillsConfig,
};
use everruns_core::tools::{Tool, ToolExecutionResult};
use include_dir::{Dir, include_dir};
use serde_json::{Value, json};
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

pub(crate) const SKILL_MANAGEMENT_CAPABILITY_ID: &str = "yolop_skill_management";

const SKILL_MANAGEMENT_PROMPT: &str = "<capability id=\"yolop_skill_management\">\n\
    To uninstall a skill, call `delete_skill` with its name and scope \
    (`workspace` or `global`). It removes the installed skill directory in that \
    scope. System skills are read-only and cannot be deleted. Install and update \
    still go through `write_skill`; this only handles removal.\n\
    </capability>";

/// Skill removal for yolop's writable scopes. The upstream
/// `ScopedSkillsCapability` owns discovery and `list/activate/read/write_skill`
/// but has no uninstall, so yolop contributes `delete_skill` here. It operates
/// on the same workspace/global directories the resolver exposes (system stays
/// read-only), giving the agent a first-class, conversational uninstall instead
/// of shelling out to `rm`.
pub(crate) struct SkillManagementCapability {
    dirs: SkillDirs,
}

impl SkillManagementCapability {
    pub(crate) fn new(dirs: SkillDirs) -> Self {
        Self { dirs }
    }
}

#[async_trait]
impl Capability for SkillManagementCapability {
    fn id(&self) -> &str {
        SKILL_MANAGEMENT_CAPABILITY_ID
    }
    fn name(&self) -> &str {
        "Skill Management"
    }
    fn description(&self) -> &str {
        "Uninstall workspace or global skills (delete_skill)."
    }
    fn status(&self) -> CapabilityStatus {
        CapabilityStatus::Available
    }
    fn category(&self) -> Option<&str> {
        Some("Examples")
    }
    fn system_prompt_addition(&self) -> Option<&str> {
        Some(SKILL_MANAGEMENT_PROMPT)
    }
    fn tools(&self) -> Vec<Box<dyn Tool>> {
        vec![Box::new(DeleteSkillTool {
            dirs: self.dirs.clone(),
        })]
    }
}

struct DeleteSkillTool {
    dirs: SkillDirs,
}

impl DeleteSkillTool {
    /// Resolve the host directory for a writable scope. `system` is rejected
    /// (read-only); an unconfigured `global` scope is reported rather than
    /// silently falling back to the workspace.
    fn base_for(&self, scope: &str) -> Result<PathBuf, String> {
        match scope {
            "workspace" | "local" => Ok(self.dirs.workspace.clone()),
            "global" => self
                .dirs
                .global
                .clone()
                .ok_or_else(|| "no global skills directory is configured".to_string()),
            "system" => Err("system skills are read-only and cannot be deleted".to_string()),
            other => Err(format!(
                "unknown scope `{other}`; expected `workspace` or `global`"
            )),
        }
    }
}

/// A skill name must be a single, plain path component — no separators, no `.`
/// or `..`. This keeps `delete_skill` from escaping the scope directory.
fn validate_skill_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("'name' is required".to_string());
    }
    let mut components = Path::new(name).components();
    let only = components.next();
    if components.next().is_some() {
        return Err(format!(
            "invalid skill name `{name}`: must be a single path segment"
        ));
    }
    match only {
        Some(std::path::Component::Normal(segment)) if segment == name => Ok(()),
        _ => Err(format!(
            "invalid skill name `{name}`: must not contain path separators, `.`, or `..`"
        )),
    }
}

#[async_trait]
impl Tool for DeleteSkillTool {
    fn name(&self) -> &str {
        "delete_skill"
    }
    fn display_name(&self) -> Option<&str> {
        Some("Delete skill")
    }
    fn description(&self) -> &str {
        "Uninstall a skill by removing its directory from a writable scope \
         (`workspace` or `global`). System skills cannot be deleted. Use this to \
         remove a skill the user no longer wants; install/update go through `write_skill`."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "The skill's directory name (matches its SKILL.md `name`)."
                },
                "scope": {
                    "type": "string",
                    "description": "Which writable scope to remove it from.",
                    "enum": ["workspace", "global"],
                    "default": "workspace"
                }
            },
            "required": ["name"],
            "additionalProperties": false
        })
    }
    async fn execute(&self, arguments: Value) -> ToolExecutionResult {
        let name = arguments
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        if let Err(err) = validate_skill_name(name) {
            return ToolExecutionResult::tool_error(err);
        }
        let scope = arguments
            .get("scope")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("workspace");
        let base = match self.base_for(scope) {
            Ok(base) => base,
            Err(err) => return ToolExecutionResult::tool_error(err),
        };
        let target = base.join(name);
        if !target.is_dir() {
            return ToolExecutionResult::tool_error(format!(
                "no `{name}` skill installed in the {scope} scope"
            ));
        }
        // Guard against deleting an unrelated directory: a real skill always
        // carries a SKILL.md. Refuse anything that does not look like a skill.
        if !target.join("SKILL.md").is_file() {
            return ToolExecutionResult::tool_error(format!(
                "`{}` is not a skill (no SKILL.md); refusing to delete",
                target.display()
            ));
        }
        match std::fs::remove_dir_all(&target) {
            Ok(()) => ToolExecutionResult::success(json!({
                "success": true,
                "name": name,
                "scope": scope,
                "removed": target.display().to_string(),
                "message": format!("uninstalled `{name}` from the {scope} scope"),
            })),
            Err(err) => {
                ToolExecutionResult::tool_error(format!("failed to delete `{name}`: {err}"))
            }
        }
    }
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

    #[test]
    fn embedded_system_skills_parse_with_matching_names() {
        // Materialize the binary's system skills and validate each through the
        // upstream parser the capability uses. Every embedded skill must parse,
        // and its frontmatter `name` must equal its directory name — that name
        // is the activation/precedence key, so a mismatch is shipped-broken.
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("system-skills");
        materialize_system_skills(&dest).unwrap();

        let mut dir_names = Vec::new();
        for entry in std::fs::read_dir(&dest).unwrap() {
            // Unwrap each entry/file_type so a filesystem error fails the test
            // loudly instead of silently skipping an embedded skill.
            let entry = entry.unwrap();
            if !entry.file_type().unwrap().is_dir() {
                continue;
            }
            let dir_name = entry.file_name().into_string().unwrap();
            let md = std::fs::read_to_string(entry.path().join("SKILL.md"))
                .unwrap_or_else(|_| panic!("{dir_name} has no SKILL.md"));
            let parsed = everruns_core::skill::parse_skill_md(&md)
                .unwrap_or_else(|e| panic!("{dir_name}/SKILL.md failed to parse: {e:?}"));
            assert_eq!(
                parsed.name, dir_name,
                "system skill `name` must match its directory"
            );
            dir_names.push(dir_name);
        }
        assert!(!dir_names.is_empty(), "expected embedded system skills");

        // The ast-grep skill is shipped, parses, and is user-invocable.
        assert!(
            dir_names.iter().any(|n| n == "ast-grep"),
            "ast-grep system skill is shipped: {dir_names:?}"
        );
        let md = std::fs::read_to_string(dest.join("ast-grep").join("SKILL.md")).unwrap();
        let parsed = everruns_core::skill::parse_skill_md(&md).unwrap();
        assert!(parsed.user_invocable);
        assert!(
            parsed.description.to_lowercase().contains("ast-grep"),
            "description should mention ast-grep: {:?}",
            parsed.description
        );
    }

    // ---------- delete_skill ----------

    #[test]
    fn skill_management_capability_exposes_delete_skill() {
        let dirs = SkillDirs {
            workspace: PathBuf::from("/ws/.agents/skills"),
            global: None,
            system: None,
        };
        let capability = SkillManagementCapability::new(dirs);
        let names: Vec<String> = capability
            .tools()
            .iter()
            .map(|t| t.name().to_string())
            .collect();
        assert!(
            names.iter().any(|n| n == "delete_skill"),
            "delete_skill should be exposed: {names:?}"
        );
        assert!(
            capability
                .system_prompt_addition()
                .expect("prompt")
                .contains("delete_skill")
        );
    }

    /// Build a tool whose workspace/global scopes point at fresh temp dirs.
    fn delete_tool_with_dirs(workspace: &Path, global: Option<&Path>) -> DeleteSkillTool {
        DeleteSkillTool {
            dirs: SkillDirs {
                workspace: workspace.to_path_buf(),
                global: global.map(Path::to_path_buf),
                system: None,
            },
        }
    }

    fn install_skill(base: &Path, name: &str) {
        let dir = base.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: test\n---\nbody"),
        )
        .unwrap();
    }

    #[test]
    fn validate_skill_name_rejects_traversal_and_separators() {
        assert!(validate_skill_name("good-skill").is_ok());
        assert!(validate_skill_name("").is_err());
        assert!(validate_skill_name(".").is_err());
        assert!(validate_skill_name("..").is_err());
        assert!(validate_skill_name("a/b").is_err());
        assert!(validate_skill_name("../escape").is_err());
        assert!(validate_skill_name("/abs").is_err());
    }

    #[tokio::test]
    async fn delete_skill_removes_workspace_skill() {
        let ws = tempfile::tempdir().unwrap();
        install_skill(ws.path(), "ship");
        let tool = delete_tool_with_dirs(ws.path(), None);

        let result = tool.execute(json!({ "name": "ship" })).await;

        assert!(result.is_success(), "result: {result:?}");
        assert!(
            !ws.path().join("ship").exists(),
            "skill directory should be gone"
        );
    }

    #[tokio::test]
    async fn delete_skill_defaults_to_workspace_and_uninstalls_global_when_asked() {
        let ws = tempfile::tempdir().unwrap();
        let global = tempfile::tempdir().unwrap();
        install_skill(ws.path(), "dup");
        install_skill(global.path(), "dup");
        let tool = delete_tool_with_dirs(ws.path(), Some(global.path()));

        // Default scope is workspace.
        assert!(tool.execute(json!({ "name": "dup" })).await.is_success());
        assert!(!ws.path().join("dup").exists());
        assert!(global.path().join("dup").exists(), "global untouched");

        // Explicit global scope removes the global copy.
        assert!(
            tool.execute(json!({ "name": "dup", "scope": "global" }))
                .await
                .is_success()
        );
        assert!(!global.path().join("dup").exists());
    }

    #[tokio::test]
    async fn delete_skill_rejects_system_scope() {
        let ws = tempfile::tempdir().unwrap();
        let tool = delete_tool_with_dirs(ws.path(), None);
        let result = tool
            .execute(json!({ "name": "anything", "scope": "system" }))
            .await;
        assert!(result.is_error(), "system skills must be read-only");
    }

    #[tokio::test]
    async fn delete_skill_errors_on_unconfigured_global_scope() {
        let ws = tempfile::tempdir().unwrap();
        let tool = delete_tool_with_dirs(ws.path(), None);
        let result = tool
            .execute(json!({ "name": "x", "scope": "global" }))
            .await;
        assert!(result.is_error());
    }

    #[tokio::test]
    async fn delete_skill_errors_when_missing() {
        let ws = tempfile::tempdir().unwrap();
        let tool = delete_tool_with_dirs(ws.path(), None);
        let result = tool.execute(json!({ "name": "ghost" })).await;
        assert!(result.is_error(), "deleting a missing skill should fail");
    }

    #[tokio::test]
    async fn delete_skill_refuses_directory_without_skill_md() {
        let ws = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(ws.path().join("notaskill")).unwrap();
        let tool = delete_tool_with_dirs(ws.path(), None);
        let result = tool.execute(json!({ "name": "notaskill" })).await;
        assert!(result.is_error(), "must refuse non-skill directories");
        assert!(
            ws.path().join("notaskill").exists(),
            "directory must be left intact"
        );
    }

    #[tokio::test]
    async fn delete_skill_rejects_path_traversal_argument() {
        let ws = tempfile::tempdir().unwrap();
        // A sibling dir outside the scope that must never be touched.
        let outside = ws.path().parent().unwrap().join("outside-skill");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("SKILL.md"), "---\nname: x\n---\n").unwrap();
        let tool = delete_tool_with_dirs(ws.path(), None);

        let result = tool.execute(json!({ "name": "../outside-skill" })).await;

        assert!(result.is_error(), "traversal must be rejected");
        assert!(outside.exists(), "outside directory must survive");
        let _ = std::fs::remove_dir_all(&outside);
    }
}

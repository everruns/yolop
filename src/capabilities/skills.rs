// Yolop skills wiring for the upstream `ScopedSkillsCapability`.
//
// The skills *capability* (discovery, precedence, `list_skills` /
// `activate_skill` / `read_skill` / `write_skill`, validation, substitution)
// now lives in `everruns_core` as `ScopedSkillsCapability` — yolop no longer
// vendors it. This module keeps only the yolop-specific glue the core
// capability cannot own:
//
//   * the scope set and where each maps on the *host* disk,
//   * a `SkillDirResolver` so `${SKILL_DIR}` expands to a real host path the
//     `bash` tool can read (the core default keeps it in the VFS),
//   * the system skills pre-packed in the binary and materialized once.
//
// The capability discovers/reads/writes strictly through the session
// `SessionFileSystem`. yolop's file store (`CodingCliSessionFileStore`) maps the
// disk-backed scope VFS roots onto the real directories below, so the capability never
// touches a host path directly — the host mapping lives behind the file store,
// not in the capability's configuration.
//
// Scopes (precedence: workspace > profile > global > system; the core capability
// de-dups by skill directory name, so a nearer scope shadows a farther one):
//   * workspace — `<workspace>/.agents/skills`           (writable)
//   * profile   — the active `--profile`'s skills dir    (writable; only while selected)
//   * global    — `~/.agents/skills`                     (writable; override: YOLOP_GLOBAL_SKILLS_DIR)
//   * system    — pre-packed, materialized once          (read-only; override: YOLOP_SYSTEM_SKILLS_DIR)

use crate::capabilities::narration::stable_labeled;
use async_trait::async_trait;
use everruns_builtins::{SkillDirResolver, SkillScope, SkillsConfig};
use everruns_core::tool_narration::{ToolNarrationPhase, arg_str, truncate};
use everruns_core::{Capability, CapabilityStatus};
use everruns_core::{Tool, ToolExecutionResult};
use everruns_provider::ToolCall;
use include_dir::{Dir, include_dir};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Env override for the global skills directory.
const GLOBAL_SKILLS_DIR_ENV: &str = "YOLOP_GLOBAL_SKILLS_DIR";
/// Env override for the legacy global skills directory.
const LEGACY_GLOBAL_SKILLS_DIR_ENV: &str = "YOLOP_LEGACY_GLOBAL_SKILLS_DIR";
/// Env override for the system skills directory (skips materialization).
const SYSTEM_SKILLS_DIR_ENV: &str = "YOLOP_SYSTEM_SKILLS_DIR";

/// VFS root for the workspace scope. Routes through the workspace file store to
/// `<workspace>/.agents/skills` like any other workspace path.
pub const WORKSPACE_SKILLS_VFS: &str = "/.agents/skills";
/// Synthetic VFS root for the global scope, routed by the file store to the
/// user's global skills directory (outside the workspace).
pub const GLOBAL_SKILLS_VFS: &str = "/.yolop/global-skills";
/// Synthetic VFS root for the active profile's skills directory, routed by the
/// file store to `profiles/<name>/skills` (or the profile's `skills_dir`).
pub const PROFILE_SKILLS_VFS: &str = "/.yolop/profile-skills";
/// Synthetic VFS root for the system scope, routed by the file store to the
/// materialized system skills directory.
pub const SYSTEM_SKILLS_VFS: &str = "/.yolop/system-skills";
/// Read-only, session-local skills contributed by the active host environment.
/// Unlike global/system skills this root has no disk backing; a capability
/// mount (currently Herdr) serves it through `MountFs`.
pub const ENVIRONMENT_SKILLS_VFS: &str = "/.yolop/environment-skills";
/// VFS prefix for read-only skills contributed by installed extensions. Each
/// enabled extension declaring `skills` mounts its `skills/` dir at
/// `<prefix>/<name>`, served from real disk (like global/system).
pub const EXTENSION_SKILLS_VFS_PREFIX: &str = "/.yolop/extension-skills";

/// VFS root for one extension's contributed skills.
pub fn extension_skills_vfs(name: &str) -> String {
    format!("{EXTENSION_SKILLS_VFS_PREFIX}/{name}")
}

/// Scope label for one extension's contributed skills.
pub fn extension_skills_label(name: &str) -> String {
    format!("ext:{name}")
}

/// System skills shipped inside the binary. Keep the source tree away from
/// well-known skill discovery paths so it cannot be mistaken for a writable
/// workspace/global skill location.
static SYSTEM_SKILLS: Dir<'static> = include_dir!("$CARGO_MANIFEST_DIR/src/bundled/system-skills");

/// The host directories backing each skill scope for a session.
#[derive(Clone, Debug)]
pub struct SkillDirs {
    /// `<workspace>/.agents/skills` — always present (created on demand).
    pub workspace: PathBuf,
    /// Global skills directory, or `None` when no home directory exists.
    pub global: Option<PathBuf>,
    /// The active profile's skills directory, when a profile contributes one.
    pub profile: Option<PathBuf>,
    /// Materialized system skills directory, or `None` when unavailable.
    pub system: Option<PathBuf>,
}

impl SkillDirs {
    /// Resolve the workspace/global/system directories for `workspace_root`.
    /// Materializes the embedded system skills as a side effect (idempotent).
    pub fn resolve(workspace_root: &Path, profile: Option<PathBuf>) -> Self {
        let global = global_skills_dir();
        if let (Some(primary), Some(legacy)) = (&global, legacy_global_skills_dir())
            && primary != &legacy
            && let Err(error) = import_legacy_global_skills(&legacy, primary)
        {
            tracing::warn!(
                legacy = %legacy.display(),
                primary = %primary.display(),
                %error,
                "failed to import legacy global skills"
            );
        }
        Self {
            workspace: workspace_root.join(".agents").join("skills"),
            global,
            profile,
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
/// Only disk-backed scopes whose directory resolved are included. System and
/// environment scopes are read-only; workspace/global are writable.
/// `${SKILL_DIR}` and display paths resolve through [`HostSkillDirResolver`].
pub fn skills_config(
    dirs: &SkillDirs,
    environment_active: bool,
    extensions: &[(String, PathBuf)],
) -> SkillsConfig {
    let mut scopes = vec![SkillScope::new("workspace", WORKSPACE_SKILLS_VFS, true)];
    // Between workspace and global: a profile is chosen per run, so its skills
    // should win over the user's global set but never over the repo's own.
    if dirs.profile.is_some() {
        scopes.push(SkillScope::new("profile", PROFILE_SKILLS_VFS, true));
    }
    if dirs.global.is_some() {
        scopes.push(SkillScope::new("global", GLOBAL_SKILLS_VFS, true));
    }
    if environment_active {
        scopes.push(SkillScope::new(
            "environment",
            ENVIRONMENT_SKILLS_VFS,
            false,
        ));
    }
    if dirs.system.is_some() {
        scopes.push(SkillScope::new("system", SYSTEM_SKILLS_VFS, false));
    }
    // Read-only skills contributed by enabled extensions.
    for (name, _dir) in extensions {
        scopes.push(SkillScope::new(
            extension_skills_label(name),
            extension_skills_vfs(name),
            false,
        ));
    }
    SkillsConfig {
        scopes,
        resolver: Arc::new(HostSkillDirResolver {
            dirs: dirs.clone(),
            extensions: extensions
                .iter()
                .map(|(name, dir)| (extension_skills_label(name), dir.clone()))
                .collect(),
        }),
        manage_tools: true,
    }
}

/// Resolves `${SKILL_DIR}` and display paths to real host paths so the host
/// `bash` tool can read a skill's bundled files. yolop's shell runs on the host,
/// not in the VFS, so the VFS-default resolver would hand the model unreachable
/// paths.
struct HostSkillDirResolver {
    dirs: SkillDirs,
    /// Extension scope label (`ext:<name>`) → its on-disk `skills/` directory.
    extensions: std::collections::BTreeMap<String, PathBuf>,
}

impl HostSkillDirResolver {
    fn base_for(&self, label: &str) -> PathBuf {
        if let Some(dir) = self.extensions.get(label) {
            return dir.clone();
        }
        match label {
            "global" => self.dirs.global.clone(),
            "profile" => self.dirs.profile.clone(),
            // Environment skills are VFS-only and contain no shell-side assets.
            // Keep their display/substitution path honest instead of pretending
            // they exist in a writable workspace or global directory.
            "environment" => Some(PathBuf::from(ENVIRONMENT_SKILLS_VFS)),
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

/// Global skills directory, or `None` when no home directory exists.
/// Honors `YOLOP_GLOBAL_SKILLS_DIR`; otherwise `~/.agents/skills`.
/// The path is returned even when absent so newly installed global skills become
/// available without restarting the process.
pub fn global_skills_dir() -> Option<PathBuf> {
    Some(match std::env::var(GLOBAL_SKILLS_DIR_ENV) {
        Ok(value) if !value.is_empty() => PathBuf::from(value),
        _ => dirs::home_dir()?.join(".agents").join("skills"),
    })
}

/// Previous global skills directory retained temporarily for discovery.
pub fn legacy_global_skills_dir() -> Option<PathBuf> {
    Some(match std::env::var(LEGACY_GLOBAL_SKILLS_DIR_ENV) {
        Ok(value) if !value.is_empty() => PathBuf::from(value),
        _ => dirs::config_dir()?.join("yolop").join("skills"),
    })
}

/// Copy legacy global skills into the primary root without replacing newer entries.
fn import_legacy_global_skills(legacy: &Path, primary: &Path) -> std::io::Result<()> {
    let entries = match std::fs::read_dir(legacy) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    std::fs::create_dir_all(primary)?;
    for entry in entries {
        let entry = entry?;
        let source = entry.path();
        if !entry.file_type()?.is_dir() || !source.join("SKILL.md").is_file() {
            continue;
        }
        let target = primary.join(entry.file_name());
        if !target.exists() {
            copy_dir_without_symlinks(&source, &target)?;
        }
    }
    Ok(())
}

fn copy_dir_without_symlinks(source: &Path, target: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(target)?;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let destination = target.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir_without_symlinks(&entry.path(), &destination)?;
        } else if file_type.is_file() {
            std::fs::copy(entry.path(), destination)?;
        }
    }
    Ok(())
}

/// System skills directory, materializing the embedded skills first.
///
/// Honors `YOLOP_SYSTEM_SKILLS_DIR` (used verbatim). Otherwise the embedded
/// bundled system-skill tree is written to `<data_dir>/yolop/system-skills` and
/// that path is returned. Materialization is idempotent and concurrency-safe
/// (atomic per-file writes, skipping files already present with identical
/// bytes), so parallel processes/tests do not race. Any failure is non-fatal:
/// it logs and returns `None`, leaving the system scope unavailable.
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

/// Skill removal for yolop's writable scopes. The upstream
/// `ScopedSkillsCapability` owns discovery and `list/activate/read/write_skill`
/// but has no uninstall, so yolop contributes `delete_skill` here. It operates
/// on the same workspace/global directories the resolver exposes (system stays
/// read-only), giving the agent a first-class, conversational uninstall instead
/// of shelling out to `rm`.
pub(crate) struct SkillManagementCapability {
    dirs: SkillDirs,
    registry: crate::capabilities::skill_registry::SkillRegistryClient,
}

impl SkillManagementCapability {
    pub(crate) fn new(dirs: SkillDirs) -> Self {
        Self {
            dirs,
            registry: crate::capabilities::skill_registry::SkillRegistryClient::production(),
        }
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
        "Search skills.sh, install registry skills, and uninstall workspace/global skills."
    }
    fn status(&self) -> CapabilityStatus {
        CapabilityStatus::Available
    }
    fn category(&self) -> Option<&str> {
        Some("Examples")
    }
    fn system_prompt_addition(&self) -> Option<&str> {
        // Tool descriptions already cover search → ask → install, and that
        // system skills are read-only.
        None
    }
    fn tools(&self) -> Vec<Box<dyn Tool>> {
        vec![
            Box::new(crate::capabilities::skill_registry::SearchSkillsTool::new(
                self.registry.clone(),
            )),
            Box::new(crate::capabilities::skill_registry::InstallSkillTool::new(
                self.dirs.clone(),
                self.registry.clone(),
            )),
            Box::new(DeleteSkillTool {
                dirs: self.dirs.clone(),
            }),
        ]
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
            // Only the scopes advertised in the tool schema are accepted, so the
            // documented surface and the behavior stay in lockstep.
            "workspace" => Ok(self.dirs.workspace.clone()),
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
/// or `..`. This keeps skill install/uninstall from escaping the scope directory.
pub(crate) fn validate_skill_name(name: &str) -> Result<(), String> {
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
    fn narrate(
        &self,
        tool_call: &ToolCall,
        phase: ToolNarrationPhase,
        locale: Option<&str>,
        _ctx: everruns_core::tool_narration::ToolNarrationContext<'_>,
    ) -> Option<String> {
        let _ = locale;
        let detail = arg_str(&tool_call.arguments, &["name"]).map(|name| {
            let scope = arg_str(&tool_call.arguments, &["scope"]).unwrap_or("workspace");
            truncate(&format!("{name} ({scope})"), 48)
        });
        Some(stable_labeled("Delete skill", detail, phase))
    }

    fn name(&self) -> &str {
        "delete_skill"
    }
    fn display_name(&self) -> Option<&str> {
        Some("Delete skill")
    }
    fn description(&self) -> &str {
        "Uninstall a skill by removing its directory from a writable scope \
         (`workspace` or `global`). System skills cannot be deleted."
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
        // The stat checks and the recursive delete are blocking filesystem work;
        // run them off the async runtime so a large skill directory can't stall a
        // tokio worker. The guard messages are built here so the closure owns
        // everything it needs.
        let name_owned = name.to_string();
        let scope_owned = scope.to_string();
        let target_for_delete = target.clone();
        let outcome = tokio::task::spawn_blocking(move || -> Result<(), String> {
            if !target_for_delete.is_dir() {
                return Err(format!(
                    "no `{name_owned}` skill installed in the {scope_owned} scope"
                ));
            }
            // Guard against deleting an unrelated directory: a real skill always
            // carries a SKILL.md. Refuse anything that does not look like a skill.
            if !target_for_delete.join("SKILL.md").is_file() {
                return Err(format!(
                    "`{}` is not a skill (no SKILL.md); refusing to delete",
                    target_for_delete.display()
                ));
            }
            std::fs::remove_dir_all(&target_for_delete)
                .map_err(|err| format!("failed to delete `{name_owned}`: {err}"))
        })
        .await;
        match outcome {
            Ok(Ok(())) => ToolExecutionResult::success(json!({
                "success": true,
                "name": name,
                "scope": scope,
                "removed": target.display().to_string(),
                "message": format!("uninstalled `{name}` from the {scope} scope"),
            })),
            Ok(Err(message)) => ToolExecutionResult::tool_error(message),
            Err(join_err) => {
                ToolExecutionResult::tool_error(format!("delete task failed: {join_err}"))
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
    fn imports_legacy_global_skills_without_overwriting_primary() {
        let legacy = tempfile::tempdir().unwrap();
        let primary = tempfile::tempdir().unwrap();
        let old = legacy.path().join("old-skill");
        std::fs::create_dir_all(old.join("assets")).unwrap();
        std::fs::write(old.join("SKILL.md"), "legacy").unwrap();
        std::fs::write(old.join("assets/example.txt"), "asset").unwrap();
        let existing = primary.path().join("existing");
        std::fs::create_dir_all(&existing).unwrap();
        std::fs::write(existing.join("SKILL.md"), "primary").unwrap();
        let legacy_existing = legacy.path().join("existing");
        std::fs::create_dir_all(&legacy_existing).unwrap();
        std::fs::write(legacy_existing.join("SKILL.md"), "legacy replacement").unwrap();

        import_legacy_global_skills(legacy.path(), primary.path()).unwrap();

        assert_eq!(
            std::fs::read_to_string(primary.path().join("old-skill/SKILL.md")).unwrap(),
            "legacy"
        );
        assert_eq!(
            std::fs::read_to_string(primary.path().join("old-skill/assets/example.txt")).unwrap(),
            "asset"
        );
        assert_eq!(
            std::fs::read_to_string(existing.join("SKILL.md")).unwrap(),
            "primary"
        );
    }

    #[test]
    fn config_includes_only_resolved_scopes() {
        let dirs = SkillDirs {
            workspace: PathBuf::from("/ws/.agents/skills"),
            global: None,
            profile: None,
            system: Some(PathBuf::from("/data/sys")),
        };
        let cfg = skills_config(&dirs, false, &[]);
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
    fn profile_scope_ranks_between_workspace_and_global() {
        let dirs = SkillDirs {
            workspace: PathBuf::from("/ws/.agents/skills"),
            global: Some(PathBuf::from("/home/.agents/skills")),
            profile: Some(PathBuf::from("/cfg/yolop/profiles/triage/skills")),
            system: Some(PathBuf::from("/data/sys")),
        };
        let cfg = skills_config(&dirs, false, &[]);
        let labels: Vec<&str> = cfg.scopes.iter().map(|s| s.label.as_str()).collect();
        assert_eq!(labels, vec!["workspace", "profile", "global", "system"]);
        let scope = cfg
            .scopes
            .iter()
            .find(|s| s.label == "profile")
            .expect("profile scope");
        assert!(scope.writable, "a profile owns its skills");
        assert_eq!(scope.vfs_root, PROFILE_SKILLS_VFS);
        assert_eq!(
            PathBuf::from(cfg.resolver.skill_dir(scope, "triage-intake")),
            PathBuf::from("/cfg/yolop/profiles/triage/skills").join("triage-intake")
        );
    }

    #[test]
    fn extension_scopes_are_read_only_and_routed() {
        let dirs = SkillDirs {
            workspace: PathBuf::from("/ws/.agents/skills"),
            global: None,
            profile: None,
            system: None,
        };
        let ext = vec![(
            "git-guard".to_string(),
            PathBuf::from("/ext/git-guard/skills"),
        )];
        let cfg = skills_config(&dirs, false, &ext);
        let scope = cfg
            .scopes
            .iter()
            .find(|s| s.label == "ext:git-guard")
            .expect("extension scope present");
        assert!(!scope.writable, "extension skills are read-only");
        assert_eq!(scope.vfs_root, extension_skills_vfs("git-guard"));
        // The resolver maps the scope back to the real on-disk dir.
        assert_eq!(
            cfg.resolver.skill_dir(scope, "my-skill"),
            "/ext/git-guard/skills/my-skill"
        );
    }

    #[test]
    fn resolver_returns_real_host_paths() {
        let dirs = SkillDirs {
            workspace: PathBuf::from("/ws/.agents/skills"),
            global: Some(PathBuf::from("/cfg/yolop/skills")),
            profile: None,
            system: Some(PathBuf::from("/data/sys")),
        };
        let r = HostSkillDirResolver {
            dirs,
            extensions: Default::default(),
        };
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

        // The ast-grep and yolop skills are shipped, parse, and are user-invocable.
        assert!(
            dir_names.iter().any(|n| n == "ast-grep"),
            "ast-grep system skill is shipped: {dir_names:?}"
        );
        assert!(
            dir_names.iter().any(|n| n == "yolop"),
            "yolop system skill is shipped: {dir_names:?}"
        );
        let md = std::fs::read_to_string(dest.join("ast-grep").join("SKILL.md")).unwrap();
        let parsed = everruns_core::skill::parse_skill_md(&md).unwrap();
        assert!(parsed.user_invocable);
        assert!(
            parsed.description.to_lowercase().contains("ast-grep"),
            "description should mention ast-grep: {:?}",
            parsed.description
        );
        let yolop_md = std::fs::read_to_string(dest.join("yolop").join("SKILL.md")).unwrap();
        let yolop_parsed = everruns_core::skill::parse_skill_md(&yolop_md).unwrap();
        assert!(yolop_parsed.user_invocable);
        assert!(
            yolop_parsed.description.to_lowercase().contains("keyboard"),
            "yolop skill description should mention keyboard shortcuts: {:?}",
            yolop_parsed.description
        );

        // The okf skill teaches a format from an external spec, so pin what it
        // cites: `okf.md` is not normative and describes a superseded v0.1
        // model, and the skill's documented validator command only works if the
        // nested `scripts/` directory materializes with it.
        let okf_md = std::fs::read_to_string(dest.join("okf").join("SKILL.md")).unwrap();
        let okf_parsed = everruns_core::skill::parse_skill_md(&okf_md).unwrap();
        assert!(okf_parsed.user_invocable);
        assert!(
            okf_md.contains(
                "https://github.com/GoogleCloudPlatform/knowledge-catalog/blob/main/okf/SPEC.md"
            ),
            "okf skill must cite the authoritative spec"
        );
        assert!(
            !okf_md.contains("okf.md/spec"),
            "okf skill must not cite the non-normative okf.md site"
        );
        assert!(
            dest.join("okf")
                .join("scripts")
                .join("validate_okf.py")
                .is_file(),
            "okf skill ships its validator"
        );
    }

    // ---------- delete_skill ----------

    #[test]
    fn skill_management_capability_exposes_search_install_delete() {
        let dirs = SkillDirs {
            workspace: PathBuf::from("/ws/.agents/skills"),
            global: None,
            profile: None,
            system: None,
        };
        let capability = SkillManagementCapability::new(dirs);
        let names: Vec<String> = capability
            .tools()
            .iter()
            .map(|t| t.name().to_string())
            .collect();
        for expected in ["search_skills", "install_skill", "delete_skill"] {
            assert!(
                names.iter().any(|n| n == expected),
                "{expected} should be exposed: {names:?}"
            );
        }
        // Guidance belongs to the tools, not to a per-turn prompt block:
        // the descriptions are already in context whenever the tools are offered.
        assert!(
            capability.system_prompt_addition().is_none(),
            "skill management guidance lives in the tool descriptions"
        );
        let description = capability
            .tools()
            .into_iter()
            .find(|t| t.name() == "delete_skill")
            .expect("delete_skill")
            .description()
            .to_string();
        assert!(description.contains("workspace"));
        // The read-only system scope is the one thing the schema cannot express;
        // routing between delete/write lives in the skill-management skill, not
        // in provider-visible description bytes.
        assert!(description.contains("System skills cannot be deleted"));
    }

    /// Build a tool whose workspace/global scopes point at fresh temp dirs.
    fn delete_tool_with_dirs(workspace: &Path, global: Option<&Path>) -> DeleteSkillTool {
        DeleteSkillTool {
            dirs: SkillDirs {
                workspace: workspace.to_path_buf(),
                global: global.map(Path::to_path_buf),
                profile: None,
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

// Skills — vendored from everruns-core's `SkillsCapability`, extended to source
// skills from three real on-disk folders instead of the single session-VFS path
// the upstream capability scans.
//
// Why vendored: the upstream `SkillsCapability` discovers skills by scanning one
// path (`/.agents/skills`) through the session filesystem. Supporting global and
// system scopes that way meant overlaying extra roots into that VFS path, which
// loses scope identity and forced system skills onto disk awkwardly. yolop
// instead owns the discovery loop and reads the scope folders directly. The
// SKILL.md *parser/validator/substitution* are reused verbatim from
// `everruns_core::skill` — only discovery and the two tools are reimplemented.
//
// Scopes (precedence: workspace > global > system; discovery de-dups by skill
// directory name, so a nearer scope shadows a farther one):
//   * workspace — `<workspace>/.agents/skills`
//   * global    — `<config_dir>/yolop/skills` (override: YOLOP_GLOBAL_SKILLS_DIR)
//   * system    — pre-packed in the binary, materialized once to
//                 `<data_dir>/yolop/system-skills` (override: YOLOP_SYSTEM_SKILLS_DIR)
//
// Every scope resolves to a real directory, so `${SKILL_DIR}` in an activated
// skill points at a real path the host `bash` tool can read — bundled files work
// for all three scopes. Once this multi-source resolver proves out, it is the
// natural thing to push upstream so the overlay/vendoring can be retired (see
// `specs/skills.md`).

use async_trait::async_trait;
use everruns_core::ToolContext;
use everruns_core::capabilities::{Capability, CapabilityStatus, SystemPromptContext};
use everruns_core::skill::{
    SkillContext, expand_skill_arguments, parse_skill_md, substitute_activation_vars,
    validate_skill_name,
};
use everruns_core::tool_types::ToolHints;
use everruns_core::tools::{Tool, ToolExecutionResult};
use include_dir::{Dir, include_dir};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Capability id — kept equal to the upstream constant so the harness capability
/// wiring (`AgentCapabilityConfig::new(SKILLS_CAPABILITY_ID)`) still resolves.
pub const SKILLS_CAPABILITY_ID: &str = "skills";

/// Env override for the global skills directory.
const GLOBAL_SKILLS_DIR_ENV: &str = "YOLOP_GLOBAL_SKILLS_DIR";
/// Env override for the system skills directory (skips materialization).
const SYSTEM_SKILLS_DIR_ENV: &str = "YOLOP_SYSTEM_SKILLS_DIR";

/// Max skills listed in the system prompt (the rest are reachable via `list_skills`).
const MAX_SKILLS_IN_PROMPT: usize = 15;
/// Max description length in the system-prompt listing (truncated with "…").
const MAX_DESCRIPTION_CHARS: usize = 76;

/// System skills shipped inside the binary. The crate-root `skills/` directory is
/// embedded at compile time so a `cargo install` / Homebrew build carries them.
static SYSTEM_SKILLS: Dir<'static> = include_dir!("$CARGO_MANIFEST_DIR/skills");

const SKILLS_SYSTEM_PROMPT: &str = "Skills are reusable instruction packs sourced \
from your workspace, your global config, and ones built into yolop. Use `list_skills` \
to see what's available and `activate_skill` (by name) to load one. Only activate \
skills relevant to the current task.";

/// Where a skill came from. Surfaced to the model so it can reason about trust
/// and precedence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SkillScope {
    Workspace,
    Global,
    System,
}

impl SkillScope {
    fn label(self) -> &'static str {
        match self {
            SkillScope::Workspace => "workspace",
            SkillScope::Global => "global",
            SkillScope::System => "system",
        }
    }
}

/// The ordered set of skill folders for a session, highest precedence first.
pub struct SkillSources {
    roots: Vec<(SkillScope, PathBuf)>,
}

/// One discovered skill, after parsing its `SKILL.md`.
struct DiscoveredSkill {
    scope: SkillScope,
    /// Directory name — the identifier used to activate the skill.
    dir_name: String,
    dir_path: PathBuf,
    /// `None` when `SKILL.md` failed to parse (carried in `error` instead).
    name: String,
    description: String,
    version: String,
    user_invocable: bool,
    disable_model_invocation: bool,
    error: Option<String>,
}

impl SkillSources {
    /// Resolve the workspace/global/system folders for `workspace_root`.
    pub fn resolve(workspace_root: &Path) -> Self {
        Self::from_dirs(
            Some(workspace_root.join(".agents").join("skills")),
            global_skills_dir(),
            system_skills_dir(),
        )
    }

    /// Build from explicit per-scope directories (used by `resolve` and tests).
    /// Each is included only when it is an existing directory.
    pub(crate) fn from_dirs(
        workspace: Option<PathBuf>,
        global: Option<PathBuf>,
        system: Option<PathBuf>,
    ) -> Self {
        let mut roots = Vec::new();
        for (scope, dir) in [
            (SkillScope::Workspace, workspace),
            (SkillScope::Global, global),
            (SkillScope::System, system),
        ] {
            if let Some(dir) = dir.filter(|p| p.is_dir()) {
                roots.push((scope, dir));
            }
        }
        Self { roots }
    }

    /// Discover every unique skill across scopes, in precedence order. The first
    /// occurrence of a directory name wins; later (farther-scope) duplicates are
    /// dropped.
    fn discover(&self) -> Vec<DiscoveredSkill> {
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        for (scope, root) in &self.roots {
            for dir_name in skill_dir_names(root) {
                if !seen.insert(dir_name.clone()) {
                    continue;
                }
                let dir_path = root.join(&dir_name);
                let content =
                    std::fs::read_to_string(dir_path.join("SKILL.md")).unwrap_or_default();
                let discovered = match parse_skill_md(&content) {
                    Ok(parsed) => DiscoveredSkill {
                        scope: *scope,
                        dir_name: dir_name.clone(),
                        dir_path,
                        name: parsed.name,
                        description: parsed.description,
                        version: parsed.version,
                        user_invocable: parsed.user_invocable,
                        disable_model_invocation: parsed.disable_model_invocation,
                        error: None,
                    },
                    Err(errors) => DiscoveredSkill {
                        scope: *scope,
                        name: dir_name.clone(),
                        dir_path,
                        description: String::new(),
                        version: String::new(),
                        user_invocable: false,
                        disable_model_invocation: false,
                        error: Some(format!("Invalid SKILL.md: {}", errors.join(", "))),
                        dir_name,
                    },
                };
                out.push(discovered);
            }
        }
        out
    }

    /// Locate the activatable skill named `dir_name`, honoring precedence.
    /// Returns the scope, the skill directory, and the raw `SKILL.md` content.
    fn locate(&self, dir_name: &str) -> Option<(SkillScope, PathBuf, String)> {
        for (scope, root) in &self.roots {
            let dir_path = root.join(dir_name);
            let md_path = dir_path.join("SKILL.md");
            if let Ok(content) = std::fs::read_to_string(&md_path) {
                return Some((*scope, dir_path, content));
            }
        }
        None
    }
}

/// Immediate subdirectories of `root` that contain a `SKILL.md`, sorted for
/// deterministic ordering.
fn skill_dir_names(root: &Path) -> Vec<String> {
    let mut names = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return names;
    };
    for entry in entries.flatten() {
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        if !entry.path().join("SKILL.md").is_file() {
            continue;
        }
        if let Ok(name) = entry.file_name().into_string() {
            names.push(name);
        }
    }
    names.sort();
    names
}

fn truncate_description(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let truncated: String = s.chars().take(max_chars.saturating_sub(1)).collect();
    format!("{}…", truncated.trim_end())
}

// ============================================================================
// Capability
// ============================================================================

/// yolop's skills capability: discovers + activates skills across the workspace,
/// global, and system scopes.
pub struct YolopSkillsCapability {
    sources: Arc<SkillSources>,
}

impl YolopSkillsCapability {
    pub fn new(sources: SkillSources) -> Self {
        Self {
            sources: Arc::new(sources),
        }
    }
}

#[async_trait]
impl Capability for YolopSkillsCapability {
    fn id(&self) -> &str {
        SKILLS_CAPABILITY_ID
    }

    fn name(&self) -> &str {
        "Agent Skills"
    }

    fn description(&self) -> &str {
        "Discover and activate skills from the workspace, global config, and built-in (system) scopes."
    }

    fn status(&self) -> CapabilityStatus {
        CapabilityStatus::Available
    }

    fn icon(&self) -> Option<&str> {
        Some("wand")
    }

    fn category(&self) -> Option<&str> {
        Some("Skills")
    }

    fn system_prompt_addition(&self) -> Option<&str> {
        Some(SKILLS_SYSTEM_PROMPT)
    }

    async fn system_prompt_contribution(&self, _ctx: &SystemPromptContext) -> Option<String> {
        let mut prompt = String::from(SKILLS_SYSTEM_PROMPT);

        // Skills the model may auto-invoke (i.e. not opted out).
        let visible: Vec<DiscoveredSkill> = self
            .sources
            .discover()
            .into_iter()
            .filter(|s| s.error.is_none() && !s.disable_model_invocation)
            .collect();

        if !visible.is_empty() {
            prompt.push_str("\n\nAvailable skills:\n");
            for skill in visible.iter().take(MAX_SKILLS_IN_PROMPT) {
                let desc = truncate_description(&skill.description, MAX_DESCRIPTION_CHARS);
                let invocable = if skill.user_invocable {
                    format!(" (/{})", skill.name)
                } else {
                    String::new()
                };
                prompt.push_str(&format!(
                    "- **{}** [{}]: {}{}\n",
                    skill.name,
                    skill.scope.label(),
                    desc,
                    invocable
                ));
            }
            if visible.len() > MAX_SKILLS_IN_PROMPT {
                prompt.push_str(&format!(
                    "\n({} more — use `list_skills` to see all)\n",
                    visible.len() - MAX_SKILLS_IN_PROMPT
                ));
            }
        }

        Some(format!(
            "<capability id=\"{}\">\n{}\n</capability>",
            self.id(),
            prompt
        ))
    }

    fn tools(&self) -> Vec<Box<dyn Tool>> {
        vec![
            Box::new(ListSkillsTool {
                sources: self.sources.clone(),
            }),
            Box::new(ActivateSkillTool {
                sources: self.sources.clone(),
            }),
        ]
    }
}

// ============================================================================
// list_skills
// ============================================================================

struct ListSkillsTool {
    sources: Arc<SkillSources>,
}

#[async_trait]
impl Tool for ListSkillsTool {
    fn name(&self) -> &str {
        "list_skills"
    }

    fn display_name(&self) -> Option<&str> {
        Some("List Skills")
    }

    fn description(&self) -> &str {
        "Discover available skills across the workspace, global, and system scopes. \
         Returns each skill's name, description, and scope."
    }

    fn parameters_schema(&self) -> Value {
        json!({ "type": "object", "properties": {}, "required": [] })
    }

    fn hints(&self) -> ToolHints {
        ToolHints::default()
            .with_readonly(true)
            .with_idempotent(true)
    }

    async fn execute(&self, _arguments: Value) -> ToolExecutionResult {
        let skills: Vec<Value> = self
            .sources
            .discover()
            .into_iter()
            .map(|s| match s.error {
                Some(error) => json!({
                    "name": s.dir_name,
                    "scope": s.scope.label(),
                    "path": s.dir_path.join("SKILL.md").display().to_string(),
                    "error": error,
                }),
                None => json!({
                    "name": s.name,
                    "description": s.description,
                    "scope": s.scope.label(),
                    "version": s.version,
                    "user_invocable": s.user_invocable,
                    "disable_model_invocation": s.disable_model_invocation,
                    "path": s.dir_path.join("SKILL.md").display().to_string(),
                }),
            })
            .collect();

        ToolExecutionResult::success(json!({
            "count": skills.len(),
            "skills": skills,
        }))
    }
}

// ============================================================================
// activate_skill
// ============================================================================

struct ActivateSkillTool {
    sources: Arc<SkillSources>,
}

#[async_trait]
impl Tool for ActivateSkillTool {
    fn name(&self) -> &str {
        "activate_skill"
    }

    fn display_name(&self) -> Option<&str> {
        Some("Activate Skill")
    }

    fn description(&self) -> &str {
        "Activate a skill by name to load its full instructions. Skills are resolved \
         across the workspace, global, and system scopes (workspace wins on conflict)."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "The skill directory name (e.g., 'joke')"
                },
                "arguments": {
                    "type": "string",
                    "description": "Optional arguments to pass to the skill for $ARGUMENTS substitution"
                }
            },
            "required": ["name"]
        })
    }

    fn hints(&self) -> ToolHints {
        ToolHints::default()
            .with_readonly(true)
            .with_idempotent(true)
    }

    fn requires_context(&self) -> bool {
        // Only for the `${SESSION_ID}` activation variable; discovery itself reads
        // real folders and needs no session context.
        true
    }

    async fn execute(&self, _arguments: Value) -> ToolExecutionResult {
        ToolExecutionResult::tool_error(
            "activate_skill requires session context and cannot run standalone.",
        )
    }

    async fn execute_with_context(
        &self,
        arguments: Value,
        context: &ToolContext,
    ) -> ToolExecutionResult {
        let Some(name) = arguments.get("name").and_then(|v| v.as_str()) else {
            return ToolExecutionResult::tool_error("Missing required parameter: name");
        };
        let skill_args = arguments
            .get("arguments")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        // Reject path separators up front, then enforce the full skill-name rules
        // (lowercase, digits, single hyphens, 1-64 chars).
        if name.contains("..") || name.contains('/') || name.contains('\\') {
            return ToolExecutionResult::tool_error(
                "Invalid skill name. Must be a simple directory name without path separators.",
            );
        }
        if let Err(errors) = validate_skill_name(name) {
            return ToolExecutionResult::tool_error(format!(
                "Invalid skill name '{name}': {}",
                errors.join(", ")
            ));
        }

        let Some((scope, dir_path, content)) = self.sources.locate(name) else {
            return ToolExecutionResult::tool_error(format!(
                "Skill '{name}' not found in any scope. Use list_skills to see what's available."
            ));
        };

        match parse_skill_md(&content) {
            Ok(parsed) => {
                // Substitution pipeline: arguments ($ARGUMENTS, $N) then activation
                // vars (${SESSION_ID}, ${SKILL_DIR}). `${SKILL_DIR}` is the real
                // on-disk skill folder, so bundled files are reachable via `bash`.
                //
                // Command injection (!`cmd`) is intentionally NOT expanded: yolop
                // mirrors upstream's conservative trust gate (see everruns-core
                // skills.rs / EVE-388) — activating a skill must never spawn a
                // shell on the host.
                let expanded = expand_skill_arguments(&parsed.instructions, skill_args);
                let substituted = substitute_activation_vars(
                    &expanded,
                    &context.session_id.to_string(),
                    &dir_path.display().to_string(),
                );
                let instructions = format!(
                    "<skill name=\"{}\">\n{}\n</skill>",
                    parsed.name, substituted
                );

                let mut result = json!({
                    "skill": parsed.name,
                    "scope": scope.label(),
                    "description": parsed.description,
                    "instructions": instructions,
                });
                if parsed.context == SkillContext::Fork {
                    result["context"] = json!("fork");
                    result["agent"] = json!(parsed.agent.as_deref().unwrap_or("general-purpose"));
                    if let Some(model) = &parsed.model {
                        result["model"] = json!(model);
                    }
                }
                ToolExecutionResult::success(result)
            }
            Err(errors) => ToolExecutionResult::tool_error(format!(
                "Invalid SKILL.md for '{name}': {}",
                errors.join(", ")
            )),
        }
    }
}

// ============================================================================
// Scope folder resolution + system-skill materialization
// ============================================================================

/// Global skills directory, or `None` when it does not exist.
/// Honors `YOLOP_GLOBAL_SKILLS_DIR`; otherwise `<config_dir>/yolop/skills`.
pub fn global_skills_dir() -> Option<PathBuf> {
    let dir = match std::env::var(GLOBAL_SKILLS_DIR_ENV) {
        Ok(value) if !value.is_empty() => PathBuf::from(value),
        _ => dirs::config_dir()?.join("yolop").join("skills"),
    };
    dir.is_dir().then_some(dir)
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

    fn write_skill(root: &Path, dir: &str, name: &str, description: &str) {
        let d = root.join(dir);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {description}\n---\n\n# {name}\nbody\n"),
        )
        .unwrap();
    }

    #[test]
    fn discover_merges_scopes_and_dedups_by_precedence() {
        let ws = tempfile::tempdir().unwrap();
        let gl = tempfile::tempdir().unwrap();
        let sy = tempfile::tempdir().unwrap();
        write_skill(ws.path(), "ws-only", "ws-only", "workspace skill");
        write_skill(ws.path(), "shared", "shared", "from workspace");
        write_skill(gl.path(), "global-only", "global-only", "global skill");
        write_skill(gl.path(), "shared", "shared", "from global");
        write_skill(sy.path(), "system-only", "system-only", "system skill");

        let sources = SkillSources::from_dirs(
            Some(ws.path().into()),
            Some(gl.path().into()),
            Some(sy.path().into()),
        );
        let found = sources.discover();
        let names: std::collections::HashSet<&str> =
            found.iter().map(|s| s.dir_name.as_str()).collect();
        assert!(names.contains("ws-only"));
        assert!(names.contains("global-only"));
        assert!(names.contains("system-only"));

        // `shared` resolves once, to the workspace scope.
        let shared: Vec<&DiscoveredSkill> =
            found.iter().filter(|s| s.dir_name == "shared").collect();
        assert_eq!(shared.len(), 1);
        assert_eq!(shared[0].scope, SkillScope::Workspace);
        assert_eq!(shared[0].description, "from workspace");

        let system = found.iter().find(|s| s.dir_name == "system-only").unwrap();
        assert_eq!(system.scope, SkillScope::System);
    }

    #[test]
    fn locate_prefers_workspace_then_falls_through() {
        let ws = tempfile::tempdir().unwrap();
        let gl = tempfile::tempdir().unwrap();
        write_skill(ws.path(), "shared", "shared", "from workspace");
        write_skill(gl.path(), "shared", "shared", "from global");
        write_skill(gl.path(), "global-only", "global-only", "global skill");
        let sources = SkillSources::from_dirs(Some(ws.path().into()), Some(gl.path().into()), None);

        let (scope, _dir, content) = sources.locate("shared").unwrap();
        assert_eq!(scope, SkillScope::Workspace);
        assert!(content.contains("from workspace"));

        let (scope, _dir, _content) = sources.locate("global-only").unwrap();
        assert_eq!(scope, SkillScope::Global);

        assert!(sources.locate("missing").is_none());
    }

    #[tokio::test]
    async fn activate_substitutes_skill_dir_to_real_path() {
        let gl = tempfile::tempdir().unwrap();
        let d = gl.path().join("bundled");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join("SKILL.md"),
            "---\nname: bundled\ndescription: uses a bundled file\n---\n\nRead ${SKILL_DIR}/data.txt\n",
        )
        .unwrap();

        let sources = SkillSources::from_dirs(None, Some(gl.path().into()), None);
        let tool = ActivateSkillTool {
            sources: Arc::new(sources),
        };
        let ctx = ToolContext::new(everruns_core::typed_id::SessionId::new());
        let result = tool
            .execute_with_context(json!({"name": "bundled"}), &ctx)
            .await;
        match result {
            ToolExecutionResult::Success(v) => {
                assert_eq!(v["scope"], "global");
                let instructions = v["instructions"].as_str().unwrap();
                // ${SKILL_DIR} expanded to the real on-disk directory.
                assert!(instructions.contains(&d.display().to_string()));
            }
            other => panic!("expected success, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn activate_rejects_traversal_and_missing() {
        let sources = SkillSources::from_dirs(None, None, None);
        let tool = ActivateSkillTool {
            sources: Arc::new(sources),
        };
        let ctx = ToolContext::new(everruns_core::typed_id::SessionId::new());

        let bad = tool
            .execute_with_context(json!({"name": "../etc"}), &ctx)
            .await;
        assert!(matches!(bad, ToolExecutionResult::ToolError(_)));

        let missing = tool
            .execute_with_context(json!({"name": "nope"}), &ctx)
            .await;
        match missing {
            ToolExecutionResult::ToolError(m) => assert!(m.contains("not found")),
            other => panic!("expected ToolError, got {other:?}"),
        }
    }

    #[test]
    fn embedded_system_skills_materialize_idempotently() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("system-skills");
        materialize_system_skills(&dest).unwrap();
        let skill = dest.join("joke").join("SKILL.md");
        assert!(skill.is_file());
        let first = std::fs::read(&skill).unwrap();
        // Second pass is a no-op write (identical bytes) and leaves content intact.
        materialize_system_skills(&dest).unwrap();
        assert_eq!(std::fs::read(&skill).unwrap(), first);
    }

    #[test]
    fn global_skills_dir_respects_env_override() {
        // Serialize against every other env-mutating test in this binary; the
        // guard is held for the whole window the var is set (set_var/remove_var
        // are not thread-safe — see crate::test_env).
        let _guard = crate::test_env::lock();
        let tmp = tempfile::tempdir().unwrap();
        // SAFETY: the test_env lock guarantees no other thread touches the
        // process environment for the duration of this test.
        unsafe { std::env::set_var(GLOBAL_SKILLS_DIR_ENV, tmp.path()) };
        let resolved = global_skills_dir();
        unsafe { std::env::remove_var(GLOBAL_SKILLS_DIR_ENV) };
        assert_eq!(resolved.as_deref(), Some(tmp.path()));
    }

    #[tokio::test]
    async fn list_skills_reports_scope_and_error_entries() {
        let ws = tempfile::tempdir().unwrap();
        let gl = tempfile::tempdir().unwrap();
        write_skill(ws.path(), "good", "good", "a fine skill");
        // A directory with a SKILL.md that fails to parse (missing frontmatter
        // fields) is surfaced as an error entry, not silently dropped.
        let broken = gl.path().join("broken");
        std::fs::create_dir_all(&broken).unwrap();
        std::fs::write(broken.join("SKILL.md"), "no frontmatter here").unwrap();

        let tool = ListSkillsTool {
            sources: Arc::new(SkillSources::from_dirs(
                Some(ws.path().into()),
                Some(gl.path().into()),
                None,
            )),
        };
        let ToolExecutionResult::Success(v) = tool.execute(json!({})).await else {
            panic!("expected success");
        };
        assert_eq!(v["count"], 2);
        let skills = v["skills"].as_array().unwrap();

        let good = skills.iter().find(|s| s["name"] == "good").unwrap();
        assert_eq!(good["scope"], "workspace");
        assert_eq!(good["description"], "a fine skill");
        assert!(good.get("error").is_none());

        let broken = skills.iter().find(|s| s["name"] == "broken").unwrap();
        assert_eq!(broken["scope"], "global");
        assert!(
            broken["error"]
                .as_str()
                .unwrap()
                .contains("Invalid SKILL.md")
        );
    }

    #[tokio::test]
    async fn system_prompt_lists_visible_skills_and_filters_opted_out() {
        let ws = tempfile::tempdir().unwrap();
        write_skill(ws.path(), "shown", "shown", "appears in the prompt");
        // disable-model-invocation skills are reachable via the tool but kept
        // out of the model-facing prompt listing.
        let hidden = ws.path().join("hidden");
        std::fs::create_dir_all(&hidden).unwrap();
        std::fs::write(
            hidden.join("SKILL.md"),
            "---\nname: hidden\ndescription: not auto-listed\ndisable-model-invocation: true\n---\n\nbody\n",
        )
        .unwrap();

        let cap =
            YolopSkillsCapability::new(SkillSources::from_dirs(Some(ws.path().into()), None, None));
        let ctx =
            SystemPromptContext::without_file_store(everruns_core::typed_id::SessionId::new());
        let prompt = cap.system_prompt_contribution(&ctx).await.unwrap();

        assert!(prompt.contains("**shown** [workspace]"));
        assert!(!prompt.contains("hidden"));
    }
}

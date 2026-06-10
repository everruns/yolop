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
use std::ffi::OsString;
use std::io::Write;
use std::path::Component;
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
/// Defensive cap for extra files installed with a skill.
const MAX_EXTRA_SKILL_FILES: usize = 64;
/// Defensive cap for one skill file body.
const MAX_SKILL_FILE_BYTES: usize = 1024 * 1024;

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
    /// Missing directories are kept so a skill installed later in the same
    /// process becomes discoverable on the next `list_skills`/`activate_skill`
    /// call without restarting yolop.
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
            if let Some(dir) = dir {
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

    fn root_for(&self, requested: SkillWriteScope) -> Option<PathBuf> {
        let wanted = requested.skill_scope();
        self.roots
            .iter()
            .find_map(|(scope, root)| (*scope == wanted).then(|| root.clone()))
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
            Box::new(ReadSkillTool {
                sources: self.sources.clone(),
            }),
            Box::new(WriteSkillTool {
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
// read_skill
// ============================================================================

struct ReadSkillTool {
    sources: Arc<SkillSources>,
}

#[async_trait]
impl Tool for ReadSkillTool {
    fn name(&self) -> &str {
        "read_skill"
    }

    fn display_name(&self) -> Option<&str> {
        Some("Read Skill")
    }

    fn description(&self) -> &str {
        "Read one installed skill's SKILL.md and file manifest. Use before modifying \
         or upgrading a workspace/global skill."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "The skill directory name."
                },
                "scope": {
                    "type": "string",
                    "enum": ["workspace", "local", "global", "system"],
                    "description": "Optional scope to read from. Omit to use normal precedence."
                }
            },
            "required": ["name"],
            "additionalProperties": false
        })
    }

    fn hints(&self) -> ToolHints {
        ToolHints::default()
            .with_readonly(true)
            .with_idempotent(true)
    }

    async fn execute(&self, arguments: Value) -> ToolExecutionResult {
        let Some(name) = arguments.get("name").and_then(|v| v.as_str()) else {
            return ToolExecutionResult::tool_error("Missing required parameter: name");
        };
        if let Err(e) = validate_requested_skill_name(name) {
            return ToolExecutionResult::tool_error(e);
        }

        let located = match arguments.get("scope").and_then(|v| v.as_str()) {
            Some(raw) => {
                let Some(scope) = SkillReadScope::parse(raw) else {
                    return ToolExecutionResult::tool_error(
                        "'scope' must be one of workspace, local, global, or system",
                    );
                };
                self.sources
                    .roots
                    .iter()
                    .find(|(candidate, _)| *candidate == scope.skill_scope())
                    .and_then(|(scope, root)| {
                        let dir_path = root.join(name);
                        std::fs::read_to_string(dir_path.join("SKILL.md"))
                            .ok()
                            .map(|content| (*scope, dir_path, content))
                    })
            }
            None => self.sources.locate(name),
        };

        let Some((scope, dir_path, content)) = located else {
            return ToolExecutionResult::tool_error(format!(
                "Skill '{name}' not found. Use list_skills to see what's available."
            ));
        };

        ToolExecutionResult::success(json!({
            "name": name,
            "scope": scope.label(),
            "path": dir_path.join("SKILL.md").display().to_string(),
            "skill_md": content,
            "files": skill_file_manifest(&dir_path),
        }))
    }
}

// ============================================================================
// write_skill
// ============================================================================

struct WriteSkillTool {
    sources: Arc<SkillSources>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SkillWriteScope {
    Workspace,
    Global,
}

impl SkillWriteScope {
    fn parse(raw: &str) -> Option<Self> {
        match raw {
            "workspace" | "local" => Some(Self::Workspace),
            "global" => Some(Self::Global),
            _ => None,
        }
    }

    fn skill_scope(self) -> SkillScope {
        match self {
            Self::Workspace => SkillScope::Workspace,
            Self::Global => SkillScope::Global,
        }
    }

    fn label(self) -> &'static str {
        self.skill_scope().label()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SkillReadScope {
    Workspace,
    Global,
    System,
}

impl SkillReadScope {
    fn parse(raw: &str) -> Option<Self> {
        match raw {
            "workspace" | "local" => Some(Self::Workspace),
            "global" => Some(Self::Global),
            "system" => Some(Self::System),
            _ => None,
        }
    }

    fn skill_scope(self) -> SkillScope {
        match self {
            Self::Workspace => SkillScope::Workspace,
            Self::Global => SkillScope::Global,
            Self::System => SkillScope::System,
        }
    }
}

#[async_trait]
impl Tool for WriteSkillTool {
    fn name(&self) -> &str {
        "write_skill"
    }

    fn display_name(&self) -> Option<&str> {
        Some("Write Skill")
    }

    fn description(&self) -> &str {
        "Install or update a skill in the current workspace or global yolop config. \
         Provide the full SKILL.md and optional bundled files. Use this to recreate \
         skills from a registry/GitHub source without requiring npx."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "scope": {
                    "type": "string",
                    "enum": ["workspace", "local", "global"],
                    "description": "Where to write the skill. 'local' is an alias for 'workspace'."
                },
                "name": {
                    "type": "string",
                    "description": "Skill directory name. Must match the SKILL.md frontmatter name."
                },
                "skill_md": {
                    "type": "string",
                    "description": "The complete SKILL.md contents, including frontmatter."
                },
                "files": {
                    "type": "object",
                    "description": "Optional bundled files to write under the skill directory, keyed by relative path. Do not include SKILL.md here.",
                    "additionalProperties": { "type": "string" }
                },
                "overwrite": {
                    "type": "boolean",
                    "description": "Whether to replace existing files. Defaults to true."
                }
            },
            "required": ["scope", "name", "skill_md"],
            "additionalProperties": false
        })
    }

    fn hints(&self) -> ToolHints {
        ToolHints::default().with_idempotent(true)
    }

    async fn execute(&self, arguments: Value) -> ToolExecutionResult {
        let Some(scope_raw) = arguments.get("scope").and_then(|v| v.as_str()) else {
            return ToolExecutionResult::tool_error("'scope' is required");
        };
        let Some(scope) = SkillWriteScope::parse(scope_raw) else {
            return ToolExecutionResult::tool_error(
                "'scope' must be one of workspace, local, or global",
            );
        };
        let Some(name) = arguments.get("name").and_then(|v| v.as_str()) else {
            return ToolExecutionResult::tool_error("'name' is required");
        };
        if let Err(e) = validate_requested_skill_name(name) {
            return ToolExecutionResult::tool_error(e);
        }
        let Some(skill_md) = arguments.get("skill_md").and_then(|v| v.as_str()) else {
            return ToolExecutionResult::tool_error("'skill_md' is required");
        };
        if skill_md.len() > MAX_SKILL_FILE_BYTES {
            return ToolExecutionResult::tool_error(format!(
                "'skill_md' exceeds the {MAX_SKILL_FILE_BYTES} byte limit"
            ));
        }

        let parsed = match parse_skill_md(skill_md) {
            Ok(parsed) => parsed,
            Err(errors) => {
                return ToolExecutionResult::tool_error(format!(
                    "Invalid SKILL.md: {}",
                    errors.join(", ")
                ));
            }
        };
        if parsed.name != name {
            return ToolExecutionResult::tool_error(format!(
                "'name' must match SKILL.md frontmatter name (got '{}')",
                parsed.name
            ));
        }

        let Some(root) = self.sources.root_for(scope) else {
            return ToolExecutionResult::tool_error(format!(
                "{} skills directory is unavailable on this host",
                scope.label()
            ));
        };
        let skill_dir = root.join(name);
        let overwrite = arguments
            .get("overwrite")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        let files = match collect_extra_files(arguments.get("files")) {
            Ok(files) => files,
            Err(e) => return ToolExecutionResult::tool_error(e),
        };

        if !overwrite && skill_dir.exists() {
            return ToolExecutionResult::tool_error(format!(
                "Skill '{}' already exists in {} scope; pass overwrite=true to update it",
                name,
                scope.label()
            ));
        }

        let write_result = (|| -> std::io::Result<()> {
            prepare_skill_dir(&root, &skill_dir)?;
            std::fs::create_dir_all(&skill_dir)?;
            save_skill_file(&skill_dir.join("SKILL.md"), skill_md)?;
            for (rel, content) in &files {
                let target = checked_skill_file_path(&skill_dir, rel)?;
                save_skill_file(&target, content)?;
            }
            Ok(())
        })();

        match write_result {
            Ok(()) => ToolExecutionResult::success(json!({
                "ok": true,
                "name": name,
                "scope": scope.label(),
                "path": skill_dir.join("SKILL.md").display().to_string(),
                "files_written": files.len() + 1,
                "message": "skill written; it is discoverable immediately via list_skills and activate_skill",
            })),
            Err(e) => ToolExecutionResult::tool_error(format!("could not write skill: {e}")),
        }
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

fn validate_requested_skill_name(name: &str) -> Result<(), String> {
    if name.contains("..") || name.contains('/') || name.contains('\\') {
        return Err(
            "Invalid skill name. Must be a simple directory name without path separators."
                .to_string(),
        );
    }
    validate_skill_name(name)
        .map_err(|errors| format!("Invalid skill name '{name}': {}", errors.join(", ")))
}

fn collect_extra_files(value: Option<&Value>) -> Result<Vec<(PathBuf, String)>, String> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let Some(map) = value.as_object() else {
        return Err("'files' must be an object of relative path to string content".to_string());
    };
    if map.len() > MAX_EXTRA_SKILL_FILES {
        return Err(format!(
            "'files' contains too many entries (max {MAX_EXTRA_SKILL_FILES})"
        ));
    }

    let mut out = Vec::with_capacity(map.len());
    for (raw_path, value) in map {
        let Some(content) = value.as_str() else {
            return Err(format!("file '{raw_path}' content must be a string"));
        };
        if content.len() > MAX_SKILL_FILE_BYTES {
            return Err(format!(
                "file '{raw_path}' exceeds the {MAX_SKILL_FILE_BYTES} byte limit"
            ));
        }
        let rel = validate_skill_file_path(raw_path)?;
        out.push((rel, content.to_string()));
    }
    Ok(out)
}

fn validate_skill_file_path(raw: &str) -> Result<PathBuf, String> {
    if raw.trim().is_empty() || raw.contains('\\') {
        return Err(format!("invalid skill file path '{raw}'"));
    }
    let path = PathBuf::from(raw);
    if path == Path::new("SKILL.md") || path.is_absolute() {
        return Err(format!("invalid skill file path '{raw}'"));
    }
    for component in path.components() {
        match component {
            Component::Normal(_) => {}
            _ => return Err(format!("invalid skill file path '{raw}'")),
        }
    }
    Ok(path)
}

fn save_skill_file(path: &Path, content: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if let Ok(existing) = std::fs::read_to_string(path)
        && existing == content
    {
        return Ok(());
    }

    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path.file_name().unwrap_or_default();
    static TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut tmp_name = OsString::from(".");
    tmp_name.push(file_name);
    tmp_name.push(format!(".tmp.{}-{seq}", std::process::id()));
    let tmp_path = parent.join(tmp_name);
    let write_result = (|| -> std::io::Result<()> {
        let mut file = std::fs::File::create(&tmp_path)?;
        file.write_all(content.as_bytes())?;
        file.sync_all()?;
        Ok(())
    })();
    if let Err(e) = write_result {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(e);
    }
    std::fs::rename(&tmp_path, path)
}

fn prepare_skill_dir(root: &Path, skill_dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(root)?;
    reject_symlink(skill_dir)?;
    std::fs::create_dir_all(skill_dir)?;

    let root = root.canonicalize()?;
    let skill_dir = skill_dir.canonicalize()?;
    if !skill_dir.starts_with(&root) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "skill directory resolves outside the configured skills root",
        ));
    }
    Ok(())
}

fn checked_skill_file_path(skill_dir: &Path, rel: &Path) -> std::io::Result<PathBuf> {
    let target = skill_dir.join(rel);
    reject_symlink(&target)?;
    let parent = target.parent().unwrap_or(skill_dir);
    std::fs::create_dir_all(parent)?;

    let skill_dir = skill_dir.canonicalize()?;
    let parent = parent.canonicalize()?;
    if !parent.starts_with(&skill_dir) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "skill file path resolves outside the skill directory",
        ));
    }
    Ok(target)
}

fn reject_symlink(path: &Path) -> std::io::Result<()> {
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return Ok(());
    };
    if metadata.file_type().is_symlink() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("refusing to write through symlink {}", path.display()),
        ));
    }
    Ok(())
}

fn skill_file_manifest(dir_path: &Path) -> Vec<Value> {
    let mut out = Vec::new();
    collect_skill_manifest_entries(dir_path, dir_path, &mut out);
    out.sort_by_key(|entry| {
        entry
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    });
    out
}

fn collect_skill_manifest_entries(root: &Path, dir: &Path, out: &mut Vec<Value>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            collect_skill_manifest_entries(root, &path, out);
            continue;
        }
        if !file_type.is_file() {
            continue;
        }
        let Ok(rel) = path.strip_prefix(root) else {
            continue;
        };
        out.push(json!({
            "path": rel.display().to_string(),
            "bytes": entry.metadata().map(|m| m.len()).unwrap_or(0),
        }));
    }
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
    async fn global_skill_created_after_sources_are_built_is_listed() {
        let tmp = tempfile::tempdir().unwrap();
        let missing_global = tmp.path().join("global-skills");
        let tool = ListSkillsTool {
            sources: Arc::new(SkillSources::from_dirs(
                None,
                Some(missing_global.clone()),
                None,
            )),
        };

        let ToolExecutionResult::Success(v) = tool.execute(json!({})).await else {
            panic!("expected success");
        };
        assert_eq!(v["count"], 0);

        write_skill(
            &missing_global,
            "hot-install",
            "hot-install",
            "installed after startup",
        );
        let ToolExecutionResult::Success(v) = tool.execute(json!({})).await else {
            panic!("expected success");
        };
        assert_eq!(v["count"], 1);
        assert_eq!(v["skills"][0]["name"], "hot-install");
        assert_eq!(v["skills"][0]["scope"], "global");
    }

    #[tokio::test]
    async fn write_skill_installs_global_skill_readable_without_restart() {
        let ws = tempfile::tempdir().unwrap();
        let gl = tempfile::tempdir().unwrap();
        let sources = Arc::new(SkillSources::from_dirs(
            Some(ws.path().into()),
            Some(gl.path().into()),
            None,
        ));
        let writer = WriteSkillTool {
            sources: sources.clone(),
        };
        let skill_md =
            "---\nname: new-skill\ndescription: installed by tool\n---\n\n# New\nUse data.txt\n";

        let res = writer
            .execute(json!({
                "scope": "global",
                "name": "new-skill",
                "skill_md": skill_md,
                "files": {
                    "data.txt": "hello"
                }
            }))
            .await;
        assert!(res.is_success(), "got: {res:?}");
        assert_eq!(
            std::fs::read_to_string(gl.path().join("new-skill").join("SKILL.md")).unwrap(),
            skill_md
        );

        let reader = ReadSkillTool {
            sources: sources.clone(),
        };
        let ToolExecutionResult::Success(v) = reader.execute(json!({"name": "new-skill"})).await
        else {
            panic!("expected read success");
        };
        assert_eq!(v["scope"], "global");
        assert_eq!(v["skill_md"], skill_md);
        assert_eq!(v["files"][0]["path"], "SKILL.md");

        let lister = ListSkillsTool { sources };
        let ToolExecutionResult::Success(v) = lister.execute(json!({})).await else {
            panic!("expected list success");
        };
        let skill = v["skills"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["name"] == "new-skill")
            .unwrap();
        assert_eq!(skill["scope"], "global");
    }

    #[tokio::test]
    async fn write_skill_rejects_unsafe_paths_and_readonly_scopes() {
        let ws = tempfile::tempdir().unwrap();
        let writer = WriteSkillTool {
            sources: Arc::new(SkillSources::from_dirs(Some(ws.path().into()), None, None)),
        };
        let skill_md = "---\nname: safe-skill\ndescription: safe\n---\n\nbody\n";

        let system = writer
            .execute(json!({
                "scope": "system",
                "name": "safe-skill",
                "skill_md": skill_md,
            }))
            .await;
        assert!(system.is_error());

        let traversal = writer
            .execute(json!({
                "scope": "workspace",
                "name": "safe-skill",
                "skill_md": skill_md,
                "files": {
                    "../escape.txt": "nope"
                }
            }))
            .await;
        assert!(traversal.is_error());
        assert!(!ws.path().join("escape.txt").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn write_skill_rejects_symlinked_skill_dir() {
        use std::os::unix::fs::symlink;

        let ws = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        symlink(outside.path(), ws.path().join("linked-skill")).unwrap();
        let writer = WriteSkillTool {
            sources: Arc::new(SkillSources::from_dirs(Some(ws.path().into()), None, None)),
        };
        let skill_md = "---\nname: linked-skill\ndescription: linked\n---\n\nbody\n";

        let res = writer
            .execute(json!({
                "scope": "workspace",
                "name": "linked-skill",
                "skill_md": skill_md,
            }))
            .await;

        assert!(res.is_error(), "got: {res:?}");
        assert!(!outside.path().join("SKILL.md").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn write_skill_rejects_symlinked_extra_file_parent() {
        use std::os::unix::fs::symlink;

        let ws = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let skill_dir = ws.path().join("safe-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();
        symlink(outside.path(), skill_dir.join("data")).unwrap();
        let writer = WriteSkillTool {
            sources: Arc::new(SkillSources::from_dirs(Some(ws.path().into()), None, None)),
        };
        let skill_md = "---\nname: safe-skill\ndescription: safe\n---\n\nbody\n";

        let res = writer
            .execute(json!({
                "scope": "workspace",
                "name": "safe-skill",
                "skill_md": skill_md,
                "files": {
                    "data/escape.txt": "nope"
                }
            }))
            .await;

        assert!(res.is_error(), "got: {res:?}");
        assert!(!outside.path().join("escape.txt").exists());
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

    #[test]
    fn capability_exposes_skill_management_tools() {
        let ws = tempfile::tempdir().unwrap();
        let cap =
            YolopSkillsCapability::new(SkillSources::from_dirs(Some(ws.path().into()), None, None));
        let names = cap
            .tools()
            .iter()
            .map(|tool| tool.name().to_string())
            .collect::<Vec<_>>();

        assert_eq!(
            names,
            vec!["list_skills", "read_skill", "write_skill", "activate_skill"]
        );
    }
}

//! Conversational skills.sh registry search and install.
//!
//! Yolop already has local skill lifecycle tools (`list_skills`, `write_skill`,
//! `delete_skill`). This module adds the missing discovery/install loop against
//! the public [skills.sh](https://skills.sh) registry:
//!
//! - `search_skills` — `GET /api/search`
//! - `install_skill` — `GET /api/download/{owner}/{repo}/{slug}` then write into
//!   a writable scope (workspace or global)
//!
//! The agent should search, present options, ask which to install, then call
//! `install_skill`. GitHub-style `owner/repo@skill` sources work when the
//! registry has cached the snapshot; there is no separate GitHub clone path.

use crate::capabilities::narration::stable_labeled;
use crate::capabilities::skills::{SkillDirs, validate_skill_name};
use async_trait::async_trait;
use everruns_core::tool_narration::{ToolNarrationPhase, arg_str, truncate};
use everruns_core::{Tool, ToolExecutionResult};
use everruns_provider::ToolCall;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

const USER_AGENT: &str = concat!(
    "yolop/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/everruns/yolop)"
);

const DEFAULT_REGISTRY_BASE: &str = "https://skills.sh";
const DEFAULT_SEARCH_LIMIT: usize = 8;
const MAX_SEARCH_LIMIT: usize = 25;
/// Registry snapshots can be larger than agent-authored `write_skill` payloads;
/// keep a hard cap so a bad response cannot fill the disk.
const MAX_INSTALL_FILES: usize = 200;
const MAX_SKILL_FILE_BYTES: usize = 1024 * 1024;
const MAX_INSTALL_TOTAL_BYTES: usize = 10 * 1024 * 1024;
const FETCH_TIMEOUT_SECS: u64 = 20;

#[derive(Clone, Debug)]
pub(crate) struct SkillRegistryClient {
    client: Client,
    base_url: String,
}

impl SkillRegistryClient {
    pub(crate) fn production() -> Self {
        Self::with_base_url(DEFAULT_REGISTRY_BASE.to_string())
    }

    pub(crate) fn with_base_url(base_url: String) -> Self {
        let client = Client::builder()
            .user_agent(USER_AGENT)
            .timeout(Duration::from_secs(FETCH_TIMEOUT_SECS))
            .build()
            .expect("skills registry HTTP client should build");
        Self {
            client,
            base_url: base_url.trim_end_matches('/').to_string(),
        }
    }

    async fn search(
        &self,
        query: &str,
        limit: usize,
        owner: Option<&str>,
    ) -> Result<SkillSearchResponse, String> {
        let mut url = format!(
            "{}/api/search?q={}&limit={}",
            self.base_url,
            urlencoding::encode(query),
            limit
        );
        if let Some(owner) = owner.filter(|o| !o.is_empty()) {
            url.push_str("&owner=");
            url.push_str(&urlencoding::encode(owner));
        }
        let response = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|err| format!("skills.sh search failed: {err}"))?;
        if !response.status().is_success() {
            return Err(format!(
                "skills.sh search returned HTTP {}",
                response.status()
            ));
        }
        response
            .json::<SkillSearchResponse>()
            .await
            .map_err(|err| format!("skills.sh search response was not valid JSON: {err}"))
    }

    async fn download(
        &self,
        owner: &str,
        repo: &str,
        slug: &str,
    ) -> Result<SkillDownloadResponse, String> {
        let url = format!(
            "{}/api/download/{}/{}/{}",
            self.base_url,
            urlencoding::encode(owner),
            urlencoding::encode(repo),
            urlencoding::encode(slug)
        );
        let response = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|err| format!("skills.sh download failed: {err}"))?;
        if response.status().as_u16() == 404 {
            return Err(format!(
                "skills.sh has no cached snapshot for `{owner}/{repo}/{slug}`"
            ));
        }
        if !response.status().is_success() {
            return Err(format!(
                "skills.sh download returned HTTP {}",
                response.status()
            ));
        }
        response
            .json::<SkillDownloadResponse>()
            .await
            .map_err(|err| format!("skills.sh download response was not valid JSON: {err}"))
    }
}

#[derive(Debug, Deserialize, Serialize)]
struct SkillSearchResponse {
    query: String,
    #[serde(default)]
    #[serde(rename = "searchType")]
    search_type: Option<String>,
    #[serde(default)]
    skills: Vec<SkillSearchHit>,
    #[serde(default)]
    count: Option<usize>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
struct SkillSearchHit {
    #[serde(default)]
    id: Option<String>,
    #[serde(default, rename = "skillId")]
    skill_id: Option<String>,
    name: String,
    #[serde(default)]
    installs: Option<u64>,
    #[serde(default)]
    source: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SkillDownloadResponse {
    #[serde(default)]
    files: Vec<SkillDownloadFile>,
    #[serde(default)]
    hash: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SkillDownloadFile {
    path: String,
    contents: String,
}

/// Parsed install source: GitHub owner/repo plus skill slug for skills.sh.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedSkillSource {
    owner: String,
    repo: String,
    slug: String,
}

fn to_skill_slug(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut last_dash = false;
    for ch in name.chars() {
        let lower = ch.to_ascii_lowercase();
        if lower.is_ascii_alphanumeric() {
            out.push(lower);
            last_dash = false;
        } else if matches!(lower, ' ' | '_' | '-') && !out.is_empty() && !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

fn parse_skill_source(source: &str, skill: Option<&str>) -> Result<ParsedSkillSource, String> {
    let raw = source.trim();
    if raw.is_empty() {
        return Err("'source' is required".to_string());
    }

    let mut owned = raw.to_string();
    for prefix in [
        "https://www.skills.sh/",
        "http://www.skills.sh/",
        "https://skills.sh/",
        "http://skills.sh/",
        "https://github.com/",
        "http://github.com/",
        "github.com/",
    ] {
        if let Some(stripped) = owned.strip_prefix(prefix) {
            owned = stripped.to_string();
            break;
        }
    }
    owned = owned.trim_matches('/').to_string();
    if let Some((path, _)) = owned.split_once('?') {
        owned = path.to_string();
    }
    if let Some((path, _)) = owned.split_once('#') {
        owned = path.to_string();
    }
    // Drop optional `/tree/<ref>/...` noise from GitHub URLs when present.
    if let Some(idx) = owned.find("/tree/") {
        let owner_repo = owned[..idx].to_string();
        let after = owned[idx + "/tree/".len()..].to_string();
        let mut parts = after.splitn(2, '/');
        let _ref = parts.next();
        owned = if let Some(subpath) = parts.next() {
            let skill_seg = subpath
                .trim_end_matches('/')
                .rsplit('/')
                .find(|p| !p.is_empty() && !p.eq_ignore_ascii_case("SKILL.md"))
                .unwrap_or(subpath);
            format!("{owner_repo}/{skill_seg}")
        } else {
            owner_repo
        };
    }

    // owner/repo@skill
    if let Some((left, right)) = owned.split_once('@') {
        let (owner, repo) = split_owner_repo(left)?;
        let slug = skill_slug_from(right, skill)?;
        return Ok(ParsedSkillSource { owner, repo, slug });
    }

    let segments: Vec<&str> = owned.split('/').filter(|s| !s.is_empty()).collect();
    match segments.as_slice() {
        [owner, repo] => {
            let slug = skill
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(to_skill_slug)
                .ok_or_else(|| {
                    "source `owner/repo` requires a skill name — pass `skill` or use `owner/repo@skill`"
                        .to_string()
                })?;
            Ok(ParsedSkillSource {
                owner: (*owner).to_string(),
                repo: (*repo).to_string(),
                slug,
            })
        }
        [owner, repo, skill_id] => Ok(ParsedSkillSource {
            owner: (*owner).to_string(),
            repo: (*repo).to_string(),
            slug: skill_slug_from(skill_id, skill)?,
        }),
        [owner, repo, rest @ ..] if !rest.is_empty() => {
            // skills.sh ids are owner/repo/skillId; tolerate deeper paths by
            // taking the final segment as the skill id.
            let skill_id = rest.last().expect("rest non-empty");
            Ok(ParsedSkillSource {
                owner: (*owner).to_string(),
                repo: (*repo).to_string(),
                slug: skill_slug_from(skill_id, skill)?,
            })
        }
        _ => Err(format!(
            "unrecognized skill source `{source}`; expected `owner/repo/skill`, `owner/repo@skill`, or a skills.sh URL"
        )),
    }
}

fn split_owner_repo(value: &str) -> Result<(String, String), String> {
    let parts: Vec<&str> = value.split('/').filter(|s| !s.is_empty()).collect();
    match parts.as_slice() {
        [owner, repo] => Ok(((*owner).to_string(), (*repo).to_string())),
        _ => Err(format!("expected `owner/repo` before `@`, got `{value}`")),
    }
}

fn skill_slug_from(primary: &str, override_skill: Option<&str>) -> Result<String, String> {
    let chosen = override_skill
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(primary.trim());
    if chosen.is_empty() {
        return Err("skill name is empty".to_string());
    }
    let slug = to_skill_slug(chosen);
    if slug.is_empty() {
        return Err(format!("skill name `{chosen}` has no usable slug"));
    }
    Ok(slug)
}

fn validate_relative_skill_path(raw: &str) -> Result<PathBuf, String> {
    if raw.trim().is_empty() || raw.contains('\\') {
        return Err(format!("invalid skill file path `{raw}`"));
    }
    let path = PathBuf::from(raw);
    if path.is_absolute() {
        return Err(format!("invalid skill file path `{raw}`"));
    }
    for component in path.components() {
        if !matches!(component, Component::Normal(_)) {
            return Err(format!("invalid skill file path `{raw}`"));
        }
    }
    Ok(path)
}

#[derive(Debug)]
struct PreparedInstall {
    name: String,
    skill_md: String,
    files: Vec<(PathBuf, String)>,
    hash: Option<String>,
}

fn prepare_install_files(download: SkillDownloadResponse) -> Result<PreparedInstall, String> {
    if download.files.is_empty() {
        return Err("skills.sh download returned no files".to_string());
    }
    if download.files.len() > MAX_INSTALL_FILES {
        return Err(format!(
            "skills.sh snapshot has too many files ({}; max {MAX_INSTALL_FILES})",
            download.files.len()
        ));
    }

    let mut total = 0usize;
    let mut skill_md: Option<(PathBuf, String)> = None;
    let mut extras: Vec<(PathBuf, String)> = Vec::new();

    for file in download.files {
        total = total.saturating_add(file.contents.len());
        if total > MAX_INSTALL_TOTAL_BYTES {
            return Err(format!(
                "skills.sh snapshot exceeds the {MAX_INSTALL_TOTAL_BYTES} byte install limit"
            ));
        }
        if file.contents.len() > MAX_SKILL_FILE_BYTES {
            return Err(format!(
                "file `{}` exceeds the {MAX_SKILL_FILE_BYTES} byte limit",
                file.path
            ));
        }
        let path = validate_relative_skill_path(&file.path)?;
        if path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.eq_ignore_ascii_case("SKILL.md"))
            && path.components().count() == 1
        {
            if skill_md.is_some() {
                return Err("skills.sh snapshot contains multiple root SKILL.md files".to_string());
            }
            skill_md = Some((path, file.contents));
        } else {
            // Nested SKILL.md copies are treated as ordinary bundled files.
            extras.push((path, file.contents));
        }
    }

    let Some((_path, skill_md_content)) = skill_md else {
        return Err("skills.sh snapshot is missing a root SKILL.md".to_string());
    };

    let parsed = everruns_core::skill::parse_skill_md(&skill_md_content)
        .map_err(|errors| format!("Invalid SKILL.md from registry: {}", errors.join(", ")))?;
    validate_skill_name(&parsed.name)?;

    Ok(PreparedInstall {
        name: parsed.name,
        skill_md: skill_md_content,
        files: extras,
        hash: download.hash,
    })
}

fn write_skill_install(
    target_dir: &Path,
    skill_md: &str,
    files: &[(PathBuf, String)],
    overwrite: bool,
) -> Result<(), String> {
    if target_dir.exists() {
        if !overwrite {
            return Err(format!(
                "`{}` already exists; pass overwrite=true to replace it",
                target_dir.display()
            ));
        }
        if !target_dir.join("SKILL.md").is_file() {
            return Err(format!(
                "`{}` exists but is not a skill (no SKILL.md); refusing to overwrite",
                target_dir.display()
            ));
        }
    }

    let parent = target_dir.parent().ok_or_else(|| {
        format!(
            "skill install path `{}` has no parent directory",
            target_dir.display()
        )
    })?;
    std::fs::create_dir_all(parent)
        .map_err(|err| format!("failed to create skills directory: {err}"))?;

    let staging = parent.join(format!(
        ".yolop-skill-install-{}-{}",
        target_dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("skill"),
        std::process::id()
    ));
    if staging.exists() {
        std::fs::remove_dir_all(&staging)
            .map_err(|err| format!("failed to clear staging directory: {err}"))?;
    }
    std::fs::create_dir_all(&staging)
        .map_err(|err| format!("failed to create staging directory: {err}"))?;

    let write_all = || -> Result<(), String> {
        std::fs::write(staging.join("SKILL.md"), skill_md)
            .map_err(|err| format!("failed to write SKILL.md: {err}"))?;
        for (rel, content) in files {
            let dest = staging.join(rel);
            if let Some(dir) = dest.parent() {
                std::fs::create_dir_all(dir)
                    .map_err(|err| format!("failed to create `{}`: {err}", dir.display()))?;
            }
            std::fs::write(&dest, content)
                .map_err(|err| format!("failed to write `{}`: {err}", rel.display()))?;
        }
        Ok(())
    };

    if let Err(err) = write_all() {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(err);
    }

    if target_dir.exists() {
        std::fs::remove_dir_all(target_dir).map_err(|err| {
            let _ = std::fs::remove_dir_all(&staging);
            format!("failed to replace existing skill: {err}")
        })?;
    }

    std::fs::rename(&staging, target_dir).map_err(|err| {
        let _ = std::fs::remove_dir_all(&staging);
        format!("failed to finalize skill install: {err}")
    })?;
    Ok(())
}

pub(crate) struct SearchSkillsTool {
    registry: SkillRegistryClient,
}

impl SearchSkillsTool {
    pub(crate) fn new(registry: SkillRegistryClient) -> Self {
        Self { registry }
    }
}

#[async_trait]
impl Tool for SearchSkillsTool {
    fn narrate(
        &self,
        tool_call: &ToolCall,
        phase: ToolNarrationPhase,
        locale: Option<&str>,
        _ctx: everruns_core::tool_narration::ToolNarrationContext<'_>,
    ) -> Option<String> {
        let _ = locale;
        let detail = arg_str(&tool_call.arguments, &["query"]).map(|q| truncate(q, 48));
        Some(stable_labeled("Search skills", detail, phase))
    }

    fn name(&self) -> &str {
        "search_skills"
    }

    fn display_name(&self) -> Option<&str> {
        Some("Search skills")
    }

    fn description(&self) -> &str {
        // Kept to what the model cannot infer from the schema. The search →
        // ask → install workflow and the off-registry fallbacks live in the
        // skill-management skill; repeating them here costs every session
        // tokens, since tool descriptions survive schema deferral.
        "Search the public skills.sh registry for installable agent skills. \
         Returns `owner/repo/skillId` sources for `install_skill`."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "What the skill should help with (topic, tool, or task)."
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum results to return (1–25). Defaults to 8.",
                    "minimum": 1,
                    "maximum": 25
                },
                "owner": {
                    "type": "string",
                    "description": "Optional GitHub owner/org to restrict results (e.g. `vercel-labs`)."
                }
            },
            "required": ["query"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, arguments: Value) -> ToolExecutionResult {
        let query = arguments
            .get("query")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        if query.len() < 2 {
            return ToolExecutionResult::tool_error(
                "'query' must be at least 2 characters".to_string(),
            );
        }
        let limit = arguments
            .get("limit")
            .and_then(Value::as_u64)
            .map(|n| n as usize)
            .unwrap_or(DEFAULT_SEARCH_LIMIT)
            .clamp(1, MAX_SEARCH_LIMIT);
        let owner = arguments
            .get("owner")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty());

        match self.registry.search(query, limit, owner).await {
            Ok(response) => {
                let skills: Vec<Value> = response
                    .skills
                    .into_iter()
                    .map(|hit| {
                        let id = hit
                            .id
                            .clone()
                            .or_else(|| {
                                hit.source.as_ref().map(|source| {
                                    format!(
                                        "{source}/{}",
                                        hit.skill_id
                                            .clone()
                                            .unwrap_or_else(|| to_skill_slug(&hit.name))
                                    )
                                })
                            })
                            .unwrap_or_else(|| hit.name.clone());
                        let url = format!("https://skills.sh/{id}");
                        json!({
                            "id": id,
                            "skill_id": hit.skill_id,
                            "name": hit.name,
                            "installs": hit.installs,
                            "source": hit.source,
                            "url": url,
                            "install_source": id,
                        })
                    })
                    .collect();
                ToolExecutionResult::success(json!({
                    "query": response.query,
                    "search_type": response.search_type,
                    "count": response.count.unwrap_or(skills.len()),
                    "skills": skills,
                    "registry": "skills.sh",
                    "message": "Present these options to the user and ask which skill to install before calling install_skill.",
                }))
            }
            Err(err) => ToolExecutionResult::tool_error(err),
        }
    }
}

pub(crate) struct InstallSkillTool {
    dirs: SkillDirs,
    registry: SkillRegistryClient,
}

impl InstallSkillTool {
    pub(crate) fn new(dirs: SkillDirs, registry: SkillRegistryClient) -> Self {
        Self { dirs, registry }
    }

    fn base_for(&self, scope: &str) -> Result<PathBuf, String> {
        match scope {
            "workspace" => Ok(self.dirs.workspace.clone()),
            "global" => self
                .dirs
                .global
                .clone()
                .ok_or_else(|| "no global skills directory is configured".to_string()),
            "system" => Err("system skills are read-only and cannot be installed into".to_string()),
            other => Err(format!(
                "unknown scope `{other}`; expected `workspace` or `global`"
            )),
        }
    }
}

#[async_trait]
impl Tool for InstallSkillTool {
    fn narrate(
        &self,
        tool_call: &ToolCall,
        phase: ToolNarrationPhase,
        locale: Option<&str>,
        _ctx: everruns_core::tool_narration::ToolNarrationContext<'_>,
    ) -> Option<String> {
        let _ = locale;
        let detail = arg_str(&tool_call.arguments, &["source"])
            .or_else(|| arg_str(&tool_call.arguments, &["skill"]))
            .map(|s| truncate(s, 48));
        Some(stable_labeled("Install skill", detail, phase))
    }

    fn name(&self) -> &str {
        "install_skill"
    }

    fn display_name(&self) -> Option<&str> {
        Some("Install skill")
    }

    fn description(&self) -> &str {
        "Install a skill from the skills.sh registry into the workspace or global \
         scope, live immediately. Sources are `owner/repo/skillId`, \
         `owner/repo@skill`, or a skills.sh URL; off-registry skills go through \
         `write_skill`."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "source": {
                    "type": "string",
                    "description": "Registry id (`owner/repo/skillId`), `owner/repo@skill`, skills.sh URL, or GitHub `owner/repo` (with `skill`)."
                },
                "skill": {
                    "type": "string",
                    "description": "Skill name/slug when `source` is only `owner/repo`."
                },
                "scope": {
                    "type": "string",
                    "description": "Writable scope to install into.",
                    "enum": ["workspace", "global"],
                    "default": "workspace"
                },
                "overwrite": {
                    "type": "boolean",
                    "description": "Replace an existing skill with the same name. Defaults to true.",
                    "default": true
                }
            },
            "required": ["source"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, arguments: Value) -> ToolExecutionResult {
        let source = arguments
            .get("source")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        let skill = arguments
            .get("skill")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let parsed = match parse_skill_source(source, skill) {
            Ok(parsed) => parsed,
            Err(err) => return ToolExecutionResult::tool_error(err),
        };
        let scope = arguments
            .get("scope")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("workspace");
        let overwrite = arguments
            .get("overwrite")
            .and_then(Value::as_bool)
            .unwrap_or(true);

        let base = match self.base_for(scope) {
            Ok(base) => base,
            Err(err) => return ToolExecutionResult::tool_error(err),
        };

        let download = match self
            .registry
            .download(&parsed.owner, &parsed.repo, &parsed.slug)
            .await
        {
            Ok(download) => download,
            Err(err) => return ToolExecutionResult::tool_error(err),
        };

        let prepared = match prepare_install_files(download) {
            Ok(prepared) => prepared,
            Err(err) => return ToolExecutionResult::tool_error(err),
        };

        let target = base.join(&prepared.name);
        let name_for_write = prepared.name.clone();
        let skill_md_for_write = prepared.skill_md.clone();
        let files_for_write = prepared.files.clone();
        let hash = prepared.hash.clone();
        let files_written = files_for_write.len() + 1;
        let target_for_write = target.clone();
        let outcome = tokio::task::spawn_blocking(move || {
            write_skill_install(
                &target_for_write,
                &skill_md_for_write,
                &files_for_write,
                overwrite,
            )
        })
        .await;

        match outcome {
            Ok(Ok(())) => ToolExecutionResult::success(json!({
                "success": true,
                "name": name_for_write,
                "scope": scope,
                "path": target.display().to_string(),
                "source": format!("{}/{}/{}", parsed.owner, parsed.repo, parsed.slug),
                "files_written": files_written,
                "hash": hash,
                "url": format!(
                    "https://skills.sh/{}/{}/{}",
                    parsed.owner, parsed.repo, parsed.slug
                ),
                "message": format!(
                    "installed `{name_for_write}` into the {scope} scope; activate with activate_skill or /{name_for_write} if user-invocable"
                ),
            })),
            Ok(Err(message)) => ToolExecutionResult::tool_error(message),
            Err(join_err) => {
                ToolExecutionResult::tool_error(format!("install task failed: {join_err}"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugifies_skill_names() {
        assert_eq!(
            to_skill_slug("vercel-react-best-practices"),
            "vercel-react-best-practices"
        );
        assert_eq!(to_skill_slug("Find Skills"), "find-skills");
        assert_eq!(to_skill_slug("  Foo_Bar!! "), "foo-bar");
    }

    #[test]
    fn parses_common_source_forms() {
        assert_eq!(
            parse_skill_source("vercel-labs/agent-skills/vercel-react-best-practices", None)
                .unwrap(),
            ParsedSkillSource {
                owner: "vercel-labs".into(),
                repo: "agent-skills".into(),
                slug: "vercel-react-best-practices".into(),
            }
        );
        assert_eq!(
            parse_skill_source("vercel-labs/skills@find-skills", None).unwrap(),
            ParsedSkillSource {
                owner: "vercel-labs".into(),
                repo: "skills".into(),
                slug: "find-skills".into(),
            }
        );
        assert_eq!(
            parse_skill_source("https://skills.sh/vercel-labs/skills/find-skills", None).unwrap(),
            ParsedSkillSource {
                owner: "vercel-labs".into(),
                repo: "skills".into(),
                slug: "find-skills".into(),
            }
        );
        assert_eq!(
            parse_skill_source("vercel-labs/skills", Some("find-skills")).unwrap(),
            ParsedSkillSource {
                owner: "vercel-labs".into(),
                repo: "skills".into(),
                slug: "find-skills".into(),
            }
        );
        assert!(parse_skill_source("vercel-labs/skills", None).is_err());
    }

    #[test]
    fn prepare_install_rejects_traversal_and_missing_skill_md() {
        let err = prepare_install_files(SkillDownloadResponse {
            files: vec![SkillDownloadFile {
                path: "../escape.md".into(),
                contents: "nope".into(),
            }],
            hash: None,
        })
        .unwrap_err();
        assert!(err.contains("invalid skill file path"), "{err}");

        let err = prepare_install_files(SkillDownloadResponse {
            files: vec![SkillDownloadFile {
                path: "README.md".into(),
                contents: "hi".into(),
            }],
            hash: None,
        })
        .unwrap_err();
        assert!(err.contains("missing a root SKILL.md"), "{err}");
    }

    #[test]
    fn prepare_install_accepts_valid_snapshot() {
        let prepared = prepare_install_files(SkillDownloadResponse {
            files: vec![
                SkillDownloadFile {
                    path: "SKILL.md".into(),
                    contents: "---\nname: find-skills\ndescription: Find skills\n---\nBody\n"
                        .into(),
                },
                SkillDownloadFile {
                    path: "notes/README.md".into(),
                    contents: "extra".into(),
                },
            ],
            hash: Some("abc".into()),
        })
        .unwrap();
        assert_eq!(prepared.name, "find-skills");
        assert!(prepared.skill_md.contains("name: find-skills"));
        assert_eq!(prepared.files.len(), 1);
        assert_eq!(prepared.hash.as_deref(), Some("abc"));
    }

    #[test]
    fn write_skill_install_is_atomic_and_overwrite_safe() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("find-skills");
        write_skill_install(
            &target,
            "---\nname: find-skills\ndescription: Find skills\n---\nv1\n",
            &[(PathBuf::from("extra.txt"), "one".into())],
            true,
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(target.join("extra.txt")).unwrap(),
            "one"
        );

        write_skill_install(
            &target,
            "---\nname: find-skills\ndescription: Find skills\n---\nv2\n",
            &[],
            true,
        )
        .unwrap();
        assert!(
            !target.join("extra.txt").exists(),
            "overwrite should replace the whole skill directory"
        );
        assert!(
            std::fs::read_to_string(target.join("SKILL.md"))
                .unwrap()
                .contains("v2")
        );

        let err = write_skill_install(
            &target,
            "---\nname: find-skills\ndescription: Find skills\n---\nv3\n",
            &[],
            false,
        )
        .unwrap_err();
        assert!(err.contains("already exists"), "{err}");
    }

    #[tokio::test]
    async fn search_and_install_tools_hit_registry_entrypoint() {
        let server = tiny_http::Server::http("127.0.0.1:0").expect("test server");
        let base_url = format!("http://{}", server.server_addr());
        let server_thread = std::thread::spawn(move || {
            for request in server.incoming_requests().take(2) {
                let url = request.url().to_string();
                let body = if url.starts_with("/api/search") {
                    r#"{
                      "query":"find",
                      "searchType":"fuzzy",
                      "count":1,
                      "skills":[{
                        "id":"vercel-labs/skills/find-skills",
                        "skillId":"find-skills",
                        "name":"find-skills",
                        "installs":42,
                        "source":"vercel-labs/skills"
                      }]
                    }"#
                } else if url.starts_with("/api/download/") {
                    r#"{
                      "hash":"deadbeef",
                      "files":[
                        {"path":"SKILL.md","contents":"---\nname: find-skills\ndescription: Find skills\nuser-invocable: true\n---\nBody\n"},
                        {"path":"hint.txt","contents":"tip"}
                      ]
                    }"#
                } else {
                    "{}"
                };
                request
                    .respond(tiny_http::Response::from_string(body))
                    .expect("respond");
            }
        });

        let registry = SkillRegistryClient::with_base_url(base_url);
        let search = SearchSkillsTool::new(registry.clone());
        let search_result = search.execute(json!({"query": "find", "limit": 5})).await;
        let search_out = match search_result {
            ToolExecutionResult::Success(v) => v,
            other => panic!("search should succeed: {other:?}"),
        };
        assert_eq!(search_out["skills"][0]["name"], "find-skills");
        assert_eq!(
            search_out["skills"][0]["install_source"],
            "vercel-labs/skills/find-skills"
        );
        assert!(
            search_out["message"]
                .as_str()
                .unwrap()
                .contains("ask which skill to install")
        );

        let ws = tempfile::tempdir().unwrap();
        let install = InstallSkillTool::new(
            SkillDirs {
                workspace: ws.path().to_path_buf(),
                global: None,
                profile: None,
                system: None,
            },
            registry,
        );
        let install_result = install
            .execute(json!({
                "source": "vercel-labs/skills/find-skills",
                "scope": "workspace"
            }))
            .await;
        server_thread.join().expect("server thread");
        let install_out = match install_result {
            ToolExecutionResult::Success(v) => v,
            other => panic!("install should succeed: {other:?}"),
        };
        assert_eq!(install_out["name"], "find-skills");
        assert_eq!(install_out["files_written"], 2);
        assert!(ws.path().join("find-skills").join("SKILL.md").is_file());
        assert_eq!(
            std::fs::read_to_string(ws.path().join("find-skills/hint.txt")).unwrap(),
            "tip"
        );
    }

    #[test]
    fn tool_names_are_stable() {
        let dirs = SkillDirs {
            workspace: PathBuf::from("/ws/.agents/skills"),
            global: None,
            profile: None,
            system: None,
        };
        let registry = SkillRegistryClient::with_base_url("http://127.0.0.1:9".into());
        assert_eq!(
            SearchSkillsTool::new(registry.clone()).name(),
            "search_skills"
        );
        assert_eq!(
            InstallSkillTool::new(dirs, registry).name(),
            "install_skill"
        );
    }

    /// Live smoke against production skills.sh. Opt-in rather than `#[ignore]`,
    /// which nothing ever runs: it needs no key, so gate it on the same
    /// `YOLOP_REQUIRE_LIVE_TESTS` flag the live-smoke job sets instead of
    /// letting a default `cargo test` reach an external service.
    #[tokio::test]
    async fn live_skills_sh_search_and_install() {
        if std::env::var_os("YOLOP_REQUIRE_LIVE_TESTS").is_none() {
            eprintln!("skipping live test: YOLOP_REQUIRE_LIVE_TESTS not set");
            return;
        }
        let registry = SkillRegistryClient::production();
        let search = SearchSkillsTool::new(registry.clone());
        let search_out = match search
            .execute(json!({"query": "find-skills", "limit": 3}))
            .await
        {
            ToolExecutionResult::Success(v) => v,
            other => panic!("live search failed: {other:?}"),
        };
        let first = &search_out["skills"][0];
        let source = first["install_source"].as_str().expect("install_source");
        assert!(source.contains('/'), "{source}");

        let ws = tempfile::tempdir().unwrap();
        let install = InstallSkillTool::new(
            SkillDirs {
                workspace: ws.path().to_path_buf(),
                global: None,
                profile: None,
                system: None,
            },
            registry,
        );
        let install_out = match install
            .execute(json!({"source": source, "scope": "workspace"}))
            .await
        {
            ToolExecutionResult::Success(v) => v,
            other => panic!("live install failed: {other:?}"),
        };
        let name = install_out["name"].as_str().expect("name");
        assert!(
            ws.path().join(name).join("SKILL.md").is_file(),
            "expected installed skill at {}",
            ws.path().join(name).display()
        );
    }
}

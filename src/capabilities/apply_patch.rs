//! `native_edit_tools`: model-native file-edit tool shapes (opt-in).
//!
//! Every model gets the same `edit_file`/`write_file` by default. OpenAI's GPT
//! and Codex models were trained on Codex's `apply_patch` tool and its
//! `*** Begin Patch` envelope, so a shape already in their weights should need
//! fewer retries than one they must learn from a schema. That is a hypothesis,
//! so the shape is opt-in and A/B'd in `evals/harness_basic`
//! (`native-edit-tools`); see knowledge/specs/native-edit-tools.md.
//!
//! Decisions:
//! - Dispatch is per model through `Capability::resolve_for_model`, the same
//!   mechanism upstream `auto_tool_search` uses, so a live model switch changes
//!   the tool set on the next turn without rebuilding the session.
//! - Claude models resolve to nothing: `edit_file` already has the
//!   `str_replace` contract Claude's own editor tool trains (exact, unique
//!   match, no fuzzy fallback), so a second edit tool would only add schema.
//! - `apply_patch` writes through the session file store, the same mount table,
//!   blocklist, and read-only enforcement `edit_file` uses, and the per-turn
//!   checkpoint snapshots the worktree regardless of which tool wrote it.
//!   Deletions escalate to the destructive approval tier in
//!   `tool_approval`, matching `delete_file`.
//! - The whole patch is parsed and planned against one snapshot before any
//!   write, so a bad envelope, a missing context line, or a missing file leaves
//!   the workspace untouched. Updates commit with compare-and-swap.
//! - The parser follows Codex's grammar and its lenient context matching
//!   (exact, then trailing whitespace, then surrounding whitespace). Nothing
//!   reusable exists in the everruns crates, so it lives here.

use async_trait::async_trait;
use everruns_contracts::ToolHints;
use everruns_core::ToolContext;
use everruns_core::tool_context::ToolContextService;
use everruns_core::{Capability, CapabilityStatus};
use everruns_core::{Tool, ToolExecutionResult};
use serde_json::{Value, json};

pub(crate) const NATIVE_EDIT_TOOLS_CAPABILITY_ID: &str = "native_edit_tools";
pub(crate) const APPLY_PATCH_TOOL_NAME: &str = "apply_patch";

const BEGIN_PATCH: &str = "*** Begin Patch";
const END_PATCH: &str = "*** End Patch";
const ADD_FILE: &str = "*** Add File: ";
const DELETE_FILE: &str = "*** Delete File: ";
const UPDATE_FILE: &str = "*** Update File: ";
const MOVE_TO: &str = "*** Move to: ";
const END_OF_FILE: &str = "*** End of File";

/// Whether `model` belongs to the OpenAI family trained on Codex's
/// `apply_patch` grammar. Accepts router-prefixed ids (`openai/gpt-5.5`).
pub(crate) fn uses_codex_patch_grammar(model: &str) -> bool {
    let id = model
        .rsplit('/')
        .next()
        .unwrap_or(model)
        .to_ascii_lowercase();
    let o_series = id
        .strip_prefix('o')
        .is_some_and(|rest| rest.chars().next().is_some_and(|c| c.is_ascii_digit()));
    id.starts_with("gpt-") || id.contains("codex") || o_series
}

/// The opt-in capability. Contributes nothing itself; `resolve_for_model`
/// picks the per-family implementation.
pub(crate) struct NativeEditToolsCapability {
    codex: CodexPatchTools,
    unchanged: UnchangedEditTools,
}

impl NativeEditToolsCapability {
    pub(crate) fn new() -> Self {
        Self {
            codex: CodexPatchTools,
            unchanged: UnchangedEditTools,
        }
    }
}

#[async_trait]
impl Capability for NativeEditToolsCapability {
    fn id(&self) -> &str {
        NATIVE_EDIT_TOOLS_CAPABILITY_ID
    }

    fn name(&self) -> &str {
        "Native Edit Tools"
    }

    fn description(&self) -> &str {
        "Adds the file-edit tool shape a model family was trained on: apply_patch for OpenAI GPT and Codex models (opt-in)."
    }

    fn status(&self) -> CapabilityStatus {
        CapabilityStatus::Available
    }

    fn category(&self) -> Option<&str> {
        Some("File System")
    }

    // An unknown model (`None`) gets the unchanged set: the default tools
    // work everywhere, the Codex grammar only where it was trained.
    fn resolve_for_model(&self, model: Option<&str>) -> Option<&dyn Capability> {
        match model {
            Some(model) if uses_codex_patch_grammar(model) => Some(&self.codex),
            _ => Some(&self.unchanged),
        }
    }
}

struct CodexPatchTools;

#[async_trait]
impl Capability for CodexPatchTools {
    fn id(&self) -> &str {
        "native_edit_tools.codex"
    }

    fn name(&self) -> &str {
        "Codex apply_patch"
    }

    fn description(&self) -> &str {
        "apply_patch with the Codex patch grammar."
    }

    fn tools(&self) -> Vec<Box<dyn Tool>> {
        vec![Box::new(ApplyPatchTool)]
    }
}

struct UnchangedEditTools;

#[async_trait]
impl Capability for UnchangedEditTools {
    fn id(&self) -> &str {
        "native_edit_tools.unchanged"
    }

    fn name(&self) -> &str {
        "Default edit tools"
    }

    fn description(&self) -> &str {
        "No extra edit tool: edit_file already matches this model's trained shape."
    }
}

// ---------------------------------------------------------------------------
// Patch grammar
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PatchOp {
    Add {
        path: String,
        contents: String,
    },
    Delete {
        path: String,
    },
    Update {
        path: String,
        move_to: Option<String>,
        chunks: Vec<UpdateChunk>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UpdateChunk {
    /// Text after `@@ `: a line to seek before matching `old_lines`.
    context: Option<String>,
    old_lines: Vec<String>,
    new_lines: Vec<String>,
    /// For each entry of `new_lines`, the index in `old_lines` when it is an
    /// unchanged context line. Context keeps the file's own text, so loose
    /// whitespace matching never rewrites lines the patch did not change.
    context_of: Vec<Option<usize>>,
    end_of_file: bool,
}

/// Whether the patch text deletes a file. Used by the approval gate, so it is
/// deliberately a textual check that also fires on a patch that would fail to
/// parse: asking once too often is the safe direction.
pub(crate) fn patch_deletes_files(patch: &str) -> bool {
    patch
        .lines()
        .any(|line| line.trim_start().starts_with(DELETE_FILE.trim_end()))
}

/// Strip a heredoc wrapper models sometimes copy from Codex's shell form
/// (`apply_patch <<'EOF' ... EOF`).
fn strip_heredoc(text: &str) -> &str {
    let trimmed = text.trim();
    let Some(first_newline) = trimmed.find('\n') else {
        return trimmed;
    };
    let first = trimmed[..first_newline].trim();
    if !first.contains("<<") {
        return trimmed;
    }
    let body = &trimmed[first_newline + 1..];
    match body.rfind('\n') {
        Some(last_newline) if !body[last_newline + 1..].trim().starts_with("***") => {
            body[..last_newline].trim()
        }
        _ => body.trim(),
    }
}

pub(crate) fn parse_patch(text: &str) -> Result<Vec<PatchOp>, String> {
    let text = strip_heredoc(text);
    let lines: Vec<&str> = text.lines().collect();
    if lines.first().map(|line| line.trim()) != Some(BEGIN_PATCH) {
        return Err(format!(
            "invalid patch: the first line must be '{BEGIN_PATCH}'"
        ));
    }
    if lines.len() < 2 || lines.last().map(|line| line.trim()) != Some(END_PATCH) {
        return Err(format!(
            "invalid patch: the last line must be '{END_PATCH}'"
        ));
    }
    let body = &lines[1..lines.len() - 1];
    let mut ops = Vec::new();
    let mut index = 0;
    while index < body.len() {
        let line = body[index].trim();
        if line.is_empty() {
            index += 1;
            continue;
        }
        if let Some(path) = line.strip_prefix(ADD_FILE) {
            index += 1;
            let mut contents = String::new();
            while index < body.len() && !body[index].starts_with("***") {
                let Some(added) = body[index].strip_prefix('+') else {
                    return Err(format!(
                        "invalid patch: line {} in Add File '{path}' must start with '+'",
                        index + 2
                    ));
                };
                contents.push_str(added);
                contents.push('\n');
                index += 1;
            }
            ops.push(PatchOp::Add {
                path: non_empty_path(path)?,
                contents,
            });
        } else if let Some(path) = line.strip_prefix(DELETE_FILE) {
            index += 1;
            ops.push(PatchOp::Delete {
                path: non_empty_path(path)?,
            });
        } else if let Some(path) = line.strip_prefix(UPDATE_FILE) {
            let path = non_empty_path(path)?;
            index += 1;
            let mut move_to = None;
            if let Some(target) = body.get(index).and_then(|line| line.strip_prefix(MOVE_TO)) {
                move_to = Some(non_empty_path(target)?);
                index += 1;
            }
            let (chunks, next) = parse_update_chunks(body, index, &path)?;
            index = next;
            if chunks.is_empty() && move_to.is_none() {
                return Err(format!("invalid patch: Update File '{path}' has no hunks"));
            }
            ops.push(PatchOp::Update {
                path,
                move_to,
                chunks,
            });
        } else {
            return Err(format!(
                "invalid patch: line {} is '{line}'; expected '{ADD_FILE}', '{DELETE_FILE}', or '{UPDATE_FILE}'",
                index + 2
            ));
        }
    }
    if ops.is_empty() {
        return Err("invalid patch: no file operations between the markers".to_string());
    }
    Ok(ops)
}

fn non_empty_path(path: &str) -> Result<String, String> {
    let path = path.trim();
    if path.is_empty() {
        return Err("invalid patch: a file operation is missing its path".to_string());
    }
    Ok(path.to_string())
}

fn parse_update_chunks(
    body: &[&str],
    mut index: usize,
    path: &str,
) -> Result<(Vec<UpdateChunk>, usize), String> {
    let mut chunks: Vec<UpdateChunk> = Vec::new();
    let mut current: Option<UpdateChunk> = None;
    while index < body.len() {
        let raw = body[index];
        if raw.trim() == END_OF_FILE {
            let chunk = current.as_mut().ok_or_else(|| {
                format!("invalid patch: '{END_OF_FILE}' before any hunk in '{path}'")
            })?;
            chunk.end_of_file = true;
            index += 1;
            continue;
        }
        if raw.starts_with("***") {
            break;
        }
        if let Some(header) = raw.strip_prefix("@@") {
            chunks.extend(current.take().filter(|chunk| !chunk_is_empty(chunk)));
            let header = header.trim();
            current = Some(UpdateChunk {
                context: (!header.is_empty()).then(|| header.to_string()),
                old_lines: Vec::new(),
                new_lines: Vec::new(),
                context_of: Vec::new(),
                end_of_file: false,
            });
            index += 1;
            continue;
        }
        let chunk = current.get_or_insert_with(|| UpdateChunk {
            context: None,
            old_lines: Vec::new(),
            new_lines: Vec::new(),
            context_of: Vec::new(),
            end_of_file: false,
        });
        match raw.chars().next() {
            // An empty line inside a hunk is an empty context line, and a
            // ' ' line is context with its marker.
            None | Some(' ') => {
                let text = raw.get(1..).unwrap_or_default().to_string();
                chunk.context_of.push(Some(chunk.old_lines.len()));
                chunk.old_lines.push(text.clone());
                chunk.new_lines.push(text);
            }
            Some('-') => chunk.old_lines.push(raw[1..].to_string()),
            Some('+') => {
                chunk.context_of.push(None);
                chunk.new_lines.push(raw[1..].to_string());
            }
            Some(_) => {
                return Err(format!(
                    "invalid patch: line {} in Update File '{path}' must start with ' ', '-', '+', or '@@': '{raw}'",
                    index + 2
                ));
            }
        }
        index += 1;
    }
    chunks.extend(current.filter(|chunk| !chunk_is_empty(chunk)));
    Ok((chunks, index))
}

fn chunk_is_empty(chunk: &UpdateChunk) -> bool {
    chunk.context.is_none() && chunk.old_lines.is_empty() && chunk.new_lines.is_empty()
}

/// Apply `chunks` to `content`, returning the new text. Line endings follow
/// the original file: a CRLF file stays CRLF.
pub(crate) fn apply_chunks(
    content: &str,
    chunks: &[UpdateChunk],
    path: &str,
) -> Result<String, String> {
    let crlf = content.contains("\r\n");
    let normalized = if crlf {
        content.replace("\r\n", "\n")
    } else {
        content.to_string()
    };
    let mut lines: Vec<String> = normalized.split('\n').map(str::to_string).collect();
    if lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }

    let mut replacements: Vec<(usize, usize, Vec<String>)> = Vec::new();
    let mut cursor = 0;
    for chunk in chunks {
        if let Some(context) = &chunk.context {
            let found = seek(&lines, std::slice::from_ref(context), cursor, false)
                .ok_or_else(|| format!("failed to find context line '{context}' in {path}"))?;
            cursor = found + 1;
        }
        if chunk.old_lines.is_empty() {
            // Pure insertion: at the end of the file, or right after the
            // `@@` context line when one was given.
            let at = if chunk.context.is_some() && !chunk.end_of_file {
                cursor
            } else {
                lines.len()
            };
            replacements.push((at, 0, chunk.new_lines.clone()));
            continue;
        }
        let mut old = chunk.old_lines.as_slice();
        let mut new = chunk.new_lines.as_slice();
        let mut found = seek(&lines, old, cursor, chunk.end_of_file);
        // A trailing empty context line usually stands for the final newline.
        if found.is_none() && old.last().is_some_and(String::is_empty) {
            old = &old[..old.len() - 1];
            if new.last().is_some_and(String::is_empty) {
                new = &new[..new.len() - 1];
            }
            found = seek(&lines, old, cursor, chunk.end_of_file);
        }
        let start = found.ok_or_else(|| {
            format!(
                "failed to find the expected lines in {path}:\n{}",
                chunk.old_lines.join("\n")
            )
        })?;
        let new = new
            .iter()
            .zip(&chunk.context_of)
            .map(|(line, context)| match context {
                Some(at) if *at < old.len() => lines[start + at].clone(),
                _ => line.clone(),
            })
            .collect();
        replacements.push((start, old.len(), new));
        cursor = start + old.len();
    }

    replacements.sort_by_key(|(start, _, _)| *start);
    for (start, len, new) in replacements.into_iter().rev() {
        lines.splice(start..start + len, new);
    }
    let mut out = lines.join("\n");
    out.push('\n');
    if crlf {
        out = out.replace('\n', "\r\n");
    }
    Ok(out)
}

/// Find `pattern` in `lines` at or after `start`. With `eof`, try the end of
/// the file first. Matching loosens in steps: exact, then ignoring trailing
/// whitespace, then ignoring surrounding whitespace.
fn seek(lines: &[String], pattern: &[String], start: usize, eof: bool) -> Option<usize> {
    if pattern.is_empty() {
        return Some(start.min(lines.len()));
    }
    if pattern.len() > lines.len() {
        return None;
    }
    let last_start = lines.len() - pattern.len();
    let normalizers: [fn(&str) -> &str; 3] = [|s| s, str::trim_end, str::trim];
    for normalize in normalizers {
        let matches_at = |at: usize| {
            lines[at..at + pattern.len()]
                .iter()
                .zip(pattern)
                .all(|(line, want)| normalize(line) == normalize(want))
        };
        if eof && last_start >= start && matches_at(last_start) {
            return Some(last_start);
        }
        if let Some(at) = (start..=last_start).find(|&at| matches_at(at)) {
            return Some(at);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Tool
// ---------------------------------------------------------------------------

const APPLY_PATCH_DESCRIPTION: &str = r#"Edit files with a patch. `input` is the whole patch:

*** Begin Patch
*** Add File: path/new.txt
+every line of the new file starts with +
*** Update File: path/existing.py
*** Move to: path/renamed.py   (optional)
@@ def function_name():   (optional line to seek first)
 unchanged context line
-removed line
+added line
*** Delete File: path/obsolete.txt
*** End Patch

Give about three lines of context around each change. Paths are relative to the workspace. Nothing is written unless every operation applies."#;

pub(crate) struct ApplyPatchTool;

/// A fully planned write, computed before anything touches disk.
enum PlannedWrite {
    Create {
        path: String,
        contents: String,
    },
    Replace {
        path: String,
        expected: String,
        contents: String,
    },
    Remove {
        path: String,
    },
}

#[async_trait]
impl Tool for ApplyPatchTool {
    fn name(&self) -> &str {
        APPLY_PATCH_TOOL_NAME
    }

    fn display_name(&self) -> Option<&str> {
        Some("Apply Patch")
    }

    fn description(&self) -> &str {
        APPLY_PATCH_DESCRIPTION
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "input": {
                    "type": "string",
                    "description": "The entire patch, from '*** Begin Patch' to '*** End Patch'."
                }
            },
            "required": ["input"],
            "additionalProperties": false
        })
    }

    fn hints(&self) -> ToolHints {
        // Same scheduling as edit_file: serialized against other workspace
        // writes and bash. Deletes escalate per call in `tool_approval`.
        ToolHints::default().with_concurrency_class("session_workspace")
    }

    async fn execute(&self, _arguments: Value) -> ToolExecutionResult {
        ToolExecutionResult::tool_error(
            "apply_patch requires context. This tool must be executed with session context.",
        )
    }

    async fn execute_with_context(
        &self,
        arguments: Value,
        context: &ToolContext,
    ) -> ToolExecutionResult {
        let Some(input) = arguments.get("input").and_then(Value::as_str) else {
            return ToolExecutionResult::tool_error("Missing required parameter: input");
        };
        let ops = match parse_patch(input) {
            Ok(ops) => ops,
            Err(error) => return ToolExecutionResult::tool_error(error),
        };
        let Some(store) = context.file_store.as_ref() else {
            return ToolExecutionResult::tool_error("File system not available in this context");
        };
        let session = context.session_id;

        // Plan every operation against the current workspace before writing.
        let mut plan = Vec::new();
        let mut summary = Vec::new();
        for op in &ops {
            match op {
                PatchOp::Add { path, contents } => {
                    plan.push(PlannedWrite::Create {
                        path: path.clone(),
                        contents: contents.clone(),
                    });
                    summary.push(json!({ "status": "A", "path": store.display_path(path) }));
                }
                PatchOp::Delete { path } => {
                    match read_text(store.as_ref(), session, path, "delete").await {
                        Ok(_) => {}
                        Err(error) => return error,
                    }
                    plan.push(PlannedWrite::Remove { path: path.clone() });
                    summary.push(json!({ "status": "D", "path": store.display_path(path) }));
                }
                PatchOp::Update {
                    path,
                    move_to,
                    chunks,
                } => {
                    let current = match read_text(store.as_ref(), session, path, "update").await {
                        Ok(current) => current,
                        Err(error) => return error,
                    };
                    let display = store.display_path(path);
                    let updated = match apply_chunks(&current, chunks, &display) {
                        Ok(updated) => updated,
                        Err(error) => return ToolExecutionResult::tool_error(error),
                    };
                    // A move onto its own path is a plain update; planning it
                    // as write-then-delete would remove the file.
                    match move_to.as_ref().filter(|target| *target != path) {
                        Some(target) => {
                            plan.push(PlannedWrite::Create {
                                path: target.clone(),
                                contents: updated,
                            });
                            plan.push(PlannedWrite::Remove { path: path.clone() });
                            summary.push(json!({
                                "status": "M",
                                "path": store.display_path(target),
                                "moved_from": display,
                            }));
                        }
                        None => {
                            plan.push(PlannedWrite::Replace {
                                path: path.clone(),
                                expected: current,
                                contents: updated,
                            });
                            summary.push(json!({ "status": "M", "path": display }));
                        }
                    }
                }
            }
        }

        for write in plan {
            let outcome = match write {
                PlannedWrite::Create { path, contents } => store
                    .write_file(session, &path, &contents, "text")
                    .await
                    .map(|_| ())
                    .map_err(|error| format!("{}: {error}", store.display_path(&path))),
                PlannedWrite::Replace {
                    path,
                    expected,
                    contents,
                } => match store
                    .write_file_if_content_matches(
                        session, &path, &expected, "text", &contents, "text",
                    )
                    .await
                {
                    Ok(Some(_)) => Ok(()),
                    Ok(None) => Err(format!(
                        "{} changed while the patch was applied; read it again and retry",
                        store.display_path(&path)
                    )),
                    Err(error) => Err(format!("{}: {error}", store.display_path(&path))),
                },
                PlannedWrite::Remove { path } => store
                    .delete_file(session, &path, false)
                    .await
                    .map(|_| ())
                    .map_err(|error| format!("{}: {error}", store.display_path(&path))),
            };
            if let Err(error) = outcome {
                return ToolExecutionResult::tool_error(format!("apply_patch failed: {error}"));
            }
        }

        let listing = summary
            .iter()
            .map(|file| {
                format!(
                    "{} {}",
                    file["status"].as_str().unwrap_or("?"),
                    file["path"].as_str().unwrap_or("")
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        ToolExecutionResult::success(json!({
            "output": format!("Success. Updated the following files:\n{listing}"),
            "files": summary,
        }))
    }

    fn requires_context(&self) -> bool {
        true
    }

    fn required_context_services(&self) -> &'static [ToolContextService] {
        &[ToolContextService::SessionFileSystem]
    }
}

/// Read an existing text file for `action`, or the tool error to return.
async fn read_text(
    store: &dyn everruns_core::SessionFileSystem,
    session: everruns_contracts::typed_id::SessionId,
    path: &str,
    action: &str,
) -> Result<String, ToolExecutionResult> {
    let display = store.display_path(path);
    match store.read_file(session, path).await {
        Ok(Some(file)) if file.is_directory => Err(ToolExecutionResult::tool_error(format!(
            "cannot {action} {display}: it is a directory"
        ))),
        Ok(Some(file)) if file.encoding != "text" => Err(ToolExecutionResult::tool_error(format!(
            "cannot {action} {display}: it is not a text file"
        ))),
        Ok(Some(file)) => Ok(file.content.unwrap_or_default()),
        Ok(None) => Err(ToolExecutionResult::tool_error(format!(
            "cannot {action} {display}: file not found"
        ))),
        Err(error) => Err(ToolExecutionResult::tool_error(format!(
            "cannot {action} {display}: {error}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use everruns_core::host::RealDiskFileStore;
    use std::sync::Arc;

    fn context(dir: &std::path::Path) -> ToolContext {
        let store = Arc::new(RealDiskFileStore::new(dir).expect("disk store"));
        ToolContext::with_file_store(Default::default(), store)
    }

    async fn apply(dir: &std::path::Path, patch: &str) -> ToolExecutionResult {
        ApplyPatchTool
            .execute_with_context(json!({ "input": patch }), &context(dir))
            .await
    }

    fn error_text(result: ToolExecutionResult) -> String {
        match result {
            ToolExecutionResult::ToolError(error) => error,
            other => panic!("expected a tool error, got {other:?}"),
        }
    }

    #[test]
    fn model_family_detection() {
        for model in [
            "gpt-5.5",
            "openai/gpt-5.6-terra",
            "gpt-5-codex",
            "o3",
            "o4-mini",
        ] {
            assert!(uses_codex_patch_grammar(model), "{model}");
        }
        for model in [
            "claude-sonnet-5-5",
            "anthropic/claude-opus-4-8",
            "glm-5.2",
            "ollama",
        ] {
            assert!(!uses_codex_patch_grammar(model), "{model}");
        }
    }

    #[test]
    fn capability_resolves_apply_patch_only_for_openai_models() {
        let capability = NativeEditToolsCapability::new();
        let tools = |model: Option<&str>| {
            capability
                .resolve_for_model(model)
                .expect("always resolves")
                .tools()
                .iter()
                .map(|tool| tool.name().to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(tools(Some("gpt-5.5")), vec![APPLY_PATCH_TOOL_NAME]);
        assert!(tools(Some("claude-sonnet-5-5")).is_empty());
        assert!(tools(None).is_empty());
    }

    /// The real collection path, as a turn sees it: the model in
    /// `SystemPromptContext` decides whether `apply_patch` is offered.
    #[tokio::test]
    async fn collection_offers_apply_patch_per_model() {
        use everruns_core::capabilities::{
            CapabilityRegistry, SystemPromptContext, collect_capabilities,
        };

        let mut registry = CapabilityRegistry::new();
        registry.register(NativeEditToolsCapability::new());
        let ids = vec![NATIVE_EDIT_TOOLS_CAPABILITY_ID.to_string()];
        let names = |collected: everruns_core::capabilities::CollectedCapabilities| {
            collected
                .tool_definitions
                .iter()
                .map(|definition| definition.name().to_string())
                .collect::<Vec<_>>()
        };
        for (model, expected) in [
            ("gpt-5.5", vec![APPLY_PATCH_TOOL_NAME.to_string()]),
            ("claude-sonnet-5-5", Vec::new()),
        ] {
            let ctx = SystemPromptContext::without_file_store(Default::default()).with_model(model);
            let collected = collect_capabilities(&ids, &registry, &ctx).await;
            assert_eq!(names(collected), expected, "model={model}");
        }
    }

    #[test]
    fn parses_every_operation() {
        let ops = parse_patch(
            "*** Begin Patch\n*** Add File: new.txt\n+hello\n*** Delete File: old.txt\n*** Update File: a.py\n*** Move to: b.py\n@@ def f():\n-    return 1\n+    return 2\n*** End Patch",
        )
        .expect("parse");
        assert_eq!(ops.len(), 3);
        assert_eq!(
            ops[0],
            PatchOp::Add {
                path: "new.txt".into(),
                contents: "hello\n".into()
            }
        );
        assert_eq!(
            ops[1],
            PatchOp::Delete {
                path: "old.txt".into()
            }
        );
        let PatchOp::Update {
            move_to, chunks, ..
        } = &ops[2]
        else {
            panic!("expected update");
        };
        assert_eq!(move_to.as_deref(), Some("b.py"));
        assert_eq!(chunks[0].context.as_deref(), Some("def f():"));
    }

    #[test]
    fn heredoc_wrapper_is_accepted() {
        let ops = parse_patch(
            "apply_patch <<'EOF'\n*** Begin Patch\n*** Delete File: x\n*** End Patch\nEOF\n",
        )
        .expect("parse");
        assert_eq!(ops.len(), 1);
    }

    #[test]
    fn bad_envelopes_are_rejected() {
        for (patch, expected) in [
            ("*** Update File: a\n-x\n+y\n*** End Patch", "first line"),
            ("*** Begin Patch\n*** Delete File: a", "last line"),
            ("*** Begin Patch\n*** End Patch", "no file operations"),
            (
                "*** Begin Patch\n*** Frobnicate File: a\n*** End Patch",
                "expected",
            ),
            (
                "*** Begin Patch\n*** Add File: a\nno plus\n*** End Patch",
                "must start with '+'",
            ),
            (
                "*** Begin Patch\n*** Update File: a\n*** End Patch",
                "no hunks",
            ),
            (
                "*** Begin Patch\n*** Update File: a\n?bad\n*** End Patch",
                "must start with",
            ),
        ] {
            let error = parse_patch(patch).expect_err(patch);
            assert!(error.contains(expected), "{patch:?} -> {error}");
        }
    }

    #[test]
    fn applies_hunks_with_context_and_preserves_crlf() {
        let chunks = match &parse_patch(
            "*** Begin Patch\n*** Update File: f\n@@ fn two\n a\n-b\n+B\n*** End Patch",
        )
        .unwrap()[0]
        {
            PatchOp::Update { chunks, .. } => chunks.clone(),
            _ => unreachable!(),
        };
        // The first `a`/`b` pair sits before the context line and must not match.
        let out = apply_chunks("a\nb\nfn two\na\nb\n", &chunks, "f").unwrap();
        assert_eq!(out, "a\nb\nfn two\na\nB\n");
        let out = apply_chunks("x\r\nfn two\r\na\r\nb\r\n", &chunks, "f").unwrap();
        assert_eq!(out, "x\r\nfn two\r\na\r\nB\r\n");
    }

    #[test]
    fn whitespace_drift_in_context_still_matches() {
        let chunks = match &parse_patch(
            "*** Begin Patch\n*** Update File: f\n     keep\n-old\n+new\n*** End Patch",
        )
        .unwrap()[0]
        {
            PatchOp::Update { chunks, .. } => chunks.clone(),
            _ => unreachable!(),
        };
        let out = apply_chunks("  keep  \nold\n", &chunks, "f").unwrap();
        assert_eq!(out, "  keep  \nnew\n");
    }

    #[tokio::test]
    async fn add_update_move_and_delete_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("lib.rs"), "fn a() {}\nfn b() {\n    1\n}\n").unwrap();
        std::fs::write(dir.path().join("old.txt"), "bye\n").unwrap();
        std::fs::write(dir.path().join("from.txt"), "keep\nchange\n").unwrap();

        let result = apply(
            dir.path(),
            "*** Begin Patch\n*** Add File: src/new.rs\n+pub fn new() {}\n*** Update File: lib.rs\n@@ fn b() {\n-    1\n+    2\n*** Delete File: old.txt\n*** Update File: from.txt\n*** Move to: to.txt\n keep\n-change\n+changed\n*** End Patch",
        )
        .await;
        let ToolExecutionResult::Success(value) = result else {
            panic!("expected success, got {result:?}");
        };
        let output = value["output"].as_str().unwrap();
        assert!(output.starts_with("Success. Updated the following files:"));
        assert!(output.contains("D ") && output.contains("A ") && output.contains("M "));

        let read = |name: &str| std::fs::read_to_string(dir.path().join(name)).unwrap();
        assert_eq!(read("src/new.rs"), "pub fn new() {}\n");
        assert_eq!(read("lib.rs"), "fn a() {}\nfn b() {\n    2\n}\n");
        assert_eq!(read("to.txt"), "keep\nchanged\n");
        assert!(!dir.path().join("old.txt").exists());
        assert!(!dir.path().join("from.txt").exists());
    }

    #[tokio::test]
    async fn move_onto_the_same_path_keeps_the_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        let result = apply(
            dir.path(),
            "*** Begin Patch\n*** Update File: a.txt\n*** Move to: a.txt\n-one\n+two\n*** End Patch",
        )
        .await;
        assert!(
            matches!(result, ToolExecutionResult::Success(_)),
            "{result:?}"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "two\n"
        );
    }

    #[tokio::test]
    async fn missing_context_fails_without_writing_anything() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "one\ntwo\n").unwrap();

        let error = error_text(
            apply(
                dir.path(),
                "*** Begin Patch\n*** Add File: created.txt\n+x\n*** Update File: a.txt\n one\n-three\n+3\n*** End Patch",
            )
            .await,
        );
        assert!(
            error.contains("failed to find the expected lines"),
            "{error}"
        );
        // The add planned before the failing update must not have landed.
        assert!(!dir.path().join("created.txt").exists());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "one\ntwo\n"
        );

        let error = error_text(
            apply(
                dir.path(),
                "*** Begin Patch\n*** Update File: a.txt\n@@ nowhere\n-one\n+1\n*** End Patch",
            )
            .await,
        );
        assert!(
            error.contains("failed to find context line 'nowhere'"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn deleting_or_updating_a_missing_file_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("keep.txt"), "keep\n").unwrap();

        let error = error_text(
            apply(
                dir.path(),
                "*** Begin Patch\n*** Delete File: keep.txt\n*** Delete File: ghost.txt\n*** End Patch",
            )
            .await,
        );
        assert!(
            error.contains("ghost.txt") && error.contains("file not found"),
            "{error}"
        );
        assert!(
            dir.path().join("keep.txt").exists(),
            "nothing is deleted on failure"
        );

        let error = error_text(
            apply(
                dir.path(),
                "*** Begin Patch\n*** Update File: ghost.txt\n-a\n+b\n*** End Patch",
            )
            .await,
        );
        assert!(error.contains("file not found"), "{error}");
    }

    #[tokio::test]
    async fn bad_envelope_and_missing_input_are_tool_errors() {
        let dir = tempfile::tempdir().unwrap();
        let error = error_text(apply(dir.path(), "--- a/x\n+++ b/x\n").await);
        assert!(error.contains("*** Begin Patch"), "{error}");
        let error = error_text(
            ApplyPatchTool
                .execute_with_context(json!({}), &context(dir.path()))
                .await,
        );
        assert!(error.contains("input"), "{error}");
    }

    #[test]
    fn delete_detection_is_textual_and_conservative() {
        assert!(patch_deletes_files(
            "*** Begin Patch\n*** Delete File: a\n*** End Patch"
        ));
        assert!(!patch_deletes_files(
            "*** Begin Patch\n*** Update File: a\n-x\n+y\n*** End Patch"
        ));
    }
}

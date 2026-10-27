//! Deterministic workspace text search (`rg` tool, `utils` capability).
//!
//! The `rg` tool runs the same engine as `yolop utils rg`, so scripted
//! checks reproduce agent results exactly. Model output is bounded on
//! purpose: at most `max_results` lines and 64 KiB total, long content lines
//! cut at 2000 characters, and `truncated` reports when a bound cut the
//! results (counts are lower bounds then). Prefer this tool over shelling out
//! to `rg`, which may not be installed.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use everruns_contracts::ToolHints;
use everruns_core::tool_narration::ToolNarrationPhase;
use everruns_core::{Capability, CapabilityStatus, Tool, ToolExecutionResult};
use serde_json::{Value, json};

use crate::exec::workspace_host::{WorkspaceHost, resolve_host_scope};
use crate::utils::{self, RgOptions};

pub(crate) const UTILS_CAPABILITY_ID: &str = "utils";

/// Default and ceiling for the `max_results` tool argument.
const DEFAULT_MAX_RESULTS: u64 = 200;
const MAX_RESULTS_LIMIT: u64 = 2000;
/// Fixed model output bounds (not tool arguments, keeps the schema tight).
const OUTPUT_BYTE_CAP: u64 = 65536;
const OUTPUT_COLUMN_CAP: usize = 2000;

#[cfg(windows)]
const RG_BINARY_NAMES: [&str; 2] = ["rg.exe", "rg"];
#[cfg(not(windows))]
const RG_BINARY_NAMES: [&str; 1] = ["rg"];

/// True when a system `rg` binary resolves on `PATH`.
///
/// The built-in `rg` tool only registers when this is false: machines with
/// a real ripgrep keep using that, and everywhere else the tool surface
/// stays identical. Any lookup failure means false, so a broken `PATH`
/// still gets a working search tool.
pub(crate) fn system_rg_available() -> bool {
    std::env::var_os("PATH").is_some_and(|var| rg_found_on_search_path(&var))
}

fn rg_found_on_search_path(path_var: &std::ffi::OsString) -> bool {
    std::env::split_paths(path_var)
        .flat_map(|dir| RG_BINARY_NAMES.iter().map(move |name| dir.join(name)))
        .any(|candidate| is_runnable_file(&candidate))
}

fn is_runnable_file(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

#[derive(Clone)]
pub(crate) struct RgCapability {
    workspace: Arc<WorkspaceHost>,
}

impl RgCapability {
    pub(crate) fn new(workspace: Arc<WorkspaceHost>) -> Self {
        Self { workspace }
    }
}

impl Capability for RgCapability {
    fn id(&self) -> &'static str {
        UTILS_CAPABILITY_ID
    }

    fn name(&self) -> &'static str {
        "Utils"
    }

    fn description(&self) -> &'static str {
        "Deterministic local helpers for scripts and agents. Currently: rg, \
         a ripgrep-compatible workspace search sharing its \
         engine with `yolop utils rg`, registered only when no system `rg` is on `PATH`."
    }

    fn status(&self) -> CapabilityStatus {
        CapabilityStatus::Available
    }

    fn category(&self) -> Option<&str> {
        Some("Code Navigation")
    }

    fn tools(&self) -> Vec<Box<dyn Tool>> {
        vec![Box::new(RgTool {
            workspace: self.workspace.clone(),
        })]
    }

    fn system_prompt_addition(&self) -> Option<&str> {
        // Tool definitions already cover the flags; only the non-inferable
        // routing guidance belongs here.
        Some(
            "Use the `rg` tool (not `rg` via bash, which may be missing) for \
             recursive workspace text search; `yolop utils rg` runs the same \
             engine for scripts. Prefer `ast_grep` for structural matches.",
        )
    }
}

#[derive(Clone)]
pub(crate) struct RgTool {
    workspace: Arc<WorkspaceHost>,
}

/// Tool arguments mirroring the `yolop utils rg` flag subset that makes
/// sense for the model (no `--color`/`--heading` compatibility no-ops, no
/// `--null`; filenames and line numbers always on).
#[derive(Debug, Default)]
struct RgToolArgs {
    patterns: Vec<String>,
    paths: Vec<String>,
    files: bool,
    fixed_strings: bool,
    ignore_case: bool,
    smart_case: bool,
    case_sensitive: bool,
    word: bool,
    line_regexp: bool,
    invert_match: bool,
    globs: Vec<String>,
    hidden: bool,
    no_ignore: bool,
    follow: bool,
    max_depth: Option<usize>,
    files_with_matches: bool,
    count: bool,
    max_count: Option<u64>,
    before: usize,
    after: usize,
    column: bool,
    vimgrep: bool,
    max_results: u64,
}

fn arg_str(args: &Value, key: &str) -> Option<String> {
    args.get(key).and_then(Value::as_str).map(str::to_string)
}

fn arg_bool(args: &Value, key: &str) -> bool {
    args.get(key).and_then(Value::as_bool).unwrap_or(false)
}

fn arg_u64(args: &Value, key: &str) -> Option<u64> {
    args.get(key).and_then(Value::as_u64)
}

fn arg_usize(args: &Value, key: &str) -> Option<usize> {
    arg_u64(args, key).and_then(|n| usize::try_from(n).ok())
}

fn arg_list(args: &Value, key: &str) -> Vec<String> {
    args.get(key)
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

impl RgToolArgs {
    fn from_arguments(arguments: &Value) -> Result<Self> {
        let mut patterns = Vec::new();
        if let Some(pattern) = arg_str(arguments, "pattern") {
            patterns.push(pattern);
        }
        patterns.extend(arg_list(arguments, "regexps"));
        let context = arg_usize(arguments, "context").unwrap_or(0);
        let max_results = arg_u64(arguments, "max_results")
            .unwrap_or(DEFAULT_MAX_RESULTS)
            .clamp(1, MAX_RESULTS_LIMIT);
        Ok(Self {
            patterns,
            paths: arg_list(arguments, "paths"),
            files: arg_bool(arguments, "files"),
            fixed_strings: arg_bool(arguments, "fixed_strings"),
            ignore_case: arg_bool(arguments, "ignore_case"),
            smart_case: arg_bool(arguments, "smart_case"),
            case_sensitive: arg_bool(arguments, "case_sensitive"),
            word: arg_bool(arguments, "word"),
            line_regexp: arg_bool(arguments, "line_regexp"),
            invert_match: arg_bool(arguments, "invert_match"),
            globs: arg_list(arguments, "globs"),
            hidden: arg_bool(arguments, "hidden"),
            no_ignore: arg_bool(arguments, "no_ignore"),
            follow: arg_bool(arguments, "follow"),
            max_depth: arg_usize(arguments, "max_depth"),
            files_with_matches: arg_bool(arguments, "files_with_matches"),
            count: arg_bool(arguments, "count"),
            max_count: arg_u64(arguments, "max_count"),
            before: arg_usize(arguments, "before_context")
                .or(Some(context))
                .unwrap_or(0),
            after: arg_usize(arguments, "after_context")
                .or(Some(context))
                .unwrap_or(0),
            column: arg_bool(arguments, "column"),
            vimgrep: arg_bool(arguments, "vimgrep"),
            max_results,
        })
    }
}

impl RgTool {
    fn run(&self, arguments: &Value) -> Result<Value> {
        let args = RgToolArgs::from_arguments(arguments)?;
        if args.patterns.is_empty() && !args.files {
            anyhow::bail!("no pattern given (pass pattern or files=true)");
        }
        let workspace_root = self
            .workspace
            .host_root()
            .map_err(|err| anyhow::anyhow!("workspace root unavailable: {err}"))?
            .canonicalize()
            .context("cannot resolve workspace root")?;
        let requested = if args.paths.is_empty() {
            vec![".".to_string()]
        } else {
            args.paths.clone()
        };
        let mut roots = Vec::with_capacity(requested.len());
        for path in &requested {
            // Scope enforcement is free: outside-workspace paths fail here.
            let scoped = resolve_host_scope(&workspace_root, Some(path))
                .with_context(|| format!("path rejected (outside workspace): {path}"))?;
            roots.push(scoped.into_path_buf());
        }

        let opts = RgOptions {
            patterns: args.patterns.clone(),
            paths: roots,
            files: args.files,
            fixed_strings: args.fixed_strings,
            ignore_case: args.ignore_case,
            smart_case: args.smart_case,
            case_sensitive: args.case_sensitive,
            word: args.word,
            line_regexp: args.line_regexp,
            invert_match: args.invert_match,
            globs: args.globs.clone(),
            hidden: args.hidden,
            no_ignore: args.no_ignore,
            follow: args.follow,
            max_depth: args.max_depth,
            unrestricted: 0,
            files_with_matches: args.files_with_matches,
            count: args.count,
            quiet: false,
            max_count: args.max_count,
            before: args.before,
            after: args.after,
            line_number: true,
            column: args.column,
            vimgrep: args.vimgrep,
            // Filenames always on: the model needs paths to act on results.
            with_filename: true,
            no_filename: false,
            null: false,
            max_lines: Some(args.max_results),
            max_bytes: Some(OUTPUT_BYTE_CAP),
            max_columns: Some(OUTPUT_COLUMN_CAP),
        };
        let mut buf = Vec::new();
        let stats = utils::run_rg(&opts, &mut buf).context("search failed")?;

        // Stable relative paths: strip the canonical root prefix line-wise.
        // Every engine output line is either path-led or the `--` separator.
        let prefix = format!("{}{}", workspace_root.display(), std::path::MAIN_SEPARATOR);
        let output = String::from_utf8_lossy(&buf)
            .lines()
            .map(|line| line.strip_prefix(&prefix).unwrap_or(line))
            .collect::<Vec<_>>()
            .join("\n");
        Ok(json!({
            "ok": true,
            "workspace_root": workspace_root.display().to_string(),
            "patterns": args.patterns,
            "paths": requested,
            "output": output,
            "matching_lines": stats.matching_lines,
            "files_with_hits": stats.files_with_hits,
            "truncated": stats.truncated,
        }))
    }
}

#[async_trait]
impl Tool for RgTool {
    fn narrate(
        &self,
        tool_call: &everruns_contracts::tool_types::ToolCall,
        phase: ToolNarrationPhase,
        locale: Option<&str>,
        ctx: everruns_core::tool_narration::ToolNarrationContext<'_>,
    ) -> Option<String> {
        Some(everruns_core::tool_narration::narrate_grep_files(
            &tool_call.arguments,
            phase,
            locale,
            ctx,
        ))
    }

    fn name(&self) -> &'static str {
        "rg"
    }

    fn display_name(&self) -> Option<&str> {
        Some("Text search")
    }

    fn description(&self) -> &'static str {
        "Deterministic recursive text search with familiar ripgrep flags \
         (pattern, paths, fixed_strings, ignore_case, smart_case, word, \
         globs, context flags, files_with_matches, count, vimgrep). Same \
         engine as `yolop utils rg`, so scripted checks reproduce agent \
         results exactly. Output is path:line:content text, bounded by \
         max_results (default 200); truncated reports when bounds cut the \
         results and counts are lower bounds then. Prefer for codebase-wide \
         text search; prefer ast_grep for syntax-aware structural search and \
         repo_map for symbol-level orientation. Prefer this tool over shelling out \
         to `rg`, which may not be installed."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Text pattern to search for (regex unless fixed_strings). Required unless files is true."
                },
                "paths": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Workspace-relative files or directories to search. Defaults to the workspace root."
                },
                "regexps": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Additional patterns ORed with pattern."
                },
                "files": {
                    "type": "boolean",
                    "description": "List files that would be searched instead of searching.",
                    "default": false
                },
                "fixed_strings": {
                    "type": "boolean",
                    "description": "Treat the pattern as a literal string.",
                    "default": false
                },
                "ignore_case": {
                    "type": "boolean",
                    "description": "Case insensitive search.",
                    "default": false
                },
                "smart_case": {
                    "type": "boolean",
                    "description": "Case insensitive when the pattern is all lowercase.",
                    "default": false
                },
                "case_sensitive": {
                    "type": "boolean",
                    "description": "Force case sensitive search.",
                    "default": false
                },
                "word": {
                    "type": "boolean",
                    "description": "Match whole words only.",
                    "default": false
                },
                "line_regexp": {
                    "type": "boolean",
                    "description": "Match entire lines only.",
                    "default": false
                },
                "invert_match": {
                    "type": "boolean",
                    "description": "Report lines that do not match.",
                    "default": false
                },
                "globs": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Gitignore-style file filters, repeatable. Globs without a slash match the file name; ! negates."
                },
                "hidden": {
                    "type": "boolean",
                    "description": "Search hidden files and directories.",
                    "default": false
                },
                "no_ignore": {
                    "type": "boolean",
                    "description": "Ignore .gitignore, .ignore, and global excludes.",
                    "default": false
                },
                "follow": {
                    "type": "boolean",
                    "description": "Follow symbolic links.",
                    "default": false
                },
                "max_depth": {
                    "type": "integer",
                    "description": "Descend at most NUM directories."
                },
                "files_with_matches": {
                    "type": "boolean",
                    "description": "Print only the paths of files with a match.",
                    "default": false
                },
                "count": {
                    "type": "boolean",
                    "description": "Print per-file match counts instead of matching lines.",
                    "default": false
                },
                "max_count": {
                    "type": "integer",
                    "description": "Stop each file after NUM matching lines."
                },
                "context": {
                    "type": "integer",
                    "description": "Show NUM lines before and after each match."
                },
                "before_context": {
                    "type": "integer",
                    "description": "Show NUM lines before each match."
                },
                "after_context": {
                    "type": "integer",
                    "description": "Show NUM lines after each match."
                },
                "column": {
                    "type": "boolean",
                    "description": "Show the byte column of the first match on each line.",
                    "default": false
                },
                "vimgrep": {
                    "type": "boolean",
                    "description": "path:line:column:content on every line.",
                    "default": false
                },
                "max_results": {
                    "type": "integer",
                    "description": "Stop after NUM output lines. Clamped to 2000.",
                    "default": 200
                }
            }
        })
    }

    fn hints(&self) -> ToolHints {
        ToolHints::default().with_readonly(true)
    }

    async fn execute(&self, arguments: Value) -> ToolExecutionResult {
        match self.run(&arguments) {
            Ok(value) => ToolExecutionResult::success(value),
            Err(err) => ToolExecutionResult::tool_error(format!("{err:#}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::{Path, PathBuf};

    fn host(root: &Path) -> Arc<WorkspaceHost> {
        Arc::new(
            WorkspaceHost::new(
                Arc::new(std::sync::RwLock::new(root.to_path_buf())),
                root.to_path_buf(),
            )
            .expect("workspace host"),
        )
    }

    fn rg_capability() -> RgCapability {
        let dir = tempfile::tempdir().expect("tempdir");
        // Leak the tempdir: the capability only needs a valid workspace root
        // for metadata assertions, never for I/O.
        let root = dir.keep();
        RgCapability::new(host(&root))
    }

    fn rg_tool() -> (tempfile::TempDir, RgTool) {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(
            dir.path().join("a.txt"),
            "hello world\nfoo bar\nhello again\n",
        )
        .expect("write a.txt");
        fs::create_dir(dir.path().join("sub")).expect("mkdir sub");
        fs::write(dir.path().join("sub/b.txt"), "nothing here\nhello there\n")
            .expect("write b.txt");
        let tool = RgTool {
            workspace: host(dir.path()),
        };
        (dir, tool)
    }

    fn search_path(dirs: &[PathBuf]) -> std::ffi::OsString {
        std::env::join_paths(dirs.iter().collect::<Vec<_>>()).expect("join paths")
    }

    fn rg_binary_name() -> &'static str {
        if cfg!(windows) { "rg.exe" } else { "rg" }
    }

    #[cfg(unix)]
    fn make_executable(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(path).expect("metadata").permissions();
        perms.set_mode(0o755);
        fs::set_permissions(path, perms).expect("chmod");
    }

    #[cfg(not(unix))]
    fn make_executable(_path: &Path) {}

    #[test]
    fn probe_finds_rg_on_search_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let candidate = dir.path().join(rg_binary_name());
        fs::write(&candidate, "#!/bin/sh\nexit 0\n").expect("write fake rg");
        make_executable(&candidate);
        assert!(rg_found_on_search_path(&search_path(&[dir
            .path()
            .to_path_buf()])));
    }

    #[test]
    fn probe_misses_empty_search_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(!rg_found_on_search_path(&search_path(&[dir
            .path()
            .to_path_buf()])));
    }

    #[test]
    fn probe_rejects_non_runnable_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join(rg_binary_name()), "not an executable").expect("write");
        #[cfg(unix)]
        assert!(!rg_found_on_search_path(&search_path(&[dir
            .path()
            .to_path_buf()])));
        #[cfg(not(unix))]
        assert!(rg_found_on_search_path(&search_path(&[dir
            .path()
            .to_path_buf()])));
    }

    #[test]
    fn capability_exposes_rg_tool() {
        let capability = rg_capability();
        assert_eq!(capability.id(), UTILS_CAPABILITY_ID);
        assert!(!capability.name().is_empty());
        assert!(!capability.description().is_empty());
        let tools = capability.tools();
        let names: Vec<_> = tools.iter().map(|tool| tool.name()).collect();
        assert_eq!(names, vec!["rg"]);
    }

    #[tokio::test]
    async fn rg_finds_pattern_with_relative_paths() {
        let (_dir, tool) = rg_tool();
        let result = tool.execute(json!({ "pattern": "hello" })).await;
        let ToolExecutionResult::Success(value) = result else {
            panic!("expected success, got {result:?}");
        };
        let output = value.get("output").and_then(Value::as_str).unwrap_or("");
        assert!(output.contains("a.txt:1:hello world"), "got:\n{output}");
        assert!(output.contains("a.txt:3:hello again"), "got:\n{output}");
        assert!(
            output.contains(&format!(
                "sub{}b.txt:2:hello there",
                std::path::MAIN_SEPARATOR
            )),
            "got:\n{output}"
        );
        assert!(!output.contains("foo bar"), "got:\n{output}");
        assert_eq!(value.get("matching_lines").and_then(Value::as_u64), Some(3));
        assert_eq!(value.get("truncated").and_then(Value::as_bool), Some(false));
        // No absolute workspace paths leak into model output.
        for line in output.lines() {
            assert!(!line.starts_with('/'), "absolute path leaked: {line}");
        }
    }

    #[tokio::test]
    async fn rg_rejects_paths_outside_the_workspace() {
        let (_dir, tool) = rg_tool();
        let result = tool
            .execute(json!({ "pattern": "hello", "paths": [".."] }))
            .await;
        assert!(
            matches!(result, ToolExecutionResult::ToolError(_)),
            "expected error, got {result:?}"
        );
    }

    #[tokio::test]
    async fn rg_reports_missing_pattern() {
        let (_dir, tool) = rg_tool();
        let result = tool.execute(json!({})).await;
        assert!(
            matches!(result, ToolExecutionResult::ToolError(_)),
            "expected error, got {result:?}"
        );
    }

    #[tokio::test]
    async fn rg_marks_truncated_results() {
        let (_dir, tool) = rg_tool();
        let result = tool
            .execute(json!({ "pattern": "hello", "max_results": 1 }))
            .await;
        let ToolExecutionResult::Success(value) = result else {
            panic!("expected success, got {result:?}");
        };
        assert_eq!(value.get("truncated").and_then(Value::as_bool), Some(true));
        let output = value.get("output").and_then(Value::as_str).unwrap_or("");
        assert_eq!(output.lines().count(), 1, "got:\n{output}");
    }

    #[tokio::test]
    async fn rg_lists_files_with_matches() {
        let (_dir, tool) = rg_tool();
        let result = tool
            .execute(json!({ "pattern": "hello", "files_with_matches": true }))
            .await;
        let ToolExecutionResult::Success(value) = result else {
            panic!("expected success, got {result:?}");
        };
        let output = value.get("output").and_then(Value::as_str).unwrap_or("");
        assert!(output.contains("a.txt"), "got:\n{output}");
        assert!(!output.contains("hello world"), "got:\n{output}");
    }
}

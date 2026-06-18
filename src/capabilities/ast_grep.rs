//! Read-only structural search backed by ast-grep.
//!
//! `grep_files` remains the right first move for exact text. This tool is for
//! code shapes: call expressions, function declarations, wrappers, and other
//! patterns where lexical search is too noisy.

use anyhow::{Context, Result, bail};
use ast_grep_core::matcher::Pattern;
use ast_grep_core::meta_var::MetaVariable;
use ast_grep_core::tree_sitter::StrDoc;
use ast_grep_core::{AstGrep, Node};
use ast_grep_language::SupportLang;
use async_trait::async_trait;
use everruns_core::capabilities::{Capability, CapabilityStatus, SystemPromptContext};
use everruns_core::tool_narration::ToolNarrationPhase;
use everruns_core::tools::{Tool, ToolExecutionResult};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::fs;
use std::path::{Component, Path, PathBuf};

pub(crate) const AST_GREP_CAPABILITY_ID: &str = "ast_grep";

const DEFAULT_LIMIT: usize = 50;
const MAX_LIMIT: usize = 500;
const DEFAULT_MAX_FILE_BYTES: usize = 512 * 1024;
const MAX_FILE_BYTES: usize = 2 * 1024 * 1024;
const MAX_MATCH_TEXT_CHARS: usize = 500;
const MAX_CAPTURE_TEXT_CHARS: usize = 240;
const SKIP_DIRS: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    "target",
    "node_modules",
    ".next",
    "dist",
    "build",
    ".direnv",
    ".venv",
    "venv",
];

#[derive(Clone)]
pub(crate) struct AstGrepCapability {
    workspace_root: PathBuf,
}

impl AstGrepCapability {
    pub(crate) fn new(workspace_root: PathBuf) -> Self {
        Self { workspace_root }
    }
}

#[async_trait]
impl Capability for AstGrepCapability {
    fn id(&self) -> &str {
        AST_GREP_CAPABILITY_ID
    }

    fn name(&self) -> &str {
        "AST Grep"
    }

    fn description(&self) -> &str {
        "Read-only multi-language structural code search backed by ast-grep patterns."
    }

    fn status(&self) -> CapabilityStatus {
        CapabilityStatus::Available
    }

    fn category(&self) -> Option<&str> {
        Some("Code Navigation")
    }

    async fn system_prompt_contribution(&self, _ctx: &SystemPromptContext) -> Option<String> {
        Some(
            "<capability id=\"ast_grep\">\n\
             For structural code search, call `ast_grep` with an ast-grep pattern and usually \
             a language filter. Use it after repo-map/grep have narrowed the area, and read \
             matched files before editing.\n\
             </capability>"
                .to_string(),
        )
    }

    fn system_prompt_preview(&self) -> Option<String> {
        Some(
            "<capability id=\"ast_grep\">\n\
             Use `ast_grep` for read-only structural pattern search.\n\
             </capability>"
                .to_string(),
        )
    }

    fn tools(&self) -> Vec<Box<dyn Tool>> {
        vec![Box::new(AstGrepTool {
            workspace_root: self.workspace_root.clone(),
        })]
    }
}

struct AstGrepTool {
    workspace_root: PathBuf,
}

#[async_trait]
impl Tool for AstGrepTool {
    fn narrate(
        &self,
        tool_call: &everruns_core::tool_types::ToolCall,
        phase: ToolNarrationPhase,
        locale: Option<&str>,
    ) -> Option<String> {
        Some(everruns_core::tool_narration::narrate_grep_files(
            &tool_call.arguments,
            phase,
            locale,
        ))
    }

    fn name(&self) -> &str {
        "ast_grep"
    }

    fn display_name(&self) -> Option<&str> {
        Some("AST grep")
    }

    fn description(&self) -> &str {
        "Search workspace code with ast-grep structural patterns. Supports Rust, Python, \
         TypeScript/TSX, JavaScript/JSX, C#, Go, CSS, HTML, and Bash."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Required ast-grep pattern, for example `fn $NAME($$$ARGS) { $$$BODY }`."
                },
                "path": {
                    "type": "string",
                    "description": "Optional workspace-relative file or directory to scan. Defaults to the workspace root."
                },
                "language": {
                    "type": "string",
                    "description": "Optional language filter. Supported values include rust, python, typescript, tsx, javascript, csharp, go, css, html, and bash."
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": MAX_LIMIT,
                    "description": "Maximum number of matches to return. Defaults to 50."
                },
                "max_file_bytes": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": MAX_FILE_BYTES,
                    "description": "Skip supported source files larger than this many bytes. Defaults to 524288."
                }
            },
            "required": ["pattern"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, arguments: Value) -> ToolExecutionResult {
        match ast_grep_from_arguments(&self.workspace_root, &arguments) {
            Ok(report) => ToolExecutionResult::success(json!({
                "ok": true,
                "workspace_root": self.workspace_root.display().to_string(),
                "path": report.path,
                "pattern": report.pattern,
                "language": report.language,
                "languages": report.languages,
                "count": report.matches.len(),
                "scanned_files": report.scanned_files,
                "skipped_unsupported_files": report.skipped_unsupported_files,
                "skipped_files": report.skipped_files,
                "skipped_large_files": report.skipped_large_files,
                "parse_error_files": report.parse_error_files,
                "pattern_error_languages": report.pattern_error_languages,
                "truncated": report.truncated,
                "matches": report.matches,
            })),
            Err(err) => ToolExecutionResult::tool_error(err.to_string()),
        }
    }
}

fn ast_grep_from_arguments(workspace_root: &Path, arguments: &Value) -> Result<AstGrepReport> {
    let pattern = arguments
        .get("pattern")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|pattern| !pattern.is_empty())
        .context("`pattern` is required")?
        .to_string();
    let path = arguments
        .get("path")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .map(ToOwned::to_owned);
    let language = arguments
        .get("language")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|language| !language.is_empty())
        .map(ToOwned::to_owned);
    let limit = bounded_usize(arguments.get("limit"), DEFAULT_LIMIT, MAX_LIMIT, "limit")?;
    let max_file_bytes = bounded_usize(
        arguments.get("max_file_bytes"),
        DEFAULT_MAX_FILE_BYTES,
        MAX_FILE_BYTES,
        "max_file_bytes",
    )?;

    ast_grep(
        workspace_root,
        AstGrepOptions {
            pattern,
            path,
            language,
            limit,
            max_file_bytes,
        },
    )
}

#[derive(Clone, Debug)]
struct AstGrepOptions {
    pattern: String,
    path: Option<String>,
    language: Option<String>,
    limit: usize,
    max_file_bytes: usize,
}

#[derive(Clone, Debug)]
struct SourceFile {
    path: PathBuf,
    language: &'static LanguageSpec,
}

#[derive(Clone, Copy, Debug)]
struct LanguageSpec {
    name: &'static str,
    ast_lang: SupportLang,
    filters: &'static [&'static str],
    extensions: &'static [&'static str],
    filenames: &'static [&'static str],
}

const LANGUAGES: &[LanguageSpec] = &[
    LanguageSpec {
        name: "rust",
        ast_lang: SupportLang::Rust,
        filters: &["rust", "rs"],
        extensions: &["rs"],
        filenames: &[],
    },
    LanguageSpec {
        name: "python",
        ast_lang: SupportLang::Python,
        filters: &["python", "py"],
        extensions: &["py", "pyi", "py3", "bzl"],
        filenames: &[],
    },
    LanguageSpec {
        name: "typescript",
        ast_lang: SupportLang::TypeScript,
        filters: &["typescript", "ts"],
        extensions: &["ts", "mts", "cts"],
        filenames: &[],
    },
    LanguageSpec {
        name: "tsx",
        ast_lang: SupportLang::Tsx,
        filters: &["tsx"],
        extensions: &["tsx"],
        filenames: &[],
    },
    LanguageSpec {
        name: "javascript",
        ast_lang: SupportLang::JavaScript,
        filters: &["javascript", "js", "jsx"],
        extensions: &["js", "jsx", "mjs", "cjs"],
        filenames: &[],
    },
    LanguageSpec {
        name: "csharp",
        ast_lang: SupportLang::CSharp,
        filters: &["csharp", "cs", "c#"],
        extensions: &["cs"],
        filenames: &[],
    },
    LanguageSpec {
        name: "go",
        ast_lang: SupportLang::Go,
        filters: &["go", "golang"],
        extensions: &["go"],
        filenames: &[],
    },
    LanguageSpec {
        name: "css",
        ast_lang: SupportLang::Css,
        filters: &["css", "scss"],
        extensions: &["css", "scss"],
        filenames: &[],
    },
    LanguageSpec {
        name: "html",
        ast_lang: SupportLang::Html,
        filters: &["html"],
        extensions: &["html", "htm", "xhtml"],
        filenames: &[],
    },
    LanguageSpec {
        name: "bash",
        ast_lang: SupportLang::Bash,
        filters: &["bash", "sh", "zsh", "shell"],
        extensions: &[
            "bash", "bats", "cgi", "command", "env", "fcgi", "ksh", "sh", "zsh",
        ],
        filenames: &[".bashrc", ".bash_profile", ".zshrc", ".profile"],
    },
];

impl LanguageSpec {
    fn matches_filter(&self, filter: &str) -> bool {
        self.filters
            .iter()
            .any(|candidate| candidate.eq_ignore_ascii_case(filter))
    }
}

#[derive(Debug, Serialize)]
struct AstGrepReport {
    path: String,
    pattern: String,
    language: Option<String>,
    languages: Vec<String>,
    scanned_files: usize,
    skipped_unsupported_files: usize,
    skipped_files: usize,
    skipped_large_files: usize,
    parse_error_files: usize,
    pattern_error_languages: Vec<PatternLanguageError>,
    truncated: bool,
    matches: Vec<AstGrepMatch>,
}

#[derive(Debug, Serialize)]
struct PatternLanguageError {
    language: String,
    error: String,
}

#[derive(Debug, Serialize)]
struct AstGrepMatch {
    path: String,
    language: String,
    kind: String,
    line: usize,
    column: usize,
    end_line: usize,
    end_column: usize,
    byte_start: usize,
    byte_end: usize,
    text: String,
    captures: Vec<AstGrepCapture>,
}

#[derive(Debug, Serialize)]
struct AstGrepCapture {
    name: String,
    text: String,
}

fn ast_grep(workspace_root: &Path, options: AstGrepOptions) -> Result<AstGrepReport> {
    let root = workspace_root
        .canonicalize()
        .context("canonicalize workspace root")?;
    let scope = resolve_scope(&root, options.path.as_deref())?;
    let language_filter = match options.language.as_deref() {
        Some(language) => Some(resolve_language_filter(language)?),
        None => None,
    };

    let mut files = Vec::new();
    let mut skipped_unsupported_files = 0;
    collect_supported_files(
        &scope,
        language_filter,
        &mut files,
        &mut skipped_unsupported_files,
    )?;

    let mut matches = Vec::new();
    let mut scanned_files = 0;
    let mut skipped_files = 0;
    let mut skipped_large_files = 0;
    let mut parse_error_files = 0;
    let mut pattern_errors = HashMap::<&'static str, String>::new();
    let mut compiled_patterns = HashMap::<&'static str, Pattern>::new();

    for file in files {
        if matches.len() >= options.limit {
            break;
        }
        let metadata = match fs::metadata(&file.path) {
            Ok(metadata) => metadata,
            Err(_) => {
                skipped_files += 1;
                continue;
            }
        };
        if !metadata.is_file() {
            skipped_files += 1;
            continue;
        }
        if metadata.len() as usize > options.max_file_bytes {
            skipped_large_files += 1;
            continue;
        }
        let pattern = match compiled_patterns.get(file.language.name) {
            Some(pattern) => pattern,
            None if pattern_errors.contains_key(file.language.name) => continue,
            None => match Pattern::try_new(&options.pattern, file.language.ast_lang) {
                Ok(pattern) => {
                    compiled_patterns.insert(file.language.name, pattern);
                    compiled_patterns
                        .get(file.language.name)
                        .expect("compiled pattern inserted")
                }
                Err(err) => {
                    pattern_errors.insert(file.language.name, err.to_string());
                    continue;
                }
            },
        };
        let source = match fs::read_to_string(&file.path) {
            Ok(source) => source,
            Err(_) => {
                skipped_files += 1;
                continue;
            }
        };
        scanned_files += 1;
        let doc = match StrDoc::try_new(&source, file.language.ast_lang) {
            Ok(doc) => doc,
            Err(_) => {
                parse_error_files += 1;
                continue;
            }
        };
        let root_ast = AstGrep::doc(doc);
        let remaining = options.limit - matches.len();
        for node_match in root_ast.root().find_all(pattern).take(remaining) {
            matches.push(format_match(&root, &file, &node_match));
        }
    }

    let mut languages = matches
        .iter()
        .map(|mat| mat.language.clone())
        .collect::<Vec<_>>();
    languages.sort();
    languages.dedup();

    let mut pattern_error_languages = pattern_errors
        .into_iter()
        .map(|(language, error)| PatternLanguageError {
            language: language.to_string(),
            error,
        })
        .collect::<Vec<_>>();
    pattern_error_languages.sort_by(|left, right| left.language.cmp(&right.language));

    Ok(AstGrepReport {
        path: options.path.unwrap_or_else(|| ".".to_string()),
        pattern: options.pattern,
        language: options.language,
        languages,
        scanned_files,
        skipped_unsupported_files,
        skipped_files,
        skipped_large_files,
        parse_error_files,
        pattern_error_languages,
        truncated: matches.len() >= options.limit,
        matches,
    })
}

fn format_match(
    workspace_root: &Path,
    file: &SourceFile,
    node_match: &ast_grep_core::matcher::NodeMatch<'_, StrDoc<SupportLang>>,
) -> AstGrepMatch {
    let node = node_match.get_node();
    let start = node.start_pos();
    let end = node.end_pos();
    let range = node.range();
    AstGrepMatch {
        path: relative_path(workspace_root, &file.path),
        language: file.language.name.to_string(),
        kind: node.kind().to_string(),
        line: start.line() + 1,
        column: start.column(node) + 1,
        end_line: end.line() + 1,
        end_column: end.column(node) + 1,
        byte_start: range.start,
        byte_end: range.end,
        text: truncate_chars(&node.text(), MAX_MATCH_TEXT_CHARS),
        captures: format_captures(node_match),
    }
}

fn format_captures(
    node_match: &ast_grep_core::matcher::NodeMatch<'_, StrDoc<SupportLang>>,
) -> Vec<AstGrepCapture> {
    let env = node_match.get_env();
    let mut captures = Vec::new();
    for variable in env.get_matched_variables() {
        match variable {
            MetaVariable::Capture(name, _) => {
                if let Some(node) = env.get_match(&name) {
                    captures.push(AstGrepCapture {
                        name,
                        text: truncate_chars(&node.text(), MAX_CAPTURE_TEXT_CHARS),
                    });
                }
            }
            MetaVariable::MultiCapture(name) => {
                let text = env
                    .get_multiple_matches(&name)
                    .iter()
                    .map(Node::text)
                    .collect::<Vec<_>>()
                    .join(" ");
                captures.push(AstGrepCapture {
                    name,
                    text: truncate_chars(&text, MAX_CAPTURE_TEXT_CHARS),
                });
            }
            MetaVariable::Dropped(_) | MetaVariable::Multiple => {}
        }
    }
    captures.sort_by(|left, right| left.name.cmp(&right.name));
    captures
}

fn resolve_language_filter(filter: &str) -> Result<&'static str> {
    LANGUAGES
        .iter()
        .find(|language| language.matches_filter(filter))
        .map(|language| language.name)
        .with_context(|| format!("unsupported ast-grep language filter: {filter}"))
}

fn bounded_usize(value: Option<&Value>, default: usize, max: usize, name: &str) -> Result<usize> {
    let Some(value) = value else {
        return Ok(default);
    };
    let Some(number) = value.as_u64() else {
        bail!("`{name}` must be a positive integer");
    };
    if number == 0 || number > max as u64 {
        bail!("`{name}` must be between 1 and {max}");
    }
    Ok(number as usize)
}

fn resolve_scope(root: &Path, path: Option<&str>) -> Result<PathBuf> {
    let Some(path) = path else {
        return Ok(root.to_path_buf());
    };
    let relative = path.trim().trim_start_matches('/');
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

fn collect_supported_files(
    path: &Path,
    language_filter: Option<&str>,
    files: &mut Vec<SourceFile>,
    skipped_unsupported_files: &mut usize,
) -> Result<()> {
    if path.is_file() {
        match language_for_path(path, language_filter) {
            Some(language) => files.push(SourceFile {
                path: path.to_path_buf(),
                language,
            }),
            None => *skipped_unsupported_files += 1,
        }
        return Ok(());
    }
    if !path.is_dir() {
        return Ok(());
    }

    let mut entries = fs::read_dir(path)
        .with_context(|| format!("read directory {}", path.display()))?
        .filter_map(std::result::Result::ok)
        .collect::<Vec<_>>();
    entries.sort_by_key(|entry| entry.path());

    for entry in entries {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            if path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| SKIP_DIRS.contains(&name))
            {
                continue;
            }
            collect_supported_files(&path, language_filter, files, skipped_unsupported_files)?;
        } else if file_type.is_file() {
            match language_for_path(&path, language_filter) {
                Some(language) => files.push(SourceFile { path, language }),
                None => *skipped_unsupported_files += 1,
            }
        }
    }
    Ok(())
}

fn language_for_path(path: &Path, language_filter: Option<&str>) -> Option<&'static LanguageSpec> {
    let file_name = path.file_name().and_then(|name| name.to_str())?;
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase);
    LANGUAGES.iter().find(|language| {
        language_filter.is_none_or(|filter| language.name == filter)
            && (language.filenames.contains(&file_name)
                || extension
                    .as_deref()
                    .is_some_and(|extension| language.extensions.contains(&extension)))
    })
}

fn relative_path(root: &Path, path: &Path) -> String {
    let relative = path.strip_prefix(root).unwrap_or(path);
    let rendered = relative.to_string_lossy().replace('\\', "/");
    if rendered.is_empty() {
        ".".to_string()
    } else {
        rendered
    }
}

fn truncate_chars(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        return value.to_string();
    }
    let suffix = "...";
    let mut truncated: String = value
        .chars()
        .take(max.saturating_sub(suffix.len()))
        .collect();
    truncated.push_str(suffix);
    truncated
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent");
        }
        fs::write(path, content).expect("write file");
    }

    #[test]
    fn finds_rust_function_pattern() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            &dir.path().join("src/lib.rs"),
            "fn keep() {}\nfn skip(arg: usize) { let _ = arg; }\n",
        );

        let report = ast_grep(
            dir.path(),
            AstGrepOptions {
                pattern: "fn $NAME() {}".to_string(),
                path: None,
                language: Some("rust".to_string()),
                limit: 20,
                max_file_bytes: DEFAULT_MAX_FILE_BYTES,
            },
        )
        .expect("scan");

        assert_eq!(report.matches.len(), 1);
        let mat = &report.matches[0];
        assert_eq!(mat.path, "src/lib.rs");
        assert_eq!(mat.language, "rust");
        assert_eq!(mat.kind, "function_item");
        assert!(
            mat.captures
                .iter()
                .any(|capture| capture.name == "NAME" && capture.text == "keep")
        );
    }

    #[test]
    fn filters_by_path_and_language() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(&dir.path().join("app.py"), "print(value)\n");
        write(&dir.path().join("web/app.ts"), "console.log(value);\n");

        let report = ast_grep(
            dir.path(),
            AstGrepOptions {
                pattern: "console.log($A)".to_string(),
                path: Some("/web".to_string()),
                language: Some("typescript".to_string()),
                limit: 20,
                max_file_bytes: DEFAULT_MAX_FILE_BYTES,
            },
        )
        .expect("scan");

        assert_eq!(report.languages, vec!["typescript"]);
        assert_eq!(report.matches.len(), 1);
        assert_eq!(report.matches[0].path, "web/app.ts");
        assert!(
            report.matches[0]
                .captures
                .iter()
                .any(|capture| capture.name == "A" && capture.text == "value")
        );
    }

    #[tokio::test]
    async fn ast_grep_tool_executes_structural_search() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(&dir.path().join("src/lib.rs"), "fn target() {}\n");

        let capability = AstGrepCapability::new(dir.path().to_path_buf());
        let tools = capability.tools();
        let tool = tools
            .iter()
            .find(|tool| tool.name() == "ast_grep")
            .expect("ast_grep tool");
        let result = tool
            .execute(json!({
                "pattern": "fn $NAME() {}",
                "language": "rust",
                "limit": 10
            }))
            .await;
        let ToolExecutionResult::Success(value) = result else {
            panic!("expected success, got {result:?}");
        };

        assert_eq!(value["count"], json!(1));
        assert_eq!(value["matches"][0]["path"], json!("src/lib.rs"));
        assert_eq!(value["matches"][0]["captures"][0]["name"], json!("NAME"));
        assert_eq!(value["matches"][0]["captures"][0]["text"], json!("target"));
    }

    #[test]
    fn rejects_paths_outside_workspace() {
        let dir = tempfile::tempdir().expect("tempdir");
        let err = ast_grep(
            dir.path(),
            AstGrepOptions {
                pattern: "fn $NAME() {}".to_string(),
                path: Some("../outside".to_string()),
                language: Some("rust".to_string()),
                limit: 20,
                max_file_bytes: DEFAULT_MAX_FILE_BYTES,
            },
        )
        .expect_err("outside path should fail");

        assert!(err.to_string().contains("inside the workspace"));
    }

    #[test]
    fn rejects_oversized_numeric_limits_before_casting() {
        let dir = tempfile::tempdir().expect("tempdir");
        let err = ast_grep_from_arguments(
            dir.path(),
            &json!({
                "pattern": "fn $NAME() {}",
                "limit": u64::MAX,
            }),
        )
        .expect_err("oversized limit should fail");

        assert!(err.to_string().contains("between 1 and"));
    }

    #[cfg(unix)]
    #[test]
    fn skips_symlinked_directories_during_recursive_scan() {
        let dir = tempfile::tempdir().expect("tempdir");
        let outside = tempfile::tempdir().expect("outside tempdir");
        write(&dir.path().join("src/lib.rs"), "fn inside_fn() {}\n");
        write(&outside.path().join("outside.rs"), "fn outside_fn() {}\n");
        std::os::unix::fs::symlink(outside.path(), dir.path().join("linked_outside"))
            .expect("symlink outside dir");

        let report = ast_grep(
            dir.path(),
            AstGrepOptions {
                pattern: "fn $NAME() {}".to_string(),
                path: None,
                language: Some("rust".to_string()),
                limit: 20,
                max_file_bytes: DEFAULT_MAX_FILE_BYTES,
            },
        )
        .expect("scan");

        assert!(
            report
                .matches
                .iter()
                .any(|mat| mat.text == "fn inside_fn() {}")
        );
        assert!(
            !report
                .matches
                .iter()
                .any(|mat| mat.text == "fn outside_fn() {}")
        );
    }
}

//! On-demand symbol maps for broad repository orientation.
//!
//! This capability is intentionally read-only and scan-on-demand. Grep/read
//! remain the hot path; repo-map gives the model a compact structural view when
//! lexical search is too narrow.

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use everruns_core::capabilities::{Capability, CapabilityStatus, SystemPromptContext};
use everruns_core::tools::{Tool, ToolExecutionResult};
use serde::Serialize;
use serde_json::{Value, json};
use std::fs;
use std::path::{Component, Path, PathBuf};
use tree_sitter::{Language, Node, Parser};

pub(crate) const REPO_MAP_CAPABILITY_ID: &str = "repo_map";

const DEFAULT_LIMIT: usize = 200;
const MAX_LIMIT: usize = 1000;
const DEFAULT_MAX_FILE_BYTES: usize = 512 * 1024;
const MAX_FILE_BYTES: usize = 2 * 1024 * 1024;
const MAX_SIGNATURE_CHARS: usize = 180;
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
pub(crate) struct RepoMapCapability {
    workspace_root: PathBuf,
}

impl RepoMapCapability {
    pub(crate) fn new(workspace_root: PathBuf) -> Self {
        Self { workspace_root }
    }
}

#[async_trait]
impl Capability for RepoMapCapability {
    fn id(&self) -> &str {
        REPO_MAP_CAPABILITY_ID
    }

    fn name(&self) -> &str {
        "Repo Map"
    }

    fn description(&self) -> &str {
        "Read-only, on-demand multi-language symbol map for broad codebase orientation."
    }

    fn status(&self) -> CapabilityStatus {
        CapabilityStatus::Available
    }

    fn category(&self) -> Option<&str> {
        Some("Code Navigation")
    }

    async fn system_prompt_contribution(&self, _ctx: &SystemPromptContext) -> Option<String> {
        Some(
            "<capability id=\"repo_map\">\n\
             For broad codebase orientation, call `repo_map` or `repo_symbols` to get an \
             on-demand symbol overview. Use grep/read for exact text and implementation \
             details; the map is structural context, not a replacement for reading code.\n\
             </capability>"
                .to_string(),
        )
    }

    fn system_prompt_preview(&self) -> Option<String> {
        Some(
            "<capability id=\"repo_map\">\n\
             Use `repo_map` / `repo_symbols` for on-demand multi-language symbol overviews.\n\
             </capability>"
                .to_string(),
        )
    }

    fn tools(&self) -> Vec<Box<dyn Tool>> {
        vec![
            Box::new(RepoMapTool {
                workspace_root: self.workspace_root.clone(),
            }),
            Box::new(RepoSymbolsTool {
                workspace_root: self.workspace_root.clone(),
            }),
        ]
    }
}

struct RepoMapTool {
    workspace_root: PathBuf,
}

#[async_trait]
impl Tool for RepoMapTool {
    fn name(&self) -> &str {
        "repo_map"
    }

    fn display_name(&self) -> Option<&str> {
        Some("Repo map")
    }

    fn description(&self) -> &str {
        "Build a compact, grouped multi-language symbol map for the workspace or a subpath. \
         Use this for broad orientation before targeted grep/read."
    }

    fn parameters_schema(&self) -> Value {
        scan_schema(
            "Optional case-insensitive filter matched against path, name, kind, parent, and signature.",
        )
    }

    async fn execute(&self, arguments: Value) -> ToolExecutionResult {
        match scan_from_arguments(&self.workspace_root, &arguments) {
            Ok(report) => ToolExecutionResult::success(json!({
                "ok": true,
                "workspace_root": self.workspace_root.display().to_string(),
                "path": report.path,
                "query": report.query,
                "languages": report.languages,
                "count": report.symbols.len(),
                "scanned_files": report.scanned_files,
                "skipped_unsupported_files": report.skipped_unsupported_files,
                "skipped_files": report.skipped_files,
                "skipped_large_files": report.skipped_large_files,
                "parse_error_files": report.parse_error_files,
                "truncated": report.truncated,
                "files": grouped_symbols(&report.symbols),
            })),
            Err(err) => ToolExecutionResult::tool_error(err.to_string()),
        }
    }
}

struct RepoSymbolsTool {
    workspace_root: PathBuf,
}

#[async_trait]
impl Tool for RepoSymbolsTool {
    fn name(&self) -> &str {
        "repo_symbols"
    }

    fn display_name(&self) -> Option<&str> {
        Some("Repo symbols")
    }

    fn description(&self) -> &str {
        "Return a flat list of symbols from the workspace or a subpath, optionally \
         filtered by query. Use when you need specific symbol candidates."
    }

    fn parameters_schema(&self) -> Value {
        scan_schema(
            "Optional case-insensitive filter matched against path, name, kind, parent, and signature.",
        )
    }

    async fn execute(&self, arguments: Value) -> ToolExecutionResult {
        match scan_from_arguments(&self.workspace_root, &arguments) {
            Ok(report) => ToolExecutionResult::success(json!({
                "ok": true,
                "workspace_root": self.workspace_root.display().to_string(),
                "path": report.path,
                "query": report.query,
                "languages": report.languages,
                "count": report.symbols.len(),
                "scanned_files": report.scanned_files,
                "skipped_unsupported_files": report.skipped_unsupported_files,
                "skipped_files": report.skipped_files,
                "skipped_large_files": report.skipped_large_files,
                "parse_error_files": report.parse_error_files,
                "truncated": report.truncated,
                "symbols": report.symbols,
            })),
            Err(err) => ToolExecutionResult::tool_error(err.to_string()),
        }
    }
}

fn scan_schema(query_description: &str) -> Value {
    json!({
        "type": "object",
        "properties": {
            "path": {
                "type": "string",
                "description": "Optional workspace-relative file or directory to scan. Defaults to the workspace root."
            },
            "query": {
                "type": "string",
                "description": query_description
            },
            "language": {
                "type": "string",
                "description": "Optional language filter. Supported values include rust, python, typescript, tsx, javascript, csharp, go, zig, css, html, bash, and sql."
            },
            "limit": {
                "type": "integer",
                "minimum": 1,
                "maximum": MAX_LIMIT,
                "description": "Maximum number of matching symbols to return. Defaults to 200."
            },
            "max_file_bytes": {
                "type": "integer",
                "minimum": 1,
                "maximum": MAX_FILE_BYTES,
                "description": "Skip supported source files larger than this many bytes. Defaults to 524288."
            }
        },
        "additionalProperties": false
    })
}

fn scan_from_arguments(workspace_root: &Path, arguments: &Value) -> Result<SymbolScanReport> {
    let path = arguments
        .get("path")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .map(str::to_string);
    let query = arguments
        .get("query")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|query| !query.is_empty())
        .map(str::to_string);
    let language = arguments
        .get("language")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|language| !language.is_empty())
        .map(str::to_string);
    let limit = bounded_usize(arguments, "limit", DEFAULT_LIMIT, MAX_LIMIT)?;
    let max_file_bytes = bounded_usize(
        arguments,
        "max_file_bytes",
        DEFAULT_MAX_FILE_BYTES,
        MAX_FILE_BYTES,
    )?;

    collect_repo_symbols(
        workspace_root,
        SymbolScanOptions {
            path,
            query,
            language,
            limit,
            max_file_bytes,
        },
    )
}

fn bounded_usize(arguments: &Value, key: &str, default: usize, max: usize) -> Result<usize> {
    let Some(raw) = arguments.get(key) else {
        return Ok(default);
    };
    let Some(value) = raw.as_u64() else {
        bail!("`{key}` must be a positive integer");
    };
    if value == 0 || value as usize > max {
        bail!("`{key}` must be between 1 and {max}");
    }
    Ok(value as usize)
}

#[derive(Clone, Debug)]
struct SymbolScanOptions {
    path: Option<String>,
    query: Option<String>,
    language: Option<String>,
    limit: usize,
    max_file_bytes: usize,
}

#[derive(Clone, Debug)]
struct SymbolScanReport {
    path: String,
    query: Option<String>,
    languages: Vec<String>,
    scanned_files: usize,
    skipped_unsupported_files: usize,
    skipped_files: usize,
    skipped_large_files: usize,
    parse_error_files: usize,
    truncated: bool,
    symbols: Vec<RepoSymbol>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
struct RepoSymbol {
    path: String,
    language: String,
    kind: String,
    name: String,
    parent: Option<String>,
    visibility: Option<String>,
    signature: String,
    line: usize,
    column: usize,
}

#[derive(Serialize)]
struct SymbolFileGroup<'a> {
    path: &'a str,
    symbols: Vec<&'a RepoSymbol>,
}

fn collect_repo_symbols(
    workspace_root: &Path,
    options: SymbolScanOptions,
) -> Result<SymbolScanReport> {
    let root = workspace_root
        .canonicalize()
        .with_context(|| format!("canonicalize workspace root {}", workspace_root.display()))?;
    let scope = resolve_scope(&root, options.path.as_deref())?;
    let language_filter = options.language.as_deref().map(normalize_language_filter);
    if let Some(filter) = language_filter.as_deref()
        && !LANGUAGES
            .iter()
            .any(|language| language.matches_filter(filter))
    {
        bail!("unsupported language `{filter}`");
    }
    let mut files = Vec::new();
    let mut skipped_unsupported_files = 0;
    collect_supported_files(
        &scope,
        language_filter.as_deref(),
        &mut files,
        &mut skipped_unsupported_files,
    )?;
    files.sort_by(|a, b| a.path.cmp(&b.path));

    let query = options.query.as_ref().map(|query| query.to_lowercase());
    let mut report = SymbolScanReport {
        path: relative_path(&root, &scope),
        query: options.query,
        languages: Vec::new(),
        scanned_files: 0,
        skipped_unsupported_files,
        skipped_files: 0,
        skipped_large_files: 0,
        parse_error_files: 0,
        truncated: false,
        symbols: Vec::new(),
    };

    let mut parser = Parser::new();

    for file in files {
        if report.truncated {
            break;
        }
        let metadata = match fs::metadata(&file.path) {
            Ok(metadata) => metadata,
            Err(_) => {
                report.skipped_files += 1;
                continue;
            }
        };
        if metadata.len() as usize > options.max_file_bytes {
            report.skipped_large_files += 1;
            continue;
        }
        let source = match fs::read_to_string(&file.path) {
            Ok(source) => source,
            Err(_) => {
                report.skipped_files += 1;
                continue;
            }
        };
        parser
            .set_language(&file.language.language())
            .with_context(|| format!("load {} tree-sitter grammar", file.language.name))?;
        report.scanned_files += 1;
        let Some(tree) = parser.parse(&source, None) else {
            report.parse_error_files += 1;
            continue;
        };
        if tree.root_node().has_error() {
            report.parse_error_files += 1;
        }
        if !report
            .languages
            .iter()
            .any(|language| language == file.language.name)
        {
            report.languages.push(file.language.name.to_string());
        }
        let path = relative_path(&root, &file.path);
        let mut parents = Vec::new();
        let context = SymbolCollectContext {
            source: &source,
            path: &path,
            language: file.language,
            query: query.as_deref(),
            limit: options.limit,
        };
        collect_symbols_from_node(tree.root_node(), &context, &mut parents, &mut report);
    }

    Ok(report)
}

#[derive(Clone)]
struct SourceFile {
    path: PathBuf,
    language: &'static LanguageSpec,
}

struct LanguageSpec {
    name: &'static str,
    aliases: &'static [&'static str],
    extensions: &'static [&'static str],
    filenames: &'static [&'static str],
    grammar: fn() -> Language,
    symbol_kinds: &'static [KindMap],
    parent_kinds: &'static [&'static str],
}

impl LanguageSpec {
    fn language(&self) -> Language {
        (self.grammar)()
    }

    fn matches_filter(&self, filter: &str) -> bool {
        self.name == filter || self.aliases.contains(&filter)
    }

    fn symbol_kind(&self, node_kind: &str) -> Option<&'static str> {
        self.symbol_kinds
            .iter()
            .find(|kind| kind.node == node_kind)
            .map(|kind| kind.kind)
    }

    fn is_parent_kind(&self, node_kind: &str) -> bool {
        self.parent_kinds.contains(&node_kind)
    }
}

struct KindMap {
    node: &'static str,
    kind: &'static str,
}

const fn kind(node: &'static str, kind: &'static str) -> KindMap {
    KindMap { node, kind }
}

fn rust_language() -> Language {
    tree_sitter_rust::LANGUAGE.into()
}

fn python_language() -> Language {
    tree_sitter_python::LANGUAGE.into()
}

fn javascript_language() -> Language {
    tree_sitter_javascript::LANGUAGE.into()
}

fn typescript_language() -> Language {
    tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()
}

fn tsx_language() -> Language {
    tree_sitter_typescript::LANGUAGE_TSX.into()
}

fn csharp_language() -> Language {
    tree_sitter_c_sharp::LANGUAGE.into()
}

fn go_language() -> Language {
    tree_sitter_go::LANGUAGE.into()
}

fn zig_language() -> Language {
    tree_sitter_zig::LANGUAGE.into()
}

fn css_language() -> Language {
    tree_sitter_css::LANGUAGE.into()
}

fn html_language() -> Language {
    tree_sitter_html::LANGUAGE.into()
}

fn bash_language() -> Language {
    tree_sitter_bash::LANGUAGE.into()
}

fn sql_language() -> Language {
    tree_sitter_sequel::LANGUAGE.into()
}

const RUST_KINDS: &[KindMap] = &[
    kind("const_item", "const"),
    kind("enum_item", "enum"),
    kind("function_item", "function"),
    kind("function_signature_item", "function_signature"),
    kind("impl_item", "impl"),
    kind("macro_definition", "macro"),
    kind("mod_item", "module"),
    kind("static_item", "static"),
    kind("struct_item", "struct"),
    kind("trait_item", "trait"),
    kind("type_item", "type"),
];

const PYTHON_KINDS: &[KindMap] = &[
    kind("class_definition", "class"),
    kind("function_definition", "function"),
];

const JS_KINDS: &[KindMap] = &[
    kind("class_declaration", "class"),
    kind("function_declaration", "function"),
    kind("generator_function_declaration", "function"),
    kind("method_definition", "method"),
];

const TS_KINDS: &[KindMap] = &[
    kind("abstract_class_declaration", "class"),
    kind("class_declaration", "class"),
    kind("enum_declaration", "enum"),
    kind("function_declaration", "function"),
    kind("generator_function_declaration", "function"),
    kind("interface_declaration", "interface"),
    kind("internal_module", "module"),
    kind("method_definition", "method"),
    kind("type_alias_declaration", "type"),
];

const CSHARP_KINDS: &[KindMap] = &[
    kind("class_declaration", "class"),
    kind("constructor_declaration", "constructor"),
    kind("delegate_declaration", "delegate"),
    kind("enum_declaration", "enum"),
    kind("interface_declaration", "interface"),
    kind("method_declaration", "method"),
    kind("namespace_declaration", "namespace"),
    kind("property_declaration", "property"),
    kind("record_declaration", "record"),
    kind("struct_declaration", "struct"),
];

const GO_KINDS: &[KindMap] = &[
    kind("function_declaration", "function"),
    kind("method_declaration", "method"),
    kind("type_declaration", "type"),
];

const ZIG_KINDS: &[KindMap] = &[
    kind("enum_declaration", "enum"),
    kind("function_declaration", "function"),
    kind("struct_declaration", "struct"),
    kind("variable_declaration", "variable"),
];

const CSS_KINDS: &[KindMap] = &[
    kind("import_statement", "import"),
    kind("keyframes_statement", "keyframes"),
    kind("rule_set", "selector"),
];

const HTML_KINDS: &[KindMap] = &[kind("element", "element"), kind("script_element", "script")];

const BASH_KINDS: &[KindMap] = &[kind("function_definition", "function")];

const SQL_KINDS: &[KindMap] = &[
    kind("create_database", "database"),
    kind("create_extension", "extension"),
    kind("create_function", "function"),
    kind("create_index", "index"),
    kind("create_materialized_view", "view"),
    kind("create_schema", "schema"),
    kind("create_sequence", "sequence"),
    kind("create_table", "table"),
    kind("create_trigger", "trigger"),
    kind("create_type", "type"),
    kind("create_view", "view"),
    kind("function_declaration", "function"),
];

static LANGUAGES: &[LanguageSpec] = &[
    LanguageSpec {
        name: "rust",
        aliases: &["rs"],
        extensions: &["rs"],
        filenames: &[],
        grammar: rust_language,
        symbol_kinds: RUST_KINDS,
        parent_kinds: &["impl_item", "mod_item", "trait_item"],
    },
    LanguageSpec {
        name: "python",
        aliases: &["py"],
        extensions: &["py", "pyi"],
        filenames: &[],
        grammar: python_language,
        symbol_kinds: PYTHON_KINDS,
        parent_kinds: &["class_definition"],
    },
    LanguageSpec {
        name: "typescript",
        aliases: &["ts"],
        extensions: &["ts", "mts", "cts"],
        filenames: &[],
        grammar: typescript_language,
        symbol_kinds: TS_KINDS,
        parent_kinds: &[
            "abstract_class_declaration",
            "class_declaration",
            "interface_declaration",
            "internal_module",
        ],
    },
    LanguageSpec {
        name: "tsx",
        aliases: &[],
        extensions: &["tsx"],
        filenames: &[],
        grammar: tsx_language,
        symbol_kinds: TS_KINDS,
        parent_kinds: &[
            "abstract_class_declaration",
            "class_declaration",
            "interface_declaration",
            "internal_module",
        ],
    },
    LanguageSpec {
        name: "javascript",
        aliases: &["js", "jsx"],
        extensions: &["js", "jsx", "mjs", "cjs"],
        filenames: &[],
        grammar: javascript_language,
        symbol_kinds: JS_KINDS,
        parent_kinds: &["class_declaration"],
    },
    LanguageSpec {
        name: "csharp",
        aliases: &["c#", "cs"],
        extensions: &["cs"],
        filenames: &[],
        grammar: csharp_language,
        symbol_kinds: CSHARP_KINDS,
        parent_kinds: &[
            "class_declaration",
            "interface_declaration",
            "namespace_declaration",
            "record_declaration",
            "struct_declaration",
        ],
    },
    LanguageSpec {
        name: "go",
        aliases: &["golang"],
        extensions: &["go"],
        filenames: &[],
        grammar: go_language,
        symbol_kinds: GO_KINDS,
        parent_kinds: &[],
    },
    LanguageSpec {
        name: "zig",
        aliases: &["zed"],
        extensions: &["zig", "zon"],
        filenames: &[],
        grammar: zig_language,
        symbol_kinds: ZIG_KINDS,
        parent_kinds: &["struct_declaration", "enum_declaration"],
    },
    LanguageSpec {
        name: "css",
        aliases: &[],
        extensions: &["css"],
        filenames: &[],
        grammar: css_language,
        symbol_kinds: CSS_KINDS,
        parent_kinds: &[],
    },
    LanguageSpec {
        name: "html",
        aliases: &["htm"],
        extensions: &["html", "htm"],
        filenames: &[],
        grammar: html_language,
        symbol_kinds: HTML_KINDS,
        parent_kinds: &["element"],
    },
    LanguageSpec {
        name: "bash",
        aliases: &["sh", "shell"],
        extensions: &["bash", "sh", "zsh"],
        filenames: &[".bashrc", ".bash_profile", ".zshrc", "bashrc", "zshrc"],
        grammar: bash_language,
        symbol_kinds: BASH_KINDS,
        parent_kinds: &[],
    },
    LanguageSpec {
        name: "sql",
        aliases: &[],
        extensions: &["sql"],
        filenames: &[],
        grammar: sql_language,
        symbol_kinds: SQL_KINDS,
        parent_kinds: &[],
    },
];

fn normalize_language_filter(language: &str) -> String {
    language.trim().to_ascii_lowercase()
}

fn resolve_scope(root: &Path, path: Option<&str>) -> Result<PathBuf> {
    let Some(path) = path else {
        return Ok(root.to_path_buf());
    };
    let relative = path.trim_start_matches('/');
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
        language_filter.is_none_or(|filter| language.matches_filter(filter))
            && (language.filenames.contains(&file_name)
                || extension
                    .as_deref()
                    .is_some_and(|extension| language.extensions.contains(&extension)))
    })
}

struct SymbolCollectContext<'a> {
    source: &'a str,
    path: &'a str,
    language: &'static LanguageSpec,
    query: Option<&'a str>,
    limit: usize,
}

fn collect_symbols_from_node(
    node: Node<'_>,
    context: &SymbolCollectContext<'_>,
    parents: &mut Vec<String>,
    report: &mut SymbolScanReport,
) {
    if report.truncated {
        return;
    }

    let symbol = symbol_for_node(
        node,
        context.source,
        context.path,
        context.language,
        parents.last().cloned(),
    );
    let parent_label = symbol
        .as_ref()
        .and_then(|symbol| parent_context_label(symbol, context.language, node.kind()));
    if let Some(symbol) = symbol
        && matches_query(&symbol, context.query)
    {
        if report.symbols.len() >= context.limit {
            report.truncated = true;
            return;
        }
        report.symbols.push(symbol);
    }

    if let Some(label) = parent_label.as_ref() {
        parents.push(label.clone());
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_symbols_from_node(child, context, parents, report);
        if report.truncated {
            break;
        }
    }
    if parent_label.is_some() {
        parents.pop();
    }
}

fn symbol_for_node(
    node: Node<'_>,
    source: &str,
    path: &str,
    language: &LanguageSpec,
    parent: Option<String>,
) -> Option<RepoSymbol> {
    let kind = language.symbol_kind(node.kind())?;
    let name = symbol_name(node, source, language, kind)?;
    let position = node.start_position();
    Some(RepoSymbol {
        path: path.to_string(),
        language: language.name.to_string(),
        kind: kind.to_string(),
        name,
        parent,
        visibility: visibility(node, source),
        signature: signature(node, source),
        line: position.row + 1,
        column: position.column + 1,
    })
}

fn symbol_name(
    node: Node<'_>,
    source: &str,
    language: &LanguageSpec,
    kind: &str,
) -> Option<String> {
    if kind == "impl" {
        return Some(impl_name(node, source));
    }
    if matches!(kind, "selector" | "import") {
        return Some(signature_name(node, source));
    }
    if language.name == "html" {
        return descendant_text(node, source, &["tag_name"]).map(str::to_string);
    }
    if language.name == "sql" {
        return descendant_text(
            node,
            source,
            &[
                "identifier",
                "object_reference",
                "function_reference",
                "table_reference",
            ],
        )
        .map(str::to_string)
        .or_else(|| Some(signature_name(node, source)));
    }
    node.child_by_field_name("name")
        .and_then(|name| node_text(name, source))
        .map(str::to_string)
        .or_else(|| {
            descendant_text(
                node,
                source,
                &[
                    "identifier",
                    "property_identifier",
                    "type_identifier",
                    "field_identifier",
                    "word",
                ],
            )
            .map(str::to_string)
        })
}

fn impl_name(node: Node<'_>, source: &str) -> String {
    let sig = signature(node, source);
    let without_brace = sig.split('{').next().unwrap_or(&sig).trim();
    if without_brace.is_empty() {
        "impl".to_string()
    } else {
        without_brace.to_string()
    }
}

fn visibility(node: Node<'_>, source: &str) -> Option<String> {
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .find(|child| child.kind() == "visibility_modifier")
        .and_then(|child| node_text(child, source))
        .map(str::to_string)
}

fn signature(node: Node<'_>, source: &str) -> String {
    let raw = node_text(node, source).unwrap_or_default();
    let line = raw.lines().next().unwrap_or_default().trim();
    truncate_chars(line, MAX_SIGNATURE_CHARS)
}

fn node_text<'a>(node: Node<'_>, source: &'a str) -> Option<&'a str> {
    source.get(node.byte_range())
}

fn descendant_text<'a>(node: Node<'_>, source: &'a str, kinds: &[&str]) -> Option<&'a str> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if kinds.contains(&child.kind()) {
            return node_text(child, source);
        }
        if let Some(text) = descendant_text(child, source, kinds) {
            return Some(text);
        }
    }
    None
}

fn signature_name(node: Node<'_>, source: &str) -> String {
    signature(node, source)
        .split('{')
        .next()
        .unwrap_or_default()
        .trim()
        .to_string()
}

fn parent_context_label(
    symbol: &RepoSymbol,
    language: &LanguageSpec,
    node_kind: &str,
) -> Option<String> {
    if language.is_parent_kind(node_kind) {
        if symbol.kind == "impl" {
            Some(symbol.name.clone())
        } else {
            Some(format!("{} {}", symbol.kind, symbol.name))
        }
    } else {
        None
    }
}

fn matches_query(symbol: &RepoSymbol, query: Option<&str>) -> bool {
    let Some(query) = query else {
        return true;
    };
    symbol.path.to_lowercase().contains(query)
        || symbol.kind.to_lowercase().contains(query)
        || symbol.name.to_lowercase().contains(query)
        || symbol.signature.to_lowercase().contains(query)
        || symbol
            .parent
            .as_ref()
            .is_some_and(|parent| parent.to_lowercase().contains(query))
}

fn grouped_symbols(symbols: &[RepoSymbol]) -> Vec<SymbolFileGroup<'_>> {
    let mut groups = Vec::new();
    let mut index = 0;
    while index < symbols.len() {
        let path = symbols[index].path.as_str();
        let mut group = SymbolFileGroup {
            path,
            symbols: Vec::new(),
        };
        while index < symbols.len() && symbols[index].path == path {
            group.symbols.push(&symbols[index]);
            index += 1;
        }
        groups.push(group);
    }
    groups
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
    fn collects_rust_symbols_with_parent_context() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            &dir.path().join("src/lib.rs"),
            r#"
pub struct Widget {
    id: String,
}

impl Widget {
    pub fn new(id: String) -> Self {
        Self { id }
    }

    fn id(&self) -> &str {
        &self.id
    }
}

trait Named {
    fn name(&self) -> &str;
}
"#,
        );

        let report = collect_repo_symbols(
            dir.path(),
            SymbolScanOptions {
                path: None,
                query: None,
                language: None,
                limit: 20,
                max_file_bytes: DEFAULT_MAX_FILE_BYTES,
            },
        )
        .expect("scan");

        assert_eq!(report.scanned_files, 1);
        assert!(report.symbols.iter().any(|symbol| symbol.language == "rust"
            && symbol.kind == "struct"
            && symbol.name == "Widget"));
        let new_fn = report
            .symbols
            .iter()
            .find(|symbol| symbol.kind == "function" && symbol.name == "new")
            .expect("new function");
        assert_eq!(new_fn.parent.as_deref(), Some("impl Widget"));
        assert_eq!(new_fn.visibility.as_deref(), Some("pub"));
        assert!(
            report
                .symbols
                .iter()
                .any(|symbol| symbol.kind == "function_signature" && symbol.name == "name")
        );
    }

    #[test]
    fn filters_by_query_and_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(&dir.path().join("src/lib.rs"), "pub fn keep_me() {}\n");
        write(&dir.path().join("tests/demo.rs"), "fn skip_me() {}\n");

        let report = collect_repo_symbols(
            dir.path(),
            SymbolScanOptions {
                path: Some("src".to_string()),
                query: Some("keep".to_string()),
                language: None,
                limit: 20,
                max_file_bytes: DEFAULT_MAX_FILE_BYTES,
            },
        )
        .expect("scan");

        assert_eq!(report.symbols.len(), 1);
        assert_eq!(report.symbols[0].path, "src/lib.rs");
        assert_eq!(report.symbols[0].name, "keep_me");
    }

    #[test]
    fn collects_symbols_across_supported_languages() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            &dir.path().join("app.py"),
            "class Service:\n    def run(self):\n        pass\n",
        );
        write(
            &dir.path().join("widget.ts"),
            "export interface Widget {}\nexport function render() {}\n",
        );
        write(&dir.path().join("style.css"), ".button { color: red; }\n");
        write(
            &dir.path().join("index.html"),
            "<main><section></section></main>\n",
        );
        write(&dir.path().join("script.sh"), "deploy() { echo ok; }\n");
        write(
            &dir.path().join("schema.sql"),
            "CREATE TABLE users (id int);\n",
        );

        let report = collect_repo_symbols(
            dir.path(),
            SymbolScanOptions {
                path: None,
                query: None,
                language: None,
                limit: 100,
                max_file_bytes: DEFAULT_MAX_FILE_BYTES,
            },
        )
        .expect("scan");

        for language in ["python", "typescript", "css", "html", "bash", "sql"] {
            assert!(
                report.languages.iter().any(|seen| seen == language),
                "expected {language} in {:?}",
                report.languages
            );
        }
        assert!(
            report
                .symbols
                .iter()
                .any(|symbol| symbol.language == "python"
                    && symbol.kind == "class"
                    && symbol.name == "Service")
        );
        assert!(
            report
                .symbols
                .iter()
                .any(|symbol| symbol.language == "typescript"
                    && symbol.kind == "function"
                    && symbol.name == "render")
        );
        assert!(
            report
                .symbols
                .iter()
                .any(|symbol| symbol.language == "css" && symbol.name.contains(".button"))
        );
        assert!(report.symbols.iter().any(|symbol| symbol.language == "bash"
            && symbol.kind == "function"
            && symbol.name == "deploy"));
    }

    #[test]
    fn filters_by_language() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(&dir.path().join("src/lib.rs"), "pub fn rust_fn() {}\n");
        write(&dir.path().join("app.py"), "def py_fn():\n    pass\n");

        let report = collect_repo_symbols(
            dir.path(),
            SymbolScanOptions {
                path: None,
                query: None,
                language: Some("python".to_string()),
                limit: 20,
                max_file_bytes: DEFAULT_MAX_FILE_BYTES,
            },
        )
        .expect("scan");

        assert_eq!(report.languages, vec!["python"]);
        assert!(report.symbols.iter().any(|symbol| symbol.name == "py_fn"));
        assert!(!report.symbols.iter().any(|symbol| symbol.name == "rust_fn"));
    }

    #[tokio::test]
    async fn repo_symbols_tool_executes_multilanguage_scan() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(&dir.path().join("app.py"), "def py_fn():\n    pass\n");

        let capability = RepoMapCapability::new(dir.path().to_path_buf());
        let tools = capability.tools();
        let tool = tools
            .iter()
            .find(|tool| tool.name() == "repo_symbols")
            .expect("repo_symbols tool");
        let result = tool
            .execute(json!({
                "language": "python",
                "query": "py_fn",
                "limit": 10
            }))
            .await;
        let ToolExecutionResult::Success(value) = result else {
            panic!("expected success, got {result:?}");
        };

        assert_eq!(value["languages"], json!(["python"]));
        assert_eq!(value["count"], json!(1));
        assert_eq!(value["symbols"][0]["language"], json!("python"));
        assert_eq!(value["symbols"][0]["name"], json!("py_fn"));
    }

    #[cfg(unix)]
    #[test]
    fn skips_symlinked_directories_during_recursive_scan() {
        let dir = tempfile::tempdir().expect("tempdir");
        let outside = tempfile::tempdir().expect("outside tempdir");
        write(&dir.path().join("src/lib.rs"), "pub fn inside_fn() {}\n");
        write(
            &outside.path().join("outside.rs"),
            "pub fn outside_fn() {}\n",
        );
        std::os::unix::fs::symlink(outside.path(), dir.path().join("linked_outside"))
            .expect("symlink outside dir");

        let report = collect_repo_symbols(
            dir.path(),
            SymbolScanOptions {
                path: None,
                query: None,
                language: Some("rust".to_string()),
                limit: 20,
                max_file_bytes: DEFAULT_MAX_FILE_BYTES,
            },
        )
        .expect("scan");

        assert!(
            report
                .symbols
                .iter()
                .any(|symbol| symbol.name == "inside_fn")
        );
        assert!(
            !report
                .symbols
                .iter()
                .any(|symbol| symbol.name == "outside_fn")
        );
    }

    #[test]
    fn rejects_paths_outside_workspace() {
        let dir = tempfile::tempdir().expect("tempdir");
        let err = collect_repo_symbols(
            dir.path(),
            SymbolScanOptions {
                path: Some("../outside".to_string()),
                query: None,
                language: None,
                limit: 20,
                max_file_bytes: DEFAULT_MAX_FILE_BYTES,
            },
        )
        .expect_err("outside path should fail");

        assert!(err.to_string().contains("inside the workspace"));
    }
}

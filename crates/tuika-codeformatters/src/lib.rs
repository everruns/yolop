//! Tree-sitter syntax highlighting for [`tuika`]'s
//! [`CodeBlock`](tuika::CodeBlock) and [`Markdown`](tuika::Markdown) components.
//!
//! tuika owns the *presentation* of code (framing, background, language label,
//! wrapping) but deliberately depends on no grammar. This crate fills the gap: a
//! ready-made [`Highlighter`](tuika::Highlighter) backed by the same tree-sitter
//! grammars a coding tool already carries, mapping token classes onto the host's
//! [`Theme`]'s [`code`](tuika::CodeTheme) palette so highlighted code follows the
//! theme.
//!
//! ```
//! use tuika::{CodeBlock, Theme};
//! use tuika_codeformatters::TreeSitterHighlighter;
//!
//! let hl = TreeSitterHighlighter::new();
//! let _block = CodeBlock::new("rust", "fn main() {}").highlighter(&hl);
//! let _ = Theme::default();
//! ```
//!
//! Supported languages (with common aliases): Rust, Python, TypeScript/
//! JavaScript, TSX/JSX, Go, Java, Ruby, CSS, HTML, C#, PHP, Zig, Scala, and SQL.
//! Anything else — or source that fails to parse — returns [`None`], and the
//! caller renders it as plain code.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use ratatui::style::{Modifier, Style};
use ratatui::text::Span;
use tree_sitter_highlight::{Highlight, HighlightConfiguration, HighlightEvent, Highlighter};
use tuika::{CodeTheme, Theme};

/// Highlight capture names we recognize, longest-specific first so
/// tree-sitter-highlight resolves the most precise style. Kept in sync with
/// [`style_for_name`].
const HIGHLIGHT_NAMES: &[&str] = &[
    "keyword",
    "function.builtin",
    "function.method",
    "function",
    "constructor",
    "type.builtin",
    "type",
    "constant.builtin",
    "constant.numeric",
    "constant",
    "number",
    "string.special",
    "string",
    "escape",
    "comment",
    "operator",
    "punctuation.bracket",
    "punctuation.delimiter",
    "punctuation.special",
    "punctuation",
    "property",
    "attribute",
    "tag",
    "label",
    "variable.builtin",
    "variable.parameter",
    "variable",
];

/// Map a tree-sitter capture name to a themed [`Style`] using the host palette.
fn style_for_name(name: &str, code: &CodeTheme) -> Style {
    let base = Style::default();
    match name {
        "keyword" => base.fg(code.keyword).add_modifier(Modifier::BOLD),
        "function" | "function.builtin" | "function.method" | "constructor" | "label" => {
            base.fg(code.function)
        }
        "type" | "type.builtin" => base.fg(code.type_name),
        "constant" | "constant.builtin" | "constant.numeric" | "number" | "variable.builtin" => {
            base.fg(code.constant)
        }
        "string" | "string.special" | "escape" => base.fg(code.string),
        "comment" => base.fg(code.comment).add_modifier(Modifier::ITALIC),
        "operator"
        | "punctuation"
        | "punctuation.bracket"
        | "punctuation.delimiter"
        | "punctuation.special" => base.fg(code.punctuation),
        "attribute" | "tag" => base.fg(code.keyword),
        _ => base.fg(code.text),
    }
}

/// Map a fence info string (e.g. `rust`, `py`, `ts`, `c#`) to the canonical
/// grammar key, or `None` when we don't highlight that language.
fn canonical_language(lang: &str) -> Option<&'static str> {
    let lang = lang.trim().to_ascii_lowercase();
    let key = match lang.as_str() {
        "rust" | "rs" => "rust",
        "python" | "py" => "python",
        // The TypeScript grammar is a superset that parses plain JS cleanly.
        "typescript" | "ts" | "javascript" | "js" | "mjs" | "cjs" => "typescript",
        "tsx" | "jsx" => "tsx",
        "go" | "golang" => "go",
        "java" => "java",
        "ruby" | "rb" => "ruby",
        "css" => "css",
        "html" | "htm" => "html",
        "c#" | "cs" | "csharp" | "c_sharp" => "csharp",
        "php" => "php",
        "zig" => "zig",
        "scala" => "scala",
        "sql" => "sql",
        _ => return None,
    };
    Some(key)
}

fn build_configuration(key: &str) -> Option<HighlightConfiguration> {
    let (language, query): (tree_sitter::Language, &str) = match key {
        "rust" => (
            tree_sitter_rust::LANGUAGE.into(),
            tree_sitter_rust::HIGHLIGHTS_QUERY,
        ),
        "python" => (
            tree_sitter_python::LANGUAGE.into(),
            tree_sitter_python::HIGHLIGHTS_QUERY,
        ),
        "typescript" => (
            tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            tree_sitter_typescript::HIGHLIGHTS_QUERY,
        ),
        "tsx" => (
            tree_sitter_typescript::LANGUAGE_TSX.into(),
            tree_sitter_typescript::HIGHLIGHTS_QUERY,
        ),
        "go" => (
            tree_sitter_go::LANGUAGE.into(),
            tree_sitter_go::HIGHLIGHTS_QUERY,
        ),
        "java" => (
            tree_sitter_java::LANGUAGE.into(),
            tree_sitter_java::HIGHLIGHTS_QUERY,
        ),
        "ruby" => (
            tree_sitter_ruby::LANGUAGE.into(),
            tree_sitter_ruby::HIGHLIGHTS_QUERY,
        ),
        "css" => (
            tree_sitter_css::LANGUAGE.into(),
            tree_sitter_css::HIGHLIGHTS_QUERY,
        ),
        "html" => (
            tree_sitter_html::LANGUAGE.into(),
            tree_sitter_html::HIGHLIGHTS_QUERY,
        ),
        "csharp" => (
            tree_sitter_c_sharp::LANGUAGE.into(),
            tree_sitter_c_sharp::HIGHLIGHTS_QUERY,
        ),
        "php" => (
            tree_sitter_php::LANGUAGE_PHP.into(),
            tree_sitter_php::HIGHLIGHTS_QUERY,
        ),
        "zig" => (
            tree_sitter_zig::LANGUAGE.into(),
            tree_sitter_zig::HIGHLIGHTS_QUERY,
        ),
        "scala" => (
            tree_sitter_scala::LANGUAGE.into(),
            tree_sitter_scala::HIGHLIGHTS_QUERY,
        ),
        "sql" => (
            tree_sitter_sequel::LANGUAGE.into(),
            tree_sitter_sequel::HIGHLIGHTS_QUERY,
        ),
        _ => return None,
    };
    let mut config = HighlightConfiguration::new(language, key, query, "", "").ok()?;
    let names: Vec<String> = HIGHLIGHT_NAMES.iter().map(|n| n.to_string()).collect();
    config.configure(&names);
    Some(config)
}

thread_local! {
    /// Per-language highlight configs, built on first use. `None` marks a
    /// language whose config failed to build so we don't retry it every render.
    static CONFIGS: RefCell<HashMap<&'static str, Option<Rc<HighlightConfiguration>>>> =
        RefCell::new(HashMap::new());
}

fn config_for(key: &'static str) -> Option<Rc<HighlightConfiguration>> {
    CONFIGS.with(|configs| {
        configs
            .borrow_mut()
            .entry(key)
            .or_insert_with(|| build_configuration(key).map(Rc::new))
            .clone()
    })
}

/// A tree-sitter-backed [`Highlighter`](tuika::Highlighter).
///
/// Zero-sized and cheap to construct; the per-language parser configurations are
/// built lazily and cached thread-locally on first use, so keeping one around is
/// no better than making one per frame.
#[derive(Clone, Copy, Debug, Default)]
pub struct TreeSitterHighlighter;

impl TreeSitterHighlighter {
    pub fn new() -> Self {
        Self
    }
}

impl tuika::Highlighter for TreeSitterHighlighter {
    fn highlight(
        &self,
        lang: &str,
        lines: &[&str],
        theme: &Theme,
    ) -> Option<Vec<Vec<Span<'static>>>> {
        highlight_lines(lang, lines, &theme.code)
    }
}

/// Highlight a fenced code block, returning one span vector per input line, or
/// `None` for unsupported languages / parse failures (see the crate docs).
fn highlight_lines(
    lang: &str,
    lines: &[&str],
    code: &CodeTheme,
) -> Option<Vec<Vec<Span<'static>>>> {
    if lines.is_empty() {
        return None;
    }
    let key = canonical_language(lang)?;
    let config = config_for(key)?;
    let source = lines.join("\n");

    let mut highlighter = Highlighter::new();
    let events = highlighter
        .highlight(&config, source.as_bytes(), None, |_| None)
        .ok()?;

    let default_style = Style::default().fg(code.text);
    let mut style_stack: Vec<Style> = Vec::new();
    let mut out: Vec<Vec<Span<'static>>> = vec![Vec::new()];
    for event in events {
        match event.ok()? {
            HighlightEvent::HighlightStart(Highlight(index)) => {
                let style = HIGHLIGHT_NAMES
                    .get(index)
                    .map(|name| style_for_name(name, code))
                    .unwrap_or(default_style);
                style_stack.push(style);
            }
            HighlightEvent::HighlightEnd => {
                style_stack.pop();
            }
            HighlightEvent::Source { start, end } => {
                let style = style_stack.last().copied().unwrap_or(default_style);
                let text = source.get(start..end)?;
                let mut segments = text.split('\n');
                if let Some(first) = segments.next()
                    && !first.is_empty()
                {
                    out.last_mut()?.push(Span::styled(first.to_string(), style));
                }
                for segment in segments {
                    out.push(Vec::new());
                    if !segment.is_empty() {
                        out.last_mut()?
                            .push(Span::styled(segment.to_string(), style));
                    }
                }
            }
        }
    }

    // The event stream must reproduce exactly one output line per source line;
    // if it doesn't (unexpected), bail so the caller falls back deterministically.
    if out.len() == lines.len() {
        Some(out)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tuika::Highlighter as _;

    fn plain(spans: &[Span<'static>]) -> String {
        spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn highlights_rust_keywords_distinctly() {
        let theme = Theme::default();
        let hl = TreeSitterHighlighter::new();
        let lines = vec!["fn main() {", "    let x = 1;", "}"];
        let out = hl
            .highlight("rust", &lines, &theme)
            .expect("rust highlights");
        assert_eq!(out.len(), 3);
        for (rendered, source) in out.iter().zip(lines.iter()) {
            assert_eq!(&plain(rendered), source);
        }
        let fn_span = out[0]
            .iter()
            .find(|s| s.content.as_ref() == "fn")
            .expect("fn span present");
        assert_eq!(fn_span.style.fg, Some(theme.code.keyword));
    }

    #[test]
    fn follows_the_host_theme() {
        // A different palette restyles the same token.
        let mut theme = Theme::default();
        theme.code.keyword = ratatui::style::Color::Indexed(200);
        let hl = TreeSitterHighlighter::new();
        let out = hl
            .highlight("rust", &["fn f() {}"], &theme)
            .expect("rust highlights");
        let fn_span = out[0]
            .iter()
            .find(|s| s.content.as_ref() == "fn")
            .expect("fn span");
        assert_eq!(fn_span.style.fg, Some(ratatui::style::Color::Indexed(200)));
    }

    #[test]
    fn aliases_resolve_and_js_uses_typescript_grammar() {
        let theme = Theme::default();
        let hl = TreeSitterHighlighter::new();
        assert_eq!(canonical_language("py"), Some("python"));
        assert_eq!(canonical_language("js"), Some("typescript"));
        assert_eq!(canonical_language("c#"), Some("csharp"));
        assert!(hl.highlight("js", &["const x = 1;"], &theme).is_some());
    }

    #[test]
    fn unsupported_language_returns_none() {
        let theme = Theme::default();
        let hl = TreeSitterHighlighter::new();
        assert!(hl.highlight("brainfuck", &["+++"], &theme).is_none());
        assert_eq!(canonical_language("whatever"), None);
    }

    #[test]
    fn blank_lines_inside_a_block_are_preserved() {
        let theme = Theme::default();
        let hl = TreeSitterHighlighter::new();
        let lines = vec!["x = 1", "", "y = 2"];
        let out = hl
            .highlight("python", &lines, &theme)
            .expect("python highlights");
        assert_eq!(out.len(), 3);
        assert_eq!(plain(&out[1]), "");
    }
}

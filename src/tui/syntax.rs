//! Language-aware syntax highlighting for fenced code blocks, using the
//! tree-sitter grammars yolop already bundles (the same ones `repo_map` and
//! `ast_grep` parse with) plus `tree-sitter-highlight`.
//!
//! [`highlight_lines`] takes a fence's language tag and its body lines and
//! returns per-line styled spans, or `None` when the language is unsupported or
//! the source fails to parse — in which case the caller falls back to the
//! lightweight keyword highlighter. Highlight configurations are built once per
//! language and cached on the rendering thread.

use super::{ACCENT_BLUE, ACCENT_GOLD, DIFF_ADD, TEXT_DIM, TEXT_MUTED, TEXT_PRIMARY};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use tree_sitter_highlight::{Highlight, HighlightConfiguration, HighlightEvent, Highlighter};

/// Muted teal for type names; amber for numbers/constants. Chosen to sit
/// alongside the existing accent palette without shouting.
const SYNTAX_TYPE: Color = Color::Rgb(126, 170, 176);
const SYNTAX_CONST: Color = Color::Rgb(184, 152, 120);

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

fn style_for_name(name: &str) -> Style {
    let base = Style::default();
    match name {
        "keyword" => base.fg(ACCENT_GOLD).add_modifier(Modifier::BOLD),
        "function" | "function.builtin" | "function.method" | "constructor" | "label" => {
            base.fg(ACCENT_BLUE)
        }
        "type" | "type.builtin" => base.fg(SYNTAX_TYPE),
        "constant" | "constant.builtin" | "constant.numeric" | "number" | "variable.builtin" => {
            base.fg(SYNTAX_CONST)
        }
        "string" | "string.special" | "escape" => base.fg(DIFF_ADD),
        "comment" => base.fg(TEXT_MUTED).add_modifier(Modifier::ITALIC),
        "operator"
        | "punctuation"
        | "punctuation.bracket"
        | "punctuation.delimiter"
        | "punctuation.special" => base.fg(TEXT_DIM),
        "attribute" | "tag" => base.fg(ACCENT_GOLD),
        _ => base.fg(TEXT_PRIMARY),
    }
}

fn default_style() -> Style {
    Style::default().fg(TEXT_PRIMARY)
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

/// Highlight a fenced code block, returning one span vector per input line.
/// Returns `None` for unsupported languages or on any parse failure so callers
/// can fall back to the lightweight highlighter.
pub(crate) fn highlight_lines(lang: &str, lines: &[&str]) -> Option<Vec<Vec<Span<'static>>>> {
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

    let mut style_stack: Vec<Style> = Vec::new();
    let mut out: Vec<Vec<Span<'static>>> = vec![Vec::new()];
    for event in events {
        match event.ok()? {
            HighlightEvent::HighlightStart(Highlight(index)) => {
                let style = HIGHLIGHT_NAMES
                    .get(index)
                    .map(|name| style_for_name(name))
                    .unwrap_or_else(default_style);
                style_stack.push(style);
            }
            HighlightEvent::HighlightEnd => {
                style_stack.pop();
            }
            HighlightEvent::Source { start, end } => {
                let style = style_stack.last().copied().unwrap_or_else(default_style);
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

    fn plain(spans: &[Span<'static>]) -> String {
        spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn highlights_rust_keywords_distinctly() {
        let lines = vec!["fn main() {", "    let x = 1;", "}"];
        let out = highlight_lines("rust", &lines).expect("rust highlights");
        assert_eq!(out.len(), 3);
        // Each output line reconstructs its source exactly.
        for (rendered, source) in out.iter().zip(lines.iter()) {
            assert_eq!(&plain(rendered), source);
        }
        // `fn` is a keyword and must carry the keyword color, not the default.
        let fn_span = out[0]
            .iter()
            .find(|s| s.content.as_ref() == "fn")
            .expect("fn span present");
        assert_eq!(fn_span.style.fg, Some(ACCENT_GOLD));
    }

    #[test]
    fn aliases_resolve_and_js_uses_typescript_grammar() {
        assert_eq!(canonical_language("py"), Some("python"));
        assert_eq!(canonical_language("js"), Some("typescript"));
        assert_eq!(canonical_language("c#"), Some("csharp"));
        assert!(highlight_lines("js", &["const x = 1;"]).is_some());
    }

    #[test]
    fn unsupported_language_returns_none() {
        assert!(highlight_lines("brainfuck", &["+++"]).is_none());
        assert_eq!(canonical_language("whatever"), None);
    }

    #[test]
    fn blank_lines_inside_a_block_are_preserved() {
        let lines = vec!["x = 1", "", "y = 2"];
        let out = highlight_lines("python", &lines).expect("python highlights");
        assert_eq!(out.len(), 3);
        assert_eq!(plain(&out[1]), "");
    }
}

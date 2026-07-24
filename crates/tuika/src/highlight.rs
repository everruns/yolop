//! The [`Highlighter`] seam — how a host plugs syntax highlighting into
//! [`CodeBlock`](crate::components::CodeBlock) and
//! [`Markdown`](crate::components::Markdown) without tuika taking on any grammar
//! dependency.
//!
//! tuika deliberately depends only on `ratatui`, `crossterm`, `textwrap`, and
//! the `unicode-*` crates; a real highlighter (tree-sitter, syntect, …) pulls in
//! far more. So the toolkit owns only this trait and the *presentation* of code
//! (framing, background, language label, wrapping), while the host supplies the
//! token colors. The companion crate `tuika-codeformatters` ships a tree-sitter
//! implementation; hosts can also write their own.

use ratatui_core::text::Span;

use crate::style::Theme;

/// Turns a fenced code block's source into per-line styled spans.
///
/// Implementations receive the fence's language tag, the block's body split
/// into lines, and the active [`Theme`] (so highlighted tokens follow the host
/// palette via [`Theme::code`](crate::style::CodeTheme)). They return **one span
/// vector per input line** — reconstructing each source line exactly, only
/// restyled — or [`None`] when the language is unknown or the source fails to
/// parse, in which case the caller falls back to unstyled code text.
///
/// The one-line-per-line contract lets callers zip the result against the
/// source without re-deriving line boundaries; an implementation that cannot
/// uphold it (event stream desynced from source lines) must return [`None`].
pub trait Highlighter {
    /// Highlight `lines` of `lang` source, returning one styled span vector per
    /// input line (same length and order), or [`None`] if the language is
    /// unknown or the source cannot be highlighted line-for-line.
    fn highlight(
        &self,
        lang: &str,
        lines: &[&str],
        theme: &Theme,
    ) -> Option<Vec<Vec<Span<'static>>>>;
}

/// A [`Highlighter`] that highlights nothing — every block renders as plain,
/// theme-colored code text. The default when a caller supplies no highlighter.
#[derive(Clone, Copy, Debug, Default)]
pub struct PlainHighlighter;

impl Highlighter for PlainHighlighter {
    fn highlight(
        &self,
        _lang: &str,
        _lines: &[&str],
        _theme: &Theme,
    ) -> Option<Vec<Vec<Span<'static>>>> {
        None
    }
}

/// A borrowed highlighter, or the plain fallback. Lets [`CodeBlock`] and
/// [`Markdown`] carry an optional highlighter without a generic parameter
/// leaking through the whole view tree.
///
/// [`CodeBlock`]: crate::components::CodeBlock
/// [`Markdown`]: crate::components::Markdown
#[derive(Clone, Copy, Default)]
pub enum CodeHighlighter<'a> {
    /// No highlighting; render plain theme-colored code.
    #[default]
    Plain,
    /// Delegate to a host-supplied highlighter.
    With(&'a dyn Highlighter),
}

impl<'a> CodeHighlighter<'a> {
    /// Highlight `lines`, or `None` for the plain fallback.
    pub fn highlight(
        &self,
        lang: &str,
        lines: &[&str],
        theme: &Theme,
    ) -> Option<Vec<Vec<Span<'static>>>> {
        match self {
            CodeHighlighter::Plain => None,
            CodeHighlighter::With(h) => h.highlight(lang, lines, theme),
        }
    }
}

impl<'a> From<&'a dyn Highlighter> for CodeHighlighter<'a> {
    fn from(h: &'a dyn Highlighter) -> Self {
        CodeHighlighter::With(h)
    }
}

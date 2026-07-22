//! [`CodeBlock`] — a themed, framed code block with pluggable syntax
//! highlighting.
//!
//! The component owns *presentation* — a language label, a left rail, a code
//! background, verbatim (never-reflowed) body lines — while token colors come
//! from a [`Highlighter`](crate::highlight::Highlighter) the host supplies (see
//! [`CodeHighlighter`]). With no highlighter, or for an unknown language, the
//! body renders as plain [`Theme::code`](crate::style::CodeTheme)-colored text.
//!
//! Code lines are drawn faithfully (leading whitespace preserved, clipped to
//! width, never word-wrapped) because indentation is meaningful in code — unlike
//! prose, which [`Markdown`](crate::components::Markdown) word-wraps.

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::components::text::line_width;
use crate::geometry::Size;
use crate::highlight::CodeHighlighter;
use crate::style::Theme;
use crate::surface::Surface;
use crate::view::{RenderCtx, View};

/// The left rail glyph + trailing pad that frames every code line.
const RAIL: &str = "▏ ";

/// Build the styled lines for a fenced code block: an optional language-label
/// row followed by one verbatim row per source line.
///
/// `highlighter` colors the body; when it declines (unknown language / parse
/// failure) every line falls back to plain [`CodeTheme::text`] over the code
/// background. The returned lines carry no outer indent — callers ([`CodeBlock`]
/// and [`Markdown`]) add their own — and must be drawn **without** word-wrapping
/// so code indentation survives.
///
/// [`CodeTheme::text`]: crate::style::CodeTheme::text
pub(crate) fn code_block_lines(
    lang: &str,
    body: &[&str],
    theme: &Theme,
    highlighter: CodeHighlighter,
    show_label: bool,
) -> Vec<Line<'static>> {
    let code = &theme.code;
    let rail_style = Style::default().fg(code.label).bg(code.background);
    let plain = Style::default().fg(code.text).bg(code.background);

    let mut out = Vec::new();

    let label = lang.trim();
    if show_label && !label.is_empty() {
        out.push(Line::from(vec![Span::styled(
            format!("{RAIL}{label}"),
            Style::default()
                .fg(code.label)
                .add_modifier(Modifier::ITALIC),
        )]));
    }

    // Highlighted spans (one vector per body line) or the plain fallback.
    let highlighted = highlighter.highlight(label, body, theme);

    for (row, source) in body.iter().enumerate() {
        let mut spans = vec![Span::styled(RAIL.to_string(), rail_style)];
        match highlighted.as_ref().and_then(|lines| lines.get(row)) {
            Some(cells) if !cells.is_empty() => {
                // Layer the code background under each highlighted span, keeping
                // its foreground, so the whole line reads as one block.
                for cell in cells {
                    spans.push(Span::styled(
                        cell.content.to_string(),
                        cell.style.bg(code.background),
                    ));
                }
            }
            _ => spans.push(Span::styled((*source).to_string(), plain)),
        }
        out.push(Line::from(spans));
    }

    out
}

/// A themed, syntax-highlighted code block.
///
/// ```no_run
/// use tuika::{CodeBlock, Theme};
/// let theme = Theme::default();
/// let block = CodeBlock::new("rust", "fn main() {}").label(true);
/// // `block` is a `View`; render it through `tuika::paint`, or embed it in a
/// // `Flex`. Supply a highlighter with `.highlighter(&my_highlighter)`.
/// # let _ = (theme, block);
/// ```
pub struct CodeBlock<'a> {
    lang: String,
    body: Vec<String>,
    highlighter: CodeHighlighter<'a>,
    show_label: bool,
}

impl<'a> CodeBlock<'a> {
    /// A code block for `source` in language `lang` (e.g. `"rust"`, `"py"`, or
    /// `""` for no language). `source` is split on newlines into body lines.
    pub fn new(lang: impl Into<String>, source: impl AsRef<str>) -> Self {
        Self {
            lang: lang.into(),
            body: source.as_ref().split('\n').map(str::to_owned).collect(),
            highlighter: CodeHighlighter::Plain,
            show_label: true,
        }
    }

    /// Plug in a syntax highlighter; without one the body renders plain.
    pub fn highlighter(mut self, highlighter: &'a dyn crate::highlight::Highlighter) -> Self {
        self.highlighter = CodeHighlighter::With(highlighter);
        self
    }

    /// Whether to show the language-label row (default `true`).
    pub fn label(mut self, show: bool) -> Self {
        self.show_label = show;
        self
    }

    fn lines(&self, theme: &Theme) -> Vec<Line<'static>> {
        let body: Vec<&str> = self.body.iter().map(String::as_str).collect();
        code_block_lines(&self.lang, &body, theme, self.highlighter, self.show_label)
    }
}

impl View for CodeBlock<'_> {
    fn measure(&self, available: Size) -> Size {
        let theme = Theme::default();
        let lines = self.lines(&theme);
        let width = lines
            .iter()
            .map(|l| line_width(l))
            .max()
            .unwrap_or(0)
            .min(available.width);
        Size::new(width, lines.len() as u16)
    }

    fn render(&self, area: Rect, surface: &mut Surface, ctx: &RenderCtx) {
        let lines = self.lines(ctx.theme);
        for (row, line) in lines.iter().enumerate() {
            let y = area.y.saturating_add(row as u16);
            if y >= area.bottom() {
                break;
            }
            let mut x = area.x;
            for span in &line.spans {
                if x >= area.right() {
                    break;
                }
                x = surface.set_string(x, y, span.content.as_ref(), span.style);
            }
        }
    }
}

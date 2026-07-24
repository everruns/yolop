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

use ratatui_core::layout::Rect;
use ratatui_core::style::{Modifier, Style};
use ratatui_core::text::{Line, Span};

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
/// background. `gutter` is the starting line number when a right-aligned
/// line-number column should precede the rail, or `None` for no gutter. The
/// returned lines carry no outer indent — callers ([`CodeBlock`] and
/// [`Markdown`]) add their own — and must be drawn **without** word-wrapping so
/// code indentation survives.
///
/// [`CodeTheme::text`]: crate::style::CodeTheme::text
pub(crate) fn code_block_lines(
    lang: &str,
    body: &[&str],
    theme: &Theme,
    highlighter: CodeHighlighter,
    show_label: bool,
    gutter: Option<usize>,
) -> Vec<Line<'static>> {
    let code = &theme.code;
    let rail_style = Style::default().fg(code.label).bg(code.background);
    let plain = Style::default().fg(code.text).bg(code.background);
    let gutter_style = Style::default().fg(code.label).bg(code.background);

    // Width of the numeric column: enough digits for the last line, plus a
    // leading and trailing space, shared by every row so the rail stays aligned.
    let gutter_width = gutter.map(|start| {
        let last = start + body.len().saturating_sub(1);
        let digits = last.to_string().len();
        digits + 2
    });

    let mut out = Vec::new();

    let label = lang.trim();
    if show_label && !label.is_empty() {
        let mut spans = Vec::new();
        if let Some(w) = gutter_width {
            spans.push(Span::styled(" ".repeat(w), gutter_style));
        }
        spans.push(Span::styled(
            format!("{RAIL}{label}"),
            Style::default()
                .fg(code.label)
                .add_modifier(Modifier::ITALIC),
        ));
        out.push(Line::from(spans));
    }

    // Highlighted spans (one vector per body line) or the plain fallback.
    let highlighted = highlighter.highlight(label, body, theme);

    for (row, source) in body.iter().enumerate() {
        let mut spans = Vec::new();
        if let (Some(start), Some(w)) = (gutter, gutter_width) {
            // Right-align the number within `w-1` cells, then a trailing space.
            spans.push(Span::styled(
                format!(" {:>width$} ", start + row, width = w - 2),
                gutter_style,
            ));
        }
        spans.push(Span::styled(RAIL.to_string(), rail_style));
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

/// A themed, syntax-highlighted code block — a language label, a left rail, a
/// code background, and verbatim (never-reflowed) body lines.
///
/// ![code_block demo](https://raw.githubusercontent.com/everruns/yolop/main/crates/tuika/docs/demos/code_block.gif)
///
/// Colors come entirely from [`Theme::code`](crate::CodeTheme); token classes are
/// resolved by whatever [`Highlighter`](crate::Highlighter) you plug in (none →
/// plain, theme-colored text).
///
/// # Options
///
/// | Builder | Default | Effect |
/// | --- | --- | --- |
/// | [`new(lang, source)`](Self::new) | — | language tag + source (split on `\n`) |
/// | [`highlighter(&h)`](Self::highlighter) | plain | plug in syntax highlighting |
/// | [`label(bool)`](Self::label) | `true` | show/hide the language-label row |
/// | [`line_numbers(bool)`](Self::line_numbers) | `false` | show a line-number gutter |
/// | [`start_line(n)`](Self::start_line) | `1` | first gutter line number |
///
/// ```no_run
/// use tuika::{CodeBlock, Theme};
/// let theme = Theme::default();
/// // No highlighter → plain, theme-colored code; hide the label row.
/// let block = CodeBlock::new("rust", "fn main() {}").label(false);
/// // `block` is a `View`; render it through `tuika::paint` or embed it in a
/// // `Flex`. Supply a highlighter with `.highlighter(&my_highlighter)` — see
/// // the `tuika-codeformatters` crate for a tree-sitter one.
/// # let _ = (theme, block);
/// ```
pub struct CodeBlock<'a> {
    lang: String,
    body: Vec<String>,
    highlighter: CodeHighlighter<'a>,
    show_label: bool,
    start_line: Option<usize>,
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
            start_line: None,
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

    /// Show (or hide) a right-aligned line-number gutter before the rail. The
    /// gutter counts from [`start_line`](Self::start_line) (default `1`).
    pub fn line_numbers(mut self, show: bool) -> Self {
        self.start_line = show.then(|| self.start_line.unwrap_or(1));
        self
    }

    /// Set the first gutter line number, implying [`line_numbers(true)`](Self::line_numbers).
    /// Useful when a block is a slice of a larger file.
    pub fn start_line(mut self, first: usize) -> Self {
        self.start_line = Some(first);
        self
    }

    fn lines(&self, theme: &Theme) -> Vec<Line<'static>> {
        let body: Vec<&str> = self.body.iter().map(String::as_str).collect();
        code_block_lines(
            &self.lang,
            &body,
            theme,
            self.highlighter,
            self.show_label,
            self.start_line,
        )
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::style::Theme;
    use crate::test_support::row;

    #[test]
    fn line_numbers_gutter_counts_and_aligns() {
        let theme = Theme::default();
        // Nine lines so the gutter is one digit wide; label row hidden.
        let src = (1..=9)
            .map(|n| format!("line{n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let block = CodeBlock::new("", &src).label(false).line_numbers(true);
        let buf = crate::testing::render(&block, 30, 9, &theme);
        assert!(
            row(&buf, 0).starts_with(" 1 "),
            "first row: {:?}",
            row(&buf, 0)
        );
        assert!(
            row(&buf, 8).starts_with(" 9 "),
            "last row: {:?}",
            row(&buf, 8)
        );
        // The rail follows the gutter.
        assert!(row(&buf, 0).contains('▏'));
    }

    #[test]
    fn start_line_offsets_and_widens_gutter() {
        let theme = Theme::default();
        // Starting at 99 with two lines crosses into three digits (99, 100), so
        // the gutter widens to keep the rail aligned across all rows.
        let block = CodeBlock::new("", "a\nb").label(false).start_line(99);
        let buf = crate::testing::render(&block, 30, 2, &theme);
        assert!(
            row(&buf, 0).starts_with("  99 "),
            "row0: {:?}",
            row(&buf, 0)
        );
        assert!(
            row(&buf, 1).starts_with(" 100 "),
            "row1: {:?}",
            row(&buf, 1)
        );
    }

    #[test]
    fn no_gutter_by_default() {
        let theme = Theme::default();
        let block = CodeBlock::new("", "x").label(false);
        let buf = crate::testing::render(&block, 20, 1, &theme);
        // Rail is first, with no leading numeric column.
        assert!(row(&buf, 0).starts_with('▏'), "row0: {:?}", row(&buf, 0));
    }
}

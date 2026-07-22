//! Text and paragraph components.
//!
//! [`Text`] draws pre-styled ratatui [`Line`]s faithfully (mixed styles per
//! line preserved), clipping to its area. [`Paragraph`] takes a plain string
//! plus one style and word-wraps it to the available width. [`Wrap`] is the
//! styled counterpart to `Paragraph`: it word-wraps pre-styled `Line`s while
//! preserving each span's style across the reflow (see [`wrap_lines`]).

use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_segmentation::UnicodeSegmentation;

use crate::geometry::Size;
use crate::surface::Surface;
use crate::view::{RenderCtx, View};
use crate::width::{grapheme_cols, str_cols};

/// Draw pre-styled `lines` top-down from `area`'s origin, clipping to `area`.
/// Shared by [`Text`] and [`Wrap`].
fn draw_lines(lines: &[Line<'static>], area: Rect, surface: &mut Surface) {
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

/// Display width of a styled line (sum of span widths).
pub fn line_width(line: &Line) -> u16 {
    line.spans
        .iter()
        .map(|s| str_cols(s.content.as_ref()))
        .fold(0, u16::saturating_add)
}

/// A block of pre-styled lines, drawn top-down and clipped.
///
/// ![text demo](https://raw.githubusercontent.com/everruns/yolop/main/crates/tuika/docs/demos/text.gif)
pub struct Text {
    lines: Vec<Line<'static>>,
}

impl Text {
    pub fn new(lines: Vec<Line<'static>>) -> Self {
        Self { lines }
    }

    /// A single unstyled line from a string.
    pub fn raw(text: impl Into<String>) -> Self {
        Self::new(vec![Line::from(text.into())])
    }
}

impl View for Text {
    fn measure(&self, _available: Size) -> Size {
        let width = self.lines.iter().map(line_width).max().unwrap_or(0);
        Size::new(width, self.lines.len() as u16)
    }

    fn render(&self, area: Rect, surface: &mut Surface, _ctx: &RenderCtx) {
        draw_lines(&self.lines, area, surface);
    }
}

/// Plain text word-wrapped to the render width in one style.
pub struct Paragraph {
    text: String,
    style: Style,
}

impl Paragraph {
    pub fn new(text: impl Into<String>, style: Style) -> Self {
        Self {
            text: text.into(),
            style,
        }
    }

    fn wrap(&self, width: u16) -> Vec<String> {
        if width == 0 {
            return Vec::new();
        }
        self.text
            .split('\n')
            .flat_map(|para| {
                let wrapped = textwrap::wrap(para, width as usize);
                if wrapped.is_empty() {
                    vec![String::new()]
                } else {
                    wrapped.into_iter().map(|c| c.into_owned()).collect()
                }
            })
            .collect()
    }
}

impl View for Paragraph {
    fn measure(&self, available: Size) -> Size {
        let lines = self.wrap(available.width);
        let width = lines
            .iter()
            .map(|l| str_cols(l.as_str()))
            .max()
            .unwrap_or(0);
        Size::new(width, lines.len() as u16)
    }

    fn render(&self, area: Rect, surface: &mut Surface, _ctx: &RenderCtx) {
        for (row, line) in self.wrap(area.width).into_iter().enumerate() {
            let y = area.y.saturating_add(row as u16);
            if y >= area.bottom() {
                break;
            }
            surface.set_string(area.x, y, &line, self.style);
        }
    }
}

/// Whether a grapheme cluster is a break opportunity (all-whitespace).
fn is_break(cluster: &str) -> bool {
    cluster.chars().all(char::is_whitespace)
}

/// Word-wrap pre-styled `lines` to `width` columns, preserving each span's
/// style across the wrap.
///
/// Unlike [`Paragraph`] (single style, plain text), the input may be
/// multi-styled — highlighted code, a linkified URL run, a diff line — and the
/// per-span styling survives the reflow. Wrapping is greedy and word-oriented:
/// runs of whitespace collapse to a single break opportunity, a word longer
/// than `width` is hard-broken so no output line exceeds `width`, and a blank
/// (empty or all-whitespace) input line stays exactly one blank output line.
/// Widths are counted in display columns, so wide/CJK glyphs wrap correctly.
/// A `width` of 0 returns the input unchanged.
pub fn wrap_lines(lines: &[Line<'static>], width: u16) -> Vec<Line<'static>> {
    if width == 0 {
        return lines.to_vec();
    }
    let mut out = Vec::new();
    for line in lines {
        wrap_one(line, width, &mut out);
    }
    out
}

fn wrap_one(line: &Line<'static>, width: u16, out: &mut Vec<Line<'static>>) {
    // Cells are grapheme clusters, not `char`s, so a multi-scalar emoji stays
    // intact across the reflow instead of being split mid-cluster.
    let cells: Vec<(&str, Style)> = line
        .spans
        .iter()
        .flat_map(|s| s.content.graphemes(true).map(move |g| (g, s.style)))
        .collect();
    let before = out.len();
    let mut cur: Vec<(&str, Style)> = Vec::new();
    let mut cur_w = 0u16;
    let mut i = 0;
    let n = cells.len();
    while i < n {
        // Collapse a run of whitespace into a single break opportunity.
        if is_break(cells[i].0) {
            i += 1;
            continue;
        }
        // Gather one word (a maximal run of non-whitespace).
        let start = i;
        let mut word_w = 0u16;
        while i < n && !is_break(cells[i].0) {
            word_w = word_w.saturating_add(grapheme_cols(cells[i].0));
            i += 1;
        }
        let word = &cells[start..i];
        let sep = u16::from(!cur.is_empty());
        if word_w <= width && cur_w + sep + word_w <= width {
            // Fits on the current line (with a joining space if needed). The
            // space inherits the preceding cell's style so a background run
            // stays continuous across the join.
            if sep == 1 {
                let prev = cur.last().map(|c| c.1).unwrap_or_default();
                cur.push((" ", prev));
                cur_w += 1;
            }
            cur.extend_from_slice(word);
            cur_w += word_w;
        } else if word_w <= width {
            // Doesn't fit; break to a new line, then place the word.
            if !cur.is_empty() {
                out.push(coalesce(&cur));
                cur.clear();
            }
            cur.extend_from_slice(word);
            cur_w = word_w;
        } else {
            // Word wider than the line: hard-break it across lines.
            if !cur.is_empty() {
                out.push(coalesce(&cur));
                cur.clear();
                cur_w = 0;
            }
            for &(g, st) in word {
                let w = grapheme_cols(g);
                if cur_w + w > width && !cur.is_empty() {
                    out.push(coalesce(&cur));
                    cur.clear();
                    cur_w = 0;
                }
                cur.push((g, st));
                cur_w += w;
            }
        }
    }
    if !cur.is_empty() {
        out.push(coalesce(&cur));
    }
    // Preserve a blank (empty or all-whitespace) input line as one blank row.
    if out.len() == before {
        out.push(Line::default());
    }
}

/// Merge a run of styled grapheme cells into a [`Line`], coalescing adjacent
/// cells with equal style into one [`Span`].
fn coalesce(cells: &[(&str, Style)]) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut buf = String::new();
    let mut run: Option<Style> = None;
    for &(g, st) in cells {
        match run {
            Some(s) if s == st => buf.push_str(g),
            _ => {
                if let Some(s) = run.take() {
                    spans.push(Span::styled(std::mem::take(&mut buf), s));
                }
                run = Some(st);
                buf.push_str(g);
            }
        }
    }
    if let Some(s) = run {
        spans.push(Span::styled(buf, s));
    }
    Line::from(spans)
}

/// Multi-styled text, word-wrapped to the render width with per-span styles
/// preserved.
///
/// This is the styled counterpart to [`Paragraph`]: feed it the styled
/// [`Line`]s a host already builds — syntax-highlighted code, linkified URLs, a
/// colored diff — and it reflows them to the available width without flattening
/// the styling (see [`wrap_lines`] for the exact wrapping rules).
pub struct Wrap {
    lines: Vec<Line<'static>>,
}

impl Wrap {
    pub fn new(lines: Vec<Line<'static>>) -> Self {
        Self { lines }
    }
}

impl View for Wrap {
    fn measure(&self, available: Size) -> Size {
        let wrapped = wrap_lines(&self.lines, available.width);
        let width = wrapped.iter().map(line_width).max().unwrap_or(0);
        Size::new(width.min(available.width), wrapped.len() as u16)
    }

    fn render(&self, area: Rect, surface: &mut Surface, _ctx: &RenderCtx) {
        let wrapped = wrap_lines(&self.lines, area.width);
        draw_lines(&wrapped, area, surface);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::style::Theme;
    use crate::test_support::{buffer, row};
    use crate::view::{RenderCtx, View};
    use crate::{Size, Surface};
    use ratatui::style::{Color, Modifier, Style};
    use ratatui::text::{Line, Span};

    /// Concatenated text of a line's spans.
    fn line_text(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn text_renders_and_clips_to_width() {
        let mut buf = buffer(6, 2);
        let text = Text::new(vec![Line::from("hello world"), Line::from("hi")]);
        let theme = Theme::default();
        let ctx = RenderCtx::new(&theme);
        let area = buf.area;
        let mut surface = Surface::new(&mut buf, area);
        text.render(area, &mut surface, &ctx);
        // Clipped to 6 columns ("hello " with a trailing space, which `row` trims).
        assert_eq!(row(&buf, 0), "hello");
        assert_eq!(row(&buf, 1), "hi");
    }

    #[test]
    fn paragraph_wraps_to_width() {
        let p = Paragraph::new("the quick brown fox", Style::default());
        let size = p.measure(Size::new(10, 10));
        assert!(size.height >= 2, "expected wrap, got {size:?}");
        assert!(size.width <= 10);
    }

    #[test]
    fn wrap_lines_breaks_on_word_boundaries() {
        let out = wrap_lines(&[Line::from("the quick brown fox jumps")], 9);
        assert!(
            out.iter().all(|l| line_width(l) <= 9),
            "no output line may exceed the width: {out:?}"
        );
        // Every word survives, in order, un-split.
        let words: Vec<String> = out
            .iter()
            .flat_map(|l| {
                line_text(l)
                    .split_whitespace()
                    .map(String::from)
                    .collect::<Vec<_>>()
            })
            .collect();
        assert_eq!(words, ["the", "quick", "brown", "fox", "jumps"]);
    }

    #[test]
    fn wrap_lines_preserves_span_styles() {
        let red = Style::default().fg(Color::Red);
        let blue = Style::default().fg(Color::Blue);
        let line = Line::from(vec![
            Span::styled("red", red),
            Span::raw(" "),
            Span::styled("blue", blue),
        ]);
        // Wide enough that nothing wraps.
        let out = wrap_lines(&[line], 40);
        assert_eq!(out.len(), 1);
        let spans = &out[0].spans;
        assert!(
            spans
                .iter()
                .any(|s| s.content.starts_with("red") && s.style.fg == Some(Color::Red)),
            "red run lost its style: {spans:?}"
        );
        assert!(
            spans
                .iter()
                .any(|s| s.content.contains("blue") && s.style.fg == Some(Color::Blue)),
            "blue run lost its style: {spans:?}"
        );
    }

    #[test]
    fn wrap_lines_style_survives_a_break() {
        let accent = Style::default()
            .fg(Color::Blue)
            .add_modifier(Modifier::UNDERLINED);
        // "aaaa bbbb", all accent, width 4 -> two lines, both still accent.
        let out = wrap_lines(&[Line::from(Span::styled("aaaa bbbb", accent))], 4);
        assert_eq!(out.len(), 2, "{out:?}");
        for l in &out {
            assert!(
                l.spans.iter().all(|s| s.style.fg == Some(Color::Blue)
                    && s.style.add_modifier.contains(Modifier::UNDERLINED)),
                "wrapped line dropped styling: {l:?}"
            );
        }
    }

    #[test]
    fn wrap_lines_hard_breaks_overlong_word() {
        let word = "x".repeat(20);
        let out = wrap_lines(&[Line::from(word.clone())], 8);
        assert!(out.len() >= 3, "a 20-col word at width 8 needs >=3 lines");
        assert!(out.iter().all(|l| line_width(l) <= 8));
        let joined: String = out.iter().map(|l| line_text(l)).collect();
        assert_eq!(joined, word, "hard-break must not lose characters");
    }

    #[test]
    fn wrap_lines_counts_wide_glyphs() {
        // Each CJK glyph is 2 columns; no whitespace, so it hard-breaks at width 4.
        let out = wrap_lines(&[Line::from("你好世界")], 4);
        assert!(out.iter().all(|l| line_width(l) <= 4), "{out:?}");
        let joined: String = out.iter().map(|l| line_text(l)).collect();
        assert_eq!(joined, "你好世界");
    }

    #[test]
    fn wrap_lines_keeps_emoji_clusters_intact() {
        // "❤️" carries VS16 → width 2. A grapheme must never be split mid-cluster
        // by the wrapper, and each output line must respect the width budget.
        let out = wrap_lines(&[Line::from("❤\u{FE0F} 你 ok")], 4);
        assert!(out.iter().all(|l| line_width(l) <= 4), "{out:?}");
        let joined: String = out.iter().map(|l| line_text(l)).collect();
        assert!(
            joined.contains("❤\u{FE0F}"),
            "heart+VS16 survived: {joined:?}"
        );
    }

    #[test]
    fn wrap_lines_keeps_blank_lines() {
        let lines = vec![Line::from("a"), Line::from(""), Line::from("b")];
        let out = wrap_lines(&lines, 10);
        assert_eq!(
            out.len(),
            3,
            "a blank line must stay one blank row: {out:?}"
        );
        assert_eq!(line_text(&out[1]), "");
    }

    #[test]
    fn wrap_lines_zero_width_is_identity() {
        let out = wrap_lines(&[Line::from("hello world")], 0);
        assert_eq!(out.len(), 1);
        assert_eq!(line_text(&out[0]), "hello world");
    }

    #[test]
    fn wrap_component_renders_reflowed() {
        // "aa bb cc" at width 5 wraps to "aa bb" / "cc".
        let mut buf = buffer(5, 3);
        let w = Wrap::new(vec![Line::from("aa bb cc")]);
        let theme = Theme::default();
        let ctx = RenderCtx::new(&theme);
        let area = buf.area;
        let mut surface = Surface::new(&mut buf, area);
        w.render(area, &mut surface, &ctx);
        assert_eq!(row(&buf, 0), "aa bb");
        assert_eq!(row(&buf, 1), "cc");
    }
}

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
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::geometry::Size;
use crate::surface::Surface;
use crate::view::{RenderCtx, View};

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
        .map(|s| UnicodeWidthStr::width(s.content.as_ref()) as u16)
        .fold(0, u16::saturating_add)
}

/// A block of pre-styled lines, drawn top-down and clipped.
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
            .map(|l| UnicodeWidthStr::width(l.as_str()) as u16)
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

/// Display columns of a single char (0 for zero-width).
fn char_cols(ch: char) -> u16 {
    UnicodeWidthChar::width(ch).unwrap_or(0) as u16
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
    let cells: Vec<(char, Style)> = line
        .spans
        .iter()
        .flat_map(|s| s.content.chars().map(move |c| (c, s.style)))
        .collect();
    let before = out.len();
    let mut cur: Vec<(char, Style)> = Vec::new();
    let mut cur_w = 0u16;
    let mut i = 0;
    let n = cells.len();
    while i < n {
        // Collapse a run of whitespace into a single break opportunity.
        if cells[i].0.is_whitespace() {
            i += 1;
            continue;
        }
        // Gather one word (a maximal run of non-whitespace).
        let start = i;
        let mut word_w = 0u16;
        while i < n && !cells[i].0.is_whitespace() {
            word_w = word_w.saturating_add(char_cols(cells[i].0));
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
                cur.push((' ', prev));
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
            for &(ch, st) in word {
                let w = char_cols(ch);
                if cur_w + w > width && !cur.is_empty() {
                    out.push(coalesce(&cur));
                    cur.clear();
                    cur_w = 0;
                }
                cur.push((ch, st));
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

/// Merge a run of styled cells into a [`Line`], coalescing adjacent cells with
/// equal style into one [`Span`].
fn coalesce(cells: &[(char, Style)]) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut buf = String::new();
    let mut run: Option<Style> = None;
    for &(ch, st) in cells {
        match run {
            Some(s) if s == st => buf.push(ch),
            _ => {
                if let Some(s) = run.take() {
                    spans.push(Span::styled(std::mem::take(&mut buf), s));
                }
                run = Some(st);
                buf.push(ch);
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

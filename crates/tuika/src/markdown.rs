//! Streaming Markdown rendering.
//!
//! [`MarkdownState`] renders CommonMark (via `pulldown-cmark`) to styled
//! [`Line`]s, incrementally: as a message streams in, only the **in-flight tail**
//! is re-parsed each frame. Everything before the last *stable block boundary*
//! (a blank line outside an open code fence) is parsed and highlighted once and
//! cached — so a long transcript does not re-tokenize, and tree-sitter does not
//! re-highlight settled code blocks, on every delta. This mirrors the split
//! Hermes' TUI uses for its streaming markdown.
//!
//! Output is width-aware and correct for both prose and code: prose is
//! word-wrapped (via [`wrap_lines`]) while code and tables are emitted verbatim,
//! because indentation and column alignment are meaningful there. Callers draw
//! the returned lines **without** further wrapping (e.g. ratatui's `Paragraph`
//! with no `.wrap`, or tuika's [`Text`](crate::components::Text)).
//!
//! For one-shot (non-streaming) text, [`markdown_to_lines`] renders a whole
//! string in one call. The [`Markdown`] view wraps either for direct placement
//! in a layout.

use pulldown_cmark::{Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::components::code_block::code_block_lines;
use crate::components::{line_width, wrap_lines};
use crate::geometry::Size;
use crate::highlight::{CodeHighlighter, Highlighter};
use crate::style::Theme;
use crate::surface::Surface;
use crate::view::{RenderCtx, View};

/// Columns of indentation added per level of list / block-quote nesting.
const INDENT: u16 = 2;

/// A parsed, width-independent markdown block. Wrapping and table layout happen
/// later, at [`flatten`] time, against a concrete width.
enum MdItem {
    /// Word-wrappable prose (a paragraph line, heading, list item, quote line).
    Prose {
        spans: Vec<Span<'static>>,
        indent: u16,
    },
    /// A verbatim line (code) drawn as-is at `indent`, never reflowed.
    Verbatim { line: Line<'static>, indent: u16 },
    /// A GFM table, laid out to the available width when flattened.
    Table { table: TableData, indent: u16 },
    /// A blank spacer row separating blocks.
    Blank,
}

/// Plain-text table contents captured during parsing; styled and boxed at
/// render time by [`render_table`].
struct TableData {
    aligns: Vec<Alignment>,
    header: Vec<String>,
    rows: Vec<Vec<String>>,
}

/// Parse `source` into width-independent [`MdItem`]s. Fenced code blocks are
/// highlighted here (once), via `highlighter`, using `theme`'s code palette.
fn parse(source: &str, theme: &Theme, highlighter: CodeHighlighter) -> Vec<MdItem> {
    let mut b = Builder::new(theme, highlighter);
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    for event in Parser::new_ext(source, opts) {
        b.event(event);
    }
    b.finish();
    b.items
}

/// Walks the pulldown-cmark event stream, accumulating [`MdItem`]s.
struct Builder<'a> {
    theme: &'a Theme,
    highlighter: CodeHighlighter<'a>,
    items: Vec<MdItem>,

    // Inline accumulation for the current prose block.
    inline: Vec<Span<'static>>,
    style_stack: Vec<Style>,

    // Block nesting.
    lists: Vec<Option<u64>>, // ordered start counter per open list, `None` = bullet
    quote_depth: u16,
    pending_marker: Option<Vec<Span<'static>>>,

    // Fenced/indented code block being collected.
    code: Option<(String, String)>, // (language tag, body)

    // Table being collected. Header vs body rows are routed by which End event
    // (`TableHead` / `TableRow`) closes the accumulated cells.
    table: Option<TableData>,
    cur_row: Vec<String>,
    cur_cell: String,
}

impl<'a> Builder<'a> {
    fn new(theme: &'a Theme, highlighter: CodeHighlighter<'a>) -> Self {
        Self {
            theme,
            highlighter,
            items: Vec::new(),
            inline: Vec::new(),
            style_stack: vec![Style::default().fg(theme.text)],
            lists: Vec::new(),
            quote_depth: 0,
            pending_marker: None,
            code: None,
            table: None,
            cur_row: Vec::new(),
            cur_cell: String::new(),
        }
    }

    fn cur_style(&self) -> Style {
        *self.style_stack.last().unwrap()
    }

    /// Indentation for the current block, from list + quote nesting.
    fn indent(&self) -> u16 {
        (self.lists.len().saturating_sub(1) as u16 + self.quote_depth) * INDENT
    }

    /// True when we are inside a list or block quote (nested, not top level).
    fn nested(&self) -> bool {
        !self.lists.is_empty() || self.quote_depth > 0
    }

    /// Insert a blank spacer before a new top-level block (never inside a list
    /// or quote, and never doubling blanks).
    fn separate(&mut self) {
        if self.nested() {
            return;
        }
        if matches!(self.items.last(), None | Some(MdItem::Blank)) {
            return;
        }
        self.items.push(MdItem::Blank);
    }

    /// Flush the current inline run as one prose line, attaching a pending list
    /// marker to the item's first line.
    fn flush(&mut self) {
        if self.inline.is_empty() && self.pending_marker.is_none() {
            return;
        }
        let indent = self.indent();
        let mut spans = self.pending_marker.take().unwrap_or_default();
        spans.append(&mut self.inline);
        self.items.push(MdItem::Prose { spans, indent });
    }

    fn push_text(&mut self, text: &str) {
        if let Some((_, body)) = self.code.as_mut() {
            body.push_str(text);
            return;
        }
        if self.table.is_some() {
            self.cur_cell.push_str(text);
            return;
        }
        let style = self.cur_style();
        // Only linkify bare URLs in plain body text, not inside links/headings.
        if style.add_modifier.contains(Modifier::UNDERLINED) {
            self.inline.push(Span::styled(text.to_string(), style));
        } else {
            for span in linkify(text, style, self.theme.code.link) {
                self.inline.push(span);
            }
        }
    }

    fn event(&mut self, event: Event<'a>) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(t) => self.push_text(&t),
            Event::Code(t) => {
                if self.table.is_some() {
                    self.cur_cell.push_str(&t);
                } else {
                    self.inline.push(Span::styled(
                        t.to_string(),
                        Style::default()
                            .fg(self.theme.code.text)
                            .bg(self.theme.code.background),
                    ));
                }
            }
            Event::SoftBreak => {
                if self.table.is_none() && self.code.is_none() {
                    self.inline.push(Span::raw(" "));
                }
            }
            Event::HardBreak => self.flush(),
            Event::Rule => {
                self.separate();
                self.items.push(MdItem::Prose {
                    spans: vec![Span::styled(
                        "─".repeat(24),
                        Style::default().fg(self.theme.dim),
                    )],
                    indent: self.indent(),
                });
            }
            Event::TaskListMarker(done) => {
                let glyph = if done { "[x] " } else { "[ ] " };
                self.inline.push(Span::styled(
                    glyph.to_string(),
                    Style::default().fg(self.theme.accent),
                ));
            }
            _ => {}
        }
    }

    fn start(&mut self, tag: Tag<'a>) {
        match tag {
            Tag::Paragraph => self.separate(),
            Tag::Heading { level, .. } => {
                self.separate();
                let mut style = Style::default()
                    .fg(self.theme.code.heading)
                    .add_modifier(Modifier::BOLD);
                if level > HeadingLevel::H2 {
                    style = style.add_modifier(Modifier::ITALIC);
                }
                self.style_stack.push(style);
            }
            Tag::BlockQuote(_) => {
                self.separate();
                self.quote_depth += 1;
            }
            Tag::CodeBlock(kind) => {
                self.separate();
                let lang = match kind {
                    CodeBlockKind::Fenced(info) => {
                        info.split_whitespace().next().unwrap_or("").to_string()
                    }
                    CodeBlockKind::Indented => String::new(),
                };
                self.code = Some((lang, String::new()));
            }
            Tag::List(start) => {
                self.separate();
                self.lists.push(start);
            }
            Tag::Item => {
                let marker = match self.lists.last_mut() {
                    Some(Some(n)) => {
                        let m = format!("{n}. ");
                        *self.lists.last_mut().unwrap() = Some(n.saturating_add(1));
                        m
                    }
                    _ => "• ".to_string(),
                };
                self.pending_marker = Some(vec![Span::styled(
                    marker,
                    Style::default().fg(self.theme.accent_alt),
                )]);
            }
            Tag::Emphasis => self.push_style(Modifier::ITALIC),
            Tag::Strong => self.push_style(Modifier::BOLD),
            Tag::Strikethrough => self.push_style(Modifier::CROSSED_OUT),
            Tag::Link { .. } => {
                let s = Style::default()
                    .fg(self.theme.code.link)
                    .add_modifier(Modifier::UNDERLINED);
                self.style_stack.push(s);
            }
            Tag::Table(aligns) => {
                self.separate();
                self.table = Some(TableData {
                    aligns,
                    header: Vec::new(),
                    rows: Vec::new(),
                });
            }
            Tag::TableHead => self.cur_row.clear(),
            Tag::TableRow => self.cur_row.clear(),
            Tag::TableCell => self.cur_cell.clear(),
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => self.flush(),
            TagEnd::Heading(_) => {
                self.flush();
                self.style_stack.pop();
            }
            TagEnd::BlockQuote(_) => {
                self.flush();
                self.quote_depth = self.quote_depth.saturating_sub(1);
            }
            TagEnd::CodeBlock => {
                if let Some((lang, body)) = self.code.take() {
                    let body = body.strip_suffix('\n').unwrap_or(&body);
                    let lines: Vec<&str> = body.split('\n').collect();
                    let indent = self.indent();
                    for line in code_block_lines(&lang, &lines, self.theme, self.highlighter, true)
                    {
                        self.items.push(MdItem::Verbatim { line, indent });
                    }
                }
            }
            TagEnd::List(_) => {
                self.lists.pop();
            }
            TagEnd::Item => self.flush(),
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough | TagEnd::Link => {
                self.style_stack.pop();
            }
            TagEnd::TableCell => {
                let cell = std::mem::take(&mut self.cur_cell);
                self.cur_row.push(cell.trim().to_string());
            }
            TagEnd::TableHead => {
                if let Some(t) = self.table.as_mut() {
                    t.header = std::mem::take(&mut self.cur_row);
                }
            }
            TagEnd::TableRow => {
                if let Some(t) = self.table.as_mut() {
                    t.rows.push(std::mem::take(&mut self.cur_row));
                }
            }
            TagEnd::Table => {
                if let Some(table) = self.table.take() {
                    let indent = self.indent();
                    self.items.push(MdItem::Table { table, indent });
                }
            }
            _ => {}
        }
    }

    fn push_style(&mut self, m: Modifier) {
        let s = self.cur_style().add_modifier(m);
        self.style_stack.push(s);
    }

    fn finish(&mut self) {
        // Close any dangling paragraph in truncated (still-streaming) input.
        self.flush();
        if let Some((lang, body)) = self.code.take() {
            let body = body.strip_suffix('\n').unwrap_or(&body);
            let lines: Vec<&str> = body.split('\n').collect();
            for line in code_block_lines(&lang, &lines, self.theme, self.highlighter, true) {
                self.items.push(MdItem::Verbatim { line, indent: 0 });
            }
        }
    }
}

/// Style bare `http(s)://` URLs in `text` as links, leaving the rest at `base`.
fn linkify(text: &str, base: Style, link: ratatui::style::Color) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("http://").or_else(|| rest.find("https://")) {
        let (before, from) = rest.split_at(start);
        if !before.is_empty() {
            spans.push(Span::styled(before.to_string(), base));
        }
        let raw = from.find(char::is_whitespace).unwrap_or(from.len());
        let end = from[..raw]
            .trim_end_matches(['.', ',', ';', ':', '!', '?', ')', ']'])
            .len();
        let (url, after) = from.split_at(end.max(1));
        spans.push(Span::styled(
            url.to_string(),
            base.fg(link).add_modifier(Modifier::UNDERLINED),
        ));
        rest = after;
    }
    if !rest.is_empty() {
        spans.push(Span::styled(rest.to_string(), base));
    }
    if spans.is_empty() {
        spans.push(Span::styled(text.to_string(), base));
    }
    spans
}

/// Flatten parsed items into final, width-fitted lines: prose is word-wrapped,
/// code and tables are emitted verbatim, and each is offset by its indent.
fn flatten(items: &[MdItem], width: u16, theme: &Theme) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    for item in items {
        match item {
            MdItem::Blank => out.push(Line::default()),
            MdItem::Prose { spans, indent } => {
                let avail = width.saturating_sub(*indent).max(1);
                for row in wrap_lines(&[Line::from(spans.clone())], avail) {
                    out.push(prefix_line(*indent, row.spans));
                }
            }
            MdItem::Verbatim { line, indent } => {
                out.push(prefix_line(*indent, line.spans.clone()));
            }
            MdItem::Table { table, indent } => {
                let avail = width.saturating_sub(*indent).max(1);
                for row in render_table(table, avail, theme) {
                    out.push(prefix_line(*indent, row.spans));
                }
            }
        }
    }
    out
}

/// Prefix `spans` with `indent` blank columns.
fn prefix_line(indent: u16, mut spans: Vec<Span<'static>>) -> Line<'static> {
    if indent == 0 {
        return Line::from(spans);
    }
    let mut line = vec![Span::raw(" ".repeat(indent as usize))];
    line.append(&mut spans);
    Line::from(line)
}

/// Lay a table out to `width` columns with box-drawing borders. Column widths
/// fit the content, shrinking the widest columns (and wrapping their cells)
/// until the whole table fits.
fn render_table(table: &TableData, width: u16, theme: &Theme) -> Vec<Line<'static>> {
    let cols = table
        .header
        .len()
        .max(table.rows.iter().map(Vec::len).max().unwrap_or(0))
        .max(1);

    // A boxed table needs `3*cols + 1` for borders plus ≥1 column per column.
    // Below that, drop the box and render pipe-joined rows that word-wrap to fit.
    if (width as usize) < 4 * cols + 1 {
        return render_table_plain(table, width, cols, theme);
    }

    // Natural width per column = widest cell (header + body).
    let mut widths = vec![0usize; cols];
    for (c, w) in widths.iter_mut().enumerate() {
        *w = crate::width::str_cols(cell_at(&table.header, c)) as usize;
        for row in &table.rows {
            *w = (*w).max(crate::width::str_cols(cell_at(row, c)) as usize);
        }
        *w = (*w).max(1);
    }

    // Shrink to fit: borders cost `3*cols + 1` (│ + " cell " per column). The
    // guard above guarantees `budget >= cols`, so shrinking each column toward 1
    // always reaches the budget before bottoming out.
    let budget = (width as usize).saturating_sub(3 * cols + 1).max(cols);
    while widths.iter().sum::<usize>() > budget {
        let (idx, _) = widths.iter().enumerate().max_by_key(|(_, w)| **w).unwrap();
        if widths[idx] <= 1 {
            break;
        }
        widths[idx] -= 1;
    }

    let border = Style::default().fg(theme.dim);
    let head_style = Style::default()
        .fg(theme.code.heading)
        .add_modifier(Modifier::BOLD);
    let body_style = Style::default().fg(theme.text);

    let mut out = Vec::new();
    out.push(rule_row('╭', '┬', '╮', &widths, border));
    out.extend(cell_rows(
        &table.header,
        &widths,
        &table.aligns,
        head_style,
        border,
        cols,
    ));
    out.push(rule_row('├', '┼', '┤', &widths, border));
    for row in &table.rows {
        out.extend(cell_rows(
            row,
            &widths,
            &table.aligns,
            body_style,
            border,
            cols,
        ));
    }
    out.push(rule_row('╰', '┴', '╯', &widths, border));
    out
}

/// Boxless table fallback for widths too narrow to draw borders: each row's
/// cells joined by ` | ` and word-wrapped to `width` (header bold). Guarantees
/// every returned line fits `width`.
fn render_table_plain(
    table: &TableData,
    width: u16,
    cols: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let join = |row: &[String]| -> String {
        (0..cols)
            .map(|c| cell_at(row, c))
            .collect::<Vec<_>>()
            .join(" | ")
    };
    let head_style = Style::default()
        .fg(theme.code.heading)
        .add_modifier(Modifier::BOLD);
    let body_style = Style::default().fg(theme.text);

    let mut out = Vec::new();
    out.extend(wrap_lines(
        &[Line::from(Span::styled(join(&table.header), head_style))],
        width,
    ));
    for row in &table.rows {
        out.extend(wrap_lines(
            &[Line::from(Span::styled(join(row), body_style))],
            width,
        ));
    }
    out
}

/// The `c`th cell of `row`, or `""` when the row is short.
fn cell_at(row: &[String], c: usize) -> &str {
    row.get(c).map(String::as_str).unwrap_or("")
}

fn rule_row(left: char, mid: char, right: char, widths: &[usize], style: Style) -> Line<'static> {
    let mut s = String::new();
    s.push(left);
    for (i, w) in widths.iter().enumerate() {
        s.push_str(&"─".repeat(w + 2));
        s.push(if i + 1 == widths.len() { right } else { mid });
    }
    Line::from(Span::styled(s, style))
}

#[allow(clippy::too_many_arguments)]
fn cell_rows(
    row: &[String],
    widths: &[usize],
    aligns: &[Alignment],
    text_style: Style,
    border: Style,
    cols: usize,
) -> Vec<Line<'static>> {
    // Wrap each cell to its column width; a table row is as tall as its tallest
    // cell.
    let wrapped: Vec<Vec<String>> = (0..cols)
        .map(|c| {
            let text = cell_at(row, c);
            if text.is_empty() {
                vec![String::new()]
            } else {
                textwrap::wrap(text, widths[c].max(1))
                    .into_iter()
                    .map(|s| s.into_owned())
                    .collect()
            }
        })
        .collect();
    let height = wrapped.iter().map(Vec::len).max().unwrap_or(1);

    let mut lines = Vec::new();
    for r in 0..height {
        let mut spans = vec![Span::styled("│".to_string(), border)];
        for c in 0..cols {
            let content = wrapped[c].get(r).map(String::as_str).unwrap_or("");
            let align = aligns.get(c).copied().unwrap_or(Alignment::None);
            let padded = pad_cell(content, widths[c], align);
            spans.push(Span::styled(format!(" {padded} "), text_style));
            spans.push(Span::styled("│".to_string(), border));
        }
        lines.push(Line::from(spans));
    }
    lines
}

fn pad_cell(text: &str, width: usize, align: Alignment) -> String {
    let w = crate::width::str_cols(text) as usize;
    let pad = width.saturating_sub(w);
    match align {
        Alignment::Right => format!("{}{}", " ".repeat(pad), text),
        Alignment::Center => {
            let l = pad / 2;
            format!("{}{}{}", " ".repeat(l), text, " ".repeat(pad - l))
        }
        _ => format!("{}{}", text, " ".repeat(pad)),
    }
}

/// Render a whole markdown string to width-fitted styled lines in one call.
///
/// For streaming input, prefer [`MarkdownState`], which caches the settled
/// prefix instead of re-parsing the whole buffer each frame.
pub fn markdown_to_lines(
    source: &str,
    width: u16,
    theme: &Theme,
    highlighter: CodeHighlighter,
) -> Vec<Line<'static>> {
    let items = parse(source, theme, highlighter);
    flatten(&items, width, theme)
}

/// Byte offset of the last *stable block boundary* in `source[from..]`, in
/// absolute bytes: the position just past the last blank line that sits outside
/// an open code fence. Blocks before it are complete and safe to cache; the tail
/// after it is still in flight.
fn stable_boundary(source: &str, from: usize) -> usize {
    let mut fence_open = false;
    let mut boundary = from;
    let mut pos = from;
    for line in source[from..].split_inclusive('\n') {
        let trimmed = line.trim();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            fence_open = !fence_open;
        } else if trimmed.is_empty() && !fence_open {
            boundary = pos + line.len();
        }
        pos += line.len();
    }
    boundary
}

/// Incremental markdown renderer for streamed text — the state to hold across
/// frames for a live transcript.
///
/// Feed it deltas with [`push_str`](Self::push_str) (or replace the whole buffer
/// with [`set`](Self::set)); call [`lines`](Self::lines) each frame for the
/// current width-fitted rendering. Settled blocks — everything before the last
/// blank line outside an open code fence — are parsed, highlighted, **and
/// flattened once** and cached; each delta re-does only the in-flight tail. That
/// keeps a streamed render linear in the transcript length, instead of
/// re-tokenizing and re-laying-out the whole settled prefix on every delta.
///
/// The parse/highlight cache is width-independent. The flattened-line cache is
/// per-width: a resize re-wraps the settled prefix once (then reuses it), and a
/// [`set`](Self::set) or [`Theme`] change discards everything. [`lines`](Self::lines)
/// returns a borrow of the cached line buffer — clone it with `.to_vec()` to own it.
///
/// ```
/// use tuika::{MarkdownState, CodeHighlighter, Theme};
/// let theme = Theme::default();
/// let mut md = MarkdownState::new();
/// for delta in ["# Title\n\n", "Some **bo", "ld** text.\n"] {
///     md.push_str(delta);                                  // forward each stream delta
///     let _lines = md.lines(80, &theme, CodeHighlighter::Plain); // render this frame
/// }
/// ```
#[derive(Default)]
pub struct MarkdownState {
    source: String,
    stable_len: usize,
    stable: Vec<MdItem>,
    cached_theme: Option<Theme>,
    // Settled lines are flattened *once*, as blocks settle, and kept here across
    // frames — never re-flattened while streaming. Without this, `lines` would
    // re-flatten (re-wrap, re-lay-out, re-clone) the whole settled prefix every
    // delta, making a streamed render O(n²) in the transcript length. Returning a
    // borrow of this buffer also avoids re-materializing the prefix per frame.
    /// Flattened settled lines followed by the current in-flight tail.
    rendered: Vec<Line<'static>>,
    /// Count of leading `rendered` entries that are settled (cached) lines; the
    /// rest is the per-frame tail, dropped and rebuilt on the next call.
    settled_lines: usize,
    /// Count of `stable` items already flattened into the settled prefix.
    flattened_items: usize,
    /// Width `rendered` was flattened at; a change re-wraps the whole prefix.
    rendered_width: Option<u16>,
}

impl MarkdownState {
    /// An empty renderer.
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a streamed delta to the buffer (the settled-prefix cache is kept).
    pub fn push_str(&mut self, delta: &str) {
        self.source.push_str(delta);
    }

    /// Replace the whole buffer, discarding the cache. Use for a non-streaming
    /// re-render, or to reset between messages.
    pub fn set(&mut self, source: impl Into<String>) {
        self.source = source.into();
        self.reset_cache();
    }

    /// The accumulated source so far.
    pub fn source(&self) -> &str {
        &self.source
    }

    fn reset_cache(&mut self) {
        self.stable_len = 0;
        self.stable.clear();
        self.rendered.clear();
        self.settled_lines = 0;
        self.flattened_items = 0;
        self.rendered_width = None;
    }

    /// Render the current buffer to final, width-fitted styled lines, advancing
    /// the settled-prefix cache.
    ///
    /// `width` word-wraps prose (code and tables stay verbatim); `theme` supplies
    /// every color via [`Theme::code`](crate::CodeTheme); `highlighter` colors
    /// fenced code ([`CodeHighlighter::Plain`] for none). Draw the result
    /// **without** further wrapping (e.g. ratatui `Paragraph` with no `.wrap`).
    ///
    /// Returns a borrow of an internally-cached line buffer: settled blocks are
    /// flattened once and only the in-flight tail is recomputed per call, so a
    /// streamed render stays linear in the transcript length. The borrow is valid
    /// until the next mutation of `self`; clone with `.to_vec()` if you need to
    /// own it (e.g. to move into a ratatui `Text`).
    pub fn lines(
        &mut self,
        width: u16,
        theme: &Theme,
        highlighter: CodeHighlighter,
    ) -> &[Line<'static>] {
        // A theme change restyles everything, so every cache is invalid.
        if self.cached_theme != Some(*theme) {
            self.cached_theme = Some(*theme);
            self.reset_cache();
        }
        // A width change re-wraps every settled line, but the width-independent
        // parse cache survives; drop only the flattened lines.
        if self.rendered_width != Some(width) {
            self.rendered_width = Some(width);
            self.rendered.clear();
            self.settled_lines = 0;
            self.flattened_items = 0;
        }

        let boundary = stable_boundary(&self.source, self.stable_len);
        if boundary > self.stable_len {
            let segment = &self.source[self.stable_len..boundary];
            let mut items = parse(segment, theme, highlighter);
            // Each segment parses in isolation, so the blank-line separation the
            // boundary sits on is lost — restore it between committed segments.
            if !items.is_empty()
                && !self.stable.is_empty()
                && !matches!(self.stable.last(), Some(MdItem::Blank))
            {
                self.stable.push(MdItem::Blank);
            }
            self.stable.append(&mut items);
            self.stable_len = boundary;
        }

        // Drop the previous frame's tail (and settled/tail gap), then extend the
        // settled prefix with any blocks that settled since — flattened once.
        // `flatten` maps each item independently, so appending the new items'
        // lines equals re-flattening the whole prefix.
        self.rendered.truncate(self.settled_lines);
        if self.flattened_items < self.stable.len() {
            let settled = flatten(&self.stable[self.flattened_items..], width, theme);
            self.rendered.extend(settled);
            self.flattened_items = self.stable.len();
            self.settled_lines = self.rendered.len();
        }

        let tail = parse(&self.source[self.stable_len..], theme, highlighter);
        let tail_lines = flatten(&tail, width, theme);
        // The tail begins just past the boundary's blank line; keep that gap.
        if !self.rendered.is_empty()
            && !tail_lines.is_empty()
            && !is_blank_line(self.rendered.last().unwrap())
            && !is_blank_line(&tail_lines[0])
        {
            self.rendered.push(Line::default());
        }
        self.rendered.extend(tail_lines);
        &self.rendered
    }
}

/// Whether a rendered line is visually blank (no spans, or only whitespace).
fn is_blank_line(line: &Line) -> bool {
    line.spans.iter().all(|s| s.content.trim().is_empty())
}

/// A view that renders a static markdown string to its area — word-wrapping
/// prose to the width and drawing code and tables verbatim.
///
/// ![markdown demo](https://raw.githubusercontent.com/everruns/yolop/main/crates/tuika/docs/demos/markdown.gif)
///
/// For a *streaming* transcript, hold a [`MarkdownState`] and draw its
/// [`lines`](MarkdownState::lines) directly (that is what the demo above does);
/// this view is the one-shot convenience for static markdown placed in a layout.
///
/// # Options
///
/// | Builder | Default | Effect |
/// | --- | --- | --- |
/// | [`new(source)`](Self::new) | — | the markdown source to render |
/// | [`highlighter(&h)`](Self::highlighter) | plain | syntax-highlight fenced code |
///
/// ```no_run
/// use tuika::Markdown;
/// let doc = Markdown::new("# Title\n\nSome **bold** prose.");
/// // `doc` is a `View`: render it via `tuika::paint` or embed it in a `Flex`.
/// # let _ = doc;
/// ```
pub struct Markdown<'a> {
    source: String,
    highlighter: CodeHighlighter<'a>,
}

impl<'a> Markdown<'a> {
    /// A markdown view over `source`, rendering fenced code as plain text until
    /// a highlighter is attached with [`highlighter`](Self::highlighter).
    pub fn new(source: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            highlighter: CodeHighlighter::Plain,
        }
    }

    /// Use `highlighter` to syntax-highlight fenced code blocks.
    pub fn highlighter(mut self, highlighter: &'a dyn Highlighter) -> Self {
        self.highlighter = CodeHighlighter::With(highlighter);
        self
    }
}

impl View for Markdown<'_> {
    fn measure(&self, available: Size) -> Size {
        let lines = markdown_to_lines(
            &self.source,
            available.width,
            &Theme::default(),
            self.highlighter,
        );
        let width = lines.iter().map(line_width).max().unwrap_or(0);
        Size::new(width.min(available.width), lines.len() as u16)
    }

    fn render(&self, area: Rect, surface: &mut Surface, ctx: &RenderCtx) {
        let lines = markdown_to_lines(&self.source, area.width, ctx.theme, self.highlighter);
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

    /// Plain text of a rendered line (spans concatenated).
    fn text(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    /// The whole render as plain lines, for content assertions.
    fn plain(source: &str, width: u16) -> Vec<String> {
        markdown_to_lines(source, width, &Theme::default(), CodeHighlighter::Plain)
            .iter()
            .map(text)
            .collect()
    }

    #[test]
    fn heading_is_bold_and_themed() {
        let theme = Theme::default();
        let lines = markdown_to_lines("# Title", 40, &theme, CodeHighlighter::Plain);
        let span = &lines[0].spans[0];
        assert_eq!(span.content.as_ref(), "Title");
        assert!(span.style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(span.style.fg, Some(theme.code.heading));
    }

    #[test]
    fn emphasis_and_strong_carry_modifiers() {
        let theme = Theme::default();
        let lines = markdown_to_lines(
            "plain *em* and **bold**",
            60,
            &theme,
            CodeHighlighter::Plain,
        );
        let em = lines[0]
            .spans
            .iter()
            .find(|s| s.content.contains("em"))
            .expect("emphasis span");
        assert!(em.style.add_modifier.contains(Modifier::ITALIC));
        let bold = lines[0]
            .spans
            .iter()
            .find(|s| s.content.contains("bold"))
            .expect("strong span");
        assert!(bold.style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn inline_code_gets_code_background() {
        let theme = Theme::default();
        let lines = markdown_to_lines("use `cargo test` now", 60, &theme, CodeHighlighter::Plain);
        let code = lines[0]
            .spans
            .iter()
            .find(|s| s.content.contains("cargo test"))
            .expect("inline code span");
        assert_eq!(code.style.bg, Some(theme.code.background));
    }

    #[test]
    fn bullet_list_renders_markers() {
        let out = plain("- one\n- two", 40);
        assert!(out.iter().any(|l| l.contains("• one")), "{out:?}");
        assert!(out.iter().any(|l| l.contains("• two")), "{out:?}");
    }

    #[test]
    fn ordered_list_numbers_increment() {
        let out = plain("1. first\n2. second", 40);
        assert!(out.iter().any(|l| l.contains("1. first")), "{out:?}");
        assert!(out.iter().any(|l| l.contains("2. second")), "{out:?}");
    }

    #[test]
    fn nested_list_is_indented() {
        let out = plain("- outer\n  - inner", 40);
        let inner = out.iter().find(|l| l.contains("inner")).unwrap();
        assert!(
            inner.starts_with("  "),
            "nested item should be indented: {inner:?}"
        );
    }

    #[test]
    fn fenced_code_preserves_indentation_verbatim() {
        // A line-oriented wrapper would eat the leading spaces; code must not.
        let src = "```\n    indented\n```";
        let out = plain(src, 40);
        assert!(
            out.iter().any(|l| l.contains("    indented")),
            "code indentation must survive: {out:?}"
        );
    }

    #[test]
    fn fenced_code_shows_language_label() {
        let out = plain("```rust\nfn main() {}\n```", 40);
        assert!(out.iter().any(|l| l.contains("rust")), "{out:?}");
    }

    #[test]
    fn code_fence_is_not_word_wrapped() {
        // A long code line exceeds width but is emitted as a single (clipped)
        // row, never reflowed into multiple lines.
        let long = "x".repeat(60);
        let src = format!("```\n{long}\n```");
        let lines = markdown_to_lines(&src, 20, &Theme::default(), CodeHighlighter::Plain);
        let code_rows = lines.iter().filter(|l| text(l).contains("xxxx")).count();
        assert_eq!(code_rows, 1, "code line must not wrap");
    }

    #[test]
    fn prose_word_wraps_to_width() {
        let out = plain("one two three four five six seven eight", 12);
        assert!(out.len() > 1, "long prose should wrap: {out:?}");
        for line in &out {
            assert!(line.chars().count() <= 12, "line over width: {line:?}");
        }
    }

    #[test]
    fn table_renders_boxed_with_headers() {
        let src = "| A | B |\n| - | - |\n| 1 | 2 |";
        let out = plain(src, 40);
        assert!(out.iter().any(|l| l.contains('╭')), "top border: {out:?}");
        assert!(
            out.iter().any(|l| l.contains('A') && l.contains('B')),
            "header: {out:?}"
        );
        assert!(
            out.iter().any(|l| l.contains('1') && l.contains('2')),
            "row: {out:?}"
        );
    }

    #[test]
    fn table_falls_back_to_plain_when_too_narrow_and_always_fits() {
        let src = "| Col A | Col B |\n| --- | --- |\n| alpha | beta |";
        for width in [4u16, 8, 12, 20, 48] {
            let lines = markdown_to_lines(src, width, &Theme::default(), CodeHighlighter::Plain);
            for line in &lines {
                assert!(
                    line_width(line) <= width,
                    "table line exceeded width {width}: {:?}",
                    text(line)
                );
            }
            let body: String = lines.iter().map(text).collect::<Vec<_>>().join("\n");
            // "Col" (3 cols) survives intact even at the narrowest width; wider
            // cells may wrap across rows below the boxing threshold.
            assert!(body.contains("Col"), "content survives at width {width}");
        }
    }

    #[test]
    fn bare_url_is_linkified() {
        let theme = Theme::default();
        let lines = markdown_to_lines(
            "see https://example.com now",
            60,
            &theme,
            CodeHighlighter::Plain,
        );
        let url = lines[0]
            .spans
            .iter()
            .find(|s| s.content.as_ref().contains("example.com"))
            .expect("url span");
        assert_eq!(url.style.fg, Some(theme.code.link));
        assert!(url.style.add_modifier.contains(Modifier::UNDERLINED));
    }

    #[test]
    fn streaming_matches_one_shot_render() {
        let full = "# Heading\n\nA paragraph of text.\n\n```rust\nfn main() {}\n```\n\nDone.";
        let theme = Theme::default();
        let one_shot: Vec<String> = markdown_to_lines(full, 40, &theme, CodeHighlighter::Plain)
            .iter()
            .map(text)
            .collect();

        // Feed the same content in awkward chunks.
        let mut state = MarkdownState::new();
        let mut streamed = Vec::new();
        for chunk in [
            "# Head",
            "ing\n\nA para",
            "graph of text.\n\n```rus",
            "t\nfn main() {}\n```\n\nDone.",
        ] {
            state.push_str(chunk);
            streamed = state
                .lines(40, &theme, CodeHighlighter::Plain)
                .iter()
                .map(text)
                .collect();
        }
        assert_eq!(streamed, one_shot);
    }

    #[test]
    fn streaming_then_resize_matches_one_shot_at_new_width() {
        // The settled-prefix line cache is width-specific: a width change must
        // re-wrap the whole prefix, not serve stale lines flattened at the old
        // width. Settle several blocks at a wide width, then render narrower.
        let theme = Theme::default();
        let chunks = [
            "# A wide head",
            "ing that wraps when narrow\n\nA para",
            "graph long enough to wrap differently at 24 columns than at 60.\n\n",
            "- a bullet that also wraps\n\nDone.",
        ];
        let full: String = chunks.concat();

        let mut state = MarkdownState::new();
        for chunk in chunks {
            state.push_str(chunk);
            let _ = state.lines(60, &theme, CodeHighlighter::Plain);
        }
        let resized: Vec<String> = state
            .lines(24, &theme, CodeHighlighter::Plain)
            .iter()
            .map(text)
            .collect();
        let one_shot: Vec<String> = markdown_to_lines(&full, 24, &theme, CodeHighlighter::Plain)
            .iter()
            .map(text)
            .collect();
        assert_eq!(
            resized, one_shot,
            "resized stream must equal a one-shot render at the new width"
        );
    }

    #[test]
    fn streaming_commits_a_stable_prefix() {
        let theme = Theme::default();
        let mut state = MarkdownState::new();
        state.push_str("First paragraph.\n\nSecond a");
        let _ = state.lines(40, &theme, CodeHighlighter::Plain);
        // The blank line after the first paragraph is a stable boundary, so its
        // bytes are committed to the cache and won't be re-parsed.
        assert!(state.stable_len > 0, "expected a committed prefix");
        assert!(!state.stable.is_empty());
    }

    #[test]
    fn stable_boundary_never_splits_open_code_fence() {
        // A blank line *inside* an unterminated fence is not a boundary.
        let src = "```\ncode\n\nmore code";
        assert_eq!(stable_boundary(src, 0), 0);
        // Once the fence closes, the trailing blank becomes a boundary.
        let closed = "```\ncode\n```\n\nafter";
        assert!(stable_boundary(closed, 0) > 0);
    }

    #[test]
    fn partial_emphasis_degrades_gracefully() {
        // An unterminated `**` should not panic and should still render text.
        let out = plain("this is **unfinished", 40);
        assert!(out.iter().any(|l| l.contains("unfinished")), "{out:?}");
    }

    #[test]
    fn theme_change_invalidates_cache() {
        let mut state = MarkdownState::new();
        state.push_str("Para one.\n\nPara two.\n\ntail");
        let a = Theme::default();
        let _ = state.lines(40, &a, CodeHighlighter::Plain);
        assert!(state.stable_len > 0);

        let mut b = Theme::default();
        b.code.heading = ratatui::style::Color::Indexed(200);
        let _ = state.lines(40, &b, CodeHighlighter::Plain);
        // Cache was rebuilt under the new theme; still consistent, no stale panic.
        assert_eq!(state.cached_theme, Some(b));
    }
}

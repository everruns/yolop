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
//! Output is width-aware and correct for prose, code, and tables: prose is
//! word-wrapped (via [`wrap_lines`]); code is emitted verbatim, because its
//! indentation is meaningful; and GFM tables are re-laid-out to the width each
//! frame, with per-column fitting and styled cells (bold headers, links, inline
//! code, emoji). Callers draw the returned lines **without** further wrapping
//! (e.g. ratatui's `Paragraph` with no `.wrap`, or tuika's
//! [`Text`](crate::components::Text)).
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
use crate::image::{Image, ImageData, ImageLayer, ImageSupport};
use crate::style::{StyleBundle, StyleSheet, Theme};
use crate::surface::Surface;
use crate::view::{RenderCtx, View};

/// Columns of indentation added per level of list / block-quote nesting.
const INDENT: u16 = 2;

/// Widest a block image is drawn, in cells, regardless of the available width —
/// so a resolved `![alt](url)` stays a reasonable inline size rather than filling
/// the whole transcript.
const MAX_IMAGE_COLS: u16 = 60;

/// Resolves a markdown image URL to decoded pixels.
///
/// Markdown carries only the URL, never pixels, so — exactly like
/// [`Highlighter`] does for fenced code — the host supplies the decode. A
/// resolved `![alt](url)` is rendered as a block image (real pixels via the
/// terminal graphics protocol, or the alt fallback); returning `None` leaves it
/// as the inline text placeholder. Wire one up with [`Markdown::images`].
pub trait ImageResolver {
    /// Resolve `url` to image data, or `None` to keep the text placeholder.
    fn resolve(&self, url: &str) -> Option<ImageData>;
}

/// A block image in a rendered markdown document: the row it reserved (0-based,
/// within the returned lines), its cell footprint, the alt text, and the pixels.
///
/// The [`Markdown`] view overlays these itself. A host that draws
/// [`MarkdownState::lines`] manually reads [`MarkdownState::images`] and paints
/// each one — build an [`Image`] from `data`/`cols`/`rows`/`alt` and render it at
/// [`rect`](Self::rect) against the area the lines were drawn into.
#[derive(Clone, Debug)]
pub struct MarkdownImage {
    /// Row the image reserved, 0-based within the rendered lines.
    pub row: u16,
    /// Left indent in cells (list / block-quote nesting).
    pub indent: u16,
    /// Width in cells.
    pub cols: u16,
    /// Height in cells.
    pub rows: u16,
    /// Decoded pixels to paint.
    pub data: ImageData,
    /// Alt text, shown as the fallback where graphics aren't supported.
    pub alt: String,
}

impl MarkdownImage {
    /// The absolute screen rect for this image, given the `area` the markdown
    /// lines were drawn into — clamped so it never exceeds `area`.
    pub fn rect(&self, area: Rect) -> Rect {
        let x = area.x.saturating_add(self.indent).min(area.right());
        let y = area.y.saturating_add(self.row).min(area.bottom());
        Rect {
            x,
            y,
            width: self.cols.min(area.right().saturating_sub(x)),
            height: self.rows.min(area.bottom().saturating_sub(y)),
        }
    }
}

/// Cell footprint for a block image at `avail` columns: capped at
/// [`MAX_IMAGE_COLS`], with the row count derived from the pixel aspect ratio
/// (terminal cells are about twice as tall as wide, so the height is halved).
fn image_cell_size(data: &ImageData, avail: u16) -> (u16, u16) {
    let cols = avail.clamp(1, MAX_IMAGE_COLS);
    let rows = (cols as u32 * data.pixel_height() / (data.pixel_width().max(1) * 2)).clamp(1, 30);
    (cols, rows as u16)
}

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
    /// A resolved block image: reserves rows at flatten time and is painted by an
    /// [`Image`] overlay in the view (see [`ImageResolver`]).
    Image {
        data: ImageData,
        alt: String,
        indent: u16,
    },
    /// A blank spacer row separating blocks.
    Blank,
}

/// Table contents captured during parsing: each cell is a run of pre-styled
/// inline spans (bold, links, inline code, emoji), boxed and width-fitted at
/// render time by [`render_table`].
struct TableData {
    aligns: Vec<Alignment>,
    header: Vec<Cell>,
    rows: Vec<Vec<Cell>>,
}

/// One table cell's inline content, already styled by the shared inline
/// machinery so cells carry the same markup as prose.
type Cell = Vec<Span<'static>>;

/// Parse `source` into width-independent [`MdItem`]s. Fenced code blocks are
/// highlighted here (once), via `highlighter`, using `theme`'s code palette;
/// prose roles (headings, links, emphasis, …) are styled from `sheet`.
fn parse(
    source: &str,
    theme: &Theme,
    sheet: &StyleSheet,
    highlighter: CodeHighlighter,
) -> Vec<MdItem> {
    parse_with(source, theme, sheet, highlighter, None)
}

/// [`parse`] with an optional [`ImageResolver`]: a resolved `![alt](url)` becomes
/// a block [`MdItem::Image`] instead of the inline text placeholder.
fn parse_with(
    source: &str,
    theme: &Theme,
    sheet: &StyleSheet,
    highlighter: CodeHighlighter,
    resolver: Option<&dyn ImageResolver>,
) -> Vec<MdItem> {
    let mut b = Builder::new(theme, sheet, highlighter, resolver);
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
    sheet: &'a StyleSheet,
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

    // Table being collected. Cells reuse the shared `inline` accumulator, so
    // they pick up the same styling as prose; each cell drains `inline` on its
    // `TableCell` end. Header vs body rows are routed by which End event
    // (`TableHead` / `TableRow`) closes the accumulated cells.
    table: Option<TableData>,
    cur_row: Vec<Cell>,

    // Inline image being collected: (dest URL, alt-text accumulator). Set on
    // `Tag::Image`, drained on `TagEnd::Image` into a placeholder or, when the
    // resolver yields pixels, a block `MdItem::Image`.
    image: Option<(String, String)>,

    // Host hook resolving image URLs to pixels; `None` keeps the text placeholder.
    resolver: Option<&'a dyn ImageResolver>,
}

impl<'a> Builder<'a> {
    fn new(
        theme: &'a Theme,
        sheet: &'a StyleSheet,
        highlighter: CodeHighlighter<'a>,
        resolver: Option<&'a dyn ImageResolver>,
    ) -> Self {
        Self {
            theme,
            sheet,
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
            image: None,
            resolver,
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
        // Inside an image, text is the alt description — collect it for the
        // placeholder rather than emitting it as prose.
        if let Some((_, alt)) = self.image.as_mut() {
            alt.push_str(text);
            return;
        }
        let style = self.cur_style();
        // Only linkify bare URLs in plain body text, not inside links/headings.
        if style.add_modifier.contains(Modifier::UNDERLINED) {
            self.inline.push(Span::styled(text.to_string(), style));
        } else {
            for span in linkify(text, style, self.sheet.link) {
                self.inline.push(span);
            }
        }
    }

    /// Render an inline image as a visible placeholder: a small marker glyph plus
    /// the alt text — or the URL when there is no alt — link-styled, so an image
    /// is never silently dropped. Actually painting the pixels in markdown
    /// (resolving the URL to [`ImageData`](crate::image::ImageData) and emitting
    /// through an [`ImageLayer`](crate::image::ImageLayer)) is a later phase; see
    /// `specs/tuika-images.md`.
    fn push_image_placeholder(&mut self, url: &str, alt: &str) {
        let label = if alt.trim().is_empty() { url } else { alt };
        self.inline
            .push(Span::styled("🖼 ", self.sheet.image_marker.to_style()));
        self.inline
            .push(Span::styled(label.to_string(), self.sheet.link.to_style()));
    }

    fn event(&mut self, event: Event<'a>) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(t) => self.push_text(&t),
            Event::Code(t) => {
                self.inline.push(Span::styled(
                    t.to_string(),
                    self.sheet.inline_code.to_style(),
                ));
            }
            Event::SoftBreak => {
                if self.code.is_none() {
                    self.inline.push(Span::raw(" "));
                }
            }
            // A hard break inside a cell can't split it into another block, so
            // it collapses to a space; elsewhere it flushes the prose line.
            Event::HardBreak if self.table.is_some() => self.inline.push(Span::raw(" ")),
            Event::HardBreak => self.flush(),
            Event::Rule => {
                self.separate();
                self.items.push(MdItem::Prose {
                    spans: vec![Span::styled("─".repeat(24), self.sheet.rule.to_style())],
                    indent: self.indent(),
                });
            }
            Event::TaskListMarker(done) => {
                let glyph = if done { "[x] " } else { "[ ] " };
                self.inline.push(Span::styled(
                    glyph.to_string(),
                    self.sheet.task_marker.to_style(),
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
                let mut style = self.sheet.heading.to_style();
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
                    self.sheet.list_marker.to_style(),
                )]);
            }
            Tag::Emphasis => self.push_style_bundle(self.sheet.emphasis),
            Tag::Strong => self.push_style_bundle(self.sheet.strong),
            Tag::Strikethrough => self.push_style_bundle(self.sheet.strikethrough),
            Tag::Link { .. } => {
                self.style_stack.push(self.sheet.link.to_style());
            }
            Tag::Image { dest_url, .. } => {
                // Capture the target; alt text accrues via `push_text` until the
                // matching `TagEnd::Image` renders the placeholder.
                self.image = Some((dest_url.to_string(), String::new()));
            }
            Tag::Table(aligns) => {
                self.separate();
                self.table = Some(TableData {
                    aligns,
                    header: Vec::new(),
                    rows: Vec::new(),
                });
            }
            Tag::TableHead => {
                self.cur_row.clear();
                // Header cells render in the heading style; push it as the cell
                // base so plain header text picks it up, while links and inline
                // code inside a header keep their own styling on top.
                self.style_stack.push(self.sheet.heading.to_style());
            }
            Tag::TableRow => self.cur_row.clear(),
            Tag::TableCell => self.inline.clear(),
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
            TagEnd::Image => {
                if let Some((url, alt)) = self.image.take() {
                    match self.resolver.and_then(|r| r.resolve(&url)) {
                        // Resolved: promote to a block image on its own rows. Any
                        // inline run so far is flushed first so the image stands
                        // apart from surrounding text.
                        Some(data) => {
                            self.flush();
                            let indent = self.indent();
                            self.items.push(MdItem::Image { data, alt, indent });
                        }
                        None => self.push_image_placeholder(&url, &alt),
                    }
                }
            }
            TagEnd::TableCell => {
                let cell = trim_spans(std::mem::take(&mut self.inline));
                self.cur_row.push(cell);
            }
            TagEnd::TableHead => {
                self.style_stack.pop();
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

    /// Push a style derived by overlaying `bundle` onto the current style — used
    /// for inline roles (emphasis, strong, strikethrough) that add a modifier
    /// (and possibly a color) on top of the surrounding text.
    fn push_style_bundle(&mut self, bundle: StyleBundle) {
        let s = bundle.apply(self.cur_style());
        self.style_stack.push(s);
    }

    fn finish(&mut self) {
        // A still-streaming table (open at end of input) never renders partially;
        // drop its in-progress cell rather than flushing it as stray prose.
        if self.table.is_some() {
            self.inline.clear();
        }
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

/// Trim surrounding whitespace from a cell's span run: the leading edge of the
/// first span and the trailing edge of the last, dropping any span left empty.
fn trim_spans(mut spans: Vec<Span<'static>>) -> Vec<Span<'static>> {
    if let Some(first) = spans.first_mut() {
        first.content = first.content.trim_start().to_string().into();
    }
    if let Some(last) = spans.last_mut() {
        last.content = last.content.trim_end().to_string().into();
    }
    spans.retain(|s| !s.content.is_empty());
    spans
}

/// Display columns of a cell's span run, grapheme-aware.
fn spans_cols(spans: &[Span]) -> usize {
    spans
        .iter()
        .map(|s| crate::width::str_cols(s.content.as_ref()) as usize)
        .sum()
}

/// Style bare `http(s)://` URLs in `text` with the `link` role, leaving the rest
/// at `base`. The link role is overlaid onto `base`, so a URL inside otherwise
/// plain prose keeps that prose's context and gains the link color + underline.
fn linkify(text: &str, base: Style, link: StyleBundle) -> Vec<Span<'static>> {
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
        spans.push(Span::styled(url.to_string(), link.apply(base)));
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
    flatten_into(items, width, theme, &mut Vec::new())
}

/// [`flatten`] that also collects the block images it reserved, with the row each
/// landed on, so the [`Markdown`] view can overlay an [`Image`] at the matching
/// screen rect. A block image reserves `rows` blank lines here.
fn flatten_into(
    items: &[MdItem],
    width: u16,
    theme: &Theme,
    images: &mut Vec<MarkdownImage>,
) -> Vec<Line<'static>> {
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
            MdItem::Image { data, alt, indent } => {
                let avail = width.saturating_sub(*indent).max(1);
                let (cols, rows) = image_cell_size(data, avail);
                images.push(MarkdownImage {
                    row: out.len().min(u16::MAX as usize) as u16,
                    indent: *indent,
                    cols,
                    rows,
                    data: data.clone(),
                    alt: alt.clone(),
                });
                // Reserve the image's rows; the view paints pixels over them.
                for _ in 0..rows {
                    out.push(Line::default());
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
        *w = spans_cols(cell_at(&table.header, c));
        for row in &table.rows {
            *w = (*w).max(spans_cols(cell_at(row, c)));
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

    // Cells carry their own inline styling (header bold, links, code); only the
    // borders need a style here.
    let border = Style::default().fg(theme.dim);

    let mut out = Vec::new();
    out.push(rule_row('╭', '┬', '╮', &widths, border));
    out.extend(cell_rows(
        &table.header,
        &widths,
        &table.aligns,
        border,
        cols,
    ));
    out.push(rule_row('├', '┼', '┤', &widths, border));
    for row in &table.rows {
        out.extend(cell_rows(row, &widths, &table.aligns, border, cols));
    }
    out.push(rule_row('╰', '┴', '╯', &widths, border));
    out
}

/// Boxless table fallback for widths too narrow to draw borders: each row's
/// styled cells joined by ` | ` and word-wrapped to `width` (cells keep their
/// inline styling). Guarantees every returned line fits `width`.
fn render_table_plain(
    table: &TableData,
    width: u16,
    cols: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let sep = Style::default().fg(theme.dim);
    let join = |row: &[Cell]| -> Line<'static> {
        let mut spans = Vec::new();
        for c in 0..cols {
            if c > 0 {
                spans.push(Span::styled(" | ".to_string(), sep));
            }
            spans.extend(cell_at(row, c).iter().cloned());
        }
        Line::from(spans)
    };

    let mut out = Vec::new();
    out.extend(wrap_lines(&[join(&table.header)], width));
    for row in &table.rows {
        out.extend(wrap_lines(&[join(row)], width));
    }
    out
}

/// The `c`th cell of `row`, or an empty span run when the row is short.
fn cell_at(row: &[Cell], c: usize) -> &[Span<'static>] {
    row.get(c).map(Vec::as_slice).unwrap_or(&[])
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

fn cell_rows(
    row: &[Cell],
    widths: &[usize],
    aligns: &[Alignment],
    border: Style,
    cols: usize,
) -> Vec<Line<'static>> {
    // Wrap each cell's styled spans to its column width (grapheme-aware, so wide
    // glyphs stay intact); a table row is as tall as its tallest wrapped cell.
    let wrapped: Vec<Vec<Line<'static>>> = (0..cols)
        .map(|c| {
            let cell = cell_at(row, c);
            if cell.is_empty() {
                vec![Line::default()]
            } else {
                let lines = wrap_lines(&[Line::from(cell.to_vec())], widths[c].max(1) as u16);
                if lines.is_empty() {
                    vec![Line::default()]
                } else {
                    lines
                }
            }
        })
        .collect();
    let height = wrapped.iter().map(Vec::len).max().unwrap_or(1);

    let empty = Line::default();
    let mut lines = Vec::new();
    for r in 0..height {
        let mut spans = vec![Span::styled("│".to_string(), border)];
        for (c, width) in widths.iter().enumerate() {
            let content = wrapped[c].get(r).unwrap_or(&empty);
            let pad = width.saturating_sub(line_width(content) as usize);
            let align = aligns.get(c).copied().unwrap_or(Alignment::None);
            let (left, right) = match align {
                Alignment::Right => (pad, 0),
                Alignment::Center => (pad / 2, pad - pad / 2),
                _ => (0, pad),
            };
            // A one-space gutter each side, alignment padding, then the cell's
            // own styled spans between.
            spans.push(Span::raw(format!(" {}", " ".repeat(left))));
            spans.extend(content.spans.iter().cloned());
            spans.push(Span::raw(format!("{} ", " ".repeat(right))));
            spans.push(Span::styled("│".to_string(), border));
        }
        lines.push(Line::from(spans));
    }
    lines
}

/// Render a whole markdown string to width-fitted styled lines in one call.
///
/// For streaming input, prefer [`MarkdownState`], which caches the settled
/// prefix instead of re-parsing the whole buffer each frame.
pub fn markdown_to_lines(
    source: &str,
    width: u16,
    theme: &Theme,
    sheet: &StyleSheet,
    highlighter: CodeHighlighter,
) -> Vec<Line<'static>> {
    let items = parse(source, theme, sheet, highlighter);
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
/// use tuika::{MarkdownState, CodeHighlighter, StyleSheet, Theme};
/// let theme = Theme::default();
/// let sheet = StyleSheet::from_theme(&theme);
/// let mut md = MarkdownState::new();
/// for delta in ["# Title\n\n", "Some **bo", "ld** text.\n"] {
///     md.push_str(delta);                                  // forward each stream delta
///     let _lines = md.lines(80, &theme, &sheet, CodeHighlighter::Plain); // render this frame
/// }
/// ```
#[derive(Default)]
pub struct MarkdownState {
    source: String,
    stable_len: usize,
    stable: Vec<MdItem>,
    cached_theme: Option<Theme>,
    cached_sheet: Option<StyleSheet>,
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
    /// Optional host hook turning image URLs into pixels; off ⇒ text placeholders.
    resolver: Option<Box<dyn ImageResolver>>,
    /// Block images in the settled prefix, with their absolute `rendered` rows —
    /// accumulated once as blocks settle, mirroring `settled_lines`.
    settled_images: Vec<MarkdownImage>,
    /// Settled + tail images with absolute rows, rebuilt each [`lines`](Self::lines)
    /// call; returned by [`images`](Self::images).
    frame_images: Vec<MarkdownImage>,
}

impl MarkdownState {
    /// An empty renderer.
    pub fn new() -> Self {
        Self::default()
    }

    /// Render `![alt](url)` images as real pixels, resolving each URL to
    /// [`ImageData`] via `resolver` (markdown carries only the URL — see
    /// [`ImageResolver`]). After each [`lines`](Self::lines) call, read the
    /// reserved placements from [`images`](Self::images) and paint them.
    ///
    /// The resolver may be called repeatedly for an image still in the in-flight
    /// tail (re-parsed each frame), so a host that decodes lazily should cache.
    pub fn with_image_resolver(mut self, resolver: Box<dyn ImageResolver>) -> Self {
        self.resolver = Some(resolver);
        self.reset_cache();
        self
    }

    /// The block images reserved by the last [`lines`](Self::lines) call, with
    /// rows relative to those lines. Empty unless a resolver is attached (see
    /// [`with_image_resolver`](Self::with_image_resolver)). Paint each by
    /// rendering an [`Image`] at [`MarkdownImage::rect`] against the same area.
    pub fn images(&self) -> &[MarkdownImage] {
        &self.frame_images
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
        self.settled_images.clear();
        self.frame_images.clear();
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
        sheet: &StyleSheet,
        highlighter: CodeHighlighter,
    ) -> &[Line<'static>] {
        // A theme or stylesheet change restyles everything, so every cache is
        // invalid (both feed the styles baked into the cached, parsed spans).
        if self.cached_theme != Some(*theme) || self.cached_sheet != Some(*sheet) {
            self.cached_theme = Some(*theme);
            self.cached_sheet = Some(*sheet);
            self.reset_cache();
        }
        // A width change re-wraps every settled line, but the width-independent
        // parse cache survives; drop only the flattened lines (and their images,
        // whose row offsets and sizes are width-dependent).
        if self.rendered_width != Some(width) {
            self.rendered_width = Some(width);
            self.rendered.clear();
            self.settled_lines = 0;
            self.flattened_items = 0;
            self.settled_images.clear();
        }

        let boundary = stable_boundary(&self.source, self.stable_len);
        if boundary > self.stable_len {
            let segment = &self.source[self.stable_len..boundary];
            let mut items =
                parse_with(segment, theme, sheet, highlighter, self.resolver.as_deref());
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
            let base = self.rendered.len() as u16;
            let mut settled_imgs = Vec::new();
            let settled = flatten_into(
                &self.stable[self.flattened_items..],
                width,
                theme,
                &mut settled_imgs,
            );
            for mut img in settled_imgs {
                img.row = img.row.saturating_add(base);
                self.settled_images.push(img);
            }
            self.rendered.extend(settled);
            self.flattened_items = self.stable.len();
            self.settled_lines = self.rendered.len();
        }

        let tail = parse_with(
            &self.source[self.stable_len..],
            theme,
            sheet,
            highlighter,
            self.resolver.as_deref(),
        );
        let mut tail_imgs = Vec::new();
        let tail_lines = flatten_into(&tail, width, theme, &mut tail_imgs);
        // The tail begins just past the boundary's blank line; keep that gap.
        if !self.rendered.is_empty()
            && !tail_lines.is_empty()
            && !is_blank_line(self.rendered.last().unwrap())
            && !is_blank_line(&tail_lines[0])
        {
            self.rendered.push(Line::default());
        }
        let tail_base = self.rendered.len() as u16;
        self.rendered.extend(tail_lines);

        // Republish this frame's placements: the settled prefix (fixed) plus the
        // in-flight tail, each shifted to its absolute row in `rendered`.
        self.frame_images.clear();
        self.frame_images
            .extend(self.settled_images.iter().cloned());
        for mut img in tail_imgs {
            img.row = img.row.saturating_add(tail_base);
            self.frame_images.push(img);
        }
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
/// | [`images(&r, s, &l)`](Self::images) | off | render `![alt](url)` as real pixels |
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
    resolver: Option<&'a dyn ImageResolver>,
    image_support: ImageSupport,
    image_layer: Option<ImageLayer>,
}

impl<'a> Markdown<'a> {
    /// A markdown view over `source`, rendering fenced code as plain text until
    /// a highlighter is attached with [`highlighter`](Self::highlighter).
    pub fn new(source: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            highlighter: CodeHighlighter::Plain,
            resolver: None,
            image_support: ImageSupport::None,
            image_layer: None,
        }
    }

    /// Use `highlighter` to syntax-highlight fenced code blocks.
    pub fn highlighter(mut self, highlighter: &'a dyn Highlighter) -> Self {
        self.highlighter = CodeHighlighter::With(highlighter);
        self
    }

    /// Render `![alt](url)` images as real pixels.
    ///
    /// `resolver` decodes each URL to [`ImageData`] (markdown has only the URL —
    /// see [`ImageResolver`]); a resolved image becomes a block reserved in the
    /// layout and painted over by an [`Image`] using `support`, recording its
    /// placement into `layer` for the host to [`emit`](ImageLayer::emit) after the
    /// frame. Unresolved images, and every image when `support` is
    /// [`ImageSupport::None`], fall back to the text placeholder / alt text.
    pub fn images(
        mut self,
        resolver: &'a dyn ImageResolver,
        support: ImageSupport,
        layer: &ImageLayer,
    ) -> Self {
        self.resolver = Some(resolver);
        self.image_support = support;
        self.image_layer = Some(layer.clone());
        self
    }

    /// Flatten the source to lines plus the block images it reserved.
    fn lines_and_images(
        &self,
        width: u16,
        theme: &Theme,
        sheet: &StyleSheet,
    ) -> (Vec<Line<'static>>, Vec<MarkdownImage>) {
        let items = parse_with(&self.source, theme, sheet, self.highlighter, self.resolver);
        let mut images = Vec::new();
        let lines = flatten_into(&items, width, theme, &mut images);
        (lines, images)
    }
}

impl View for Markdown<'_> {
    fn measure(&self, available: Size) -> Size {
        let (lines, _) =
            self.lines_and_images(available.width, &Theme::default(), &StyleSheet::default());
        let width = lines.iter().map(line_width).max().unwrap_or(0);
        Size::new(width.min(available.width), lines.len() as u16)
    }

    fn render(&self, area: Rect, surface: &mut Surface, ctx: &RenderCtx) {
        let (lines, images) = self.lines_and_images(area.width, ctx.theme, &ctx.sheet);
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
        // Overlay each block image on the rows it reserved, reusing the standalone
        // `Image` component for pixel emission and the alt fallback alike.
        for img in images {
            let y = area.y.saturating_add(img.row);
            let x = area.x.saturating_add(img.indent);
            if y >= area.bottom() || x >= area.right() {
                continue;
            }
            let rect = Rect {
                x,
                y,
                width: img.cols.min(area.right() - x),
                height: img.rows.min(area.bottom() - y),
            };
            let mut image = Image::new(img.data, img.cols, img.rows)
                .support(self.image_support)
                .alt(img.alt);
            if let Some(layer) = &self.image_layer {
                image = image.in_layer(layer);
            }
            image.render(rect, surface, ctx);
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
        markdown_to_lines(
            source,
            width,
            &Theme::default(),
            &StyleSheet::default(),
            CodeHighlighter::Plain,
        )
        .iter()
        .map(text)
        .collect()
    }

    #[test]
    fn heading_is_bold_and_themed() {
        let theme = Theme::default();
        let lines = markdown_to_lines(
            "# Title",
            40,
            &theme,
            &StyleSheet::from_theme(&theme),
            CodeHighlighter::Plain,
        );
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
            &StyleSheet::from_theme(&theme),
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
        let lines = markdown_to_lines(
            "use `cargo test` now",
            60,
            &theme,
            &StyleSheet::from_theme(&theme),
            CodeHighlighter::Plain,
        );
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
        let lines = markdown_to_lines(
            &src,
            20,
            &Theme::default(),
            &StyleSheet::default(),
            CodeHighlighter::Plain,
        );
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
    fn table_header_cells_are_bold_and_themed() {
        let theme = Theme::default();
        let src = "| Name | Kind |\n| --- | --- |\n| a | b |";
        let lines = markdown_to_lines(
            src,
            40,
            &theme,
            &StyleSheet::from_theme(&theme),
            CodeHighlighter::Plain,
        );
        let head = lines
            .iter()
            .flat_map(|l| &l.spans)
            .find(|s| s.content.contains("Name"))
            .expect("header cell span");
        assert!(head.style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(head.style.fg, Some(theme.code.heading));
    }

    #[test]
    fn table_cell_link_is_styled() {
        let theme = Theme::default();
        let src = "| Site |\n| --- |\n| [yolop](https://everruns.dev) |";
        let lines = markdown_to_lines(
            src,
            40,
            &theme,
            &StyleSheet::from_theme(&theme),
            CodeHighlighter::Plain,
        );
        let link = lines
            .iter()
            .flat_map(|l| &l.spans)
            .find(|s| s.content.contains("yolop"))
            .expect("link cell span");
        assert_eq!(link.style.fg, Some(theme.code.link));
        assert!(link.style.add_modifier.contains(Modifier::UNDERLINED));
    }

    #[test]
    fn table_cell_bold_and_inline_code_survive() {
        let theme = Theme::default();
        let src = "| Col |\n| --- |\n| **hi** and `cargo` |";
        let lines = markdown_to_lines(
            src,
            40,
            &theme,
            &StyleSheet::from_theme(&theme),
            CodeHighlighter::Plain,
        );
        let spans: Vec<&Span> = lines.iter().flat_map(|l| &l.spans).collect();
        let bold = spans
            .iter()
            .find(|s| s.content.contains("hi"))
            .expect("bold span");
        assert!(bold.style.add_modifier.contains(Modifier::BOLD));
        let code = spans
            .iter()
            .find(|s| s.content.contains("cargo"))
            .expect("code span");
        assert_eq!(code.style.bg, Some(theme.code.background));
    }

    #[test]
    fn table_cell_emoji_keeps_borders_aligned() {
        // A wide emoji is measured grapheme-aware, so every boxed row stays the
        // same rendered width and the borders line up.
        let src = "| Status |\n| --- |\n| ok ✅ |\n| bad |";
        let lines = markdown_to_lines(
            src,
            40,
            &Theme::default(),
            &StyleSheet::default(),
            CodeHighlighter::Plain,
        );
        let box_rows: Vec<u16> = lines
            .iter()
            .filter(|l| text(l).contains('│'))
            .map(line_width)
            .collect();
        assert!(box_rows.len() >= 2, "expected boxed rows: {box_rows:?}");
        assert!(
            box_rows.windows(2).all(|w| w[0] == w[1]),
            "boxed rows must share one width: {box_rows:?}"
        );
    }

    #[test]
    fn table_falls_back_to_plain_when_too_narrow_and_always_fits() {
        let src = "| Col A | Col B |\n| --- | --- |\n| alpha | beta |";
        for width in [4u16, 8, 12, 20, 48] {
            let lines = markdown_to_lines(
                src,
                width,
                &Theme::default(),
                &StyleSheet::default(),
                CodeHighlighter::Plain,
            );
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
            &StyleSheet::from_theme(&theme),
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
    fn custom_sheet_restyles_both_links_and_bare_urls() {
        use ratatui::style::Color;
        let theme = Theme::default();
        // One central rule remaps the link role: green + bold, no underline.
        let sheet = StyleSheet {
            link: StyleBundle::new().fg(Color::Green).bold(),
            ..StyleSheet::from_theme(&theme)
        };
        // A markdown link and a bare URL — both resolve the same `link` role.
        let lines = markdown_to_lines(
            "[docs](https://ex.com) and https://bare.example.com here",
            80,
            &theme,
            &sheet,
            CodeHighlighter::Plain,
        );
        let spans: Vec<&Span> = lines.iter().flat_map(|l| &l.spans).collect();
        for needle in ["docs", "bare.example.com"] {
            let span = spans
                .iter()
                .find(|s| s.content.contains(needle))
                .unwrap_or_else(|| panic!("missing {needle:?} span"));
            assert_eq!(span.style.fg, Some(Color::Green), "{needle}: recolored");
            assert!(
                span.style.add_modifier.contains(Modifier::BOLD),
                "{needle}: bold"
            );
            assert!(
                !span.style.add_modifier.contains(Modifier::UNDERLINED),
                "{needle}: underline dropped by the custom rule"
            );
        }
    }

    #[test]
    fn custom_sheet_restyles_headings() {
        use ratatui::style::Color;
        let theme = Theme::default();
        let sheet = StyleSheet {
            heading: StyleBundle::new().fg(Color::Magenta).italic(),
            ..StyleSheet::from_theme(&theme)
        };
        let lines = markdown_to_lines("# Title", 40, &theme, &sheet, CodeHighlighter::Plain);
        let span = &lines[0].spans[0];
        assert_eq!(span.content.as_ref(), "Title");
        assert_eq!(span.style.fg, Some(Color::Magenta));
        assert!(span.style.add_modifier.contains(Modifier::ITALIC));
        // The default heading was bold; this rule doesn't set bold, so it's gone.
        assert!(!span.style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn sheet_change_invalidates_stream_cache() {
        use ratatui::style::Color;
        let theme = Theme::default();
        let mut state = MarkdownState::new();
        state.set("A [link](https://ex.com) in prose.");

        let default_sheet = StyleSheet::from_theme(&theme);
        let link_fg = |lines: &[Line<'static>]| {
            lines
                .iter()
                .flat_map(|l| &l.spans)
                .find(|s| s.content.contains("link"))
                .expect("link span")
                .style
                .fg
        };
        assert_eq!(
            link_fg(state.lines(60, &theme, &default_sheet, CodeHighlighter::Plain)),
            Some(theme.code.link)
        );

        // Same theme, different stylesheet: the cached spans must be rebuilt.
        let recolored = StyleSheet {
            link: StyleBundle::new().fg(Color::Green),
            ..default_sheet
        };
        assert_eq!(
            link_fg(state.lines(60, &theme, &recolored, CodeHighlighter::Plain)),
            Some(Color::Green),
            "a stylesheet change must invalidate the stream cache"
        );
    }

    #[test]
    fn streaming_matches_one_shot_render() {
        let full = "# Heading\n\nA paragraph of text.\n\n```rust\nfn main() {}\n```\n\nDone.";
        let theme = Theme::default();
        let one_shot: Vec<String> = markdown_to_lines(
            full,
            40,
            &theme,
            &StyleSheet::from_theme(&theme),
            CodeHighlighter::Plain,
        )
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
                .lines(
                    40,
                    &theme,
                    &StyleSheet::from_theme(&theme),
                    CodeHighlighter::Plain,
                )
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
            let _ = state.lines(
                60,
                &theme,
                &StyleSheet::from_theme(&theme),
                CodeHighlighter::Plain,
            );
        }
        let resized: Vec<String> = state
            .lines(
                24,
                &theme,
                &StyleSheet::from_theme(&theme),
                CodeHighlighter::Plain,
            )
            .iter()
            .map(text)
            .collect();
        let one_shot: Vec<String> = markdown_to_lines(
            &full,
            24,
            &theme,
            &StyleSheet::from_theme(&theme),
            CodeHighlighter::Plain,
        )
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
        let _ = state.lines(
            40,
            &theme,
            &StyleSheet::from_theme(&theme),
            CodeHighlighter::Plain,
        );
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
        let _ = state.lines(40, &a, &StyleSheet::from_theme(&a), CodeHighlighter::Plain);
        assert!(state.stable_len > 0);

        let mut b = Theme::default();
        b.code.heading = ratatui::style::Color::Indexed(200);
        let _ = state.lines(40, &b, &StyleSheet::from_theme(&b), CodeHighlighter::Plain);
        // Cache was rebuilt under the new theme; still consistent, no stale panic.
        assert_eq!(state.cached_theme, Some(b));
    }

    #[test]
    fn image_renders_alt_as_a_marked_placeholder() {
        let theme = Theme::default();
        let lines = markdown_to_lines(
            "look: ![a cat](https://ex.com/cat.png) ok",
            60,
            &theme,
            &StyleSheet::from_theme(&theme),
            CodeHighlighter::Plain,
        );
        let whole: String = lines.iter().map(text).collect::<Vec<_>>().join("\n");
        // The alt text shows behind an image marker; the surrounding prose stays.
        assert!(whole.contains("🖼 a cat"), "expected marked alt: {whole:?}");
        assert!(whole.contains("look:") && whole.contains("ok"));
        // The alt label is link-styled, not plain body text.
        let label = lines
            .iter()
            .flat_map(|l| &l.spans)
            .find(|s| s.content.contains("a cat"))
            .expect("alt span");
        assert_eq!(label.style.fg, Some(theme.code.link));
        assert!(label.style.add_modifier.contains(Modifier::UNDERLINED));
    }

    #[test]
    fn image_without_alt_shows_the_url_so_it_is_never_dropped() {
        // Before Tag::Image handling, an alt-less image vanished entirely — URL
        // and all. Now the URL itself is the visible, link-styled label.
        let out = plain("![](https://ex.com/x.png)", 60).join("\n");
        assert!(out.contains("🖼 "), "marker present: {out:?}");
        assert!(out.contains("https://ex.com/x.png"), "url shown: {out:?}");
    }

    /// A resolver that returns a fixed image for any URL containing "ok".
    struct StubResolver;
    impl ImageResolver for StubResolver {
        fn resolve(&self, url: &str) -> Option<ImageData> {
            url.contains("ok")
                .then(|| ImageData::from_rgba(4, 2, vec![0u8; 4 * 2 * 4]).unwrap())
        }
    }

    #[test]
    fn resolved_block_image_reserves_rows_and_records_a_placement() {
        use crate::testing::render;
        let theme = Theme::default();
        let layer = ImageLayer::new();
        let resolver = StubResolver;
        let view = Markdown::new("text before\n\n![a cat](ok.png)\n\nafter").images(
            &resolver,
            ImageSupport::Kitty,
            &layer,
        );
        // Render into a wide/tall buffer; the block image reserves rows and
        // records exactly one placement into the layer.
        let _buf = render(&view, 40, 12, &theme);
        assert_eq!(layer.len(), 1, "one block image recorded");
    }

    #[test]
    fn unresolved_image_stays_an_inline_placeholder_even_with_images_enabled() {
        use crate::testing::render;
        let theme = Theme::default();
        let layer = ImageLayer::new();
        let resolver = StubResolver;
        // URL lacks "ok", so the resolver declines → inline placeholder, no pixels.
        let view =
            Markdown::new("![a dog](nope.png)").images(&resolver, ImageSupport::Kitty, &layer);
        let buf = render(&view, 40, 4, &theme);
        let mut whole = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                whole.push_str(buf[(x, y)].symbol());
            }
        }
        assert!(whole.contains("🖼 "), "placeholder marker shown: {whole:?}");
        assert!(layer.is_empty(), "declined image records no placement");
    }

    #[test]
    fn block_image_without_graphics_support_shows_alt_fallback() {
        use crate::testing::render;
        let theme = Theme::default();
        let layer = ImageLayer::new();
        let resolver = StubResolver;
        // Resolver yields pixels, but the terminal has no graphics support.
        let view = Markdown::new("![a cat](ok.png)").images(&resolver, ImageSupport::None, &layer);
        let buf = render(&view, 40, 6, &theme);
        let mut whole = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                whole.push_str(buf[(x, y)].symbol());
            }
        }
        assert!(
            whole.contains("[image: a cat]"),
            "alt fallback painted: {whole:?}"
        );
        assert!(layer.is_empty(), "no placement without graphics support");
    }

    #[test]
    fn block_image_reserves_height_proportional_to_aspect() {
        // A 40x2 image (wide) reserves few rows; a 4x40 image (tall) reserves more.
        let wide = ImageData::from_rgba(40, 2, vec![0u8; 40 * 2 * 4]).unwrap();
        let tall = ImageData::from_rgba(4, 40, vec![0u8; 4 * 40 * 4]).unwrap();
        let (_, wr) = image_cell_size(&wide, 40);
        let (_, tr) = image_cell_size(&tall, 40);
        assert!(wr < tr, "tall image reserves more rows: {wr} vs {tr}");
        assert!(wr >= 1 && tr <= 30, "rows are clamped: {wr}, {tr}");
    }

    #[test]
    fn streaming_state_reports_a_block_image_placement() {
        let theme = Theme::default();
        let mut md = MarkdownState::new().with_image_resolver(Box::new(StubResolver));
        md.set("intro line\n\n![a cat](ok.png)\n\ntail line");
        let lines = md.lines(40, &theme, &StyleSheet::from_theme(&theme), CodeHighlighter::Plain).to_vec();
        let imgs = md.images();
        assert_eq!(imgs.len(), 1, "one block image reported");
        let img = &imgs[0];
        assert_eq!(img.alt, "a cat");
        // The reported row is inside the rendered lines and is a reserved blank.
        assert!((img.row as usize) < lines.len(), "row within lines");
        assert!(
            is_blank_line(&lines[img.row as usize]),
            "reserved row is blank"
        );
        // "intro line" is above the image, "tail line" below it.
        assert!(text(&lines[0]).contains("intro"));
    }

    #[test]
    fn streaming_image_row_matches_one_shot() {
        // Feeding the same document incrementally lands the image on the same row
        // as parsing it whole — the settled/tail offset bookkeeping is consistent.
        let theme = Theme::default();
        let doc = "# Title\n\nbefore\n\n![pic](ok.png)\n\nafter paragraph here";

        let mut whole = MarkdownState::new().with_image_resolver(Box::new(StubResolver));
        whole.set(doc);
        let _ = whole.lines(30, &theme, &StyleSheet::from_theme(&theme), CodeHighlighter::Plain);
        let whole_row = whole.images()[0].row;

        let mut streamed = MarkdownState::new().with_image_resolver(Box::new(StubResolver));
        for chunk in [
            "# Title\n\nbe",
            "fore\n\n![pic](ok",
            ".png)\n\nafter ",
            "paragraph here",
        ] {
            streamed.push_str(chunk);
            let _ = streamed.lines(30, &theme, &StyleSheet::from_theme(&theme), CodeHighlighter::Plain);
        }
        assert_eq!(streamed.images().len(), 1);
        assert_eq!(
            streamed.images()[0].row,
            whole_row,
            "streamed image row matches one-shot"
        );
    }

    #[test]
    fn no_resolver_means_no_streaming_placements() {
        let theme = Theme::default();
        let mut md = MarkdownState::new();
        md.set("![a cat](ok.png)");
        let _ = md.lines(40, &theme, &StyleSheet::from_theme(&theme), CodeHighlighter::Plain);
        assert!(md.images().is_empty(), "images() empty without a resolver");
    }

    #[test]
    fn markdown_image_rect_offsets_by_area_and_row() {
        let img = MarkdownImage {
            row: 3,
            indent: 2,
            cols: 10,
            rows: 4,
            data: ImageData::from_rgba(2, 2, vec![0u8; 16]).unwrap(),
            alt: String::new(),
        };
        let rect = img.rect(Rect::new(5, 1, 40, 20));
        assert_eq!((rect.x, rect.y, rect.width, rect.height), (7, 4, 10, 4));
        // Clamped to the area when it would overflow.
        let tight = img.rect(Rect::new(5, 1, 8, 5));
        assert_eq!(tight.width, 6, "clamped to area right edge");
    }
}

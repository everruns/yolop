//! OSC 8 terminal hyperlinks.
//!
//! Like [`crate::native`] (OSC 9;4 progress), this is an out-of-band terminal
//! capability that ratatui's cell buffer cannot carry: a `Cell` holds one
//! grapheme + style, with nowhere to attach a link target, and embedding the
//! escape in a cell's symbol breaks width accounting. So hyperlinks are emitted
//! by writing styled spans *directly* to the terminal, wrapping link runs in the
//! OSC 8 sequence:
//!
//! ```text
//! ESC ] 8 ; ; <url> ST   <visible text>   ESC ] 8 ; ; ST
//! ```
//!
//! where `ST` is the string terminator (`ESC \`). Terminals that support OSC 8
//! (Ghostty, iTerm2, WezTerm, Kitty, recent VTE) make the run clickable; others
//! ignore the unknown OSC and render the text unchanged.
//!
//! [`osc8`] is the pure encoder (validated + sanitized, unit-testable with no
//! I/O). [`write_line`] serializes a ratatui [`Line`] — colors, common
//! modifiers, and OSC 8 links — to any [`Write`] sink, so a host can push a
//! transcript line to scrollback with real hyperlinks instead of going through
//! the cell buffer.

use std::io::{self, Write};
use std::num::NonZeroU16;

use crossterm::queue;
use crossterm::style::{
    Attribute, Color as CtColor, Print, ResetColor, SetAttribute, SetBackgroundColor,
    SetForegroundColor,
};
use ratatui_core::backend::{Backend, ClearType, WindowSize};
use ratatui_core::buffer::{Buffer, Cell, CellDiffOption};
use ratatui_core::layout::{Position, Rect, Size};
use ratatui_core::style::{Color, Modifier};
use ratatui_core::text::{Line, Span};
use ratatui_crossterm::CrosstermBackend;

/// String terminator for an OSC sequence: `ESC \`.
const ST: &str = "\x1b\\";

/// Which URL schemes tuika turns into OSC 8 hyperlinks.
///
/// The default ([`LinkPolicy::WEB`]) is deliberately conservative — only
/// `http(s)` — because an OSC 8 target a terminal will act on is a capability
/// surface: `file:`, `tel:`, and custom app schemes can do more than open a web
/// page, and mapping arbitrary schemes to handlers is where the real risk
/// lives. Hosts opt into anything beyond `http(s)` explicitly, so the safe set
/// is the one you get by default.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LinkPolicy {
    web: bool,
    mailto: bool,
}

impl LinkPolicy {
    /// Link nothing — makes [`HyperlinkBackend`] a pure pass-through.
    pub const NONE: Self = Self {
        web: false,
        mailto: false,
    };
    /// The conservative default: `http://` and `https://` only.
    pub const WEB: Self = Self {
        web: true,
        mailto: false,
    };

    /// Also treat `mailto:` addresses as links. The target is still stripped of
    /// control characters (the same ESC/BEL breakout defense as web URLs), and
    /// its query component (`?subject=`, `?cc=`, `?bcc=`, `?body=`, …) is
    /// dropped so a click can't be steered into pre-filled headers — a clickable
    /// `mailto:` only ever opens a compose window to the bare address.
    pub const fn with_mailto(mut self) -> Self {
        self.mailto = true;
        self
    }

    /// Whether the policy links anything at all (`false` ⇒ pass-through).
    pub const fn links_any(self) -> bool {
        self.web || self.mailto
    }

    /// Whether `url` is a valid target under this policy.
    pub fn allows(self, url: &str) -> bool {
        sanitize_url(url, self).is_some()
    }
}

impl Default for LinkPolicy {
    fn default() -> Self {
        Self::WEB
    }
}

/// Web scheme prefixes, longest first so `https://` wins over `http://`.
const WEB_PREFIXES: [&str; 2] = ["https://", "http://"];
/// The `mailto:` scheme prefix.
const MAILTO_PREFIX: &str = "mailto:";

/// Wrap `text` in an OSC 8 hyperlink to `url` under `policy`, or return `text`
/// unchanged when `url` is not a valid, safe target for that policy. Pure and
/// allocation-only — no I/O.
pub fn osc8_with(url: &str, text: &str, policy: LinkPolicy) -> String {
    match sanitize_url(url, policy) {
        Some(url) => format!("\x1b]8;;{url}{ST}{text}\x1b]8;;{ST}"),
        None => text.to_string(),
    }
}

/// [`osc8_with`] under the default ([`LinkPolicy::WEB`]) policy: wrap `text` in a
/// link to `url` when `url` is a safe `http(s)` URL, else return `text`.
pub fn osc8(url: &str, text: &str) -> String {
    osc8_with(url, text, LinkPolicy::default())
}

/// Whether `s` is a bare `http(s)://` URL with no interior whitespace — the
/// shape a host can hand to [`write_line`] as a link run.
pub fn is_web_url(s: &str) -> bool {
    (s.starts_with("http://") || s.starts_with("https://")) && !s.chars().any(char::is_whitespace)
}

/// Whether `s` is a bare URL (no interior whitespace) whose scheme `policy`
/// links. Generalizes [`is_web_url`] to the enabled scheme set — the shape a
/// span must have for [`write_line_with`] to wrap it.
fn is_linkable(s: &str, policy: LinkPolicy) -> bool {
    if s.chars().any(char::is_whitespace) {
        return false;
    }
    (policy.web && WEB_PREFIXES.iter().any(|p| s.starts_with(p)))
        || (policy.mailto && s.starts_with(MAILTO_PREFIX))
}

/// Strip control characters — including the `ESC`/`BEL` that could terminate the
/// OSC early and let a crafted target break out of the sequence — plus `DEL`.
fn strip_controls(s: &str) -> String {
    s.chars()
        .filter(|&c| !c.is_control() && c != '\u{7f}')
        .collect()
}

/// Validate `url` against `policy` and neutralize anything that could break out
/// of the OSC 8 sequence, returning the safe target or `None`.
///
/// Every accepted scheme has its control characters removed. `mailto:`
/// additionally has its query (`?…`) dropped before cleaning, so header
/// parameters can't ride along — see [`LinkPolicy::with_mailto`].
fn sanitize_url(url: &str, policy: LinkPolicy) -> Option<String> {
    if policy.web && WEB_PREFIXES.iter().any(|p| url.starts_with(p)) {
        let cleaned = strip_controls(url);
        return (cleaned.len() >= "http://".len()).then_some(cleaned);
    }
    if policy.mailto && url.starts_with(MAILTO_PREFIX) {
        // Drop the query before cleaning so `?cc=…`/`?body=…` never reach the
        // terminal, then strip control chars like any other target.
        let addr = url.split('?').next().unwrap_or(url);
        let cleaned = strip_controls(addr);
        return (cleaned.len() > MAILTO_PREFIX.len()).then_some(cleaned);
    }
    None
}

/// A hyperlink run in a rendered buffer: columns `[start_col, end_col)` on
/// `line` (0-based within the rendered lines, not screen coordinates) point at
/// `url`. Produced by markdown when a `[label](url)` (or bare URL) survives
/// wrapping; applied with [`apply_buffer_links`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BufferLink {
    /// Row index within the rendered line list (0-based).
    pub line: u16,
    /// First column of the link run, relative to the line's left edge.
    pub start_col: u16,
    /// Exclusive end column of the link run.
    pub end_col: u16,
    /// Link target (not necessarily equal to the visible label).
    pub url: String,
}

/// OSC 8 opener prefix written into a cell symbol: `ESC ] 8 ; ;`.
const OSC8_OPEN: &str = "\x1b]8;;";

/// Embed OSC 8 hyperlinks for each [`BufferLink`] into `buf`.
///
/// `origin` is the top-left of the area the linked lines were painted into
/// (`origin.x` + `start_col`, `origin.y` + `line`). Runs whose scheme `policy`
/// rejects are skipped. Boundary cells carry the opener/closer with
/// [`CellDiffOption::ForcedWidth`] so the escapes cost no columns — the same
/// technique as a post-render bare-URL pass, but driven by explicit targets so
/// a markdown `[label](url)` stays clickable even when the label is not the URL.
///
/// Idempotent per cell: a boundary that already holds an OSC 8 marker is left
/// alone, so a host can re-apply after a partial redraw without stacking
/// escapes.
pub fn apply_buffer_links(
    buf: &mut Buffer,
    origin: Position,
    links: &[BufferLink],
    policy: LinkPolicy,
) {
    if !policy.links_any() {
        return;
    }
    for link in links {
        let Some(url) = sanitize_url(&link.url, policy) else {
            continue;
        };
        if link.end_col <= link.start_col {
            continue;
        }
        let y = origin.y.saturating_add(link.line);
        let xs = origin.x.saturating_add(link.start_col);
        let xe = origin.x.saturating_add(link.end_col.saturating_sub(1));
        if y >= buf.area.bottom() || xs >= buf.area.right() || xe >= buf.area.right() || xe < xs {
            continue;
        }
        wrap_cell_osc8(&mut buf[(xs, y)], &url, true);
        wrap_cell_osc8(&mut buf[(xe, y)], &url, false);
    }
}

/// Wrap `cell`'s symbol in an OSC 8 open (`head`) or close (`!head`) sequence,
/// forcing width 1. No-ops when the cell already carries that marker.
fn wrap_cell_osc8(cell: &mut Cell, url: &str, head: bool) {
    let sym = cell.symbol();
    if head {
        if sym.contains(OSC8_OPEN) {
            return;
        }
        cell.set_symbol(&format!("{OSC8_OPEN}{url}{ST}{sym}"))
            .set_diff_option(CellDiffOption::ForcedWidth(NonZeroU16::new(1).unwrap()));
        return;
    }
    // Closer: `glyph + ESC ] 8 ; ; ST`. Skip when a closer (or a combined
    // open+close on a one-cell run) is already present.
    if sym.contains("\x1b]8;;\x1b\\") {
        return;
    }
    cell.set_symbol(&format!("{sym}{OSC8_OPEN}{ST}"))
        .set_diff_option(CellDiffOption::ForcedWidth(NonZeroU16::new(1).unwrap()));
}

/// Visible grapheme of a cell symbol with any OSC 8 wrapper stripped — used when
/// reconstructing a row for bare-URL scanning so an already-linked cell is not
/// re-wrapped and so Ctrl+click hit-testing sees the label, not the escape.
fn visible_symbol(symbol: &str) -> String {
    strip_osc8(symbol)
}

/// Remove OSC 8 open/close sequences from `s`, leaving the visible text.
fn strip_osc8(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // ESC ] 8 ; ; … ST  (ST = ESC \)
        if bytes[i..].starts_with(b"\x1b]8;;") {
            i += 5; // skip ESC ] 8 ; ;
            while i + 1 < bytes.len() {
                if bytes[i] == 0x1b && bytes[i + 1] == b'\\' {
                    i += 2;
                    break;
                }
                i += 1;
            }
            continue;
        }
        // Copy one UTF-8 scalar.
        let ch = s[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Extract an OSC 8 target from a cell symbol, if the opener is present.
fn osc8_target_in(symbol: &str) -> Option<String> {
    let rest = symbol.strip_prefix(OSC8_OPEN)?;
    let end = rest.find(ST)?;
    let url = &rest[..end];
    (!url.is_empty()).then(|| url.to_string())
}

/// Return the visible HTTP(S) URL under a Ctrl+left-button release.
///
/// Resolution order:
/// 1. An OSC 8 target already embedded in the cell run under the pointer
///    (markdown `[label](url)` after [`apply_buffer_links`]).
/// 2. A bare `http(s)://…` run visible in the row (HyperlinkBackend / plain
///    transcript URLs).
///
/// Opening the URL remains the host's responsibility.
pub fn ctrl_click_url(event: &crate::Mouse, buffer: &Buffer, area: Rect) -> Option<String> {
    ctrl_click_url_with(event, buffer, area, LinkPolicy::default())
}

/// [`ctrl_click_url`] with an explicit [`LinkPolicy`] for the bare-URL fallback.
pub fn ctrl_click_url_with(
    event: &crate::Mouse,
    buffer: &Buffer,
    area: Rect,
    policy: LinkPolicy,
) -> Option<String> {
    if event.kind != crate::MouseKind::Up(crate::MouseButton::Left)
        || !event.ctrl
        || event.shift
        || event.alt
        || event.row < area.y
        || event.row >= area.bottom()
        || event.column < area.x
        || event.column >= area.right()
    {
        return None;
    }
    // Prefer an OSC 8 target covering this column — labeled markdown links live
    // here, and the visible text may not be a URL at all.
    if let Some(url) = osc8_url_at(buffer, area, event.column, event.row)
        && sanitize_url(&url, policy).is_some()
    {
        return Some(url);
    }
    let mut row = String::new();
    let mut clicked_bytes = 0..0;
    for column in area.x..area.right() {
        let start = row.len();
        let visible = visible_symbol(buffer[(column, event.row)].symbol());
        row.push_str(&visible);
        if column == event.column {
            clicked_bytes = start..row.len();
        }
    }
    find_links(&row, policy)
        .into_iter()
        .find(|(start, end)| *start < clicked_bytes.end && clicked_bytes.start < *end)
        .map(|(start, end)| row[start..end].to_string())
}

/// Walk left from `(col, row)` for an OSC 8 opener and right for its closer;
/// return the target when `col` sits inside that run.
fn osc8_url_at(buffer: &Buffer, area: Rect, col: u16, row: u16) -> Option<String> {
    let mut url = None;
    let mut open_at = None;
    for x in area.x..=col {
        if let Some(u) = osc8_target_in(buffer[(x, row)].symbol()) {
            url = Some(u);
            open_at = Some(x);
        }
    }
    let (url, open_at) = (url?, open_at?);
    // Confirm a closer exists at or after `col` (or the open cell itself closes
    // a single-cell link), and that no later opener sits between open and col.
    for x in open_at..=col {
        if x > open_at && osc8_target_in(buffer[(x, row)].symbol()).is_some() {
            // A newer opener superseded the one we found — shouldn't happen for
            // well-formed runs; treat as not inside the original link.
            return None;
        }
    }
    let mut closed = false;
    for x in col..area.right() {
        let sym = buffer[(x, row)].symbol();
        if sym.contains("\x1b]8;;\x1b\\") || sym.ends_with("\x1b]8;;\x1b\\") {
            closed = true;
            break;
        }
        // A new opener before a closer means our run ended without covering col.
        if x > col && osc8_target_in(sym).is_some() {
            return None;
        }
    }
    closed.then_some(url)
}

/// Byte ranges of every linkable URL in `s` under `policy`, left to right,
/// non-overlapping. Each match runs to the next whitespace with trailing
/// sentence punctuation trimmed, matching how the host styles links; a
/// `mailto:` match also stops at its query `?` so the pre-fill params are
/// neither shown nor linked.
fn find_links(s: &str, policy: LinkPolicy) -> Vec<(usize, usize)> {
    const TRAILING: &[char] = &['.', ',', ';', ':', '!', '?', ')', ']', '}', '\'', '"'];
    let mut ranges = Vec::new();
    if !policy.links_any() {
        return ranges;
    }
    // (prefix, is_mailto) for each enabled scheme; web prefixes longest-first so
    // ties at the same offset resolve to `https://` over `http://`.
    let mut prefixes: Vec<(&str, bool)> = Vec::new();
    if policy.web {
        prefixes.extend(WEB_PREFIXES.iter().map(|&p| (p, false)));
    }
    if policy.mailto {
        prefixes.push((MAILTO_PREFIX, true));
    }

    let mut offset = 0;
    while offset < s.len() {
        let rest = &s[offset..];
        // Leftmost occurrence of any enabled prefix.
        let Some((rel, prefix, is_mailto)) = prefixes
            .iter()
            .filter_map(|&(p, m)| rest.find(p).map(|i| (i, p, m)))
            .min_by_key(|&(i, ..)| i)
        else {
            break;
        };
        let start = offset + rel;
        let tail = &s[start..];
        let mut raw_end = tail.find(char::is_whitespace).unwrap_or(tail.len());
        if is_mailto && let Some(q) = tail[..raw_end].find('?') {
            raw_end = q;
        }
        let len = tail[..raw_end].trim_end_matches(TRAILING).len();
        if len <= prefix.len() {
            // Scheme with no target (e.g. a bare "http://"). Skip past just the
            // prefix so a later scheme in the same string is still found.
            offset = start + prefix.len();
            continue;
        }
        ranges.push((start, start + len));
        offset = start + len;
    }
    ranges
}

/// Serialize a ratatui [`Line`] to `out` with SGR styling and OSC 8 links, then
/// reset styling. A span whose visible text is a bare web URL (see
/// [`is_web_url`]) is emitted as a hyperlink to itself; every other span is
/// printed as plain styled text. Does not emit a trailing newline — the caller
/// controls line breaks.
pub fn write_line(out: &mut impl Write, line: &Line<'_>) -> io::Result<()> {
    write_line_with(out, line, LinkPolicy::default())
}

/// [`write_line`] with an explicit [`LinkPolicy`], so a host can decide which
/// schemes (e.g. `mailto:`) become hyperlinks when it pushes a line to
/// scrollback.
pub fn write_line_with(
    out: &mut impl Write,
    line: &Line<'_>,
    policy: LinkPolicy,
) -> io::Result<()> {
    for span in &line.spans {
        write_span(out, span, policy)?;
    }
    queue!(out, ResetColor, SetAttribute(Attribute::Reset))?;
    Ok(())
}

fn write_span(out: &mut impl Write, span: &Span<'_>, policy: LinkPolicy) -> io::Result<()> {
    apply_style(out, span)?;
    let content = span.content.as_ref();
    let is_link = content.trim() == content && is_linkable(content, policy);
    if is_link {
        queue!(out, Print(osc8_with(content, content, policy)))?;
    } else {
        queue!(out, Print(content))?;
    }
    // Reset after each span so styles never bleed into the next one.
    queue!(out, ResetColor, SetAttribute(Attribute::Reset))?;
    Ok(())
}

fn apply_style(out: &mut impl Write, span: &Span<'_>) -> io::Result<()> {
    let style = span.style;
    if let Some(fg) = style.fg {
        queue!(out, SetForegroundColor(to_ct_color(fg)))?;
    }
    if let Some(bg) = style.bg {
        queue!(out, SetBackgroundColor(to_ct_color(bg)))?;
    }
    for (modifier, attribute) in [
        (Modifier::BOLD, Attribute::Bold),
        (Modifier::DIM, Attribute::Dim),
        (Modifier::ITALIC, Attribute::Italic),
        (Modifier::UNDERLINED, Attribute::Underlined),
        (Modifier::CROSSED_OUT, Attribute::CrossedOut),
        (Modifier::REVERSED, Attribute::Reverse),
    ] {
        if style.add_modifier.contains(modifier) {
            queue!(out, SetAttribute(attribute))?;
        }
    }
    Ok(())
}

/// Map a ratatui color to the crossterm equivalent. `Rgb`/`Indexed` (what the
/// host's transcript actually uses) map exactly; the named ANSI colors map to
/// their crossterm counterparts.
fn to_ct_color(color: Color) -> CtColor {
    match color {
        Color::Reset => CtColor::Reset,
        Color::Black => CtColor::Black,
        Color::Red => CtColor::DarkRed,
        Color::Green => CtColor::DarkGreen,
        Color::Yellow => CtColor::DarkYellow,
        Color::Blue => CtColor::DarkBlue,
        Color::Magenta => CtColor::DarkMagenta,
        Color::Cyan => CtColor::DarkCyan,
        Color::Gray => CtColor::Grey,
        Color::DarkGray => CtColor::DarkGrey,
        Color::LightRed => CtColor::Red,
        Color::LightGreen => CtColor::Green,
        Color::LightYellow => CtColor::Yellow,
        Color::LightBlue => CtColor::Blue,
        Color::LightMagenta => CtColor::Magenta,
        Color::LightCyan => CtColor::Cyan,
        Color::White => CtColor::White,
        Color::Rgb(r, g, b) => CtColor::Rgb { r, g, b },
        Color::Indexed(i) => CtColor::AnsiValue(i),
    }
}

/// A ratatui [`Backend`] that wraps [`CrosstermBackend`] and makes `http(s)`
/// URLs in rendered output real OSC 8 hyperlinks.
///
/// This is the only place OSC 8 can be emitted while staying inside ratatui's
/// model: every method delegates to the inner [`CrosstermBackend`] — so cursor,
/// scroll-region, and `insert_before` bookkeeping stay consistent — except
/// [`draw`](Backend::draw), which scans each contiguous run of cells for URLs
/// and wraps just those cells in the OSC 8 sequence. Non-URL text is forwarded
/// untouched. When the policy links nothing ([`LinkPolicy::NONE`]) it is a pure
/// pass-through with no scanning, so a host can gate the feature at zero cost.
pub struct HyperlinkBackend<W: Write> {
    inner: CrosstermBackend<W>,
    policy: LinkPolicy,
}

impl<W: Write> HyperlinkBackend<W> {
    /// Wrap `writer`. `enabled` turns OSC 8 emission on under the default
    /// ([`LinkPolicy::WEB`]) policy; when false, `draw` forwards straight to the
    /// inner backend. Use [`with_policy`](Self::with_policy) to link other
    /// schemes (e.g. `mailto:`).
    pub fn new(writer: W, enabled: bool) -> Self {
        let policy = if enabled {
            LinkPolicy::default()
        } else {
            LinkPolicy::NONE
        };
        Self::with_policy(writer, policy)
    }

    /// Wrap `writer` with an explicit [`LinkPolicy`], so the host decides which
    /// schemes become hyperlinks. [`LinkPolicy::NONE`] is a pure pass-through.
    pub fn with_policy(writer: W, policy: LinkPolicy) -> Self {
        Self {
            inner: CrosstermBackend::new(writer),
            policy,
        }
    }

    /// Emit one maximal contiguous run of cells, wrapping any URL sub-runs in
    /// OSC 8. Reuses the inner backend's `draw` for all SGR/cursor logic so we
    /// never reimplement styling.
    fn emit_run(&mut self, run: &[(u16, u16, &Cell)]) -> io::Result<()> {
        // Reconstruct the run's visible text (OSC 8 wrappers stripped so an
        // already-linked markdown label is not mistaken for a bare URL) and
        // remember where each cell starts so a URL byte-range maps back to a
        // cell index range.
        let mut text = String::new();
        let mut cell_starts = Vec::with_capacity(run.len());
        for (_, _, cell) in run {
            cell_starts.push(text.len());
            text.push_str(&visible_symbol(cell.symbol()));
        }

        let urls = find_links(&text, self.policy);
        if urls.is_empty() {
            return self.inner.draw(run.iter().copied());
        }

        let mut cursor = 0usize;
        for (byte_start, byte_end) in urls {
            // URL boundaries align to cell boundaries (a cell is one grapheme),
            // so partition_point lands exactly on the first cell at/after each
            // byte offset.
            let start_cell = cell_starts.partition_point(|&b| b < byte_start);
            let end_cell = cell_starts.partition_point(|&b| b < byte_end);
            if cursor < start_cell {
                self.inner.draw(run[cursor..start_cell].iter().copied())?;
            }
            if start_cell < end_cell {
                let sub = &run[start_cell..end_cell];
                // Skip re-wrapping a sub-run that [`apply_buffer_links`] already
                // marked — nesting OSC 8 breaks click targets.
                let already = sub.iter().any(|(_, _, c)| c.symbol().contains(OSC8_OPEN));
                if already {
                    self.inner.draw(sub.iter().copied())?;
                } else {
                    match sanitize_url(&text[byte_start..byte_end], self.policy) {
                        Some(url) => {
                            // CrosstermBackend implements Write, so raw OSC 8 bytes go
                            // straight through to its inner writer.
                            write!(self.inner, "\x1b]8;;{url}{ST}")?;
                            self.inner.draw(sub.iter().copied())?;
                            write!(self.inner, "\x1b]8;;{ST}")?;
                        }
                        None => self.inner.draw(sub.iter().copied())?,
                    }
                }
            }
            cursor = end_cell.max(cursor);
        }
        if cursor < run.len() {
            self.inner.draw(run[cursor..].iter().copied())?;
        }
        Ok(())
    }
}

impl<W: Write> Write for HyperlinkBackend<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.inner.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        Write::flush(&mut self.inner)
    }
}

impl<W: Write> Backend for HyperlinkBackend<W> {
    type Error = io::Error;

    fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        if !self.policy.links_any() {
            return self.inner.draw(content);
        }
        let cells: Vec<(u16, u16, &Cell)> = content.collect();
        let mut i = 0;
        while i < cells.len() {
            let mut j = i + 1;
            // A run is cells on the same row with strictly increasing adjacent
            // columns — the shape ratatui produces for a freshly drawn line.
            while j < cells.len()
                && cells[j].1 == cells[j - 1].1
                && cells[j].0 == cells[j - 1].0 + 1
            {
                j += 1;
            }
            self.emit_run(&cells[i..j])?;
            i = j;
        }
        Ok(())
    }

    fn append_lines(&mut self, n: u16) -> io::Result<()> {
        self.inner.append_lines(n)
    }
    fn hide_cursor(&mut self) -> io::Result<()> {
        self.inner.hide_cursor()
    }
    fn show_cursor(&mut self) -> io::Result<()> {
        self.inner.show_cursor()
    }
    fn get_cursor_position(&mut self) -> io::Result<Position> {
        self.inner.get_cursor_position()
    }
    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
        self.inner.set_cursor_position(position)
    }
    fn clear(&mut self) -> io::Result<()> {
        self.inner.clear()
    }
    fn clear_region(&mut self, clear_type: ClearType) -> io::Result<()> {
        self.inner.clear_region(clear_type)
    }
    fn size(&self) -> io::Result<Size> {
        self.inner.size()
    }
    fn window_size(&mut self) -> io::Result<WindowSize> {
        self.inner.window_size()
    }
    fn flush(&mut self) -> io::Result<()> {
        Backend::flush(&mut self.inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes(line: &Line<'_>) -> String {
        let mut out: Vec<u8> = Vec::new();
        write_line(&mut out, line).expect("write");
        String::from_utf8(out).expect("utf8")
    }

    #[test]
    fn osc8_wraps_valid_web_urls() {
        assert_eq!(
            osc8("https://example.com", "example"),
            "\x1b]8;;https://example.com\x1b\\example\x1b]8;;\x1b\\"
        );
    }

    #[test]
    fn osc8_passes_through_non_web_or_unsafe_urls() {
        // Non-web schemes are left as plain text.
        assert_eq!(osc8("mailto:a@b.com", "mail"), "mail");
        assert_eq!(osc8("ftp://host/x", "f"), "f");
        // A URL trying to smuggle an ESC (which could terminate the OSC early
        // and break out) has the control byte stripped; the link target keeps
        // no raw ESC, so it cannot escape the sequence.
        let sneaky = "https://evil\x1b\\.com";
        let encoded = osc8(sneaky, "x");
        assert!(
            !encoded.contains("evil\x1b"),
            "raw escape must be stripped from the target: {encoded:?}"
        );
        assert!(encoded.starts_with("\x1b]8;;https://evil"));
    }

    #[test]
    fn is_web_url_requires_scheme_and_no_whitespace() {
        assert!(is_web_url("https://a.dev/x?y=1"));
        assert!(is_web_url("http://a.dev"));
        assert!(!is_web_url("a.dev"));
        assert!(!is_web_url("https://a.dev x"));
    }

    #[test]
    fn write_line_hyperlinks_url_spans_only() {
        let line = Line::from(vec![
            Span::raw("see "),
            Span::raw("https://rust-lang.org"),
            Span::raw(" now"),
        ]);
        let out = bytes(&line);
        // The URL span is wrapped in OSC 8 to itself; plain text is untouched.
        assert!(
            out.contains("\x1b]8;;https://rust-lang.org\x1b\\https://rust-lang.org\x1b]8;;\x1b\\")
        );
        assert!(out.contains("see "));
        assert!(out.contains(" now"));
    }

    #[test]
    fn write_line_emits_color_and_underline_then_resets() {
        let line = Line::from(Span::styled(
            "https://a.dev",
            ratatui_core::style::Style::default()
                .fg(Color::Rgb(45, 91, 158))
                .add_modifier(Modifier::UNDERLINED),
        ));
        let out = bytes(&line);
        // Underline attribute (SGR 4) is present, the link is wrapped, and the
        // line ends reset. Truecolor SGR form varies by crossterm/TERM, so we
        // only require that *some* foreground command preceded the text.
        assert!(out.contains("\x1b[4m"), "underline SGR expected: {out:?}");
        assert!(
            out.contains("\x1b]8;;https://a.dev\x1b\\"),
            "OSC 8 wrap expected: {out:?}"
        );
        assert!(out.trim_end().ends_with("\x1b[0m") || out.contains("\x1b[0m"));
    }

    #[test]
    fn write_line_plain_text_has_no_osc8() {
        let line = Line::from(Span::raw("no links here"));
        let out = bytes(&line);
        assert!(!out.contains("\x1b]8;;"));
        assert!(out.contains("no links here"));
    }

    #[test]
    fn ctrl_click_returns_visible_url_under_pointer() {
        use crate::{Mouse, MouseButton, MouseKind};
        use ratatui_core::{buffer::Buffer, layout::Rect, style::Style};
        let area = Rect::new(3, 2, 40, 1);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 50, 5));
        buffer.set_string(
            area.x,
            area.y,
            "see https://example.com/docs now",
            Style::default(),
        );
        let mut event = Mouse::at(MouseKind::Up(MouseButton::Left), 15, area.y);
        event.ctrl = true;
        assert_eq!(
            ctrl_click_url(&event, &buffer, area).as_deref(),
            Some("https://example.com/docs")
        );
    }

    #[test]
    fn ctrl_click_ignores_plain_clicks_and_non_url_text() {
        use crate::{Mouse, MouseButton, MouseKind};
        use ratatui_core::{buffer::Buffer, layout::Rect, style::Style};
        let area = Rect::new(0, 0, 30, 1);
        let mut buffer = Buffer::empty(area);
        buffer.set_string(0, 0, "https://example.com plain", Style::default());
        let plain = Mouse::at(MouseKind::Up(MouseButton::Left), 10, 0);
        let mut text = Mouse::at(MouseKind::Up(MouseButton::Left), 23, 0);
        text.ctrl = true;
        assert_eq!(ctrl_click_url(&plain, &buffer, area), None);
        assert_eq!(ctrl_click_url(&text, &buffer, area), None);
    }

    #[test]
    fn find_web_urls_locates_and_trims() {
        let web = LinkPolicy::default();
        assert_eq!(find_links("see https://a.dev/x, ok", web), vec![(4, 19)]);
        assert_eq!(
            find_links("a http://x.io b https://y.io", web),
            vec![(2, 13), (16, 28)]
        );
        assert!(find_links("no links", web).is_empty());
    }

    #[test]
    fn mailto_is_off_under_default_policy() {
        // Default (web-only) policy neither encodes nor finds `mailto:`.
        assert_eq!(osc8("mailto:a@b.com", "mail"), "mail");
        assert!(find_links("write mailto:a@b.com now", LinkPolicy::default()).is_empty());
    }

    #[test]
    fn mailto_links_when_opted_in() {
        let policy = LinkPolicy::WEB.with_mailto();
        assert_eq!(
            osc8_with("mailto:a@b.com", "mail", policy),
            "\x1b]8;;mailto:a@b.com\x1b\\mail\x1b]8;;\x1b\\"
        );
        // Found in running text, trailing punctuation trimmed.
        assert_eq!(find_links("write mailto:a@b.com.", policy), vec![(6, 20)]);
        // Web still works alongside mailto.
        assert_eq!(
            find_links("mailto:a@b.com then https://x.io", policy),
            vec![(0, 14), (20, 32)]
        );
    }

    #[test]
    fn mailto_drops_query_to_block_header_injection() {
        let policy = LinkPolicy::WEB.with_mailto();
        // The `?cc=…&body=…` header params are dropped from both the linked
        // range and the sanitized target.
        assert_eq!(
            find_links("mailto:a@b.com?cc=evil@x.com&body=hi", policy),
            vec![(0, 14)]
        );
        let encoded = osc8_with("mailto:a@b.com?cc=evil@x.com&body=hi", "m", policy);
        assert_eq!(encoded, "\x1b]8;;mailto:a@b.com\x1b\\m\x1b]8;;\x1b\\");
        assert!(
            !encoded.contains("cc="),
            "query must not reach the OSC target"
        );
    }

    #[test]
    fn mailto_strips_control_bytes_from_target() {
        let policy = LinkPolicy::WEB.with_mailto();
        let sneaky = "mailto:a\x1b\\@b.com";
        let encoded = osc8_with(sneaky, "m", policy);
        assert!(
            !encoded.contains("a\x1b"),
            "raw escape must be stripped: {encoded:?}"
        );
        assert!(encoded.starts_with("\x1b]8;;mailto:a"));
    }

    #[test]
    fn mailto_without_address_is_not_a_link() {
        let policy = LinkPolicy::WEB.with_mailto();
        assert_eq!(osc8_with("mailto:", "m", policy), "m");
        assert!(find_links("bare mailto: here", policy).is_empty());
    }

    /// A `Write` whose buffer we can inspect after the backend consumes it.
    #[derive(Clone)]
    struct SharedBuf(std::rc::Rc<std::cell::RefCell<Vec<u8>>>);

    impl Write for SharedBuf {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.borrow_mut().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// Render a row of single-char cells through a backend built with `policy`
    /// and return the emitted bytes.
    fn draw_row_with(text: &str, policy: LinkPolicy) -> String {
        use ratatui_core::buffer::Cell;
        let cells: Vec<(u16, u16, Cell)> = text
            .chars()
            .enumerate()
            .map(|(i, ch)| {
                let mut cell = Cell::default();
                cell.set_symbol(&ch.to_string());
                (i as u16, 0u16, cell)
            })
            .collect();
        let buf = SharedBuf(std::rc::Rc::new(std::cell::RefCell::new(Vec::new())));
        let mut backend = HyperlinkBackend::with_policy(buf.clone(), policy);
        backend
            .draw(cells.iter().map(|(x, y, c)| (*x, *y, c)))
            .expect("draw");
        let bytes = buf.0.borrow().clone();
        String::from_utf8(bytes).expect("utf8")
    }

    /// Render a row through the `enabled`→default-policy mapping of
    /// [`HyperlinkBackend::new`].
    fn draw_row(text: &str, enabled: bool) -> String {
        let policy = if enabled {
            LinkPolicy::default()
        } else {
            LinkPolicy::NONE
        };
        draw_row_with(text, policy)
    }

    #[test]
    fn backend_wraps_url_runs_in_osc8() {
        let out = draw_row("see https://rust-lang.org now", true);
        assert!(
            out.contains("\x1b]8;;https://rust-lang.org\x1b\\"),
            "URL run should open OSC 8: {out:?}"
        );
        assert!(out.contains("\x1b]8;;\x1b\\"), "URL run should close OSC 8");
        // The link target appears exactly once as an OSC 8 target (not around
        // the surrounding words).
        assert_eq!(out.matches("\x1b]8;;https://").count(), 1);
    }

    #[test]
    fn backend_disabled_emits_no_osc8() {
        let out = draw_row("see https://rust-lang.org now", false);
        assert!(
            !out.contains("\x1b]8;;"),
            "disabled backend must not link: {out:?}"
        );
        // Text is still rendered.
        assert!(out.contains('h') && out.contains('s'));
    }

    #[test]
    fn backend_plain_row_has_no_osc8() {
        let out = draw_row("just some text", true);
        assert!(!out.contains("\x1b]8;;"));
    }

    #[test]
    fn backend_links_mailto_only_when_policy_allows() {
        let row = "mail me at mailto:a@b.com today";
        // Default (web) policy leaves the mailto as plain text.
        assert!(!draw_row_with(row, LinkPolicy::default()).contains("\x1b]8;;"));
        // With mailto opted in, the address run is wrapped in OSC 8.
        let out = draw_row_with(row, LinkPolicy::WEB.with_mailto());
        assert!(
            out.contains("\x1b]8;;mailto:a@b.com\x1b\\"),
            "mailto run should open OSC 8: {out:?}"
        );
        assert!(
            out.contains("\x1b]8;;\x1b\\"),
            "mailto run should close OSC 8"
        );
    }

    #[test]
    fn apply_buffer_links_makes_labeled_run_ctrl_clickable() {
        // Reproduction: a markdown `[label](url)` paints only the label. Without
        // carrying the destination into the buffer, Ctrl+click / Ghostty OSC 8
        // has nothing to open. apply_buffer_links embeds the target.
        use crate::{Mouse, MouseButton, MouseKind};
        use ratatui_core::style::Style;
        let area = Rect::new(2, 1, 20, 1);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 30, 4));
        buffer.set_string(area.x, area.y, "see docs here", Style::default());
        // "docs" occupies columns 6..10 (origin-relative 4..8).
        let links = [BufferLink {
            line: 0,
            start_col: 4,
            end_col: 8,
            url: "https://example.com/docs".into(),
        }];
        apply_buffer_links(
            &mut buffer,
            Position {
                x: area.x,
                y: area.y,
            },
            &links,
            LinkPolicy::WEB,
        );
        let head = buffer[(area.x + 4, area.y)].symbol();
        assert!(
            head.starts_with("\x1b]8;;https://example.com/docs\x1b\\"),
            "opener cell: {head:?}"
        );
        let mut event = Mouse::at(MouseKind::Up(MouseButton::Left), area.x + 5, area.y);
        event.ctrl = true;
        assert_eq!(
            ctrl_click_url(&event, &buffer, area).as_deref(),
            Some("https://example.com/docs")
        );
    }

    #[test]
    fn apply_buffer_links_respects_none_policy() {
        use ratatui_core::style::Style;
        let mut buffer = Buffer::empty(Rect::new(0, 0, 10, 1));
        buffer.set_string(0, 0, "docs", Style::default());
        apply_buffer_links(
            &mut buffer,
            Position { x: 0, y: 0 },
            &[BufferLink {
                line: 0,
                start_col: 0,
                end_col: 4,
                url: "https://example.com".into(),
            }],
            LinkPolicy::NONE,
        );
        assert_eq!(buffer[(0, 0)].symbol(), "d");
        assert!(!buffer[(0, 0)].symbol().contains("\x1b]8;;"));
    }

    #[test]
    fn strip_osc8_leaves_visible_label() {
        assert_eq!(
            strip_osc8("\x1b]8;;https://x.dev\x1b\\hi\x1b]8;;\x1b\\"),
            "hi"
        );
        assert_eq!(strip_osc8("plain"), "plain");
    }
}

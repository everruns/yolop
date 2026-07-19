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

use crossterm::queue;
use crossterm::style::{
    Attribute, Color as CtColor, Print, ResetColor, SetAttribute, SetBackgroundColor,
    SetForegroundColor,
};
use ratatui::backend::{Backend, ClearType, CrosstermBackend, WindowSize};
use ratatui::buffer::Cell;
use ratatui::layout::{Position, Size};
use ratatui::style::{Color, Modifier};
use ratatui::text::{Line, Span};

/// String terminator for an OSC sequence: `ESC \`.
const ST: &str = "\x1b\\";

/// Wrap `text` in an OSC 8 hyperlink to `url`, or return `text` unchanged when
/// `url` is not a valid, safe web URL. Pure and allocation-only — no I/O.
pub fn osc8(url: &str, text: &str) -> String {
    match sanitize_web_url(url) {
        Some(url) => format!("\x1b]8;;{url}{ST}{text}\x1b]8;;{ST}"),
        None => text.to_string(),
    }
}

/// Whether `s` is a bare `http(s)://` URL with no interior whitespace — the
/// shape a host can hand to [`write_line`] as a link run.
pub fn is_web_url(s: &str) -> bool {
    (s.starts_with("http://") || s.starts_with("https://")) && !s.chars().any(char::is_whitespace)
}

/// Accept only `http(s)` URLs, and strip control characters (including the
/// `ESC`/`BEL` that could terminate the OSC early and let a crafted URL break
/// out of the sequence). Returns `None` for anything else.
fn sanitize_web_url(url: &str) -> Option<String> {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return None;
    }
    let cleaned: String = url
        .chars()
        .filter(|&c| !c.is_control() && c != '\u{7f}')
        .collect();
    (!cleaned.is_empty() && cleaned.len() >= "http://".len()).then_some(cleaned)
}

/// Byte ranges of every `http(s)` URL in `s`, left to right, non-overlapping.
/// Each URL runs to the next whitespace with trailing sentence punctuation
/// trimmed, matching how the host styles links.
fn find_web_urls(s: &str) -> Vec<(usize, usize)> {
    const TRAILING: &[char] = &['.', ',', ';', ':', '!', '?', ')', ']', '}', '\'', '"'];
    let mut ranges = Vec::new();
    let mut offset = 0;
    while offset < s.len() {
        let rest = &s[offset..];
        let Some(rel) = rest
            .find("https://")
            .into_iter()
            .chain(rest.find("http://"))
            .min()
        else {
            break;
        };
        let start = offset + rel;
        let tail = &s[start..];
        let raw_end = tail.find(char::is_whitespace).unwrap_or(tail.len());
        let len = tail[..raw_end].trim_end_matches(TRAILING).len();
        if len == 0 {
            break;
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
    for span in &line.spans {
        write_span(out, span)?;
    }
    queue!(out, ResetColor, SetAttribute(Attribute::Reset))?;
    Ok(())
}

fn write_span(out: &mut impl Write, span: &Span<'_>) -> io::Result<()> {
    apply_style(out, span)?;
    let content = span.content.as_ref();
    let is_link = is_web_url(content.trim()) && content.trim() == content;
    if is_link {
        queue!(out, Print(osc8(content, content)))?;
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
/// untouched. When `enabled` is false it is a pure pass-through with no scanning,
/// so a host can gate the feature at zero cost.
pub struct HyperlinkBackend<W: Write> {
    inner: CrosstermBackend<W>,
    enabled: bool,
}

impl<W: Write> HyperlinkBackend<W> {
    /// Wrap `writer`. `enabled` turns OSC 8 emission on; when false, `draw`
    /// forwards straight to the inner backend.
    pub fn new(writer: W, enabled: bool) -> Self {
        Self {
            inner: CrosstermBackend::new(writer),
            enabled,
        }
    }

    /// Emit one maximal contiguous run of cells, wrapping any URL sub-runs in
    /// OSC 8. Reuses the inner backend's `draw` for all SGR/cursor logic so we
    /// never reimplement styling.
    fn emit_run(&mut self, run: &[(u16, u16, &Cell)]) -> io::Result<()> {
        // Reconstruct the run's visible text and remember where each cell starts
        // so a URL byte-range maps back to a cell index range.
        let mut text = String::new();
        let mut cell_starts = Vec::with_capacity(run.len());
        for (_, _, cell) in run {
            cell_starts.push(text.len());
            text.push_str(cell.symbol());
        }

        let urls = find_web_urls(&text);
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
                match sanitize_web_url(&text[byte_start..byte_end]) {
                    Some(url) => {
                        // CrosstermBackend implements Write, so raw OSC 8 bytes go
                        // straight through to its inner writer.
                        write!(self.inner, "\x1b]8;;{url}{ST}")?;
                        self.inner.draw(run[start_cell..end_cell].iter().copied())?;
                        write!(self.inner, "\x1b]8;;{ST}")?;
                    }
                    None => self.inner.draw(run[start_cell..end_cell].iter().copied())?,
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
        if !self.enabled {
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
            ratatui::style::Style::default()
                .fg(Color::Rgb(45, 91, 158))
                .add_modifier(Modifier::UNDERLINED),
        ));
        let out = bytes(&line);
        // Underline attribute (SGR 4) and a truecolor foreground are present,
        // the link is wrapped, and the line ends reset.
        assert!(out.contains("\x1b[4m"), "underline SGR expected: {out:?}");
        assert!(
            out.contains("\x1b[38;2;45;91;158m"),
            "truecolor fg expected: {out:?}"
        );
        assert!(out.contains("\x1b]8;;https://a.dev\x1b\\"));
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
    fn find_web_urls_locates_and_trims() {
        assert_eq!(find_web_urls("see https://a.dev/x, ok"), vec![(4, 19)]);
        assert_eq!(
            find_web_urls("a http://x.io b https://y.io"),
            vec![(2, 13), (16, 28)]
        );
        assert!(find_web_urls("no links").is_empty());
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

    /// Render a row of single-char cells into a `HyperlinkBackend` and return the
    /// emitted bytes.
    fn draw_row(text: &str, enabled: bool) -> String {
        use ratatui::buffer::Cell;
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
        let mut backend = HyperlinkBackend::new(buf.clone(), enabled);
        backend
            .draw(cells.iter().map(|(x, y, c)| (*x, *y, c)))
            .expect("draw");
        let bytes = buf.0.borrow().clone();
        String::from_utf8(bytes).expect("utf8")
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
}

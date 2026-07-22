//! OSC 8 terminal hyperlinks over a rendered ratatui [`Buffer`].
//!
//! ratatui's high-level `Line`/`Span` model has no hyperlink attribute, so URLs
//! were only *styled* and left to the terminal's own URL auto-detection — which
//! many terminals don't do, leaving links unclickable. ratatui-core 0.1.2 (under
//! ratatui 0.30) does let a single cell carry an arbitrary symbol string plus a
//! [`CellDiffOption::ForcedWidth`], so an OSC 8 escape can ride along in the
//! cell without throwing off column accounting.
//!
//! [`linkify_buffer`] runs *after* normal rendering: it scans each row for
//! `http(s)://…` runs and wraps each run's first and last cell with the OSC 8
//! open (`ESC ] 8 ; ; URL ESC \`) and close (`ESC ] 8 ; ; ESC \`) sequences,
//! forcing those two cells to width 1 so the hidden escapes cost no columns.
//! Terminals without OSC 8 support ignore the unknown OSC and render the visible
//! text unchanged, so this is safe to always emit.

use std::num::NonZeroU16;

use ratatui::buffer::{Buffer, CellDiffOption};
use ratatui::layout::Rect;

/// Byte offset of the next `http://` or `https://` in `s`, if any.
pub(crate) fn next_url_start(s: &str) -> Option<usize> {
    match (s.find("https://"), s.find("http://")) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(a), None) => Some(a),
        (None, b) => b,
    }
}

/// Byte length of the URL at the start of `from`: up to the next whitespace,
/// with trailing sentence punctuation trimmed so `(see https://x.dev).` links
/// just the URL.
pub(crate) fn url_end(from: &str) -> usize {
    let raw_end = from.find(char::is_whitespace).unwrap_or(from.len());
    from[..raw_end]
        .trim_end_matches(|c| {
            matches!(
                c,
                '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']' | '}' | '\'' | '"'
            )
        })
        .len()
}

/// Wrap every `http(s)://…` run visible in `area` with OSC 8 hyperlink escapes so
/// the terminal makes it clickable. Idempotent per render: it only touches the
/// two boundary cells of each run and leaves already-wrapped cells alone.
pub(crate) fn linkify_buffer(buf: &mut Buffer, area: Rect) {
    let left = area.left().max(buf.area.left());
    let right = area.right().min(buf.area.right());
    let top = area.top().max(buf.area.top());
    let bottom = area.bottom().min(buf.area.bottom());

    for y in top..bottom {
        // Reconstruct the row text and a byte→column map so a URL found in the
        // text maps back to the cells it occupies (URLs are ASCII, one cell per
        // char, but earlier cells may hold wide glyphs).
        let mut row = String::new();
        let mut col_of_byte: Vec<u16> = Vec::new();
        for x in left..right {
            let symbol = buf[(x, y)].symbol().to_string();
            for _ in symbol.bytes() {
                col_of_byte.push(x);
            }
            row.push_str(&symbol);
        }

        let mut search_from = 0;
        while let Some(rel) = next_url_start(&row[search_from..]) {
            let start = search_from + rel;
            let len = url_end(&row[start..]);
            if len == 0 {
                search_from = start + 1;
                continue;
            }
            let end = start + len; // byte-exclusive
            let url = row[start..end].to_string();
            let xs = col_of_byte[start];
            let xe = col_of_byte[end - 1];
            if xe <= xs {
                // A single-cell "URL" can't be a real link; skip it.
                search_from = end;
                continue;
            }

            // Opening sequence rides in the first cell; closing in the last.
            let head = buf[(xs, y)].symbol().to_string();
            buf[(xs, y)]
                .set_symbol(&format!("\x1b]8;;{url}\x1b\\{head}"))
                .set_diff_option(CellDiffOption::ForcedWidth(NonZeroU16::new(1).unwrap()));
            let tail = buf[(xe, y)].symbol().to_string();
            buf[(xe, y)]
                .set_symbol(&format!("{tail}\x1b]8;;\x1b\\"))
                .set_diff_option(CellDiffOption::ForcedWidth(NonZeroU16::new(1).unwrap()));

            search_from = end;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::text::Line;
    use ratatui::widgets::{Paragraph, Widget};

    fn rendered(text: &str, width: u16) -> Buffer {
        let mut buf = Buffer::empty(Rect::new(0, 0, width, 1));
        Paragraph::new(Line::from(text)).render(buf.area, &mut buf);
        buf
    }

    #[test]
    fn wraps_a_url_run_in_osc8() {
        let mut buf = rendered("see https://example.com/x now", 40);
        let area = buf.area;
        linkify_buffer(&mut buf, area);

        // The 'h' of the URL now carries the OSC 8 opener; the last URL char the
        // closer. The plain text is preserved inside the escapes.
        let head = buf[(4, 0)].symbol().to_string();
        assert!(
            head.starts_with("\x1b]8;;https://example.com/x\x1b\\") && head.ends_with('h'),
            "opener cell: {head:?}"
        );
        // "https://example.com/x" is 21 chars starting at column 4 → last at 24.
        let tail = buf[(24, 0)].symbol().to_string();
        assert!(
            tail.starts_with('x') && tail.ends_with("\x1b]8;;\x1b\\"),
            "closer cell: {tail:?}"
        );
    }

    #[test]
    fn leaves_url_free_rows_untouched() {
        let mut buf = rendered("no links here at all", 40);
        let before = buf.clone();
        let area = buf.area;
        linkify_buffer(&mut buf, area);
        assert_eq!(buf, before);
    }

    #[test]
    fn trailing_punctuation_stays_outside_the_link() {
        // "(see https://x.dev)." → the link is just the URL, ')' and '.' excluded.
        let mut buf = rendered("(see https://x.dev).", 40);
        let area = buf.area;
        linkify_buffer(&mut buf, area);
        // URL starts at column 5 ('h'); "https://x.dev" is 13 chars → last at 17.
        let tail = buf[(17, 0)].symbol().to_string();
        assert!(
            tail.starts_with('v') && tail.ends_with("\x1b]8;;\x1b\\"),
            "{tail:?}"
        );
        // The ')' at column 18 is untouched.
        assert_eq!(buf[(18, 0)].symbol(), ")");
    }
}

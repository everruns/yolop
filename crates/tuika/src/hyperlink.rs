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
}

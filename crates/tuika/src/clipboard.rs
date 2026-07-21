//! System clipboard writes via OSC 52.
//!
//! Like [`crate::hyperlink`] (OSC 8) and [`crate::native`] (OSC 9;4), this is an
//! out-of-band terminal capability: the app writes an escape sequence and the
//! terminal — not the app — puts the text on the system clipboard. It needs no
//! platform clipboard library and works over SSH, which is why it's the right
//! fit for copying a mouse selection (see [`crate::mouse`]).
//!
//! The sequence is `OSC 52 ; c ; <base64> ST`, where the payload is the text
//! base64-encoded (RFC 4648) and `c` selects the system clipboard. Terminals
//! that support OSC 52 (Ghostty, iTerm2 with it enabled, WezTerm, Kitty, recent
//! VTE, xterm) copy it; others ignore the sequence. Under tmux it requires
//! `set -g allow-passthrough on` (or tmux's own `set-clipboard`), the same
//! passthrough caveat as OSC 8 hyperlinks.
//!
//! Terminals cap an OSC 52 sequence near 100 000 bytes; encoded, that bounds
//! the copyable text at [`MAX_LEN`] bytes. Longer text is refused rather than
//! sent truncated (a truncated clipboard is worse than a failed copy).

use std::io::{self, Write};

/// Maximum length, in bytes, of text a single OSC 52 write will copy. Beyond
/// this the terminal would truncate the sequence, so [`osc52`] refuses it.
pub const MAX_LEN: usize = 74_994;

/// String terminator for the OSC sequence: `ESC \`.
const ST: &str = "\x1b\\";

/// Encode `text` as an OSC 52 clipboard-set sequence, or `None` when `text` is
/// empty or longer than [`MAX_LEN`]. Pure and allocation-only — no I/O.
pub fn osc52(text: &str) -> Option<String> {
    if text.is_empty() || text.len() > MAX_LEN {
        return None;
    }
    Some(format!("\x1b]52;c;{}{ST}", base64_encode(text.as_bytes())))
}

/// Copy `text` to the system clipboard by writing an OSC 52 sequence to `out`.
/// Returns `Ok(true)` when a sequence was written, `Ok(false)` when `text` was
/// empty or too large (see [`MAX_LEN`]).
pub fn write_clipboard(out: &mut impl Write, text: &str) -> io::Result<bool> {
    match osc52(text) {
        Some(seq) => {
            out.write_all(seq.as_bytes())?;
            out.flush()?;
            Ok(true)
        }
        None => Ok(false),
    }
}

/// Standard base64 (RFC 4648) with `=` padding. Inlined to keep `tuika` free of
/// a base64 dependency.
fn base64_encode(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[((n >> 18) & 0x3f) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 0x3f) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[((n >> 6) & 0x3f) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(n & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_known_vectors() {
        // RFC 4648 test vectors.
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn osc52_wraps_base64_payload() {
        assert_eq!(
            osc52("foobar"),
            Some("\x1b]52;c;Zm9vYmFy\x1b\\".to_string())
        );
    }

    #[test]
    fn osc52_refuses_empty_and_oversized() {
        assert_eq!(osc52(""), None);
        let big = "x".repeat(MAX_LEN + 1);
        assert_eq!(osc52(&big), None);
        // Exactly at the cap is allowed.
        assert!(osc52(&"x".repeat(MAX_LEN)).is_some());
    }

    #[test]
    fn write_clipboard_reports_whether_it_wrote() {
        let mut buf: Vec<u8> = Vec::new();
        assert!(write_clipboard(&mut buf, "hi").expect("write"));
        assert_eq!(buf, b"\x1b]52;c;aGk=\x1b\\");

        let mut empty: Vec<u8> = Vec::new();
        assert!(!write_clipboard(&mut empty, "").expect("write"));
        assert!(empty.is_empty());
    }
}

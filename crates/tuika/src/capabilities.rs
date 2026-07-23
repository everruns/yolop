//! Terminal capability detection.
//!
//! What can the terminal actually do — paint pixels, open links, copy to the
//! system clipboard, show 24-bit color? tuika answers this in two tiers, the same
//! way it splits pure logic from host I/O everywhere else:
//!
//! - [`Capabilities::from_env`] — an **instant, advisory** guess from the
//!   environment (`TERM`, `TERM_PROGRAM`, `COLORTERM`, `KITTY_WINDOW_ID`, the
//!   Ghostty marker). No terminal round-trip, so it never blocks; good enough to
//!   decide whether to *show an affordance*, but not authoritative — especially
//!   for Sixel, which has no reliable environment signal.
//! - A **Device Attributes probe** for accuracy: emit [`DA1_REQUEST`], read the
//!   reply, parse it with [`DeviceAttributes::parse`], and fold it in with
//!   [`Capabilities::with_device_attributes`] — which *confirms* Sixel (DA1
//!   feature `4`). [`Capabilities::query`] wraps that round-trip on Unix ttys.
//!
//! The `graphics` field reuses [`ImageSupport`], so image support is just
//! `caps.graphics` (or [`supports_images`](Capabilities::supports_images)). The
//! `hyperlinks` / `clipboard` / `progress` flags are **advisory only**: those
//! escapes ([`crate::hyperlink`], [`crate::clipboard`], [`crate::native`]) are
//! swallowed harmlessly by terminals that don't understand them, so you emit them
//! regardless — the flag only helps a host decide whether to render UI (a "copy"
//! hint, a link underline) the terminal can't act on. They lean conservative, so
//! a `false` means "don't assume", not "definitely unsupported".
//!
//! ```no_run
//! use std::time::Duration;
//! use tuika::Capabilities;
//!
//! // Instant, advisory (no I/O):
//! let caps = Capabilities::from_env();
//! if caps.supports_images() { /* … */ }
//!
//! // Accurate: probe the terminal once at startup, in raw mode, before the
//! // event loop reads stdin (Unix ttys only; elsewhere this is just `from_env`).
//! let caps = Capabilities::query(Duration::from_millis(100));
//! ```

use crate::image::ImageSupport;

/// The Primary Device Attributes request (`ESC [ c`). A terminal replies with
/// `ESC [ ? <codes> c`; feed that reply to [`DeviceAttributes::parse`].
pub const DA1_REQUEST: &str = "\x1b[c";

/// A snapshot of what the terminal is believed to support.
///
/// Build it with [`from_env`](Self::from_env) (advisory) or
/// [`query`](Self::query) (env + a Device Attributes probe).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Capabilities {
    /// Graphics protocol for images ([`ImageSupport::None`] if none).
    pub graphics: ImageSupport,
    /// OSC 8 hyperlinks (advisory).
    pub hyperlinks: bool,
    /// OSC 52 system-clipboard writes (advisory).
    pub clipboard: bool,
    /// OSC 9;4 native progress (advisory).
    pub progress: bool,
    /// 24-bit "true color" (advisory; from `COLORTERM` and known terminals).
    pub truecolor: bool,
}

impl Capabilities {
    /// Detect from the process environment only — instant, no terminal I/O.
    ///
    /// Advisory: reliable for Kitty/iTerm2 graphics and true color, conservative
    /// (false-leaning) for the OSC features, and weak for Sixel. Use
    /// [`query`](Self::query) when accuracy matters.
    pub fn from_env() -> Self {
        let env = |k: &str| std::env::var(k).ok();
        Self::from_env_parts(
            env("TERM").as_deref(),
            env("TERM_PROGRAM").as_deref(),
            env("COLORTERM").as_deref(),
            env("KITTY_WINDOW_ID").as_deref(),
            env("GHOSTTY_RESOURCES_DIR").as_deref(),
        )
    }

    /// The pure core of [`from_env`](Self::from_env), taking the environment
    /// explicitly so the heuristics are unit-testable.
    pub fn from_env_parts(
        term: Option<&str>,
        term_program: Option<&str>,
        colorterm: Option<&str>,
        kitty_window_id: Option<&str>,
        ghostty_resources_dir: Option<&str>,
    ) -> Self {
        let graphics =
            ImageSupport::detect_from(term, term_program, kitty_window_id, ghostty_resources_dir);
        let program = term_program.unwrap_or_default().to_ascii_lowercase();
        let term = term.unwrap_or_default();
        let colorterm = colorterm.unwrap_or_default().to_ascii_lowercase();
        let is = |p: &str| program == p;
        let term_has = |s: &str| term.contains(s);

        // A "modern" terminal: anything with a graphics protocol, plus a curated
        // set known to speak OSC 8 and OSC 52. Conservative — a terminal we can't
        // place (a bare `xterm-256color` from GNOME Terminal, say) reads as false.
        let modern = graphics != ImageSupport::None
            || is("wezterm")
            || is("ghostty")
            || is("iterm.app")
            || term_has("kitty")
            || term_has("foot")
            || term_has("contour")
            || term_has("alacritty");

        Self {
            graphics,
            hyperlinks: modern,
            clipboard: modern,
            // OSC 9;4 has a narrower support set than the OSC 8/52 features.
            progress: is("ghostty") || is("wezterm") || is("konsole") || term_has("konsole"),
            truecolor: modern || colorterm == "truecolor" || colorterm == "24bit",
        }
    }

    /// Detect from the environment, then refine with a Device Attributes probe.
    ///
    /// On a Unix tty this writes [`DA1_REQUEST`] and reads the reply (up to
    /// `timeout`), upgrading `graphics` to Sixel when the terminal reports it —
    /// the accuracy the env-only path can't reach. **Call once at startup, after
    /// entering raw mode and before the event loop reads stdin.** On non-ttys and
    /// non-Unix platforms it is exactly [`from_env`](Self::from_env).
    pub fn query(timeout: std::time::Duration) -> Self {
        let caps = Self::from_env();
        match probe_device_attributes(timeout) {
            Some(da) => caps.with_device_attributes(&da),
            None => caps,
        }
    }

    /// Fold a parsed Device Attributes reply into these capabilities: a reported
    /// Sixel feature upgrades `graphics` when the environment found no protocol.
    /// A protocol already detected from the environment (Kitty/iTerm2, which DA1
    /// does not report) is kept.
    pub fn with_device_attributes(mut self, da: &DeviceAttributes) -> Self {
        if da.sixel() && self.graphics == ImageSupport::None {
            self.graphics = ImageSupport::Sixel;
        }
        self
    }

    /// Whether any image graphics protocol is available.
    pub fn supports_images(&self) -> bool {
        self.graphics != ImageSupport::None
    }
}

/// A parsed Primary Device Attributes (DA1) reply — the terminal's feature codes.
///
/// The reply is `ESC [ ? <n> (; <n>)* c`; the numbers are VT feature codes, of
/// which `4` means Sixel graphics and `22` means ANSI color.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceAttributes {
    codes: Vec<u16>,
}

impl DeviceAttributes {
    /// Parse a DA1 reply, or `None` if `response` has no `ESC [ … c` frame with a
    /// numeric code. Ignores anything before the frame and any non-numeric
    /// parameters, so a reply mixed with other terminal chatter still parses.
    pub fn parse(response: &[u8]) -> Option<Self> {
        let start = response.windows(2).position(|w| w == b"\x1b[")? + 2;
        let rest = &response[start..];
        let rest = rest.strip_prefix(b"?").unwrap_or(rest);
        let end = rest.iter().position(|&b| b == b'c')?;
        let mut codes = Vec::new();
        for part in rest[..end].split(|&b| b == b';') {
            if let Ok(s) = std::str::from_utf8(part)
                && let Ok(n) = s.parse::<u16>()
            {
                codes.push(n);
            }
        }
        (!codes.is_empty()).then_some(Self { codes })
    }

    /// Whether the reply lists VT feature `code`.
    pub fn has(&self, code: u16) -> bool {
        self.codes.contains(&code)
    }

    /// Whether the terminal reported Sixel graphics (DA1 feature `4`).
    pub fn sixel(&self) -> bool {
        self.has(4)
    }
}

/// Write [`DA1_REQUEST`] to the terminal and read the reply, returning the parsed
/// attributes or `None` on a non-tty, a timeout, or an I/O error.
///
/// Unix only: reads raw bytes from fd 0 (which must be in raw mode) in a helper
/// thread bounded by `timeout`. A real tty answers DA1 immediately, so the read
/// completes at once; the timeout only guards exotic terminals that ignore the
/// query. Borrows fd 0 without owning it, so it never closes stdin, and reads
/// exactly up to the reply's terminating `c`, so it consumes no input beyond it.
#[cfg(unix)]
fn probe_device_attributes(timeout: std::time::Duration) -> Option<DeviceAttributes> {
    use std::fs::File;
    use std::io::{IsTerminal, Read, Write, stdin, stdout};
    use std::mem::ManuallyDrop;
    use std::os::fd::FromRawFd;
    use std::sync::mpsc;
    use std::thread;

    if !stdin().is_terminal() || !stdout().is_terminal() {
        return None;
    }
    {
        let mut out = stdout();
        out.write_all(DA1_REQUEST.as_bytes()).ok()?;
        out.flush().ok()?;
    }

    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        // Borrow fd 0 without owning it (ManuallyDrop ⇒ never closes stdin), and
        // read unbuffered so no bytes past the reply are swallowed.
        let mut input = ManuallyDrop::new(unsafe { File::from_raw_fd(0) });
        let mut buf = Vec::new();
        let mut byte = [0u8; 1];
        while buf.len() < 64 {
            match input.read(&mut byte) {
                Ok(1) => {
                    buf.push(byte[0]);
                    // The reply ends at the `c` that closes the CSI frame.
                    if byte[0] == b'c' && buf.contains(&b'[') {
                        break;
                    }
                }
                _ => break,
            }
        }
        let _ = tx.send(buf);
    });

    let buf = rx.recv_timeout(timeout).ok()?;
    DeviceAttributes::parse(&buf)
}

/// Non-Unix stub: no DA probe, so [`Capabilities::query`] is just `from_env`.
#[cfg(not(unix))]
fn probe_device_attributes(_timeout: std::time::Duration) -> Option<DeviceAttributes> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_detects_kitty_graphics_and_truecolor() {
        let c = Capabilities::from_env_parts(Some("xterm-kitty"), None, None, Some("1"), None);
        assert_eq!(c.graphics, ImageSupport::Kitty);
        assert!(c.supports_images());
        assert!(
            c.hyperlinks && c.clipboard,
            "graphics terminals speak OSC 8/52"
        );
        assert!(c.truecolor);
    }

    #[test]
    fn env_marks_ghostty_progress() {
        let c = Capabilities::from_env_parts(None, Some("ghostty"), None, None, Some("/x"));
        assert!(c.progress, "Ghostty drives OSC 9;4");
        assert!(c.hyperlinks);
    }

    #[test]
    fn env_is_conservative_for_unknown_terminals() {
        // A bare xterm-256color we can't place: no image protocol, OSC flags off,
        // but COLORTERM still reveals true color.
        let c = Capabilities::from_env_parts(
            Some("xterm-256color"),
            Some("Apple_Terminal"),
            Some("truecolor"),
            None,
            None,
        );
        assert_eq!(c.graphics, ImageSupport::None);
        assert!(!c.supports_images());
        assert!(!c.hyperlinks && !c.clipboard && !c.progress);
        assert!(c.truecolor, "COLORTERM=truecolor still detected");
    }

    #[test]
    fn env_foot_gets_sixel_and_osc() {
        let c = Capabilities::from_env_parts(Some("foot"), None, None, None, None);
        assert_eq!(c.graphics, ImageSupport::Sixel);
        assert!(c.hyperlinks && c.clipboard);
    }

    #[test]
    fn parse_reads_da1_feature_codes() {
        let da = DeviceAttributes::parse(b"\x1b[?62;1;4;22c").expect("parses");
        assert!(da.has(62) && da.has(4) && da.has(22));
        assert!(da.sixel());
    }

    #[test]
    fn parse_finds_the_frame_amid_other_bytes() {
        // A reply can arrive glued to unrelated output; the frame is still found.
        let da = DeviceAttributes::parse(b"junk\x1b[?1;2c").expect("parses");
        assert!(da.has(1) && da.has(2));
        assert!(!da.sixel());
    }

    #[test]
    fn parse_rejects_non_da_input() {
        assert!(DeviceAttributes::parse(b"no attributes here").is_none());
        assert!(
            DeviceAttributes::parse(b"\x1b[?c").is_none(),
            "no numeric codes"
        );
    }

    #[test]
    fn device_attributes_upgrade_sixel_only_when_env_found_nothing() {
        let sixel = DeviceAttributes::parse(b"\x1b[?4c").unwrap();
        // Env found nothing → DA1 Sixel report promotes graphics to Sixel.
        let none = Capabilities::from_env_parts(Some("xterm-256color"), None, None, None, None);
        assert_eq!(none.graphics, ImageSupport::None);
        assert_eq!(
            none.with_device_attributes(&sixel).graphics,
            ImageSupport::Sixel
        );
        // A Kitty terminal (DA1 never reports Kitty) is left untouched.
        let kitty = Capabilities::from_env_parts(Some("xterm-kitty"), None, None, None, None);
        let no_sixel = DeviceAttributes::parse(b"\x1b[?1;2c").unwrap();
        assert_eq!(
            kitty.with_device_attributes(&no_sixel).graphics,
            ImageSupport::Kitty
        );
    }
}

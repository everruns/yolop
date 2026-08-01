//! Terminal-variance smoke for the `tuika` full-screen renderer.
//!
//! Unit and snapshot tests render into an in-memory buffer — they never touch a
//! real terminal. This drives the actual `yolop` binary under a pseudo-terminal
//! (`portable-pty`) and asserts the terminal-facing behavior the buffer tests
//! can't see: entering/leaving the alternate screen, driving the native OSC 9;4
//! progress indicator, surviving a resize, and exiting cleanly. It exercises
//! the hidden `tuika-gallery` demo, which has no provider/network dependencies.
//!
//! This covers the *protocol* a terminal receives; genuinely terminal-specific
//! rendering (does Ghostty draw the Braille, does the taskbar light up) is the
//! manual matrix documented in `knowledge/specs/release.md`.
#![cfg(unix)]

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, NativePtySystem, PtySize, PtySystem};

const BIN: &str = env!("CARGO_BIN_EXE_yolop");

struct GalleryRun {
    /// Everything the terminal received, through clean exit. Its final vt100
    /// state is the *restored* main screen, so byte-level assertions use this
    /// but grid assertions must use `live` (see below).
    output: Vec<u8>,
    /// A snapshot of the byte stream taken while the gallery was still painting
    /// the alternate screen, i.e. before `q`. Parsing this into a vt100 screen
    /// reconstructs the live gallery grid; parsing `output` would show the blank
    /// main screen after the alt-screen is torn down on exit.
    live: Vec<u8>,
    exited_ok: bool,
    rows: u16,
    cols: u16,
}

impl GalleryRun {
    /// Reconstruct the live gallery grid: a reference terminal (vt100) applies
    /// every escape, so assertions read the resulting cell grid rather than the
    /// raw byte stream.
    fn live_screen(&self) -> vt100::Parser {
        let mut parser = vt100::Parser::new(self.rows, self.cols, 0);
        parser.process(&self.live);
        parser
    }
}

/// Spawn `yolop tuika-gallery` under a pty of the given size, optionally resize
/// mid-run, then send `q` and collect everything the terminal received.
fn run_gallery(
    rows: u16,
    cols: u16,
    resize_to: Option<(u16, u16)>,
    hyperlinks: bool,
) -> GalleryRun {
    let home = tempfile::tempdir().expect("home tempdir");
    let pty = NativePtySystem::default();
    let pair = pty
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("open pty");

    let mut cmd = CommandBuilder::new(BIN);
    cmd.arg("tuika-gallery");
    cmd.env("TERM", "xterm-256color");
    // This matrix row specifically proves RGB output. Keep the child isolated
    // from automation shells that export NO_COLOR for their own logs.
    cmd.env("COLORTERM", "truecolor");
    cmd.env_remove("NO_COLOR");
    cmd.env("HOME", home.path());
    cmd.env("XDG_CONFIG_HOME", home.path().join(".config"));
    cmd.env("XDG_DATA_HOME", home.path().join(".local/share"));
    // Opt-in OSC 8 hyperlinks (the gallery honors this like the main TUI).
    cmd.env("YOLOP_HYPERLINKS", if hyperlinks { "1" } else { "0" });

    let mut child = pair.slave.spawn_command(cmd).expect("spawn gallery");
    drop(pair.slave);

    // Reader thread accumulates all bytes until the pty closes (child exit).
    let reader = pair.master.try_clone_reader().expect("clone reader");
    let buffer = Arc::new(Mutex::new(Vec::new()));
    let reader_buffer = Arc::clone(&buffer);
    let reader_handle = thread::spawn(move || {
        let mut reader = reader;
        let mut chunk = [0u8; 8192];
        while let Ok(n) = reader.read(&mut chunk) {
            if n == 0 {
                break;
            }
            reader_buffer
                .lock()
                .expect("lock read buffer")
                .extend_from_slice(&chunk[..n]);
        }
    });

    // Let it paint a few frames.
    thread::sleep(Duration::from_millis(1500));
    let (mut live_rows, mut live_cols) = (rows, cols);
    if let Some((c, r)) = resize_to {
        pair.master
            .resize(PtySize {
                rows: r,
                cols: c,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("resize pty");
        (live_rows, live_cols) = (r, c);
        thread::sleep(Duration::from_millis(800));
    }

    // Snapshot the byte stream while the gallery is still on the alternate
    // screen, before `q` tears it down — this is what grid assertions parse.
    let live = buffer.lock().expect("lock read buffer").clone();

    // Quit.
    {
        let mut writer = pair.master.take_writer().expect("pty writer");
        writer.write_all(b"q").expect("send q");
        writer.flush().expect("flush q");
    }

    // Wait for a clean exit.
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut exited_ok = false;
    loop {
        match child.try_wait().expect("poll child") {
            Some(status) => {
                exited_ok = status.success();
                break;
            }
            None if Instant::now() > deadline => {
                let _ = child.kill();
                break;
            }
            None => thread::sleep(Duration::from_millis(100)),
        }
    }

    // The child has exited, so the reader hits EOF; join it and take the bytes.
    drop(pair.master);
    let _ = reader_handle.join();
    let output = Arc::try_unwrap(buffer)
        .expect("reader thread finished")
        .into_inner()
        .expect("read buffer");
    GalleryRun {
        output,
        live,
        exited_ok,
        rows: live_rows,
        cols: live_cols,
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// Whether `text` contains a codepoint from the U+2800 Braille block — what the
/// gallery's default (Braille) spinner draws.
fn has_braille(text: &str) -> bool {
    text.chars().any(|c| ('\u{2800}'..='\u{28ff}').contains(&c))
}

/// Whether any cell in the parsed screen is painted with a 24-bit RGB color
/// (foreground or background) — proof the truecolor theme survived a real
/// terminal parser, not just that RGB bytes appeared in the stream.
fn has_rgb_cell(screen: &vt100::Screen) -> bool {
    let (rows, cols) = screen.size();
    (0..rows).any(|r| {
        (0..cols).any(|c| {
            screen.cell(r, c).is_some_and(|cell| {
                matches!(cell.fgcolor(), vt100::Color::Rgb(..))
                    || matches!(cell.bgcolor(), vt100::Color::Rgb(..))
            })
        })
    })
}

/// Strip CSI sequences so we can look for rendered text.
fn visible_text(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let mut out = String::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            // Skip an escape sequence: ESC [ ... <final>, or ESC ] ... BEL.
            match chars.next() {
                Some('[') => {
                    for e in chars.by_ref() {
                        if e.is_ascii_alphabetic() || e == '~' {
                            break;
                        }
                    }
                }
                Some(']') => {
                    for e in chars.by_ref() {
                        if e == '\u{7}' {
                            break;
                        }
                    }
                }
                _ => {}
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// The URL rendered in the gallery footer, emitted as an OSC 8 hyperlink target
/// when `YOLOP_HYPERLINKS` is enabled.
const GALLERY_URL: &str = "https://github.com/everruns/yolop";

#[test]
fn gallery_drives_altscreen_and_native_progress() {
    let run = run_gallery(24, 80, None, false);
    let out = &run.output;

    assert!(
        run.exited_ok,
        "gallery should exit cleanly on `q`: {}",
        visible_text(out)
    );
    assert!(
        contains(out, b"\x1b[?1049h"),
        "should enter the alternate screen"
    );
    assert!(
        contains(out, b"\x1b[?1049l"),
        "should restore the main screen on exit"
    );
    assert!(contains(out, b"\x1b[?25l"), "should hide the cursor");
    assert!(
        contains(out, b"\x1b[?25h"),
        "should restore the cursor on exit"
    );
    assert!(contains(out, b"\x1b[?1000h"), "should enable mouse capture");
    assert!(
        contains(out, b"\x1b[?1000l"),
        "should disable mouse capture on exit"
    );
    // OSC 9;4: indeterminate on start, cleared on exit.
    assert!(
        contains(out, b"\x1b]9;4;3"),
        "should emit the native indeterminate progress sequence"
    );
    assert!(
        contains(out, b"\x1b]9;4;0"),
        "should clear the native progress indicator on exit"
    );

    let text = visible_text(out);
    assert!(
        text.contains("spinners") && text.contains("progress"),
        "gallery chrome should render: {text:?}"
    );
}

#[test]
fn gallery_paints_truecolor_and_braille() {
    let run = run_gallery(24, 80, None, false);
    assert!(
        run.exited_ok,
        "gallery should exit cleanly: {}",
        visible_text(&run.output)
    );

    // Assert against the parsed grid — a reference terminal (vt100) applies
    // every escape — so these cover the matrix rows at the cell level, not just
    // as a substring in the byte stream a real emulator might interpret away.
    let parser = run.live_screen();
    let screen = parser.screen();

    // Truecolor: the default theme is 24-bit RGB, so at least one painted cell
    // must carry an RGB color after parsing — the "truecolor" matrix row.
    assert!(
        has_rgb_cell(screen),
        "expected a 24-bit truecolor cell from the RGB theme:\n{}",
        screen.contents()
    );

    // Braille: the default spinner draws from the U+2800 block, so a Braille
    // glyph must land in a grid cell — the "Braille glyphs" matrix row.
    assert!(
        has_braille(&screen.contents()),
        "expected Braille spinner glyphs on screen:\n{}",
        screen.contents()
    );
}

#[test]
fn gallery_survives_resize() {
    // Start wide, shrink to a small size mid-run.
    let run = run_gallery(24, 100, Some((40, 12)), false);
    assert!(
        run.exited_ok,
        "gallery should survive a resize and exit cleanly: {}",
        visible_text(&run.output)
    );
    assert!(
        contains(&run.output, b"\x1b[?1049l"),
        "should still restore the screen after a resize"
    );
}

#[test]
fn gallery_emits_osc8_hyperlink_when_enabled() {
    let run = run_gallery(24, 80, None, true);
    assert!(run.exited_ok, "gallery should exit cleanly");
    // With hyperlinks on, the footer URL is wrapped in an OSC 8 sequence
    // (`ESC ] 8 ; ; <url> ST`) targeting itself — proven end-to-end through the
    // real binary + HyperlinkBackend, not just the unit tests.
    let osc8 = format!("\x1b]8;;{GALLERY_URL}\x1b\\");
    assert!(
        contains(&run.output, osc8.as_bytes()),
        "expected OSC 8 hyperlink for {GALLERY_URL} when YOLOP_HYPERLINKS=1"
    );

    // vt100 does not expose OSC 8 targets, but it does prove the wrapping left
    // the surrounding footer text intact — the "surrounding text/wrapping
    // undamaged" concern in the OSC 8 matrix row. The footer reads
    // `docs <url>  ·  …`, so both the label and the full URL survive on one row.
    let parser = run.live_screen();
    let footer = parser
        .screen()
        .contents()
        .lines()
        .find(|line| line.contains(GALLERY_URL))
        .map(ToOwned::to_owned)
        .unwrap_or_default();
    assert!(
        footer.contains("docs") && footer.contains(GALLERY_URL),
        "OSC 8 wrapping should leave the footer text intact: {footer:?}"
    );
}

#[test]
fn gallery_omits_osc8_hyperlink_when_disabled() {
    let run = run_gallery(24, 80, None, false);
    assert!(run.exited_ok, "gallery should exit cleanly");
    // Default (disabled) backend is a pure pass-through: the URL still renders
    // as visible text, but no OSC 8 escape is emitted.
    assert!(
        !contains(&run.output, b"\x1b]8;;"),
        "no OSC 8 hyperlink should be emitted when hyperlinks are disabled"
    );
    let text = visible_text(&run.output);
    assert!(
        text.contains(GALLERY_URL),
        "the URL should still be visible as text: {text:?}"
    );
}

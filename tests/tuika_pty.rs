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
//! manual matrix documented in `src/tuika/README.md`.
#![cfg(unix)]

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, NativePtySystem, PtySize, PtySystem};

const BIN: &str = env!("CARGO_BIN_EXE_yolop");

struct GalleryRun {
    output: Vec<u8>,
    exited_ok: bool,
}

/// Spawn `yolop tuika-gallery` under a pty of the given size, optionally resize
/// mid-run, then send `q` and collect everything the terminal received.
fn run_gallery(rows: u16, cols: u16, resize_to: Option<(u16, u16)>) -> GalleryRun {
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
    cmd.env("HOME", home.path());
    cmd.env("XDG_CONFIG_HOME", home.path().join(".config"));
    cmd.env("XDG_DATA_HOME", home.path().join(".local/share"));

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
    if let Some((c, r)) = resize_to {
        pair.master
            .resize(PtySize {
                rows: r,
                cols: c,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("resize pty");
        thread::sleep(Duration::from_millis(800));
    }

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
    GalleryRun { output, exited_ok }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
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

#[test]
fn gallery_drives_altscreen_and_native_progress() {
    let run = run_gallery(24, 80, None);
    let out = &run.output;

    assert!(run.exited_ok, "gallery should exit cleanly on `q`");
    assert!(
        contains(out, b"\x1b[?1049h"),
        "should enter the alternate screen"
    );
    assert!(
        contains(out, b"\x1b[?1049l"),
        "should restore the main screen on exit"
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
fn gallery_survives_resize() {
    // Start wide, shrink to a small size mid-run.
    let run = run_gallery(24, 100, Some((40, 12)));
    assert!(
        run.exited_ok,
        "gallery should survive a resize and exit cleanly"
    );
    assert!(
        contains(&run.output, b"\x1b[?1049l"),
        "should still restore the screen after a resize"
    );
}

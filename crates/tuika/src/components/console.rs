//! [`ConsoleLog`] + [`Console`] — capture stdout/log output into a ring buffer
//! and show it in a toggleable overlay.
//!
//! Under an alternate screen there is nowhere for `println!`/`tracing` output to
//! go. A [`ConsoleLog`] is a cheap, cloneable handle over a shared, capped ring
//! of lines that also implements [`std::io::Write`], so it drops straight into a
//! logging pipeline — `tracing_subscriber::fmt().with_writer(move || log.clone())`,
//! or any `writeln!(log.clone(), …)`. The handle is `Send`/`Sync` (an
//! `Arc<Mutex<…>>` inside), so background threads can log into it while the render
//! thread paints a [`Console`] tail of the most recent lines, usually in an
//! [`OverlaySpec`](crate::OverlaySpec) the host toggles with a hotkey — the
//! console-overlay analog of OpenTUI's captured console.

use std::collections::VecDeque;
use std::io::{self, Write};
use std::sync::{Arc, Mutex};

use ratatui_core::layout::Rect;
use ratatui_core::style::{Modifier, Style};

use crate::geometry::Size;
use crate::surface::Surface;
use crate::view::{RenderCtx, View};

/// Default number of lines a [`ConsoleLog`] retains.
pub const DEFAULT_CAPACITY: usize = 500;

struct Inner {
    lines: VecDeque<String>,
    capacity: usize,
    /// Bytes written since the last newline, not yet a complete line.
    pending: String,
}

/// A shared, capped ring of captured log lines and an [`io::Write`] sink for it.
///
/// Clone freely — every clone points at the same buffer. Writes are split on
/// `\n`; a trailing partial line is held until its newline arrives.
#[derive(Clone)]
pub struct ConsoleLog {
    inner: Arc<Mutex<Inner>>,
}

impl ConsoleLog {
    /// A log retaining the most recent `capacity` lines (`0` is treated as `1`).
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                lines: VecDeque::new(),
                capacity: capacity.max(1),
                pending: String::new(),
            })),
        }
    }

    /// Append a complete line (any embedded newlines split it further).
    pub fn line(&self, text: impl AsRef<str>) {
        let mut inner = self.lock();
        for (i, part) in text.as_ref().split('\n').enumerate() {
            if i > 0 {
                Self::commit_pending(&mut inner);
            }
            inner.pending.push_str(part);
        }
        Self::commit_pending(&mut inner);
    }

    /// Remove every captured line.
    pub fn clear(&self) {
        let mut inner = self.lock();
        inner.lines.clear();
        inner.pending.clear();
    }

    /// Number of complete captured lines.
    pub fn len(&self) -> usize {
        self.lock().lines.len()
    }

    /// Whether no complete lines have been captured.
    pub fn is_empty(&self) -> bool {
        self.lock().lines.is_empty()
    }

    /// A snapshot of the captured lines, oldest first.
    pub fn snapshot(&self) -> Vec<String> {
        self.lock().lines.iter().cloned().collect()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        // A poisoned lock only means a prior logger panicked mid-write; the ring
        // is still consistent, so recover the guard rather than propagate.
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Push `pending` as a finished line (trimming a trailing `\r`) and enforce
    /// the capacity.
    fn commit_pending(inner: &mut Inner) {
        let mut line = std::mem::take(&mut inner.pending);
        if line.ends_with('\r') {
            line.pop();
        }
        inner.lines.push_back(line);
        while inner.lines.len() > inner.capacity {
            inner.lines.pop_front();
        }
    }

    fn write_bytes(&self, buf: &[u8]) {
        // Lossy is fine: captured console output is for display, not round-trip.
        let text = String::from_utf8_lossy(buf);
        let mut inner = self.lock();
        let mut parts = text.split('\n').peekable();
        while let Some(part) = parts.next() {
            inner.pending.push_str(part);
            if parts.peek().is_some() {
                Self::commit_pending(&mut inner);
            }
        }
    }
}

impl Default for ConsoleLog {
    fn default() -> Self {
        Self::new(DEFAULT_CAPACITY)
    }
}

impl Write for ConsoleLog {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.write_bytes(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

// Allow `writeln!(&log, …)` without cloning, mirroring `impl Write for &File`.
impl Write for &ConsoleLog {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.write_bytes(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A tailing view of a [`ConsoleLog`]: the most recent lines that fit the area,
/// bottom-aligned like a terminal scrollback, over the theme surface.
pub struct Console<'a> {
    log: &'a ConsoleLog,
    title: Option<String>,
}

impl<'a> Console<'a> {
    /// A console view over `log`.
    pub fn new(log: &'a ConsoleLog) -> Self {
        Self { log, title: None }
    }

    /// Add a one-row header (e.g. `" console "`) above the tail.
    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }
}

impl View for Console<'_> {
    fn measure(&self, available: Size) -> Size {
        available
    }

    fn render(&self, area: Rect, surface: &mut Surface, ctx: &RenderCtx) {
        if area.is_empty() {
            return;
        }
        let fill = Style::default().fg(ctx.theme.text).bg(ctx.theme.surface);
        surface.fill(fill);

        let mut body = area;
        if let Some(title) = &self.title {
            surface.set_string(
                area.x,
                area.y,
                title,
                Style::default()
                    .fg(ctx.theme.accent)
                    .bg(ctx.theme.surface)
                    .add_modifier(Modifier::BOLD),
            );
            body = Rect::new(
                area.x,
                area.y + 1,
                area.width,
                area.height.saturating_sub(1),
            );
        }
        if body.is_empty() {
            return;
        }

        let lines = self.log.snapshot();
        // Show the tail: the last `body.height` lines, bottom-aligned.
        let visible = body.height as usize;
        let start = lines.len().saturating_sub(visible);
        for (row, line) in lines[start..].iter().enumerate() {
            let y = body.y + row as u16;
            surface.set_string(body.x, y, line, fill);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::style::Theme;
    use crate::test_support::row;

    #[test]
    fn line_capture_and_capacity() {
        let log = ConsoleLog::new(2);
        log.line("one");
        log.line("two");
        log.line("three");
        // Oldest evicted; newest retained.
        assert_eq!(log.snapshot(), vec!["two".to_string(), "three".to_string()]);
        assert_eq!(log.len(), 2);
    }

    #[test]
    fn write_splits_on_newlines_and_holds_partials() {
        let log = ConsoleLog::new(10);
        let mut w = log.clone();
        write!(w, "hel").unwrap();
        write!(w, "lo\nwor").unwrap(); // completes "hello", starts "wor"
        assert_eq!(log.snapshot(), vec!["hello".to_string()]);
        writeln!(w, "ld").unwrap(); // completes "world"
        assert_eq!(
            log.snapshot(),
            vec!["hello".to_string(), "world".to_string()]
        );
        // A trailing CR from CRLF output is trimmed.
        writeln!(w, "crlf\r").unwrap();
        assert_eq!(log.snapshot().last().unwrap(), "crlf");
    }

    #[test]
    fn shared_handles_write_to_the_same_ring() {
        let log = ConsoleLog::new(10);
        let a = log.clone();
        a.line("from clone");
        assert_eq!(log.len(), 1);
        log.clear();
        assert!(log.is_empty());
    }

    #[test]
    fn console_tails_and_titles() {
        let theme = Theme::default();
        let log = ConsoleLog::new(100);
        for i in 0..10 {
            log.line(format!("line{i}"));
        }
        // A 3-row body (4 rows minus the title) shows the last three lines.
        let view = Console::new(&log).title(" console ");
        let buf = crate::testing::render(&view, 20, 4, &theme);
        assert!(row(&buf, 0).contains("console"), "title row");
        assert!(row(&buf, 1).contains("line7"));
        assert!(row(&buf, 3).contains("line9"), "newest at the bottom");
    }

    #[test]
    fn console_empty_and_tiny_do_not_panic() {
        let theme = Theme::default();
        let log = ConsoleLog::new(10);
        for (w, h) in [(0u16, 0u16), (1, 1), (5, 1)] {
            let _ = crate::testing::render(&Console::new(&log).title("x"), w, h, &theme);
        }
    }
}

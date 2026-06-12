use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use portable_pty::{
    Child, CommandBuilder, ExitStatus, MasterPty, NativePtySystem, PtySize, PtySystem,
};

pub struct TuiHarness {
    pub child: Box<dyn Child + Send + Sync>,
    master: Box<dyn MasterPty + Send>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    output_rx: Receiver<Vec<u8>>,
    output: Vec<u8>,
    answer_cursor_queries: Arc<AtomicBool>,
    _session_dir: tempfile::TempDir,
    _home: tempfile::TempDir,
}

#[derive(Clone, Copy)]
pub struct TuiSpawnOptions {
    pub rows: u16,
    pub cols: u16,
    pub cursor_row: u16,
    /// How many `CSI 6n` cursor-position queries the harness answers before
    /// going silent, emulating an emulator that stops replying mid-session.
    /// `usize::MAX` (the default) answers every query like a real terminal.
    pub cursor_reply_budget: usize,
}

impl Default for TuiSpawnOptions {
    fn default() -> Self {
        Self {
            rows: 24,
            cols: 80,
            cursor_row: 1,
            cursor_reply_budget: usize::MAX,
        }
    }
}

impl TuiHarness {
    pub fn write_input(&mut self, bytes: &[u8]) {
        let mut writer = self.writer.lock().expect("lock pty writer");
        writer.write_all(bytes).expect("write pty input");
        writer.flush().expect("flush pty input");
    }

    /// Pause or resume the harness answering `CSI 6n` cursor-position
    /// queries. Pausing emulates a busy terminal emulator that stops
    /// replying (see `tui_survives_slow_cursor_position_reply_after_resize`).
    pub fn set_answer_cursor_queries(&self, answer: bool) {
        self.answer_cursor_queries.store(answer, Ordering::SeqCst);
    }

    /// The settings.toml the spawned TUI reads and writes.
    pub fn settings_path(&self) -> PathBuf {
        #[cfg(target_os = "macos")]
        {
            self._home
                .path()
                .join("Library/Application Support/yolop/settings.toml")
        }

        #[cfg(not(target_os = "macos"))]
        self._home.path().join(".config/yolop/settings.toml")
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("resize pty");
    }

    pub fn wait_for_output(&mut self, needle: &str, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            self.drain_output();
            if self.output_text().contains(needle) {
                return true;
            }
            thread::sleep(Duration::from_millis(20));
        }
        self.drain_output();
        self.output_text().contains(needle)
    }

    pub fn wait_or_kill(&mut self, timeout: Duration) -> ExitStatus {
        if let Some(status) = wait_for_exit(&mut *self.child, timeout) {
            return status;
        }
        let _ = self.child.kill();
        panic!(
            "TUI did not exit within {:?}: {}",
            timeout,
            self.output_text()
        );
    }

    pub fn output_text(&mut self) -> String {
        self.drain_output();
        String::from_utf8_lossy(&self.output).into_owned()
    }

    fn drain_output(&mut self) {
        while let Ok(chunk) = self.output_rx.try_recv() {
            self.output.extend_from_slice(&chunk);
        }
    }
}

impl Drop for TuiHarness {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
        }
    }
}

pub fn spawn_tui_llmsim(binary: &PathBuf) -> TuiHarness {
    spawn_tui_llmsim_with(binary, TuiSpawnOptions::default())
}

pub fn spawn_tui_llmsim_with(binary: &PathBuf, options: TuiSpawnOptions) -> TuiHarness {
    spawn_tui_llmsim_with_settings(binary, options, "provider = \"llmsim\"\n")
}

pub fn spawn_tui_llmsim_with_settings(
    binary: &PathBuf,
    options: TuiSpawnOptions,
    settings_toml: &str,
) -> TuiHarness {
    let session_dir = tempfile::tempdir().expect("session tempdir");
    let home = tempfile::tempdir().expect("home tempdir");
    for settings_dir in [
        home.path().join(".config/yolop"),
        home.path().join("Library/Application Support/yolop"),
    ] {
        std::fs::create_dir_all(&settings_dir).expect("create settings dir");
        std::fs::write(settings_dir.join("settings.toml"), settings_toml).expect("write settings");
    }
    let pty_system = NativePtySystem::default();
    let pair = pty_system
        .openpty(PtySize {
            rows: options.rows,
            cols: options.cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("open pty");

    let mut cmd = CommandBuilder::new(binary);
    cmd.args(["--provider", "llmsim", "--session-dir"]);
    cmd.arg(session_dir.path());
    cmd.env("HOME", home.path());
    cmd.env("XDG_CONFIG_HOME", home.path().join(".config"));
    cmd.env("XDG_DATA_HOME", home.path().join(".local/share"));
    cmd.env("TERM", "xterm-256color");
    cmd.env_remove("OPENAI_API_KEY");
    cmd.env_remove("ANTHROPIC_API_KEY");
    cmd.env_remove("OPENROUTER_API_KEY");
    cmd.env_remove("GEMINI_API_KEY");
    cmd.env_remove("GOOGLE_API_KEY");
    cmd.env_remove("OLLAMA_BASE_URL");
    cmd.env_remove("OLLAMA_API_KEY");
    cmd.env_remove("CUSTOM_BASE_URL");
    cmd.env_remove("CUSTOM_API_KEY");
    cmd.env_remove("EVERRUNS_CLI_MODEL");
    cmd.env_remove("EVERRUNS_CLI_REASONING_EFFORT");

    let child = pair.slave.spawn_command(cmd).expect("spawn yolop TUI");
    drop(pair.slave);

    let writer = Arc::new(Mutex::new(
        pair.master.take_writer().expect("take pty writer"),
    ));
    let answer_cursor_queries = Arc::new(AtomicBool::new(true));

    // Real terminals answer every `CSI 6n` cursor-position query; ratatui's
    // inline viewport issues them at init, inside `insert_before` (via
    // `Terminal::clear`), and on resize. The reader thread plays terminal:
    // it scans the output stream for queries and replies with the configured
    // cursor row, unless paused or out of budget.
    let (output_tx, output_rx) = mpsc::channel();
    let mut reader = pair.master.try_clone_reader().expect("clone pty reader");
    let responder_writer = Arc::clone(&writer);
    let responder_enabled = Arc::clone(&answer_cursor_queries);
    let cursor_row = options.cursor_row.clamp(1, options.rows.max(1));
    let reply_budget = AtomicUsize::new(options.cursor_reply_budget);
    thread::spawn(move || {
        const QUERY: &[u8] = b"\x1b[6n";
        let mut buf = [0_u8; 4096];
        // Carry the unmatched tail across reads so a query split between
        // chunks is still recognized.
        let mut pending: Vec<u8> = Vec::new();
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0 {
                break;
            }
            pending.extend_from_slice(&buf[..n]);
            let mut queries = 0;
            while let Some(at) = find_subsequence(&pending, QUERY) {
                pending.drain(..at + QUERY.len());
                queries += 1;
            }
            let keep = pending.len().min(QUERY.len() - 1);
            let tail: Vec<u8> = pending[pending.len() - keep..].to_vec();
            pending = tail;
            for _ in 0..queries {
                if !responder_enabled.load(Ordering::SeqCst) {
                    continue;
                }
                if reply_budget
                    .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |budget| {
                        (budget > 0).then(|| budget.saturating_sub(1))
                    })
                    .is_err()
                {
                    continue;
                }
                let mut writer = responder_writer.lock().expect("lock pty writer");
                let _ = writer.write_all(format!("\x1b[{cursor_row};1R").as_bytes());
                let _ = writer.flush();
            }
            if output_tx.send(buf[..n].to_vec()).is_err() {
                break;
            }
        }
    });

    TuiHarness {
        child,
        master: pair.master,
        writer,
        output_rx,
        output: Vec::new(),
        answer_cursor_queries,
        _session_dir: session_dir,
        _home: home,
    }
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

pub fn wait_for_exit(child: &mut dyn Child, timeout: Duration) -> Option<ExitStatus> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().expect("poll child exit") {
            return Some(status);
        }
        thread::sleep(Duration::from_millis(20));
    }
    child.try_wait().expect("poll child exit")
}

pub fn assert_cursor_near_bottom(tui: &mut TuiHarness, rows: u16) {
    let output = tui.output_text();
    let cursor_row = max_absolute_cursor_row(output.as_bytes())
        .unwrap_or_else(|| panic!("missing cursor move in TUI output: {output}"));
    let minimum_row = rows.saturating_sub(2).max(1);
    assert!(
        cursor_row >= minimum_row,
        "composer cursor should be near terminal bottom (row {cursor_row}, expected >= {minimum_row}): {output}"
    );
}

pub fn max_absolute_cursor_row(output: &[u8]) -> Option<u16> {
    let mut row = None;
    let mut i = 0;
    while i + 2 < output.len() {
        if output[i] != 0x1b || output[i + 1] != b'[' {
            i += 1;
            continue;
        }
        let start = i + 2;
        let mut end = start;
        while end < output.len() && !matches!(output[end], 0x40..=0x7e) {
            end += 1;
        }
        if end == output.len() {
            break;
        }
        let final_byte = output[end];
        if matches!(final_byte, b'H' | b'f') {
            let params = std::str::from_utf8(&output[start..end]).ok()?;
            if !params.starts_with('?') {
                let parsed = params
                    .split(';')
                    .next()
                    .filter(|value| !value.is_empty())
                    .unwrap_or("1")
                    .parse::<u16>()
                    .ok();
                if let Some(parsed) = parsed {
                    row = Some(row.map_or(parsed, |current: u16| current.max(parsed)));
                }
            }
        }
        i = end + 1;
    }
    row
}

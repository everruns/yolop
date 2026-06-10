use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use portable_pty::{
    Child, CommandBuilder, ExitStatus, MasterPty, NativePtySystem, PtySize, PtySystem,
};

pub struct TuiHarness {
    pub child: Box<dyn Child + Send + Sync>,
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    output_rx: Receiver<Vec<u8>>,
    output: Vec<u8>,
    _session_dir: tempfile::TempDir,
    _home: tempfile::TempDir,
}

#[derive(Clone, Copy)]
pub struct TuiSpawnOptions {
    pub rows: u16,
    pub cols: u16,
    pub cursor_row: u16,
}

impl Default for TuiSpawnOptions {
    fn default() -> Self {
        Self {
            rows: 24,
            cols: 80,
            cursor_row: 1,
        }
    }
}

impl TuiHarness {
    pub fn write_input(&mut self, bytes: &[u8]) {
        self.writer.write_all(bytes).expect("write pty input");
        self.writer.flush().expect("flush pty input");
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
    let session_dir = tempfile::tempdir().expect("session tempdir");
    let home = tempfile::tempdir().expect("home tempdir");
    for settings_dir in [
        home.path().join(".config/yolop"),
        home.path().join("Library/Application Support/yolop"),
    ] {
        std::fs::create_dir_all(&settings_dir).expect("create settings dir");
        std::fs::write(
            settings_dir.join("settings.toml"),
            "provider = \"llmsim\"\n",
        )
        .expect("write settings");
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
    cmd.env_remove("OLLAMA_BASE_URL");
    cmd.env_remove("OLLAMA_API_KEY");

    let child = pair.slave.spawn_command(cmd).expect("spawn yolop TUI");
    drop(pair.slave);

    let (output_tx, output_rx) = mpsc::channel();
    let mut reader = pair.master.try_clone_reader().expect("clone pty reader");
    thread::spawn(move || {
        let mut buf = [0_u8; 4096];
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0 {
                break;
            }
            if output_tx.send(buf[..n].to_vec()).is_err() {
                break;
            }
        }
    });

    let mut writer = pair.master.take_writer().expect("take pty writer");
    let cursor_row = options.cursor_row.clamp(1, options.rows.max(1));
    writer
        .write_all(format!("\x1b[{cursor_row};1R").as_bytes())
        .expect("seed cursor position response");
    writer.flush().expect("flush cursor position response");
    TuiHarness {
        child,
        master: pair.master,
        writer,
        output_rx,
        output: Vec::new(),
        _session_dir: session_dir,
        _home: home,
    }
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

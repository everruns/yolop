// Integration tests for the yolop binary.
//
// The unignored tests exercise the offline `llmsim` provider so CI can prove
// the binary still launches and the agent loop wires up correctly without any
// API key.
//
// The `#[ignore]`-marked test reaches a real OpenAI endpoint and is meant to
// be run under Doppler in CI's live-smoke job:
//
//     doppler run -- cargo test --test integration -- --ignored

use std::path::PathBuf;
use std::process::Command;
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};
use std::{io::Read, io::Write};

use portable_pty::{Child, CommandBuilder, ExitStatus, NativePtySystem, PtySize, PtySystem};

fn yolop_binary() -> PathBuf {
    // CARGO_BIN_EXE_<name> is set by Cargo for integration tests.
    PathBuf::from(env!("CARGO_BIN_EXE_yolop"))
}

#[test]
fn help_flag_succeeds() {
    let output = Command::new(yolop_binary())
        .arg("--help")
        .output()
        .expect("spawn yolop --help");
    assert!(
        output.status.success(),
        "yolop --help failed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("yolop"), "help output missing binary name");
    assert!(
        stdout.contains("--provider"),
        "help output missing --provider"
    );
    assert!(stdout.contains("--print"), "help output missing --print");
}

#[test]
fn version_flag_succeeds() {
    let output = Command::new(yolop_binary())
        .arg("--version")
        .output()
        .expect("spawn yolop --version");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_version_output(&stdout);
}

#[test]
fn version_command_succeeds() {
    let output = Command::new(yolop_binary())
        .arg("version")
        .output()
        .expect("spawn yolop version");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_version_output(&stdout);
}

fn assert_version_output(stdout: &str) {
    assert!(
        stdout.contains("yolop"),
        "version output missing binary name: {stdout}"
    );
    assert!(
        stdout.contains(env!("CARGO_PKG_VERSION")),
        "version output missing package version: {stdout}"
    );
    assert!(
        stdout.contains("commit "),
        "version output missing commit SHA: {stdout}"
    );
    assert!(
        stdout.contains("everruns-runtime "),
        "version output missing runtime version: {stdout}"
    );
}

#[test]
fn into_zed_command_writes_acp_settings() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let settings = tmp.path().join("zed/settings.json");

    let output = Command::new(yolop_binary())
        .args([
            "into",
            "zed",
            "--settings",
            settings.to_str().unwrap(),
            "--command",
            "/tmp/yolop",
        ])
        .output()
        .expect("spawn yolop into zed");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "into zed failed: stdout={stdout} stderr={stderr}"
    );
    assert!(
        stdout.contains("added `yolop` ACP agent"),
        "unexpected into stdout: {stdout}"
    );
    let value: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(settings).expect("settings")).unwrap();
    assert_eq!(value["agent_servers"]["yolop"]["type"], "custom");
    assert_eq!(value["agent_servers"]["yolop"]["command"], "/tmp/yolop");
    assert_eq!(
        value["agent_servers"]["yolop"]["args"],
        serde_json::json!(["--acp"])
    );
}

#[test]
fn llmsim_print_smoke() {
    // The llmsim provider needs no API key and returns deterministic output.
    // We point --session-dir at a temp dir so the test never touches the
    // user's real ~/.local/share/yolop.
    let tmp = tempfile::tempdir().expect("tempdir");
    let output = Command::new(yolop_binary())
        .args([
            "--provider",
            "llmsim",
            "--session-dir",
            tmp.path().to_str().unwrap(),
            "-p",
            "hi",
        ])
        .env_remove("OPENAI_API_KEY")
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("OPENROUTER_API_KEY")
        .env_remove("OLLAMA_BASE_URL")
        .env_remove("OLLAMA_API_KEY")
        .output()
        .expect("spawn yolop --provider llmsim -p hi");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "yolop llmsim run failed: stdout={stdout} stderr={stderr}"
    );
    // The print driver always emits a `done success=...` summary line.
    assert!(
        stdout.contains("done") && stdout.contains("success="),
        "missing done summary line: {stdout}"
    );
    // Session line should mention the llmsim model so we know the provider
    // wiring picked the offline driver.
    assert!(
        stdout.contains("llmsim"),
        "expected llmsim in stdout: {stdout}"
    );
}

#[test]
fn llmsim_resume_replays_prior_events() {
    // Two-shot test: the first invocation starts a fresh session and writes a
    // JSONL log; the second invocation resumes that session via `--session <id>`
    // and must replay the prior events (startup line reports "N prior event(s)"
    // with N > 0). Proves the session_dir + session id wiring round-trips.
    let tmp = tempfile::tempdir().expect("tempdir");
    let session_dir = tmp.path().to_str().unwrap();

    let first = Command::new(yolop_binary())
        .args([
            "--provider",
            "llmsim",
            "--session-dir",
            session_dir,
            "-p",
            "hi",
        ])
        .env_remove("OPENAI_API_KEY")
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("OPENROUTER_API_KEY")
        .env_remove("OLLAMA_BASE_URL")
        .env_remove("OLLAMA_API_KEY")
        .output()
        .expect("spawn first yolop run");
    let first_stdout = String::from_utf8_lossy(&first.stdout).to_string();
    let first_stderr = String::from_utf8_lossy(&first.stderr).to_string();
    assert!(
        first.status.success(),
        "first run failed: stdout={first_stdout} stderr={first_stderr}"
    );
    let session_id = extract_session_id(&first_stdout)
        .unwrap_or_else(|| panic!("could not find session id in stdout: {first_stdout}"));
    assert!(
        first_stdout.contains("0 prior event(s)"),
        "first run should start with no replayed events: {first_stdout}"
    );

    let second = Command::new(yolop_binary())
        .args([
            "--provider",
            "llmsim",
            "--session-dir",
            session_dir,
            "--session",
            &session_id,
            "-p",
            "second turn",
        ])
        .env_remove("OPENAI_API_KEY")
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("OPENROUTER_API_KEY")
        .env_remove("OLLAMA_BASE_URL")
        .env_remove("OLLAMA_API_KEY")
        .output()
        .expect("spawn resume yolop run");
    let second_stdout = String::from_utf8_lossy(&second.stdout).to_string();
    let second_stderr = String::from_utf8_lossy(&second.stderr).to_string();
    assert!(
        second.status.success(),
        "resume run failed: stdout={second_stdout} stderr={second_stderr}"
    );
    // Resume should reuse the same session id and report a non-zero replay count.
    assert!(
        second_stdout.contains(&session_id),
        "resume stdout should mention reused session id {session_id}: {second_stdout}"
    );
    let prior = parse_prior_events(&second_stdout)
        .unwrap_or_else(|| panic!("could not find prior event count in stdout: {second_stdout}"));
    assert!(
        prior > 0,
        "resume run must replay >0 events, got {prior}: {second_stdout}"
    );
}

#[test]
fn llmsim_unknown_session_id_is_invalid() {
    // A malformed `--session` value should fail at parse time with a clear
    // error, not crash later in the runtime layer.
    let tmp = tempfile::tempdir().expect("tempdir");
    let output = Command::new(yolop_binary())
        .args([
            "--provider",
            "llmsim",
            "--session-dir",
            tmp.path().to_str().unwrap(),
            "--session",
            "not-a-valid-id",
            "-p",
            "hi",
        ])
        .env_remove("OPENAI_API_KEY")
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("OPENROUTER_API_KEY")
        .env_remove("OLLAMA_BASE_URL")
        .env_remove("OLLAMA_API_KEY")
        .output()
        .expect("spawn yolop with bad session id");
    assert!(
        !output.status.success(),
        "expected non-zero exit for malformed --session"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("invalid --session") || stderr.contains("session"),
        "expected diagnostic mentioning session id: {stderr}"
    );
}

#[test]
fn tui_escape_does_not_exit_and_ctrl_c_exits() {
    let mut tui = spawn_tui_llmsim();
    assert!(
        tui.wait_for_output("type /help", Duration::from_secs(3)),
        "TUI did not render startup banner: {}",
        tui.output_text()
    );

    tui.write_input(b"\x1b");
    assert!(
        wait_for_exit(&mut *tui.child, Duration::from_millis(700)).is_none(),
        "Esc should not exit the TUI: {}",
        tui.output_text()
    );

    tui.write_input(b"\x03");
    let status = tui.wait_or_kill(Duration::from_secs(3));
    assert!(
        status.success(),
        "Ctrl-C should exit cleanly, got {status:?}: {}",
        tui.output_text()
    );
}

#[test]
fn tui_alt_enter_sequence_submits_like_enter() {
    let mut tui = spawn_tui_llmsim();
    assert!(
        tui.wait_for_output("type /help", Duration::from_secs(3)),
        "TUI did not render startup banner: {}",
        tui.output_text()
    );

    tui.write_input(b"one\x1b\r");
    assert!(
        tui.wait_for_output("one", Duration::from_secs(3)),
        "Alt-Enter should submit like Enter: {}",
        tui.output_text()
    );
    assert!(
        tui.wait_for_output("offline mode", Duration::from_secs(3)),
        "first turn did not complete before second input: {}",
        tui.output_text()
    );

    tui.write_input(b"two\r");
    assert!(
        tui.wait_for_output("two", Duration::from_secs(3)),
        "plain Enter did not submit second input: {}",
        tui.output_text()
    );
    let after_submit = strip_ansi(&tui.output_text());
    assert!(
        after_submit.contains("one") && after_submit.contains("two"),
        "submitted text should render both turns: {after_submit}"
    );

    tui.write_input(b"\x03");
    let status = tui.wait_or_kill(Duration::from_secs(3));
    assert!(
        status.success(),
        "Ctrl-C should exit cleanly, got {status:?}: {}",
        tui.output_text()
    );
}

#[test]
fn tui_double_ctrl_c_exits() {
    let mut tui = spawn_tui_llmsim();
    assert!(
        tui.wait_for_output("type /help", Duration::from_secs(3)),
        "TUI did not render startup banner: {}",
        tui.output_text()
    );

    tui.write_input(b"\x03\x03");
    let status = tui.wait_or_kill(Duration::from_secs(3));
    assert!(
        status.success(),
        "double Ctrl-C should exit cleanly, got {status:?}: {}",
        tui.output_text()
    );
}

/// Parse the session id printed on the `session …` line of `--print` stdout.
/// The line shape is:
/// `session   <id> (folder: ...; log: ...; N prior event(s))`
fn extract_session_id(stdout: &str) -> Option<String> {
    for line in stdout.lines() {
        // The line begins with a possibly-coloured "session" token and a run
        // of whitespace before the id. Strip ANSI escapes defensively.
        let stripped = strip_ansi(line);
        let trimmed = stripped.trim_start();
        if let Some(rest) = trimmed.strip_prefix("session") {
            let rest = rest.trim_start();
            // First whitespace-delimited token is the id.
            let id = rest.split_whitespace().next()?;
            if !id.is_empty() {
                return Some(id.to_string());
            }
        }
    }
    None
}

/// Parse the `N prior event(s)` count from the same session line.
fn parse_prior_events(stdout: &str) -> Option<u64> {
    for line in stdout.lines() {
        let stripped = strip_ansi(line);
        if let Some(idx) = stripped.find(" prior event(s)") {
            let head = &stripped[..idx];
            let count = head.rsplit(|c: char| !c.is_ascii_digit()).next()?;
            if !count.is_empty() {
                return count.parse().ok();
            }
        }
    }
    None
}

fn strip_ansi(input: &str) -> String {
    // Tiny ANSI CSI stripper: enough for the colour escapes the print driver
    // emits. Avoids pulling in a crate just for one helper.
    let mut out = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'[' {
            i += 2;
            while i < bytes.len() && !matches!(bytes[i], 0x40..=0x7e) {
                i += 1;
            }
            if i < bytes.len() {
                i += 1;
            }
            continue;
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

struct TuiHarness {
    child: Box<dyn Child + Send + Sync>,
    writer: Box<dyn Write + Send>,
    output_rx: Receiver<Vec<u8>>,
    output: Vec<u8>,
    _session_dir: tempfile::TempDir,
    _home: tempfile::TempDir,
}

impl TuiHarness {
    fn write_input(&mut self, bytes: &[u8]) {
        self.writer.write_all(bytes).expect("write pty input");
        self.writer.flush().expect("flush pty input");
    }

    fn wait_for_output(&mut self, needle: &str, timeout: Duration) -> bool {
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

    fn wait_or_kill(&mut self, timeout: Duration) -> ExitStatus {
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

    fn output_text(&mut self) -> String {
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

fn spawn_tui_llmsim() -> TuiHarness {
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
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("open pty");

    let mut cmd = CommandBuilder::new(yolop_binary());
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
    writer
        .write_all(b"\x1b[1;1R")
        .expect("seed cursor position response");
    writer.flush().expect("flush cursor position response");
    TuiHarness {
        child,
        writer,
        output_rx,
        output: Vec::new(),
        _session_dir: session_dir,
        _home: home,
    }
}

fn wait_for_exit(child: &mut dyn Child, timeout: Duration) -> Option<ExitStatus> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().expect("poll child exit") {
            return Some(status);
        }
        thread::sleep(Duration::from_millis(20));
    }
    child.try_wait().expect("poll child exit")
}

/// Result of one scripted ACP handshake against the real binary.
struct AcpHandshake {
    init: serde_json::Value,
    session_id: String,
    prompt: serde_json::Value,
    /// All `agent_message_chunk` text streamed during the prompt, concatenated.
    assistant_text: String,
    /// True if the process exited cleanly after stdin was closed.
    exited_cleanly: bool,
}

/// Spawn `yolop --acp <provider>` over real OS stdin/stdout pipes and drive the
/// full JSON-RPC handshake: initialize → session/new → session/prompt, closing
/// stdin to let the agent exit. Returns the responses and streamed text so
/// callers can assert per-provider behaviour. Exercises the binary's actual
/// ACP wiring, not just the in-process `serve` tests.
fn run_acp_handshake(provider: &str, prompt_text: &str) -> AcpHandshake {
    use std::io::BufRead;
    use std::process::{Command as StdCommand, Stdio};

    let session_dir = tempfile::tempdir().expect("session tempdir");
    let workspace = tempfile::tempdir().expect("workspace tempdir");

    let mut child = StdCommand::new(yolop_binary())
        .args([
            "--acp",
            "--provider",
            provider,
            "--session-dir",
            session_dir.path().to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn yolop --acp");

    let mut stdin = child.stdin.take().expect("acp stdin");
    let (line_tx, line_rx) = mpsc::channel::<String>();
    let stdout = child.stdout.take().expect("acp stdout");
    let reader = thread::spawn(move || {
        let mut buf = std::io::BufReader::new(stdout);
        loop {
            let mut line = String::new();
            match buf.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {
                    if line_tx.send(line).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    let send = |stdin: &mut std::process::ChildStdin, value: serde_json::Value| {
        let line = format!("{value}\n");
        stdin.write_all(line.as_bytes()).expect("write acp request");
        stdin.flush().expect("flush acp request");
    };

    // Collect lines until one parses to a JSON object with the given response
    // id (carrying result or error). `agent_message_chunk` notifications seen
    // along the way are accumulated into `assistant_text`.
    let assistant_text = std::cell::RefCell::new(String::new());
    let await_response = |rx: &Receiver<String>, id: i64| -> serde_json::Value {
        let deadline = Instant::now() + Duration::from_secs(60);
        while Instant::now() < deadline {
            match rx.recv_timeout(Duration::from_millis(500)) {
                Ok(line) => {
                    let Ok(value) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
                        continue;
                    };
                    if value["params"]["update"]["sessionUpdate"] == "agent_message_chunk"
                        && let Some(text) = value["params"]["update"]["content"]["text"].as_str()
                    {
                        assistant_text.borrow_mut().push_str(text);
                    }
                    if value.get("id").and_then(serde_json::Value::as_i64) == Some(id)
                        && (value.get("result").is_some() || value.get("error").is_some())
                    {
                        return value;
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        panic!("timed out awaiting acp response id={id}");
    };

    send(
        &mut stdin,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 0,
            "method": "initialize",
            "params": {
                "protocolVersion": 1,
                "clientCapabilities": { "fs": { "readTextFile": true, "writeTextFile": true } }
            }
        }),
    );
    let init = await_response(&line_rx, 0);

    send(
        &mut stdin,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "session/new",
            "params": { "cwd": workspace.path().to_str().unwrap(), "mcpServers": [] }
        }),
    );
    let new_session = await_response(&line_rx, 1);
    let session_id = new_session["result"]["sessionId"]
        .as_str()
        .unwrap_or_else(|| panic!("sessionId in response: {new_session}"))
        .to_string();

    send(
        &mut stdin,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "session/prompt",
            "params": {
                "sessionId": session_id,
                "prompt": [{ "type": "text", "text": prompt_text }]
            }
        }),
    );
    let prompt = await_response(&line_rx, 2);

    // Closing stdin makes the agent's read loop hit EOF and exit cleanly.
    drop(stdin);
    let status = wait_for_process_exit(&mut child, Duration::from_secs(10));
    let _ = reader.join();

    AcpHandshake {
        init,
        session_id,
        prompt,
        assistant_text: assistant_text.into_inner(),
        exited_cleanly: status.map(|s| s.success()).unwrap_or(false),
    }
}

#[test]
fn acp_stdio_handshake_smoke() {
    // env_remove on the parent process is unnecessary: --provider llmsim wins,
    // and the offline driver needs no key.
    let result = run_acp_handshake("llmsim", "hi");
    assert_eq!(
        result.init["result"]["protocolVersion"], 1,
        "initialize response: {}",
        result.init
    );
    assert!(
        result.session_id.starts_with("session_"),
        "unexpected session id: {}",
        result.session_id
    );
    assert_eq!(
        result.prompt["result"]["stopReason"], "end_turn",
        "prompt response: {}",
        result.prompt
    );
    assert!(
        !result.assistant_text.is_empty(),
        "expected a streamed agent_message_chunk"
    );
    assert!(
        result.exited_cleanly,
        "yolop --acp should exit cleanly after stdin close"
    );
}

#[test]
#[ignore = "requires OPENAI_API_KEY; run under doppler with --ignored"]
fn acp_openai_handshake_smoke() {
    let Ok(_) = std::env::var("OPENAI_API_KEY") else {
        panic!("OPENAI_API_KEY required for live ACP smoke test");
    };
    let result = run_acp_handshake("openai", "Reply with exactly the single word: pong");
    assert_eq!(
        result.prompt["result"]["stopReason"], "end_turn",
        "prompt response: {}",
        result.prompt
    );
    assert!(
        result.assistant_text.to_lowercase().contains("pong"),
        "expected `pong` in streamed assistant text, got: {:?}",
        result.assistant_text
    );
    assert!(result.exited_cleanly, "agent should exit cleanly");
}

fn wait_for_process_exit(
    child: &mut std::process::Child,
    timeout: Duration,
) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        match child.try_wait().expect("poll acp child") {
            Some(status) => return Some(status),
            None => thread::sleep(Duration::from_millis(20)),
        }
    }
    let _ = child.kill();
    child.try_wait().expect("poll acp child after kill")
}

#[test]
#[ignore = "requires OPENAI_API_KEY; run under doppler with --ignored"]
fn openai_print_smoke() {
    let Ok(_) = std::env::var("OPENAI_API_KEY") else {
        panic!("OPENAI_API_KEY required for live smoke test");
    };
    let tmp = tempfile::tempdir().expect("tempdir");
    let output = Command::new(yolop_binary())
        .args([
            "--provider",
            "openai",
            "--session-dir",
            tmp.path().to_str().unwrap(),
            "-p",
            "Reply with exactly the single word: pong",
        ])
        .output()
        .expect("spawn yolop --provider openai");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "yolop openai smoke failed: stdout={stdout} stderr={stderr}"
    );
    assert!(
        stdout.to_lowercase().contains("pong"),
        "expected `pong` in stdout: {stdout}"
    );
    assert!(
        stdout.contains("success=true"),
        "expected success=true: {stdout}"
    );
}

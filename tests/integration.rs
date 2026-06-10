// Integration tests for the yolop binary.
//
// The unignored tests exercise the offline `llmsim` provider so CI can prove
// the binary still launches and the agent loop wires up correctly without any
// API key.
//
// The live tests reach real provider endpoints (OpenAI and OpenRouter). They
// skip themselves when the relevant API key is absent, so a plain `cargo test`
// stays offline — no `#[ignore]` needed. CI's live-smoke job runs them under
// Doppler with `YOLOP_REQUIRE_LIVE_TESTS=1`, which upgrades a missing key from
// "skip" to a hard failure so a misconfigured secret can't report a false
// green:
//
//     YOLOP_REQUIRE_LIVE_TESTS=1 doppler run -- cargo test --test integration
//
// The OpenRouter tests default to a Nemotron 3 model and guard the Chat
// Completions tool-calling path (everruns EVE-522 / EVE-523).

mod support;

use std::path::PathBuf;
use std::process::Command;
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};
use std::io::Write;

use support::strip_ansi;
use support::tui_harness::{
    TuiSpawnOptions, assert_cursor_near_bottom, spawn_tui_llmsim, spawn_tui_llmsim_with,
    wait_for_exit,
};

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
    let mut tui = spawn_tui_llmsim(&yolop_binary());
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
    assert!(
        tui.output_text().contains("Resume with yolop --session"),
        "Ctrl-C cleanup should print resume hint: {}",
        tui.output_text()
    );
}

#[test]
fn tui_alt_enter_sequence_submits_like_enter() {
    let mut tui = spawn_tui_llmsim(&yolop_binary());
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
fn tui_survives_slow_cursor_position_reply_after_resize() {
    // Regression test for the TUI dying right around turn completion under
    // xterm.js-backed terminals (ttyd / vhs recordings). Those emulators
    // resize the PTY mid-session (fit-addon re-measuring once the scrollbar
    // appears or fonts settle) and can be slow to answer the `CSI 6n`
    // cursor-position query ratatui issues to re-anchor the inline viewport
    // after a resize — crossterm gives up on that query after 2 seconds.
    // That transient failure must not exit the TUI.
    let mut tui = spawn_tui_llmsim(&yolop_binary());
    assert!(
        tui.wait_for_output("type /help", Duration::from_secs(3)),
        "TUI did not render startup banner: {}",
        tui.output_text()
    );

    tui.write_input(b"hi\r");
    assert!(
        tui.wait_for_output("offline mode", Duration::from_secs(5)),
        "turn did not complete: {}",
        tui.output_text()
    );

    // Shrink the PTY by one column, like xterm.js does when its scrollbar
    // appears as the transcript first overflows. The harness deliberately
    // does NOT answer the cursor-position query this triggers, emulating a
    // busy emulator. crossterm's 2s query timeout fires at least once.
    tui.resize(79, 24);
    assert!(
        wait_for_exit(&mut *tui.child, Duration::from_secs(5)).is_none(),
        "TUI exited after an unanswered cursor-position query: {}",
        tui.output_text()
    );

    // The emulator catches up: answer the (retried) query, then Ctrl-C. The
    // reply must come first — the event loop is blocked inside the cursor
    // query until it arrives.
    tui.write_input(b"\x1b[1;1R");
    tui.write_input(b"\x03");
    let status = tui.wait_or_kill(Duration::from_secs(10));
    assert!(
        status.success(),
        "Ctrl-C should exit cleanly after recovery, got {status:?}: {}",
        tui.output_text()
    );
}

#[test]
fn tui_startup_anchors_composer_from_top_in_tall_terminal() {
    let mut tui = spawn_tui_llmsim_with(
        &yolop_binary(),
        TuiSpawnOptions {
            rows: 40,
            cols: 100,
            cursor_row: 1,
        },
    );
    assert!(
        tui.wait_for_output("type /help", Duration::from_secs(3)),
        "TUI did not render startup banner: {}",
        tui.output_text()
    );

    assert_cursor_near_bottom(&mut tui, 40);
}

#[test]
fn tui_startup_anchors_composer_when_prompt_is_already_near_bottom() {
    let mut tui = spawn_tui_llmsim_with(
        &yolop_binary(),
        TuiSpawnOptions {
            rows: 24,
            cols: 80,
            cursor_row: 23,
        },
    );
    assert!(
        tui.wait_for_output("type /help", Duration::from_secs(3)),
        "TUI did not render startup banner: {}",
        tui.output_text()
    );

    assert_cursor_near_bottom(&mut tui, 24);
}

#[test]
fn tui_startup_anchors_composer_in_short_terminal() {
    let mut tui = spawn_tui_llmsim_with(
        &yolop_binary(),
        TuiSpawnOptions {
            rows: 8,
            cols: 80,
            cursor_row: 1,
        },
    );
    assert!(
        tui.wait_for_output("type /help", Duration::from_secs(3)),
        "TUI did not render startup banner: {}",
        tui.output_text()
    );

    assert_cursor_near_bottom(&mut tui, 8);
}

#[test]
fn tui_setup_overlay_renders_in_real_pty() {
    let mut tui = spawn_tui_llmsim(&yolop_binary());
    assert!(
        tui.wait_for_output("type /help", Duration::from_secs(3)),
        "TUI did not render startup banner: {}",
        tui.output_text()
    );

    tui.write_input(b"/setup\r");
    assert!(
        tui.wait_for_output("Set Up Yolop", Duration::from_secs(3)),
        "/setup should render setup overlay: {}",
        tui.output_text()
    );
    assert!(
        tui.wait_for_output("Esc cancel", Duration::from_secs(3)),
        "/setup footer should render without clipping: {}",
        tui.output_text()
    );
}

#[test]
fn tui_submit_turn_renders_assistant_in_scrollback() {
    let mut tui = spawn_tui_llmsim(&yolop_binary());
    assert!(
        tui.wait_for_output("type /help", Duration::from_secs(3)),
        "TUI did not render startup banner: {}",
        tui.output_text()
    );

    tui.write_input(b"scrollback smoke\r");
    assert!(
        tui.wait_for_output("scrollback smoke", Duration::from_secs(3)),
        "submitted prompt should appear in scrollback: {}",
        tui.output_text()
    );
    assert!(
        tui.wait_for_output("offline mode", Duration::from_secs(5)),
        "assistant reply should land in scrollback after turn completion: {}",
        tui.output_text()
    );

    let transcript = strip_ansi(&tui.output_text());
    assert!(
        transcript.contains("scrollback smoke") && transcript.contains("offline mode"),
        "scrollback should retain both user prompt and assistant reply: {transcript}"
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
    let mut tui = spawn_tui_llmsim(&yolop_binary());
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

/// Resolve a provider API key for a live test.
///
/// Returns `None` (the test should then `return` early) when the key is absent,
/// so a plain `cargo test` run stays offline without any `#[ignore]`. CI's
/// live-smoke job sets `YOLOP_REQUIRE_LIVE_TESTS=1`, which turns a missing key
/// into a hard failure — a misconfigured secret must not let the live check
/// report a false green.
///
/// This is a presence check only: it never reads the key value into memory, so
/// the secret is not materialized here and a non-UTF-8 value still counts as
/// present.
fn live_key_or_skip(var: &str) -> Option<()> {
    if std::env::var_os(var).is_some_and(|value| !value.is_empty()) {
        return Some(());
    }
    assert!(
        std::env::var_os("YOLOP_REQUIRE_LIVE_TESTS").is_none(),
        "{var} is required when YOLOP_REQUIRE_LIVE_TESTS is set"
    );
    eprintln!("skipping live test: {var} not set");
    None
}

#[test]
fn acp_openai_handshake_smoke() {
    let Some(_) = live_key_or_skip("OPENAI_API_KEY") else {
        return;
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

#[test]
#[ignore = "requires ANTHROPIC_API_KEY; run under doppler with --ignored"]
fn acp_anthropic_handshake_smoke() {
    let Ok(_) = std::env::var("ANTHROPIC_API_KEY") else {
        panic!("ANTHROPIC_API_KEY required for live ACP smoke test");
    };
    let result = run_acp_handshake("anthropic", "Reply with exactly the single word: pong");
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
fn openai_print_smoke() {
    let Some(_) = live_key_or_skip("OPENAI_API_KEY") else {
        return;
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

/// Model used by the live OpenRouter smoke tests. Defaults to a Nemotron 3
/// variant (the path these tests exist to protect); override with
/// `YOLOP_LIVE_OPENROUTER_MODEL` to pin a cheaper or less rate-limited model in
/// CI without touching code.
fn live_openrouter_model() -> String {
    std::env::var("YOLOP_LIVE_OPENROUTER_MODEL")
        .unwrap_or_else(|_| "nvidia/nemotron-3-ultra-550b-a55b".to_string())
}

/// True when a live run failed only because the upstream provider rate-limited
/// us (HTTP 429). That is infrastructure, not a yolop regression, so the live
/// OpenRouter tests skip on it rather than fail — the shared Nemotron endpoints
/// 429 often enough to make a required CI check flaky otherwise. A real
/// regression in the tool-calling path produces a missing sentinel or
/// `success=false`, not a 429, so it is still caught.
fn looks_rate_limited(combined: &str) -> bool {
    let lower = combined.to_lowercase();
    combined.contains("429")
        && (lower.contains("rate-limit") || lower.contains("too many requests"))
}

#[test]
fn openrouter_print_smoke() {
    let Some(_) = live_key_or_skip("OPENROUTER_API_KEY") else {
        return;
    };
    let tmp = tempfile::tempdir().expect("tempdir");
    let model = live_openrouter_model();
    let output = Command::new(yolop_binary())
        .args([
            "--provider",
            "openrouter",
            "--model",
            &model,
            "--session-dir",
            tmp.path().to_str().unwrap(),
            "-p",
            "Reply with exactly the single word: pong",
        ])
        .output()
        .expect("spawn yolop --provider openrouter");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() && looks_rate_limited(&format!("{stdout}{stderr}")) {
        eprintln!("skipping live test: upstream provider rate-limited (429)");
        return;
    }
    assert!(
        output.status.success(),
        "yolop openrouter smoke failed: stdout={stdout} stderr={stderr}"
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

/// Live regression test for the OpenRouter tool-calling path (everruns EVE-522
/// and EVE-523).
///
/// OpenRouter's `/responses` endpoint is stateless (it ignores
/// `previous_response_id`), so yolop routes OpenRouter through the Chat
/// Completions driver. On that path, OpenRouter/DeepInfra streams an empty
/// `content: ""` in the same chunk as `finish_reason: "tool_calls"`, which used
/// to make the runtime silently drop the tool call — the agent would emit a
/// `read_file` call, never execute it, and end the turn with no action.
///
/// This test seeds a unique sentinel in a workspace file and asks the model to
/// read it back. The sentinel is *not* in the prompt, so it can only appear in
/// the answer if `read_file` actually executed and its result flowed back into
/// the next turn. A regression on either bug makes this fail.
#[test]
fn openrouter_tool_call_executes_end_to_end() {
    let Some(_) = live_key_or_skip("OPENROUTER_API_KEY") else {
        return;
    };
    let workspace = tempfile::tempdir().expect("workspace tempdir");
    let sessions = tempfile::tempdir().expect("sessions tempdir");
    // Unique per run so a stale cache or a model that pattern-matches a known
    // token can't fake a pass — the value only exists in the file we just wrote.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock after epoch")
        .as_nanos();
    let sentinel = format!("MARMOT-{}-{nanos}", std::process::id());
    std::fs::write(
        workspace.path().join("secret.txt"),
        format!("The access token is {sentinel}.\n"),
    )
    .expect("write secret.txt");

    let model = live_openrouter_model();
    let output = Command::new(yolop_binary())
        .args([
            "--provider",
            "openrouter",
            "--model",
            &model,
            "-C",
            workspace.path().to_str().unwrap(),
            "--session-dir",
            sessions.path().to_str().unwrap(),
            "-p",
            "Read the file secret.txt in the workspace and reply with ONLY the \
             access token it contains, and nothing else.",
        ])
        .output()
        .expect("spawn yolop --provider openrouter");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() && looks_rate_limited(&format!("{stdout}{stderr}")) {
        eprintln!("skipping live test: upstream provider rate-limited (429)");
        return;
    }
    assert!(
        output.status.success(),
        "yolop openrouter tool-call smoke failed: stdout={stdout} stderr={stderr}"
    );
    assert!(
        stdout.contains(&sentinel),
        "expected sentinel {sentinel} in stdout (proves read_file executed and \
         its result reached the model): {stdout}"
    );
    assert!(
        stdout.contains("success=true"),
        "expected success=true: {stdout}"
    );
}

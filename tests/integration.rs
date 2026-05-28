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
    assert!(
        stdout.contains("yolop"),
        "version output missing binary name: {stdout}"
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

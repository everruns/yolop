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

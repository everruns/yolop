//! Black-box end-to-end MCP tests.
//!
//! These spin up a **real** stdio MCP server (the small Python fixture in
//! `tests/fixtures/mcp_echo_server.py`) and drive a real `InProcessRuntime`
//! against it, with the bundled llmsim scripted to call the server's tools.
//! Nothing here is mocked except the LLM: live `tools/list` discovery and real
//! `tools/call` execution over the stdio transport all run for real. The
//! server writes a marker file on each call, so we assert *via the filesystem*
//! whether a tool actually executed.
//!
//! `python3` is required. In CI (`CI` env set) a missing `python3` is a hard
//! failure — silently skipping would let the test report green without
//! exercising anything (cf. the live-smoke job in `.github/workflows/ci.yml`).
//! On a local box without `python3` the tests skip with a warning.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use everruns_core::llmsim_driver::{LlmSimConfig, SimToolCall, SimTurn};
use serde_json::json;

use crate::runtime::{BuildOptions, BuiltRuntime, ProviderChoice, build_with_options};
use crate::settings::SettingsStore;

const TURN_TIMEOUT: Duration = Duration::from_secs(20);

/// Resolve `python3` from `PATH`.
fn python3() -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths)
        .map(|dir| dir.join("python3"))
        .find(|candidate| candidate.is_file())
}

/// Resolve `python3`, deciding whether a miss is a skip or a failure. In CI
/// (`CI` env set) a missing `python3` panics — a silently green check would not
/// be exercising anything (matching the live-smoke job's stance in
/// `.github/workflows/ci.yml`). Locally it skips with a warning.
fn require_python3(test: &str) -> Option<PathBuf> {
    if let Some(python) = python3() {
        return Some(python);
    }
    assert!(
        std::env::var_os("CI").is_none(),
        "{test}: python3 is required to run the MCP e2e tests in CI but was not found on PATH",
    );
    eprintln!("skipping {test}: python3 not found (set CI=1 to make this a hard failure)");
    None
}

fn fixture_server() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mcp_echo_server.py")
}

/// The prefixed tool name the runtime exposes for `<server>`/`<tool>`
/// (`mcp_<server>__<tool>`); both names here sanitize to themselves.
fn mcp_tool(server: &str, tool: &str) -> String {
    format!("mcp_{server}__{tool}")
}

/// One scripted tool call followed by a closing assistant turn.
fn script(tool: &str, message: &str) -> LlmSimConfig {
    LlmSimConfig::scripted(vec![
        SimTurn::ToolCalls(vec![SimToolCall {
            name: tool.to_string(),
            arguments: json!({ "message": message }),
            id: None,
        }]),
        SimTurn::Assistant("done".to_string()),
    ])
}

/// Build a runtime whose workspace `.mcp.json` points the `echo` server at the
/// Python fixture, passing `marker_dir` so calls leave a filesystem trace.
async fn build_runtime(config: LlmSimConfig, marker_dir: &Path, python: &Path) -> BuiltRuntime {
    let workspace_root = tempfile::tempdir().expect("workspace").keep();
    let sessions_root = tempfile::tempdir().expect("sessions").keep();

    let mcp_json = json!({
        "mcpServers": {
            "echo": {
                "type": "stdio",
                "command": python.to_str().unwrap(),
                "args": [
                    fixture_server().to_str().unwrap(),
                    marker_dir.to_str().unwrap(),
                ]
            }
        }
    });
    std::fs::write(
        workspace_root.join(".mcp.json"),
        serde_json::to_vec_pretty(&mcp_json).unwrap(),
    )
    .expect("write .mcp.json");

    let settings = Arc::new(SettingsStore::open(sessions_root.join("settings.toml")));
    build_with_options(
        workspace_root,
        ProviderChoice::Sim,
        None,
        sessions_root,
        settings,
        BuildOptions {
            llmsim_override: Some(config.with_model("llmsim-yolop")),
            ..BuildOptions::default()
        },
    )
    .await
    .expect("build runtime")
}

async fn run_turn(runtime: &BuiltRuntime, text: &str) -> everruns_runtime::TurnResult {
    let session_id = runtime.handles.session_id;
    let input = runtime.model.input_message(text.to_string());
    tokio::time::timeout(
        TURN_TIMEOUT,
        runtime.handles.runtime.run_turn(session_id, input),
    )
    .await
    .expect("run_turn timed out")
    .expect("run_turn errored")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_tool_executes_over_real_stdio_server() {
    let Some(python) = require_python3("mcp_tool_executes_over_real_stdio_server") else {
        return;
    };
    let marker = tempfile::tempdir().expect("marker").keep();
    let tool = mcp_tool("echo", "echo");
    let runtime = build_runtime(script(&tool, "hello-mcp"), &marker, &python).await;

    let result = run_turn(&runtime, "use the echo tool").await;

    assert!(result.success, "turn must succeed: {result:?}");
    assert_eq!(result.tool_calls_count, 1, "exactly one MCP call expected");
    let called = marker.join("echo.called");
    assert!(
        called.exists(),
        "the real MCP server must have executed tools/call (marker missing)"
    );
    let body = std::fs::read_to_string(&called).unwrap();
    assert!(
        body.contains("hello-mcp"),
        "server should receive the call arguments: {body}"
    );
}

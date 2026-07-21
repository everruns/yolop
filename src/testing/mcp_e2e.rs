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

use crate::config::SettingsStore;
use crate::runtime::{BuildOptions, BuiltRuntime, ProviderChoice, build_with_options};

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

/// The `.mcp.json` value pointing the `echo` server at the Python fixture,
/// passing `marker_dir` so calls leave a filesystem trace.
fn echo_mcp_json(python: &Path, marker_dir: &Path) -> serde_json::Value {
    json!({
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
    })
}

/// A `.mcp.json` with no servers configured.
fn empty_mcp_json() -> serde_json::Value {
    json!({ "mcpServers": {} })
}

fn write_mcp_json(workspace_root: &Path, value: &serde_json::Value) {
    std::fs::write(
        workspace_root.join(".mcp.json"),
        serde_json::to_vec_pretty(value).unwrap(),
    )
    .expect("write .mcp.json");
}

/// Names of the MCP servers currently attached to the live session (i.e. what
/// the next turn will resolve). Reads the stored session directly rather than
/// the on-disk config, so it reflects reloads applied via
/// `RuntimeHandles::reload_mcp_servers`.
async fn live_mcp_server_names(runtime: &BuiltRuntime) -> Vec<String> {
    use everruns_core::SessionStore;
    let session = runtime
        .handles
        .session_store
        .get_session(runtime.handles.session_id)
        .await
        .expect("get session")
        .expect("session exists");
    let mut names: Vec<String> = session.mcp_servers.keys().cloned().collect();
    names.sort();
    names
}

/// Build a runtime whose workspace `.mcp.json` is seeded with `mcp_json`.
async fn build_runtime_with(config: LlmSimConfig, mcp_json: &serde_json::Value) -> BuiltRuntime {
    let workspace_root = tempfile::tempdir().expect("workspace").keep();
    let sessions_root = tempfile::tempdir().expect("sessions").keep();

    write_mcp_json(&workspace_root, mcp_json);

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

/// Build a runtime whose workspace `.mcp.json` points the `echo` server at the
/// Python fixture, passing `marker_dir` so calls leave a filesystem trace.
async fn build_runtime(config: LlmSimConfig, marker_dir: &Path, python: &Path) -> BuiltRuntime {
    build_runtime_with(config, &echo_mcp_json(python, marker_dir)).await
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

/// The reload seam swaps the live session's scoped MCP servers so add / remove
/// take effect without rebuilding the runtime. This assertion is config-only
/// (an HTTP server that is never contacted), so it runs in CI without
/// `python3`: proving the mechanism the TUI `/mcp` command and `/mcp reload`
/// drive.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reload_swaps_live_session_mcp_servers() {
    // Membership checks (not exact equality) so any ambient global MCP config
    // on the developer's machine can't perturb the assertions.
    let has = |names: &[String], name: &str| names.iter().any(|n| n == name);

    // Start with the workspace declaring no servers.
    let runtime = build_runtime_with(LlmSimConfig::fixed("hi"), &empty_mcp_json()).await;
    assert!(
        !has(&live_mcp_server_names(&runtime).await, "docs"),
        "docs is absent before it is configured"
    );

    // Add a server on disk, reload, and observe it attached live.
    write_mcp_json(
        &runtime.startup.workspace_root,
        &json!({ "mcpServers": { "docs": { "type": "http", "url": "https://example.com/mcp" } } }),
    );
    let names = runtime
        .handles
        .reload_mcp_servers()
        .await
        .expect("reload after add");
    assert!(has(&names, "docs"), "reload reports the added server");
    assert!(
        has(&live_mcp_server_names(&runtime).await, "docs"),
        "the live session now carries the added server"
    );

    // Remove it again and confirm the live session drops it.
    write_mcp_json(&runtime.startup.workspace_root, &empty_mcp_json());
    let names = runtime
        .handles
        .reload_mcp_servers()
        .await
        .expect("reload after remove");
    assert!(!has(&names, "docs"), "reload drops the removed server");
    assert!(
        !has(&live_mcp_server_names(&runtime).await, "docs"),
        "the live session drops the removed server"
    );
}

/// A server added to `.mcp.json` after startup is discovered and executable on
/// the next turn once reloaded — no restart. Proven end-to-end via the real
/// stdio fixture writing its marker file.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reload_adds_mcp_server_and_executes_live() {
    let Some(python) = require_python3("reload_adds_mcp_server_and_executes_live") else {
        return;
    };
    let marker = tempfile::tempdir().expect("marker").keep();
    let tool = mcp_tool("echo", "echo");

    // No server at startup: the scripted echo call has nothing to run yet.
    let runtime = build_runtime_with(script(&tool, "added-live"), &empty_mcp_json()).await;
    assert!(
        !live_mcp_server_names(&runtime)
            .await
            .iter()
            .any(|n| n == "echo"),
        "echo server is absent before reload"
    );

    // Add the server and reload the live session.
    write_mcp_json(
        &runtime.startup.workspace_root,
        &echo_mcp_json(&python, &marker),
    );
    let names = runtime
        .handles
        .reload_mcp_servers()
        .await
        .expect("reload after add");
    assert!(names.iter().any(|n| n == "echo"), "reload attaches echo");

    // The next turn discovers and executes the freshly added server.
    let result = run_turn(&runtime, "use the echo tool").await;
    assert!(result.success, "turn must succeed: {result:?}");
    assert_eq!(result.tool_calls_count, 1, "exactly one MCP call expected");
    let called = marker.join("echo.called");
    assert!(
        called.exists(),
        "the reloaded MCP server must have executed tools/call (marker missing)"
    );
}

/// A server removed from `.mcp.json` after startup stops being executable once
/// reloaded. Proven via the absence of the fixture's marker file after a turn
/// that scripts the (now-removed) tool call.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reload_removes_mcp_server_live() {
    let Some(python) = require_python3("reload_removes_mcp_server_live") else {
        return;
    };
    let marker = tempfile::tempdir().expect("marker").keep();
    let tool = mcp_tool("echo", "echo");

    // Start with the echo server present.
    let runtime = build_runtime_with(
        script(&tool, "should-not-run"),
        &echo_mcp_json(&python, &marker),
    )
    .await;
    assert!(
        live_mcp_server_names(&runtime)
            .await
            .iter()
            .any(|n| n == "echo"),
        "echo server is present at startup"
    );

    // Remove it and reload the live session.
    write_mcp_json(&runtime.startup.workspace_root, &empty_mcp_json());
    let names = runtime
        .handles
        .reload_mcp_servers()
        .await
        .expect("reload after remove");
    assert!(
        !names.iter().any(|n| n == "echo"),
        "echo dropped after remove"
    );

    // The scripted tool call can no longer reach a server: no marker is written.
    let _ = run_turn(&runtime, "use the echo tool").await;
    assert!(
        !marker.join("echo.called").exists(),
        "the removed MCP server must not execute after reload"
    );
}

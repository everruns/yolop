//! Black-box end-to-end MCP tests.
//!
//! These spin up a **real** stdio MCP server (the small Python fixture in
//! `tests/fixtures/mcp_echo_server.py`) and drive a real `InProcessRuntime`
//! against it, with the bundled llmsim scripted to call the server's tools.
//! Nothing here is mocked except the LLM: live `tools/list` discovery, the
//! annotation→`ToolHints` mapping, the `McpApprovalCapability` pre-tool hook,
//! and real `tools/call` execution over the stdio transport all run for real.
//! The server writes a marker file on each call, so we assert *via the
//! filesystem* whether a tool actually executed.
//!
//! Skipped (with a warning) when `python3` is unavailable so a Python-less CI
//! box does not fail.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use everruns_core::llmsim_driver::{LlmSimConfig, SimToolCall, SimTurn};
use serde_json::json;

use crate::approval::{ApprovalGate, ApprovalRequest};
use crate::runtime::{BuildOptions, BuiltRuntime, ProviderChoice, build_with_options};
use crate::settings::SettingsStore;

const TURN_TIMEOUT: Duration = Duration::from_secs(20);

/// Resolve `python3` from `PATH`, or `None` to skip.
fn python3() -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths)
        .map(|dir| dir.join("python3"))
        .find(|candidate| candidate.is_file())
}

fn fixture_server() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mcp_echo_server.py")
}

/// The prefixed tool name the runtime exposes for `<server>`/`<tool>`
/// (`mcp_<server>__<tool>`); both names here sanitize to themselves.
fn mcp_tool(server: &str, tool: &str) -> String {
    format!("mcp_{server}__{tool}")
}

/// A channel gate that denies every request, plus a task that answers `false`.
/// Used to prove blocking — and that readonly/non-MCP tools never reach it.
fn deny_gate() -> Arc<ApprovalGate> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(
        ApprovalRequest,
        tokio::sync::oneshot::Sender<bool>,
    )>();
    tokio::spawn(async move {
        while let Some((_req, responder)) = rx.recv().await {
            let _ = responder.send(false);
        }
    });
    ApprovalGate::channel(tx)
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
async fn build_runtime(
    config: LlmSimConfig,
    gate: Arc<ApprovalGate>,
    marker_dir: &Path,
    python: &Path,
) -> BuiltRuntime {
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
        gate,
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
    let Some(python) = python3() else {
        eprintln!("skipping mcp_tool_executes_over_real_stdio_server: python3 not found");
        return;
    };
    let marker = tempfile::tempdir().expect("marker").keep();
    let tool = mcp_tool("echo", "echo");
    let runtime = build_runtime(
        script(&tool, "hello-mcp"),
        ApprovalGate::auto(),
        &marker,
        &python,
    )
    .await;

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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_tool_blocked_when_approval_denied() {
    let Some(python) = python3() else {
        eprintln!("skipping mcp_tool_blocked_when_approval_denied: python3 not found");
        return;
    };
    let marker = tempfile::tempdir().expect("marker").keep();
    let tool = mcp_tool("echo", "echo");
    let runtime = build_runtime(script(&tool, "nope"), deny_gate(), &marker, &python).await;

    let result = run_turn(&runtime, "use the echo tool").await;

    // The turn still completes — the block surfaces to the model as a tool
    // error and the scripted assistant turn closes the loop.
    assert!(
        result.success,
        "turn completes after a blocked tool: {result:?}"
    );
    assert!(
        !marker.join("echo.called").exists(),
        "a denied MCP tool must NOT reach the server"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn readonly_mcp_tool_runs_without_approval() {
    let Some(python) = python3() else {
        eprintln!("skipping readonly_mcp_tool_runs_without_approval: python3 not found");
        return;
    };
    let marker = tempfile::tempdir().expect("marker").keep();
    // `peek` advertises `readOnlyHint: true`; even under a denying gate it must
    // run, proving the annotation→hint→hook-skip chain end to end.
    let tool = mcp_tool("echo", "peek");
    let runtime = build_runtime(script(&tool, "ok"), deny_gate(), &marker, &python).await;

    let result = run_turn(&runtime, "peek at it").await;

    assert!(result.success, "turn must succeed: {result:?}");
    assert!(
        marker.join("peek.called").exists(),
        "a readonly MCP tool must run even with a denying gate"
    );
}

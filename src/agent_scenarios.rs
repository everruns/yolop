//! Scripted agent loop scenario tests.
//!
//! These tests drive a real `InProcessRuntime` (via `runtime::build_with_options`)
//! against the bundled llmsim driver in **scripted** mode, where each
//! assistant turn is pre-specified as `SimTurn::{Assistant, ToolCalls, Mixed,
//! Error}`. That gives us deterministic multi-turn agent behavior without
//! reaching a real LLM provider, so we can pin down:
//!
//!   * the agent loop completes when the script returns plain text
//!   * a scripted bash tool call actually runs through `BashTool` and has
//!     the documented filesystem side effect
//!   * the agent loops back to the LLM after a tool result and consumes the
//!     next scripted turn
//!   * a scripted `SimError` propagates as a turn failure
//!   * `OnExhausted::Error` makes a second `run_turn` fail once the script
//!     is consumed; the default `RepeatLast` keeps replaying the last turn
//!
//! Streaming-pipeline coverage lives in `streaming_tests`; this module is
//! specifically about the agent control flow.

use std::sync::Arc;
use std::time::Duration;

use everruns_core::llmsim_driver::{LlmSimConfig, OnExhausted, SimError, SimToolCall, SimTurn};
use serde_json::json;

use crate::approval::ApprovalGate;
use crate::runtime::{BuildOptions, BuiltRuntime, ProviderChoice, build_with_options};
use crate::settings::SettingsStore;

/// Wall-clock cap on a single scripted `run_turn`. Scripted llmsim has no
/// latency by default; the budget is generous so a slow CI box won't flake.
const TURN_TIMEOUT: Duration = Duration::from_secs(15);

/// Build a runtime backed by a scripted llmsim config. The workspace and
/// session-dir tempdirs are intentionally leaked past the test body — the
/// runtime canonicalizes the workspace path and we keep the handle alive
/// by `std::mem::forget`. These are tmpfs paths under the OS-managed temp
/// tree, so the OS cleans them.
///
/// Returns both the built runtime and the workspace root so tests that
/// assert on filesystem side effects (scripted bash tool calls) can read
/// files back.
async fn build_scripted_runtime(config: LlmSimConfig) -> (BuiltRuntime, std::path::PathBuf) {
    let workspace = tempfile::tempdir().expect("workspace tempdir");
    let sessions = tempfile::tempdir().expect("sessions tempdir");
    let workspace_root = workspace.path().to_path_buf();

    let llmsim = config.with_model("llmsim-yolop");
    let settings_path = sessions.path().join("settings.toml");
    let settings = Arc::new(SettingsStore::open(settings_path));
    let runtime = build_with_options(
        workspace_root.clone(),
        ProviderChoice::Sim,
        ApprovalGate::auto(),
        None,
        sessions.path().to_path_buf(),
        settings,
        BuildOptions {
            llmsim_override: Some(llmsim),
        },
    )
    .await
    .expect("build scripted llmsim runtime");

    std::mem::forget(workspace);
    std::mem::forget(sessions);
    (runtime, workspace_root)
}

/// Drive one user turn against the scripted runtime under a wall-clock
/// timeout so a hung agent loop fails the test instead of hanging CI.
async fn run_single_turn(runtime: &BuiltRuntime, user_text: &str) -> everruns_runtime::TurnResult {
    let session_id = runtime.handles.session_id;
    let input = runtime.model.input_message(user_text.to_string());
    tokio::time::timeout(
        TURN_TIMEOUT,
        runtime.handles.runtime.run_turn(session_id, input),
    )
    .await
    .expect("run_turn timed out")
    .expect("run_turn errored")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scripted_assistant_text_returns_success_with_no_tools() {
    let (runtime, _ws) = build_scripted_runtime(LlmSimConfig::scripted(vec![SimTurn::Assistant(
        "hello from scripted llmsim".to_string(),
    )]))
    .await;

    let result = run_single_turn(&runtime, "ping").await;

    assert!(
        result.success,
        "scripted assistant turn must succeed: {result:?}"
    );
    assert_eq!(
        result.tool_calls_count, 0,
        "Assistant-only turn must not run tools"
    );
    assert!(
        result.iterations >= 1,
        "agent loop must record at least one iteration: {result:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scripted_tool_call_executes_bash_then_assistant_completes() {
    // First scripted turn issues a bash tool call that touches a marker
    // file in the workspace. Second turn closes the loop with plain text
    // so the agent stops iterating.
    let marker = "scripted_bash_ran.marker";
    let cmd = format!("touch {marker}");
    let (runtime, workspace) = build_scripted_runtime(LlmSimConfig::scripted(vec![
        SimTurn::ToolCalls(vec![SimToolCall {
            name: "bash".to_string(),
            arguments: json!({ "command": cmd }),
            id: None,
        }]),
        SimTurn::Assistant("did the bash".to_string()),
    ]))
    .await;

    let result = run_single_turn(&runtime, "do the thing").await;

    assert!(
        result.success,
        "tool-call + assistant turn must succeed: {result:?}"
    );
    assert_eq!(
        result.tool_calls_count, 1,
        "exactly one bash invocation expected"
    );
    assert!(
        workspace.join(marker).exists(),
        "BashTool must have run the scripted command and created {marker:?} in {workspace:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scripted_error_turn_marks_run_failed() {
    let (runtime, _ws) = build_scripted_runtime(LlmSimConfig::scripted(vec![SimTurn::Error(
        SimError::Other("scripted llmsim refused the request".to_string()),
    )]))
    .await;

    let result = run_single_turn(&runtime, "anything").await;

    assert!(
        !result.success,
        "scripted error turn must surface as failure"
    );
    let error = result
        .error
        .as_deref()
        .expect("failed run_turn must carry an error message");
    assert!(
        error.contains("scripted llmsim refused"),
        "the scripted error message must reach the caller, got: {error}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scripted_default_repeat_last_lets_second_call_succeed_after_exhaustion() {
    // Default OnExhausted::RepeatLast: once the script is consumed the
    // last turn keeps serving. Two run_turns against a 1-turn script
    // must both succeed and the second must still see the same text.
    let (runtime, _ws) = build_scripted_runtime(LlmSimConfig::scripted(vec![SimTurn::Assistant(
        "only turn".to_string(),
    )]))
    .await;

    let first = run_single_turn(&runtime, "first").await;
    let second = run_single_turn(&runtime, "second").await;

    assert!(first.success, "first run_turn must succeed");
    assert!(
        second.success,
        "RepeatLast must let a second run_turn succeed past exhaustion: {second:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scripted_on_exhausted_error_fails_second_call() {
    // OnExhausted::Error makes the agent loop fail once the script has
    // been consumed. Proves the exhaustion mode is wired through end-to-end.
    let (runtime, _ws) = build_scripted_runtime(
        LlmSimConfig::scripted(vec![SimTurn::Assistant("once".to_string())])
            .with_on_exhausted(OnExhausted::Error),
    )
    .await;

    let first = run_single_turn(&runtime, "first").await;
    assert!(
        first.success,
        "first run_turn before exhaustion must succeed"
    );

    let second = run_single_turn(&runtime, "second").await;
    assert!(
        !second.success,
        "OnExhausted::Error must surface as a failed turn after the script is consumed"
    );
}

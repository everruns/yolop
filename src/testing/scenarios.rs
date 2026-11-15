//! Scripted agent loop scenario tests.
//!
//! These tests drive a registered Engine session (via `runtime::build_with_options`)
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

use everruns_core::SessionTaskState;
use everruns_llmsim::{LlmSimConfig, OnExhausted, SimError, SimToolCall, SimTurn};
use serde_json::json;

use crate::config::SettingsStore;
use crate::runtime::{BuildOptions, BuiltRuntime, ProviderChoice, build_with_options};
use crate::tui::host_ui::UiCommand;

/// Wall-clock cap on a single scripted `run_turn`. Scripted llmsim has no
/// latency by default; the budget is generous so a slow CI box won't flake.
const TURN_TIMEOUT: Duration = Duration::from_secs(15);

/// Build a runtime backed by a scripted llmsim config. The workspace and
/// session-dir tempdirs are turned into kept `PathBuf`s via
/// `TempDir::keep()` so the directories outlive `build_scripted_runtime`
/// (the runtime canonicalizes paths and holds on to them past return)
/// without leaking the `TempDir` handle. These are tmpfs paths under the
/// OS-managed temp tree, so the OS still cleans the directories.
///
/// Returns both the built runtime and the workspace root so tests that
/// assert on filesystem side effects (scripted bash tool calls) can read
/// files back.
async fn build_scripted_runtime(config: LlmSimConfig) -> (BuiltRuntime, std::path::PathBuf) {
    build_scripted_runtime_with_workspace(config, |_| {}).await
}

async fn build_scripted_runtime_with_workspace(
    config: LlmSimConfig,
    setup_workspace: impl FnOnce(&std::path::Path),
) -> (BuiltRuntime, std::path::PathBuf) {
    build_scripted_runtime_with_workspace_and_options(
        config,
        setup_workspace,
        BuildOptions::default(),
    )
    .await
}

async fn build_scripted_runtime_with_workspace_and_options(
    config: LlmSimConfig,
    setup_workspace: impl FnOnce(&std::path::Path),
    options: BuildOptions,
) -> (BuiltRuntime, std::path::PathBuf) {
    build_scripted_runtime_inner(config, setup_workspace, options, false).await
}

/// Like [`build_scripted_runtime_with_workspace`], with the workspace
/// `.agents/hooks.json` written by `setup_workspace` trusted by the user.
async fn build_scripted_runtime_with_trusted_hooks(
    config: LlmSimConfig,
    setup_workspace: impl FnOnce(&std::path::Path),
) -> (BuiltRuntime, std::path::PathBuf) {
    build_scripted_runtime_inner(config, setup_workspace, BuildOptions::default(), true).await
}

/// Like [`build_scripted_runtime`], with settings written before the
/// runtime is built (settings the agent definition reads at build time,
/// such as the continuation limits).
async fn build_scripted_runtime_with_settings(
    config: LlmSimConfig,
    configure: impl FnOnce(&SettingsStore),
) -> (BuiltRuntime, std::path::PathBuf) {
    build_scripted_runtime_configured(config, |_| {}, BuildOptions::default(), false, configure)
        .await
}

async fn build_scripted_runtime_inner(
    config: LlmSimConfig,
    setup_workspace: impl FnOnce(&std::path::Path),
    options: BuildOptions,
    trust_workspace_hooks: bool,
) -> (BuiltRuntime, std::path::PathBuf) {
    build_scripted_runtime_configured(
        config,
        setup_workspace,
        options,
        trust_workspace_hooks,
        |_| {},
    )
    .await
}

async fn build_scripted_runtime_configured(
    config: LlmSimConfig,
    setup_workspace: impl FnOnce(&std::path::Path),
    mut options: BuildOptions,
    trust_workspace_hooks: bool,
    configure: impl FnOnce(&SettingsStore),
) -> (BuiltRuntime, std::path::PathBuf) {
    let workspace_root = tempfile::tempdir().expect("workspace tempdir").keep();
    let sessions_root = tempfile::tempdir().expect("sessions tempdir").keep();
    setup_workspace(&workspace_root);

    options.llmsim_override = Some(config.with_model("llmsim-yolop"));
    let settings_path = sessions_root.join("settings.toml");
    let settings = Arc::new(SettingsStore::open(settings_path));
    configure(&settings);
    if trust_workspace_hooks {
        crate::config::hooks::HooksStore::beside_settings(&settings, workspace_root.clone())
            .trust_workspace()
            .expect("trust workspace hooks");
    }
    let runtime = build_with_options(
        workspace_root.clone(),
        ProviderChoice::Sim,
        None,
        sessions_root,
        settings,
        options,
    )
    .await
    .expect("build scripted llmsim runtime");

    (runtime, workspace_root)
}

/// Drive one user turn against the scripted runtime under a wall-clock
/// timeout so a hung agent loop fails the test instead of hanging CI.
async fn run_single_turn(runtime: &BuiltRuntime, user_text: &str) -> everruns::Turn {
    let input = runtime.model.input_message(user_text.to_string());
    tokio::time::timeout(
        TURN_TIMEOUT,
        runtime
            .handles
            .execute_input(input, everruns::CancellationToken::new()),
    )
    .await
    .expect("run_turn timed out")
    .expect("run_turn errored")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scripted_get_config_capabilities_smoke() {
    let (runtime, _ws) = build_scripted_runtime(LlmSimConfig::scripted(vec![
        SimTurn::ToolCalls(vec![SimToolCall {
            name: "get_config".to_string(),
            arguments: json!({ "key": "capabilities" }),
            id: None,
        }]),
        SimTurn::Assistant("listed capabilities".to_string()),
    ]))
    .await;

    let result = run_single_turn(&runtime, "what capabilities can I configure?").await;

    assert!(
        result.success,
        "get_config capabilities smoke must succeed: {result:?}"
    );
    assert_eq!(
        result.tool_calls, 1,
        "scripted get_config must run exactly once"
    );
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
        result.tool_calls, 0,
        "Assistant-only turn must not run tools"
    );
    assert!(
        result.iterations >= 1,
        "agent loop must record at least one iteration: {result:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scripted_prompt_command_tool_queues_quit_ui_command() {
    let (mut runtime, _ws) = build_scripted_runtime_with_workspace_and_options(
        LlmSimConfig::scripted(vec![
            SimTurn::ToolCalls(vec![SimToolCall {
                name: "run_command".to_string(),
                arguments: json!({ "command": "exit" }),
                id: None,
            }]),
            SimTurn::Assistant("exiting".to_string()),
        ]),
        |_| {},
        BuildOptions {
            client_commands: true,
            ..BuildOptions::default()
        },
    )
    .await;

    let result = run_single_turn(&runtime, "exit").await;

    assert!(
        result.success,
        "prompt command turn must succeed: {result:?}"
    );
    assert_eq!(
        result.tool_calls, 1,
        "natural-language command should run through the tool entry point"
    );
    let command = runtime.ui_rx.try_recv().expect("queued UI command");
    assert_eq!(command.command, UiCommand::Quit);
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
    assert_eq!(result.tool_calls, 1, "exactly one bash invocation expected");
    assert!(
        workspace.join(marker).exists(),
        "BashTool must have run the scripted command and created {marker:?} in {workspace:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auto_worktree_gate_blocks_mutation_until_stay_in_the_agent_loop() {
    let (runtime, workspace) = build_scripted_runtime_with_workspace(
        LlmSimConfig::scripted(vec![
            SimTurn::ToolCalls(vec![SimToolCall {
                name: "write_file".to_string(),
                arguments: json!({ "path": "before.txt", "content": "blocked" }),
                id: None,
            }]),
            SimTurn::ToolCalls(vec![SimToolCall {
                name: "run_command".to_string(),
                arguments: json!({ "command": "worktree", "args": ["stay"] }),
                id: None,
            }]),
            SimTurn::ToolCalls(vec![SimToolCall {
                name: "write_file".to_string(),
                arguments: json!({ "path": "after.txt", "content": "allowed" }),
                id: None,
            }]),
            SimTurn::Assistant("made the isolation decision".to_string()),
        ]),
        |workspace| {
            assert!(
                std::process::Command::new("git")
                    .args(["init", "-q"])
                    .current_dir(workspace)
                    .status()
                    .expect("initialize repository")
                    .success()
            );
        },
    )
    .await;

    assert!(runtime.worktree.needs_explicit_init());
    let result = run_single_turn(&runtime, "make the change in this checkout").await;
    assert!(result.success, "scripted worktree decision: {result:?}");
    assert!(!workspace.join("before.txt").exists());
    assert_eq!(
        std::fs::read_to_string(workspace.join("after.txt")).expect("allowed write"),
        "allowed"
    );
    assert!(!runtime.worktree.needs_explicit_init());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repeated_read_reuse_runs_through_real_file_tools_and_invalidates_on_write() {
    use everruns_core::EventData;

    let original = "ORIGINAL-EVIDENCE\n".repeat(400);
    let changed = "CHANGED-EVIDENCE\n".repeat(400);
    let (runtime, _workspace) = build_scripted_runtime_with_workspace(
        LlmSimConfig::scripted(vec![
            SimTurn::ToolCalls(vec![SimToolCall {
                name: "read_file".to_string(),
                arguments: json!({ "path": "evidence.txt" }),
                id: None,
            }]),
            SimTurn::ToolCalls(vec![SimToolCall {
                name: "read_file".to_string(),
                arguments: json!({ "path": "evidence.txt" }),
                id: None,
            }]),
            SimTurn::ToolCalls(vec![SimToolCall {
                name: "write_file".to_string(),
                arguments: json!({ "path": "evidence.txt", "content": changed }),
                id: None,
            }]),
            SimTurn::ToolCalls(vec![SimToolCall {
                name: "read_file".to_string(),
                arguments: json!({ "path": "evidence.txt" }),
                id: None,
            }]),
            SimTurn::Assistant("used fresh evidence".to_string()),
        ]),
        |workspace| {
            std::fs::write(workspace.join("evidence.txt"), &original).expect("seed evidence");
        },
    )
    .await;

    let result = run_single_turn(&runtime, "inspect, repeat, update, and inspect").await;
    assert!(result.success, "scripted discovery turn: {result:?}");
    assert_eq!(result.tool_calls, 4);

    let events = runtime.handles.host.events().await.expect("runtime events");
    let reads = events
        .iter()
        .filter_map(|event| match &event.data {
            EventData::ToolCompleted(data) if data.tool_name == "read_file" => {
                Some(serde_json::to_string(&data.result).expect("serialize read result"))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(reads.len(), 3);
    assert!(reads[0].contains("ORIGINAL-EVIDENCE"));
    assert!(reads[1].contains("unchanged_since_last_read"));
    assert!(!reads[1].contains("ORIGINAL-EVIDENCE"));
    assert!(
        reads[2].contains("CHANGED-EVIDENCE"),
        "a real write must invalidate compact reuse: {}",
        reads[2]
    );
    assert!(!reads[2].contains("unchanged_since_last_read"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scripted_spawn_background_runs_bash_detached() {
    let marker = "spawn_background_bash.marker";
    let cmd = format!("printf background-ok > {marker}");
    let (runtime, workspace) = build_scripted_runtime(LlmSimConfig::scripted(vec![
        SimTurn::ToolCalls(vec![SimToolCall {
            name: "spawn_background".to_string(),
            arguments: json!({
                "tool": "bash",
                "args": { "command": cmd },
                "title": "write marker",
                "signal_on_completion": false
            }),
            id: None,
        }]),
        SimTurn::Assistant("background started".to_string()),
    ]))
    .await;

    assert!(
        runtime
            .startup
            .tool_names
            .contains(&"spawn_background".to_string()),
        "spawn_background should be visible when bash supports background: {:?}",
        runtime.startup.tool_names
    );

    let result = run_single_turn(&runtime, "start the background marker write").await;

    assert!(
        result.success,
        "spawn_background turn must succeed: {result:?}"
    );
    assert_eq!(
        result.tool_calls, 1,
        "the scripted turn should call spawn_background once"
    );
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if workspace.join(marker).exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("background bash did not create marker in time");
    let marker_text = tokio::fs::read_to_string(workspace.join(marker))
        .await
        .expect("read marker");
    assert_eq!(marker_text, "background-ok");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn predictable_long_command_returns_immediately_and_terminal_result_is_durable() {
    let (runtime, workspace) = build_scripted_runtime(LlmSimConfig::scripted(vec![
        SimTurn::ToolCalls(vec![SimToolCall {
            name: "spawn_background".to_string(),
            arguments: json!({
                "tool": "bash",
                "args": { "command": "while [ ! -f release-background ]; do sleep 0.01; done; printf durable-result" },
                "title": "synthetic long validation",
                "signal_on_completion": false
            }),
            id: None,
        }]),
        SimTurn::Assistant("background validation started".to_string()),
    ]))
    .await;

    let session_id = runtime.handles.session_id;
    let foreground = tokio::time::timeout(
        Duration::from_secs(10),
        run_single_turn(&runtime, "run the predictable long validation"),
    )
    .await;
    let tasks = runtime.task_registry.list(session_id, None).await;
    // Release the command before assertions, including when foreground times out.
    std::fs::write(workspace.join("release-background"), "release").unwrap();
    let turn = foreground.expect("foreground must finish while background is still gated");
    assert!(
        turn.success,
        "background launch turn must succeed: {turn:?}"
    );
    let task = tasks
        .expect("list real session tasks")
        .into_iter()
        .find(|task| task.kind == "background_tool")
        .expect("spawn_background must create a session task");
    assert!(
        !task.state.is_terminal(),
        "background must still await release"
    );
    let terminal = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let current = runtime
                .task_registry
                .get(session_id, &task.id)
                .await
                .expect("retrieve task while it runs")
                .expect("task must remain retrievable");
            if current.state.is_terminal() {
                break current;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("background command should finish");

    assert_eq!(terminal.state, SessionTaskState::Succeeded);
    assert!(
        terminal.result_path.is_some(),
        "terminal result path is delivered"
    );
    let retrieved = runtime
        .task_registry
        .get(session_id, &task.id)
        .await
        .expect("retrieve completed task")
        .expect("completed tasks must not disappear before wait_task/get_task");
    assert_eq!(retrieved.id, task.id);
    assert_eq!(retrieved.result_path, terminal.result_path);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workspace_hook_blocks_matching_bash_call() {
    let (runtime, workspace, marker) = run_git_against_workspace_hook(true).await;

    assert!(
        !workspace.join(marker).exists(),
        "pre_tool_use hook must block bash before it creates {marker:?}"
    );
    assert_eq!(
        runtime.startup.hook_count, 1,
        "workspace hook should be loaded into startup info"
    );
    assert!(runtime.startup.untrusted_hooks_notice.is_none());
}

/// A repository's hooks file is ignored until the user trusts it, and the
/// skip is reported once through startup info.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn untrusted_workspace_hook_does_not_run() {
    let (runtime, workspace, marker) = run_git_against_workspace_hook(false).await;

    assert!(
        workspace.join(marker).exists(),
        "untrusted pre_tool_use hook must not intercept bash"
    );
    assert_eq!(runtime.startup.hook_count, 0);
    let notice = runtime
        .startup
        .untrusted_hooks_notice
        .as_deref()
        .expect("skip is reported");
    assert!(notice.contains(".agents") && notice.contains("yolop config hooks trust"));
}

async fn run_git_against_workspace_hook(
    trusted: bool,
) -> (BuiltRuntime, std::path::PathBuf, &'static str) {
    let marker = "git_hook_should_block.marker";
    let cmd = format!("git status > {marker}");
    let config = LlmSimConfig::scripted(vec![
        SimTurn::ToolCalls(vec![SimToolCall {
            name: "bash".to_string(),
            arguments: json!({ "command": cmd }),
            id: None,
        }]),
        SimTurn::Assistant("git was blocked".to_string()),
    ]);
    let setup = |workspace: &std::path::Path| {
        let hooks_dir = workspace.join(".agents");
        std::fs::create_dir_all(&hooks_dir).expect("create hooks dir");
        std::fs::write(
                hooks_dir.join("hooks.json"),
                r#"{
                  "hooks": [
                    {
                      "id": "block-git",
                      "event": "pre_tool_use",
                      "matcher": {
                        "tool_name": "bash",
                        "args_jsonpath": "$.command",
                        "match_regex": "(^|[;&|()[:space:]])git([[:space:]]|$)"
                      },
                      "executor": {
                        "type": "bash",
                        "command": "printf '%s\\n' '{\"decision\":\"block\",\"reason\":\"git blocked by test hook\",\"user_message\":\"git is blocked\"}'"
                      },
                      "timeout_ms": 1000,
                      "on_error": "block",
                      "description": "Block git"
                    }
                  ]
                }"#,
            )
            .expect("write hooks config");
    };
    let (runtime, workspace) = if trusted {
        build_scripted_runtime_with_trusted_hooks(config, setup).await
    } else {
        build_scripted_runtime_with_workspace(config, setup).await
    };

    let result = run_single_turn(&runtime, "try git").await;
    assert!(result.success, "agent should continue after the tool call");
    (runtime, workspace, marker)
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
async fn ask_user_dispatch_returns_the_default_in_noninteractive_runtime() {
    let (runtime, _workspace) = build_scripted_runtime(LlmSimConfig::scripted(vec![
        SimTurn::ToolCalls(vec![SimToolCall {
            name: "ask_user".into(),
            arguments: json!({"questions": [{"id": "color", "header": "Color", "question": "Which color?", "allow_other": false, "options": [{"label": "red", "description": "red color"}, {"label": "blue", "description": "blue color", "default": true}]}]}),
            id: None,
        }]),
        SimTurn::Assistant("Default received.".into()),
    ]))
    .await;
    let result = run_single_turn(&runtime, "Ask for a color").await;
    assert!(result.success, "ask_user turn: {result:?}");
    assert_eq!(result.tool_calls, 1);
    let events = runtime.handles.host.events().await.expect("events");
    let answer = events
        .iter()
        .find_map(|event| match &event.data {
            everruns_core::EventData::ToolCompleted(data) if data.tool_name == "ask_user" => {
                Some(&data.result)
            }
            _ => None,
        })
        .expect("ask_user completed through the runtime");
    let output = serde_json::to_string(answer).expect("answer JSON");
    assert!(output.contains("blue"), "default answer: {output}");
    assert!(output.contains("unattended"), "answer origin: {output}");
    assert!(!output.contains("error"), "tool must succeed: {output}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scripted_default_repeat_last_lets_second_call_succeed_after_exhaustion() {
    // Default OnExhausted::RepeatLast: once the script is consumed the
    // last turn keeps serving. Two run_turns against a 1-turn script
    // must both succeed — proving the agent loop does not error out
    // when the script has been exhausted under the default mode.
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn print_waits_for_failed_background_work_and_continues_with_or_without_signal() {
    for signal in [true, false] {
        let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
        let config = LlmSimConfig::scripted(vec![
            SimTurn::ToolCalls(vec![SimToolCall {
                name: "spawn_background".into(),
                arguments: json!({"tool":"bash", "args":{"command":"printf FAILED_VALIDATION; exit 101"}, "title":"validate", "signal_on_completion":signal}),
                id: None,
            }]),
            // Let the detached failure settle before the first candidate final.
            SimTurn::ToolCalls(vec![SimToolCall {
                name: "bash".into(), arguments: json!({"command":"sleep 0.2"}), id: None,
            }]),
            SimTurn::Assistant("Validation is running. I will finish the repair afterward.".into()),
            // The engine reviews the first turn; print then hands off the
            // finished validation as a new turn.
            SimTurn::Assistant(r#"{"state":"blocked","reason":"waiting on the validation result"}"#.into()),
            SimTurn::ToolCalls(vec![SimToolCall {
                name: "bash".into(),
                arguments: json!({"command":"printf verified > repair.marker"}),
                id: None,
            }]),
            SimTurn::Assistant("Repair verified.".into()),
            SimTurn::Assistant(r#"{"state":"achieved","reason":"repair verified"}"#.into()),
        ]).with_on_exhausted(OnExhausted::Error).with_message_capture(captured.clone());
        let (runtime, workspace) = build_scripted_runtime(config).await;
        tokio::time::timeout(
            TURN_TIMEOUT,
            crate::run_print_mode(
                runtime,
                "Fix and verify the failing validation. ORIGINAL_PRINT_TASK".into(),
                vec![],
                None,
            ),
        )
        .await
        .expect("print must resume after background work")
        .expect("print succeeded");
        assert_eq!(
            tokio::fs::read_to_string(workspace.join("repair.marker"))
                .await
                .unwrap(),
            "verified"
        );
        let requests = captured.lock().unwrap();
        assert_eq!(
            requests.len(),
            7,
            "background waiting must not spend model calls polling"
        );
        assert!(
            requests[4]
                .iter()
                .any(|message| message.content_as_text().contains("ORIGINAL_PRINT_TASK"))
        );
        if signal {
            assert!(
                requests[4]
                    .iter()
                    .any(|message| message.content_as_text().contains("FAILED_VALIDATION"))
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn print_does_not_rehandoff_a_terminal_result_already_read_by_foreground() {
    let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
    let config = LlmSimConfig::scripted(vec![
        SimTurn::ToolCalls(vec![SimToolCall {
            name: "spawn_background".into(),
            arguments: json!({"tool":"bash","args":{"command":"printf verified"},"title":"validate","signal_on_completion":true}),
            id: None,
        }]),
        SimTurn::ToolCalls(vec![SimToolCall {
            name: "bash".into(), arguments: json!({"command":"sleep 0.2"}), id: None,
        }]),
        SimTurn::ToolCalls(vec![SimToolCall {
            name: "list_tasks".into(), arguments: json!({}), id: None,
        }]),
        SimTurn::Assistant("Verified the completed task.".into()),
        SimTurn::Assistant(r#"{"state":"achieved","reason":"verified"}"#.into()),
    ]).with_on_exhausted(OnExhausted::Error).with_message_capture(captured.clone());
    let (runtime, _workspace) = build_scripted_runtime(config).await;
    tokio::time::timeout(
        TURN_TIMEOUT,
        crate::run_print_mode(runtime, "Verify this work.".into(), vec![], None),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        captured.lock().unwrap().len(),
        5,
        "an observed terminal result must not trigger another model turn"
    );
}

/// A turn the engine's completion check reviews: it ran a tool, then gave
/// `answer`, then the review answered `review`. Each automatic continuation
/// after it repeats the answer and the review (the turn already ran tools,
/// so every later candidate is reviewed too).
fn reviewed_turns(answer: &str, review: &str, continuations: usize) -> Vec<SimTurn> {
    let mut turns = vec![SimTurn::ToolCalls(vec![SimToolCall {
        name: "get_config".to_string(),
        arguments: json!({ "key": "capabilities" }),
        id: None,
    }])];
    for _ in 0..=continuations {
        turns.push(SimTurn::Assistant(answer.into()));
        turns.push(SimTurn::Assistant(review.into()));
    }
    turns
}

fn completion_checks(
    events: &[everruns_core::Event],
) -> Vec<&everruns_core::events::TurnCompletionCheckedData> {
    events
        .iter()
        .filter_map(|event| match &event.data {
            everruns_core::EventData::TurnCompletionChecked(data) => Some(data),
            _ => None,
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn print_does_not_report_success_for_known_unfinished_work() {
    for (review, continuations, expected) in [
        (
            r#"{"state":"in_progress","reason":"validation still needs repair"}"#,
            5,
            "Automatic continuation budget exhausted",
        ),
        (
            r#"{"state":"blocked","reason":"required credentials are missing"}"#,
            0,
            "required outside input",
        ),
    ] {
        let (runtime, _workspace) = build_scripted_runtime(
            LlmSimConfig::scripted(reviewed_turns(
                "Validation still needs repair.",
                review,
                continuations,
            ))
            .with_on_exhausted(OnExhausted::Error),
        )
        .await;
        let error = crate::run_print_mode(runtime, "Fix validation".into(), vec![], None)
            .await
            .expect_err("unfinished print work must exit unsuccessfully");
        assert!(error.to_string().contains(expected), "{error}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn print_enforces_configured_continuation_limit() {
    for (limit, label) in [(1, "after 1 continuation "), (7, "after 7 continuations")] {
        let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (runtime, _) = build_scripted_runtime_with_settings(
            LlmSimConfig::scripted(reviewed_turns(
                "Work remains.",
                r#"{"state":"in_progress","reason":"verify remaining work"}"#,
                limit,
            ))
            .with_on_exhausted(OnExhausted::Error)
            .with_message_capture(captured.clone()),
            |settings| {
                settings
                    .set_continuation_max_turns(Some(limit as u64))
                    .unwrap()
            },
        )
        .await;
        let handles = runtime.handles.clone();
        let error = crate::run_print_mode(runtime, "Complete verification".into(), vec![], None)
            .await
            .expect_err("unfinished work must stop at the configured limit");
        assert!(error.to_string().contains(label), "{error}");
        assert!(
            error.to_string().contains("Send a message to resume"),
            "{error}"
        );
        assert_eq!(captured.lock().unwrap().len(), 1 + (limit + 1) * 2);
        let events = handles.host.events().await.unwrap();
        let checks = completion_checks(&events);
        assert_eq!(checks.len(), limit + 1);
        let exhausted = checks.last().unwrap();
        assert_eq!(
            exhausted.action,
            everruns_core::events::CompletionCheckAction::BudgetExhausted
        );
        assert_eq!(exhausted.reason, "verify remaining work");
        assert_eq!(exhausted.max_continuations as usize, limit);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn print_answer_is_what_follows_the_last_continuation() {
    let mut turns = reviewed_turns(
        "I'll run the remaining check now.",
        r#"{"state":"in_progress","reason":"the check has not run"}"#,
        0,
    );
    turns.extend([
        SimTurn::Assistant("The check passed.".into()),
        SimTurn::Assistant(r#"{"state":"achieved","reason":"check ran"}"#.into()),
    ]);
    let (runtime, _) =
        build_scripted_runtime(LlmSimConfig::scripted(turns).with_on_exhausted(OnExhausted::Error))
            .await;
    let handles = runtime.handles.clone();
    let model = runtime.model.clone();
    let turn = crate::collect_print_turn(&handles, &model, model.input_message("Run the check"))
        .await
        .unwrap();
    assert_eq!(turn.output, vec!["The check passed.".to_string()]);
    assert_eq!(
        turn.completion_check.unwrap().verdict,
        Some(everruns_core::events::CompletionVerdict::Achieved)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn completion_review_failure_is_durable_and_searchable() {
    let (runtime, _) = build_scripted_runtime(
        LlmSimConfig::scripted(reviewed_turns("Work remains.", "invalid review JSON", 0))
            .with_on_exhausted(OnExhausted::Error),
    )
    .await;
    let result = run_single_turn(&runtime, "Finish verification").await;
    assert!(result.success);
    let events = runtime.handles.host.events().await.unwrap();
    let check = crate::session_state::task_completion::last_check(&events, &result.turn_id)
        .expect("the engine records the failed review");
    assert_eq!(
        check.action,
        everruns_core::events::CompletionCheckAction::ReviewFailed
    );
    assert!(
        crate::session_state::task_completion::notice(check)
            .unwrap()
            .starts_with("Completion review unavailable")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn print_reviews_monitors_and_input_requests_without_waiting_for_execution() {
    use everruns_core::session_task::{CreateSessionTask, TASK_KIND_MONITOR, TaskWakePolicy};
    for (kind, state, review, success) in [
        (
            TASK_KIND_MONITOR,
            SessionTaskState::Running,
            "achieved",
            true,
        ),
        (
            "subagent",
            SessionTaskState::AwaitingInput,
            "blocked",
            false,
        ),
    ] {
        let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (runtime, _workspace) = build_scripted_runtime(
            LlmSimConfig::scripted(reviewed_turns(
                "Task setup is complete; its state is reported above.",
                &json!({"state":review,"reason":"task state reviewed"}).to_string(),
                0,
            ))
            .with_on_exhausted(OnExhausted::Error)
            .with_message_capture(captured.clone()),
        )
        .await;
        runtime
            .task_registry
            .create(CreateSessionTask {
                session_id: runtime.handles.session_id,
                id: None,
                kind: kind.into(),
                display_name: "existing session task".into(),
                spec: json!({}),
                state,
                links: Default::default(),
                wake_policy: TaskWakePolicy::Silent,
            })
            .await
            .unwrap();
        // TURN_TIMEOUT, not a tight bound: an execution wait would block far past it,
        // while a 3s limit tripped on loaded CI runners (#881, #889).
        let result = tokio::time::timeout(
            TURN_TIMEOUT,
            crate::run_print_mode(runtime, "Report the configured task".into(), vec![], None),
        )
        .await
        .expect("monitors and outside input must not cause an execution wait");
        assert_eq!(result.is_ok(), success);
        assert_eq!(
            captured.lock().unwrap().len(),
            3,
            "candidate must be reviewed"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_shell_outcome_is_a_failed_runtime_event_with_diagnostics() {
    use everruns_core::EventData;
    let (runtime, _) = build_scripted_runtime(LlmSimConfig::scripted(vec![
        SimTurn::ToolCalls(vec![SimToolCall {
            name: "bash".into(),
            arguments: json!({"command": format!("printf producer-diagnostic >&2; exit 7 # {}", "x".repeat(32 * 1024))}),
            id: None,
        }]),
        SimTurn::Assistant("Recorded the expected command failure.".into()),
    ]))
    .await;
    run_single_turn(
        &runtime,
        "Run the negative shell check and report its failure.",
    )
    .await;
    let events = runtime.handles.host.events().await.unwrap();
    let completed = events
        .iter()
        .find_map(|event| match &event.data {
            EventData::ToolCompleted(data) if data.tool_name == "bash" => Some(data),
            _ => None,
        })
        .expect("shell completion event");
    assert!(
        !completed.success,
        "a nonzero command must not be recorded as success"
    );
    assert!(
        completed
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("producer-diagnostic")
    );
    let diagnostic: serde_json::Value = serde_json::from_str(
        completed
            .error
            .as_ref()
            .unwrap()
            .split_once("Diagnostic result: ")
            .expect("shell diagnostic")
            .1,
    )
    .expect("diagnostic JSON");
    assert_eq!(diagnostic["exit_code"], 7);
    assert_eq!(diagnostic["stderr"], "producer-diagnostic");
    assert_eq!(diagnostic["command_truncated"], true);
    assert!(diagnostic["command"].as_str().unwrap().len() <= 512);
    assert!(
        events.iter().any(|event| matches!(&event.data,
            EventData::ToolStarted(data) if data.tool_call.name == "bash"
                && data.tool_call.arguments["command"].as_str().unwrap().len() > 32 * 1024
        )),
        "result previews must not rewrite approved command arguments"
    );
    assert!(
        completed.error.as_ref().unwrap().len() < 8 * 1024,
        "command text must not crowd the failure evidence out of the provider result"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn billing_failure_stops_without_retrying_the_unchanged_request() {
    use everruns_core::EventData;
    let (runtime, _) = build_scripted_runtime(
        LlmSimConfig::scripted(vec![
            SimTurn::Error(SimError::QuotaExhausted),
            SimTurn::Assistant("UNEXPECTED_BILLING_RETRY".into()),
        ])
        .with_on_exhausted(OnExhausted::Error),
    )
    .await;
    let result = run_single_turn(&runtime, "Run the billing failure check.").await;
    assert!(!result.success);
    let events = runtime.handles.host.events().await.unwrap();
    let failures = events
        .iter()
        .filter(|event| {
            matches!(&event.data,
        EventData::ReasonCompleted(data) if !data.success)
        })
        .count();
    assert_eq!(failures, 1);
    let text = serde_json::to_string(&events).unwrap();
    assert!(text.contains("provider_quota_exhausted"));
    assert!(!text.contains("UNEXPECTED_BILLING_RETRY"));
}

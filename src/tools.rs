// Bash tool for the coding CLI.
//
// Read/write/edit/list/grep/stat all live in the built-in `file_system`
// capability now that yolop selects `RealDiskFileStore` through its platform
// filesystem factory. The bash tool stays custom because the built-in `virtual_bash`
// runs commands against the VFS, not against the real workspace, and the
// security model for unsandboxed shell-on-host needs yolop-specific policy
// (timeout, output cap). See EVE-478 for the eventual runtime-side story.

use crate::workspace_host::WorkspaceHost;
use async_trait::async_trait;
use everruns_core::exec_tool_result::ExecToolResultPayload;
use everruns_core::tool_narration::ToolNarrationPhase;
use everruns_core::tool_types::ToolHints;
use everruns_core::tools::{Tool, ToolExecutionResult};
use everruns_core::{
    BackgroundEventSink, BackgroundExecutableTool, BackgroundOutcome, BackgroundProgress,
    ToolContext,
};
use serde_json::{Value, json};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::AsyncReadExt;
use tokio::process::Command;

/// Workspace context for the bash tool. The host disk repoints when a session
/// worktree is activated mid-session (EVE-660).
#[derive(Clone)]
pub struct Workspace {
    host: Arc<WorkspaceHost>,
}

impl Workspace {
    pub fn new(host: Arc<WorkspaceHost>) -> Self {
        Self { host }
    }

    #[cfg(test)]
    pub fn from_path(root: std::path::PathBuf) -> Self {
        use std::sync::RwLock;
        Self::new(Arc::new(
            WorkspaceHost::new(Arc::new(RwLock::new(root.clone())), root).expect("workspace host"),
        ))
    }
}

pub struct BashTool {
    ws: Workspace,
    timeout_secs: u64,
    max_output_bytes: usize,
}

struct BashRunOutput {
    stdout_text: String,
    stderr_text: String,
    exit_code: i32,
    out_truncated: bool,
    err_truncated: bool,
    duration: Duration,
}

impl BashTool {
    pub fn new(ws: Workspace) -> Self {
        Self {
            ws,
            timeout_secs: 120,
            max_output_bytes: 1024 * 1024,
        }
    }

    async fn run_command(
        &self,
        command: &str,
        sink: Option<Arc<dyn BackgroundEventSink>>,
    ) -> Result<BashRunOutput, ToolExecutionResult> {
        let cwd = match self.ws.host.spawn_cwd() {
            Ok(cwd) => cwd,
            Err(message) => {
                return Err(ToolExecutionResult::tool_error(message));
            }
        };
        let timeout = Duration::from_secs(self.timeout_secs);
        let max_bytes = self.max_output_bytes;

        if let Some(sink) = &sink {
            let _ = sink.status("Running bash command").await;
        }

        // kill_on_drop ensures a timed-out or canceled background command is
        // reaped when the owning future is dropped.
        let mut child = Command::new("bash")
            .arg("-lc")
            .arg(command)
            .current_dir(&cwd)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| ToolExecutionResult::tool_error(format!("spawn failed: {e}")))?;
        let mut stdout = child.stdout.take().unwrap();
        let mut stderr = child.stderr.take().unwrap();

        let start = Instant::now();
        let run = async {
            let mut out_buf = Vec::with_capacity(4096);
            let mut err_buf = Vec::with_capacity(4096);
            let mut o = vec![0u8; 4096];
            let mut e = vec![0u8; 4096];
            let mut out_truncated = false;
            let mut err_truncated = false;
            let mut out_done = false;
            let mut err_done = false;
            while !(out_done && err_done) {
                tokio::select! {
                    n = stdout.read(&mut o), if !out_done => match n {
                        Ok(0) | Err(_) => out_done = true,
                        Ok(n) => {
                            let remaining = max_bytes.saturating_sub(out_buf.len());
                            let accepted = n.min(remaining);
                            if accepted > 0 {
                                out_buf.extend_from_slice(&o[..accepted]);
                                if let Some(sink) = &sink {
                                    let text = String::from_utf8_lossy(&o[..accepted]);
                                    let _ = sink.output("stdout", &text).await;
                                }
                            }
                            if accepted < n {
                                out_truncated = true;
                                let _ = child.start_kill();
                                break;
                            }
                        },
                    },
                    n = stderr.read(&mut e), if !err_done => match n {
                        Ok(0) | Err(_) => err_done = true,
                        Ok(n) => {
                            let remaining = max_bytes.saturating_sub(err_buf.len());
                            let accepted = n.min(remaining);
                            if accepted > 0 {
                                err_buf.extend_from_slice(&e[..accepted]);
                                if let Some(sink) = &sink {
                                    let text = String::from_utf8_lossy(&e[..accepted]);
                                    let _ = sink.output("stderr", &text).await;
                                }
                            }
                            if accepted < n {
                                err_truncated = true;
                                let _ = child.start_kill();
                                break;
                            }
                        },
                    },
                }
            }
            let status = child.wait().await;
            (status, out_buf, err_buf, out_truncated, err_truncated)
        };

        let (status, out_buf, err_buf, out_truncated, err_truncated) =
            match tokio::time::timeout(timeout, run).await {
                Ok(r) => r,
                Err(_) => {
                    // The timeout is where poll loops are born: a foreground
                    // watch dies here and the model falls back to
                    // sleep-and-recheck turns. Name the escape hatch instead.
                    return Err(ToolExecutionResult::tool_error(format!(
                        "command timed out after {}s. If it was waiting on an external \
                         event (CI run, deploy, long build), re-run it detached via \
                         spawn_background and end the turn — completion wakes the agent \
                         (in one-shot mode, block on the task with wait_task instead).",
                        self.timeout_secs
                    )));
                }
            };
        Ok(BashRunOutput {
            stdout_text: String::from_utf8_lossy(&out_buf).to_string(),
            stderr_text: String::from_utf8_lossy(&err_buf).to_string(),
            exit_code: status.ok().and_then(|s| s.code()).unwrap_or(-1),
            out_truncated,
            err_truncated,
            duration: start.elapsed(),
        })
    }
}

#[async_trait]
impl Tool for BashTool {
    fn narrate(
        &self,
        tool_call: &everruns_core::tool_types::ToolCall,
        phase: ToolNarrationPhase,
        locale: Option<&str>,
    ) -> Option<String> {
        Some(everruns_core::tool_narration::narrate_shell_exec(
            &tool_call.arguments,
            self.display_name().unwrap_or("Bash"),
            phase,
            locale,
        ))
    }

    fn name(&self) -> &str {
        "bash"
    }
    fn display_name(&self) -> Option<&str> {
        Some("Bash")
    }
    fn description(&self) -> &str {
        "Run a bash command. Each call is a fresh non-interactive shell already \
         rooted at the workspace, so no state persists between calls: the working \
         directory, shell variables, and exports reset every time. A bare `cd` is \
         pointless — you are already at the workspace root; use paths relative to \
         it, or chain within one call (`cd sub && cmd`). Captures stdout/stderr \
         with configurable verbosity. 120s timeout; run commands that wait on \
         external events (CI runs, deploys) detached via `spawn_background` \
         instead."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {"type": "string", "description": "Shell command to run via bash -lc."},
                "output": everruns_core::tool_output_sanitizer::output_verbosity_schema()
            },
            "required": ["command"],
            "additionalProperties": false
        })
    }
    fn hints(&self) -> ToolHints {
        ToolHints::default()
            .with_long_running(true)
            .with_persist_output(true)
            .with_supports_background(true)
    }
    async fn execute(&self, arguments: Value) -> ToolExecutionResult {
        let command = match arguments.get("command").and_then(Value::as_str) {
            Some(c) => c.to_string(),
            None => return ToolExecutionResult::tool_error("'command' is required"),
        };
        // EVE-489: default to `auto` (persistence-first). On success, returns
        // a compact ~512 B summary while full output stays in `/outputs/` via
        // ToolOutputPersistenceCapability. On failure, returns a `normal`
        // (~8 KiB) diagnostic window. Explicit modes (silent/concise/normal/
        // verbose/full) still override this behavior.
        let output_mode = arguments
            .get("output")
            .and_then(Value::as_str)
            .unwrap_or("auto");
        let output = match self.run_command(&command, None).await {
            Ok(output) => output,
            Err(err) => return err,
        };
        let payload = ExecToolResultPayload::new(
            &output.stdout_text,
            &output.stderr_text,
            output.exit_code,
            output_mode,
        );
        let ExecToolResultPayload {
            stdout,
            stderr,
            exit_code,
            success,
            truncated,
            total_lines,
            raw_output,
        } = payload;

        ToolExecutionResult::success_with_raw_output(
            json!({
                "command": command,
                "exit_code": exit_code,
                "success": success,
                "stdout": stdout,
                "stderr": stderr,
                "truncated": truncated || output.out_truncated || output.err_truncated,
                "total_lines": total_lines,
                "output_limited": output.out_truncated || output.err_truncated,
            }),
            raw_output,
        )
    }

    fn as_background_executable(&self) -> Option<&dyn BackgroundExecutableTool> {
        Some(self)
    }
}

#[async_trait]
impl BackgroundExecutableTool for BashTool {
    async fn execute_background(
        &self,
        arguments: Value,
        _context: ToolContext,
        sink: Arc<dyn BackgroundEventSink>,
    ) -> Result<BackgroundOutcome, ToolExecutionResult> {
        let command = match arguments.get("command").and_then(Value::as_str) {
            Some(c) => c.to_string(),
            None => return Err(ToolExecutionResult::tool_error("'command' is required")),
        };
        let output_mode = arguments
            .get("output")
            .and_then(Value::as_str)
            .unwrap_or("auto");
        let output = self.run_command(&command, Some(sink.clone())).await?;
        let payload = ExecToolResultPayload::new(
            &output.stdout_text,
            &output.stderr_text,
            output.exit_code,
            output_mode,
        );
        let ExecToolResultPayload {
            stdout,
            stderr,
            exit_code,
            success,
            truncated,
            total_lines,
            raw_output,
        } = payload;
        let output_limited = output.out_truncated || output.err_truncated;
        let _ = sink
            .progress(BackgroundProgress {
                current: Some(output.duration.as_millis() as u64),
                total: None,
                unit: Some("ms".to_string()),
                label: Some("runtime".to_string()),
            })
            .await;
        let result = json!({
            "command": command,
            "exit_code": exit_code,
            "success": success,
            "stdout": stdout,
            "stderr": stderr,
            "truncated": truncated || output_limited,
            "total_lines": total_lines,
            "output_limited": output_limited,
        });

        if success {
            Ok(BackgroundOutcome {
                summary: format!(
                    "Bash command exited with code {exit_code} after {} ms",
                    output.duration.as_millis()
                ),
                result,
                raw_output: Some(raw_output),
            })
        } else {
            Err(ToolExecutionResult::tool_error(format!(
                "Bash command exited with code {exit_code} after {} ms",
                output.duration.as_millis()
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use everruns_core::capabilities::{Capability, ToolOutputPersistenceCapability};
    use everruns_core::typed_id::SessionId;
    use everruns_core::{ToolCall, ToolContext};
    use everruns_runtime::RealDiskFileStore;
    use std::sync::Mutex;

    #[test]
    fn bash_tool_requests_output_persistence() {
        let tool = BashTool::new(Workspace::from_path(std::env::current_dir().unwrap()));

        assert_eq!(tool.hints().persist_output, Some(true));
        assert_eq!(tool.hints().long_running, Some(true));
        assert_eq!(tool.hints().supports_background, Some(true));
        assert!(tool.as_background_executable().is_some());
    }

    // The description must warn that the shell is stateless and already at the
    // workspace root. A weak model (nemotron-3-ultra in the swebench matrix run)
    // burned ~30 turns issuing bare `cd <workdir>` commands expecting the cwd to
    // persist; spelling out the contract is what steers it away.
    #[test]
    fn bash_description_documents_stateless_shell() {
        let tool = BashTool::new(Workspace::from_path(std::env::current_dir().unwrap()));
        let desc = tool.description().to_lowercase();
        assert!(desc.contains("no state persists") || desc.contains("state persists"));
        assert!(desc.contains("cd"));
        assert!(desc.contains("workspace root"));
    }

    // Lock the behavior the description promises: a `cd` in one call does not
    // carry into the next; every call starts at the workspace root.
    #[tokio::test]
    async fn bash_cwd_does_not_persist_between_calls() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        let tool = BashTool::new(Workspace::from_path(dir.path().to_path_buf()));

        let ToolExecutionResult::Success(first) =
            tool.execute(json!({ "command": "cd sub && pwd" })).await
        else {
            panic!("expected success");
        };
        assert_eq!(first["success"], true);

        // The next call must be back at the root, not in `sub`.
        let ToolExecutionResult::Success(second) = tool
            .execute(json!({ "command": "basename \"$PWD\"" }))
            .await
        else {
            panic!("expected success");
        };
        let out = second["stdout"].as_str().unwrap_or("");
        let root_name = dir.path().file_name().unwrap().to_string_lossy();
        assert_eq!(
            out.trim(),
            root_name,
            "cwd should reset to the workspace root between calls"
        );
    }

    #[tokio::test]
    async fn bash_reports_missing_workspace_directory_instead_of_spawn_error() {
        let dir = tempfile::tempdir().unwrap();
        let active = Arc::new(std::sync::RwLock::new(dir.path().to_path_buf()));
        let host =
            Arc::new(WorkspaceHost::new(active.clone(), dir.path().to_path_buf()).expect("host"));
        *active.write().expect("lock") = dir.path().join("removed");
        let tool = BashTool::new(Workspace::new(host));

        let result = tool.execute(json!({ "command": "true" })).await;
        let ToolExecutionResult::ToolError(message) = result else {
            panic!("expected tool error, got {result:?}");
        };
        assert!(
            message.contains("workspace directory does not exist"),
            "got: {message}"
        );
    }

    #[tokio::test]
    async fn bash_background_executable_streams_and_returns_success_outcome() {
        #[derive(Default)]
        struct RecordingSink {
            output: Mutex<Vec<(String, String)>>,
            statuses: Mutex<Vec<String>>,
        }

        #[async_trait]
        impl BackgroundEventSink for RecordingSink {
            async fn status(&self, message: &str) -> everruns_core::Result<()> {
                self.statuses.lock().unwrap().push(message.to_string());
                Ok(())
            }

            async fn output(&self, stream: &str, delta: &str) -> everruns_core::Result<()> {
                self.output
                    .lock()
                    .unwrap()
                    .push((stream.to_string(), delta.to_string()));
                Ok(())
            }

            async fn progress(&self, _progress: BackgroundProgress) -> everruns_core::Result<()> {
                Ok(())
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let tool = BashTool::new(Workspace::from_path(dir.path().to_path_buf()));
        let sink = Arc::new(RecordingSink::default());
        let outcome = tool
            .as_background_executable()
            .expect("background executor")
            .execute_background(
                json!({ "command": "printf stdout-line; printf stderr-line >&2" }),
                ToolContext::new(SessionId::new()),
                sink.clone(),
            )
            .await
            .expect("background bash succeeds");

        assert_eq!(outcome.result["success"], true);
        assert_eq!(outcome.result["exit_code"], 0);
        assert_eq!(outcome.result["stdout"], "stdout-line");
        assert_eq!(outcome.result["stderr"], "stderr-line");
        assert!(
            sink.statuses
                .lock()
                .unwrap()
                .contains(&"Running bash command".to_string())
        );
        let output = sink.output.lock().unwrap();
        assert!(
            output
                .iter()
                .any(|(stream, chunk)| { stream == "stdout" && chunk.contains("stdout-line") })
        );
        assert!(
            output
                .iter()
                .any(|(stream, chunk)| { stream == "stderr" && chunk.contains("stderr-line") })
        );
    }

    #[tokio::test]
    async fn bash_background_executable_marks_nonzero_exit_as_failure() {
        #[derive(Default)]
        struct NoopSink;

        #[async_trait]
        impl BackgroundEventSink for NoopSink {
            async fn status(&self, _message: &str) -> everruns_core::Result<()> {
                Ok(())
            }

            async fn output(&self, _stream: &str, _delta: &str) -> everruns_core::Result<()> {
                Ok(())
            }

            async fn progress(&self, _progress: BackgroundProgress) -> everruns_core::Result<()> {
                Ok(())
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let tool = BashTool::new(Workspace::from_path(dir.path().to_path_buf()));
        let err = tool
            .as_background_executable()
            .expect("background executor")
            .execute_background(
                json!({ "command": "printf nope; exit 7" }),
                ToolContext::new(SessionId::new()),
                Arc::new(NoopSink),
            )
            .await
            .expect_err("nonzero shell exit should fail the background task");

        match err {
            ToolExecutionResult::ToolError(message) => {
                assert!(message.contains("code 7"), "got: {message}");
            }
            other => panic!("expected ToolError, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn bash_background_streaming_respects_output_cap() {
        #[derive(Default)]
        struct RecordingSink {
            output: Mutex<Vec<(String, String)>>,
        }

        #[async_trait]
        impl BackgroundEventSink for RecordingSink {
            async fn status(&self, _message: &str) -> everruns_core::Result<()> {
                Ok(())
            }

            async fn output(&self, stream: &str, delta: &str) -> everruns_core::Result<()> {
                self.output
                    .lock()
                    .unwrap()
                    .push((stream.to_string(), delta.to_string()));
                Ok(())
            }

            async fn progress(&self, _progress: BackgroundProgress) -> everruns_core::Result<()> {
                Ok(())
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let mut tool = BashTool::new(Workspace::from_path(dir.path().to_path_buf()));
        tool.max_output_bytes = 5;
        let sink = Arc::new(RecordingSink::default());
        let output = tool
            .run_command("printf 1234567890", Some(sink.clone()))
            .await
            .expect("background command should return capped output");

        assert!(output.out_truncated);
        assert_eq!(output.stdout_text, "12345");
        let streamed_stdout: String = sink
            .output
            .lock()
            .unwrap()
            .iter()
            .filter(|(stream, _)| stream == "stdout")
            .map(|(_, chunk)| chunk.as_str())
            .collect();
        assert_eq!(streamed_stdout, "12345");
    }

    #[tokio::test]
    async fn bash_tool_uses_exec_payload_shape_and_raw_output() {
        let dir = tempfile::tempdir().unwrap();
        let tool = BashTool::new(Workspace::from_path(dir.path().to_path_buf()));

        let result = tool
            .execute(json!({
                "command": "for i in {1..400}; do echo line-$i; done",
                "output": "silent"
            }))
            .await;

        let ToolExecutionResult::Success(value) = result else {
            panic!("expected success");
        };
        assert_eq!(value["exit_code"], 0);
        assert_eq!(value["success"], true);
        assert_eq!(value["total_lines"], 400);
        assert_eq!(value["truncated"], true);
        assert!(value["stdout"].as_str().unwrap().contains("line-1"));
        assert!(value["stdout"].as_str().unwrap().len() < 2048);
        assert!(value["_raw_output"].as_str().unwrap().contains("line-400"));
    }

    #[tokio::test]
    async fn bash_tool_output_persistence_hook_saves_full_output_to_outputs_folder() {
        let dir = tempfile::tempdir().unwrap();
        let tool = BashTool::new(Workspace::from_path(dir.path().to_path_buf()));
        let call = ToolCall {
            id: "call-persist".to_string(),
            name: "bash".to_string(),
            arguments: json!({
                "command": "for i in {1..3000}; do echo saved-line-$i; done",
                "output": "silent"
            }),
        };
        let mut result = tool
            .execute(call.arguments.clone())
            .await
            .into_tool_result(&call.id, &call.name);
        let file_store = Arc::new(RealDiskFileStore::new(dir.path()).unwrap());
        let context = ToolContext::with_file_store(Default::default(), file_store.clone());
        let tool_def = tool.to_definition();

        for hook in ToolOutputPersistenceCapability.post_tool_exec_hooks() {
            hook.after_exec(&call, &tool_def, &mut result, &context)
                .await;
        }

        let output_files = result
            .result
            .as_ref()
            .and_then(|value| value.get("output_files"))
            .and_then(|value| value.as_array())
            .expect("output_files should be populated");
        assert_eq!(output_files.len(), 1);
        assert_eq!(
            output_files[0].as_str(),
            Some("/workspace/outputs/call-persist.stdout")
        );

        let saved = tokio::fs::read_to_string(dir.path().join("outputs/call-persist.stdout"))
            .await
            .expect("persisted stdout should be readable from the outputs folder");
        assert!(saved.contains("saved-line-3000"));
    }

    // ====================================================================
    // EVE-489: persistence-first `auto` output mode
    // ====================================================================

    /// Issue EVE-489 reproducer: successful bash output should be a compact
    /// inline summary when full output is persisted to `/outputs/`. Before
    /// the fix, requesting `output: "normal"` returned ~8 KiB inline even
    /// though the full log was already saved. With `auto` (the new default),
    /// successful runs return ≤512 bytes inline.
    ///
    #[tokio::test]
    async fn bash_success_output_should_be_persistent_first_when_output_is_saved() {
        let dir = tempfile::tempdir().unwrap();
        let tool = BashTool::new(Workspace::from_path(dir.path().to_path_buf()));
        let call = ToolCall {
            id: "call-auto-compact".to_string(),
            name: "bash".to_string(),
            arguments: json!({
                "command": "for i in {1..2000}; do echo success-line-$i; done",
                "output": "auto"
            }),
        };
        let mut result = tool
            .execute(call.arguments.clone())
            .await
            .into_tool_result(&call.id, &call.name);
        let file_store = Arc::new(RealDiskFileStore::new(dir.path()).unwrap());
        let context = ToolContext::with_file_store(Default::default(), file_store);
        let tool_def = tool.to_definition();

        for hook in ToolOutputPersistenceCapability.post_tool_exec_hooks() {
            hook.after_exec(&call, &tool_def, &mut result, &context)
                .await;
        }

        let value = result.result.expect("bash result should be present");
        let stdout = value["stdout"].as_str().expect("stdout should be a string");
        assert_eq!(value["success"], true);
        assert!(
            value["output_files"]
                .as_array()
                .is_some_and(|files| !files.is_empty()),
            "full output should be persisted"
        );
        assert!(
            stdout.len() <= 512,
            "successful persisted bash output should be a compact inline summary, got {} bytes",
            stdout.len()
        );
    }

    #[tokio::test]
    async fn bash_defaults_to_auto_mode_for_compact_success() {
        // No `output` parameter at all — the new default must behave like `auto`.
        let dir = tempfile::tempdir().unwrap();
        let tool = BashTool::new(Workspace::from_path(dir.path().to_path_buf()));

        let result = tool
            .execute(json!({
                "command": "for i in {1..2000}; do echo line-$i; done"
            }))
            .await;

        let ToolExecutionResult::Success(value) = result else {
            panic!("expected success");
        };
        assert_eq!(value["success"], true);
        let stdout = value["stdout"].as_str().unwrap();
        assert!(
            stdout.len() <= 512,
            "default mode should compact successful output, got {} bytes",
            stdout.len()
        );
        // raw_output retains full content for persistence hook.
        let raw = value["_raw_output"].as_str().unwrap();
        assert!(raw.contains("line-2000"));
    }

    #[tokio::test]
    async fn bash_auto_failure_returns_diagnostic_inline_output() {
        let dir = tempfile::tempdir().unwrap();
        let tool = BashTool::new(Workspace::from_path(dir.path().to_path_buf()));

        // Produce lots of stdout, then exit non-zero with a useful stderr line.
        let result = tool
            .execute(json!({
                "command": "for i in {1..2000}; do echo line-$i; done; echo 'error: something broke' 1>&2; exit 7",
                "output": "auto"
            }))
            .await;

        let ToolExecutionResult::Success(value) = result else {
            panic!("expected success-wrapped tool result");
        };
        assert_eq!(value["success"], false);
        assert_eq!(value["exit_code"], 7);
        let stderr = value["stderr"].as_str().unwrap();
        assert!(
            stderr.contains("error: something broke"),
            "failure stderr should expose diagnostics inline, got: {stderr}"
        );
        let stdout = value["stdout"].as_str().unwrap();
        // Failure path should give substantially more than the success compact budget.
        assert!(
            stdout.len() > 512,
            "auto+failure stdout should not collapse to the success compact budget, got {} bytes",
            stdout.len()
        );
    }

    #[tokio::test]
    async fn bash_explicit_normal_still_returns_larger_inline_output() {
        let dir = tempfile::tempdir().unwrap();
        let tool = BashTool::new(Workspace::from_path(dir.path().to_path_buf()));

        let result = tool
            .execute(json!({
                "command": "for i in {1..2000}; do echo line-$i; done",
                "output": "normal"
            }))
            .await;

        let ToolExecutionResult::Success(value) = result else {
            panic!("expected success");
        };
        let stdout = value["stdout"].as_str().unwrap();
        // Explicit `normal` must keep the larger inline window even on success.
        assert!(
            stdout.len() > 512,
            "explicit normal should not collapse to auto-success budget, got {} bytes",
            stdout.len()
        );
        assert!(
            stdout.len() <= 8 * 1024,
            "explicit normal should respect NORMAL_BUDGET, got {} bytes",
            stdout.len()
        );
    }

    #[tokio::test]
    async fn bash_tool_missing_command_argument_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let tool = BashTool::new(Workspace::from_path(dir.path().to_path_buf()));

        let result = tool.execute(json!({})).await;
        match result {
            ToolExecutionResult::ToolError(msg) => {
                assert!(msg.contains("command"), "got: {msg}");
            }
            other => panic!("expected ToolError, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn bash_tool_non_string_command_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let tool = BashTool::new(Workspace::from_path(dir.path().to_path_buf()));

        let result = tool.execute(json!({ "command": 42 })).await;
        match result {
            ToolExecutionResult::ToolError(msg) => {
                assert!(msg.contains("command"), "got: {msg}");
            }
            other => panic!("expected ToolError, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn bash_explicit_full_returns_unlimited_inline_output() {
        let dir = tempfile::tempdir().unwrap();
        let tool = BashTool::new(Workspace::from_path(dir.path().to_path_buf()));

        let result = tool
            .execute(json!({
                "command": "for i in {1..200}; do echo line-$i; done",
                "output": "full"
            }))
            .await;

        let ToolExecutionResult::Success(value) = result else {
            panic!("expected success");
        };
        let stdout = value["stdout"].as_str().unwrap();
        // `full` must include every line — first and last.
        assert!(
            stdout.contains("line-1\n"),
            "stdout must contain first line"
        );
        assert!(stdout.contains("line-200"), "stdout must contain last line");
        assert_eq!(value["truncated"], false);
    }
}

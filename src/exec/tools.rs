// Bash tool for the coding CLI.
//
// Read/write/edit/list/grep/stat all live in the built-in `file_system`
// capability now that yolop selects `RealDiskFileStore` through its platform
// filesystem factory. The bash tool stays custom because the built-in `virtual_bash`
// runs commands against the VFS, not against the real workspace, and the
// security model for real child processes needs yolop-specific containment,
// timeout, and output policy.

use crate::config::{ApprovalPolicy, SandboxMode};
use crate::exec::sandbox::SandboxProvider;
use crate::exec::workspace_host::WorkspaceHost;
use crate::sandbox_approval::{ApprovalGate, ApprovalRequest};
use async_trait::async_trait;
use everruns_core::exec_tool_result::ExecToolResultPayload;
use everruns_core::tool_narration::ToolNarrationPhase;
use everruns_core::{
    BackgroundEventSink, BackgroundExecutableTool, BackgroundOutcome, BackgroundProgress,
    ToolContext,
};
use everruns_core::{Tool, ToolExecutionResult};
use everruns_provider::ToolHints;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::AsyncReadExt;

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
    sandbox: Arc<dyn SandboxProvider>,
    approval_policy: ApprovalPolicy,
    approval_gate: Arc<ApprovalGate>,
    control: Option<Arc<dyn crate::control::ControlService>>,
    foreground_timeout_secs: u64,
    background_timeout_secs: u64,
    max_output_bytes: usize,
}

struct BashRunOutput {
    stdout_text: String,
    stderr_text: String,
    exit_code: i32,
    out_truncated: bool,
    err_truncated: bool,
    duration: Duration,
    sandbox_mode: SandboxMode,
}

fn likely_sandbox_denial(mode: SandboxMode, exit_code: i32, stderr: &str) -> bool {
    if mode == SandboxMode::DangerFullAccess || exit_code == 0 {
        return false;
    }
    let stderr = stderr.to_ascii_lowercase();
    stderr.contains("operation not permitted") || stderr.contains("permission denied")
}

fn command_failure_hint(exit_code: i32, stderr: &str) -> Option<&'static str> {
    (exit_code == 127 && stderr.to_ascii_lowercase().contains("command not found")).then_some(
        "Command not found. Use a built-in tool when available (for repository search, use \
         grep_files first); otherwise use a broadly available fallback such as grep -R. Use \
         git grep only after confirming a Git worktree, or check PATH/install the executable \
         before retrying.",
    )
}

impl BashTool {
    #[cfg(test)]
    pub fn new(ws: Workspace) -> Self {
        // Linux's native provider re-execs the yolop binary. Unit tests run in
        // the libtest harness, so its real-binary contract lives in
        // tests/integration.rs instead.
        let mode = if cfg!(target_os = "linux") {
            SandboxMode::DangerFullAccess
        } else {
            SandboxMode::WorkspaceWrite
        };
        Self::with_policy(
            ws,
            crate::exec::sandbox::provider(mode),
            ApprovalPolicy::Never,
            ApprovalGate::deny(),
        )
    }

    pub(crate) fn with_policy(
        ws: Workspace,
        sandbox: Arc<dyn SandboxProvider>,
        approval_policy: ApprovalPolicy,
        approval_gate: Arc<ApprovalGate>,
    ) -> Self {
        Self {
            ws,
            sandbox,
            approval_policy,
            approval_gate,
            control: None,
            foreground_timeout_secs: 120,
            background_timeout_secs: 24 * 60 * 60,
            max_output_bytes: 1024 * 1024,
        }
    }

    pub(crate) fn with_control(
        mut self,
        control: Option<Arc<dyn crate::control::ControlService>>,
    ) -> Self {
        self.control = control;
        self
    }

    fn timeout_secs(&self, background: bool) -> u64 {
        if background {
            self.background_timeout_secs
        } else {
            self.foreground_timeout_secs
        }
    }

    async fn run_command(
        &self,
        command: &str,
        sink: Option<Arc<dyn BackgroundEventSink>>,
        sandbox: &Arc<dyn SandboxProvider>,
    ) -> Result<BashRunOutput, ToolExecutionResult> {
        let cwd = match self.ws.host.spawn_cwd() {
            Ok(cwd) => cwd,
            Err(message) => {
                return Err(ToolExecutionResult::tool_error(message));
            }
        };
        let timeout_secs = self.timeout_secs(sink.is_some());
        let timeout = Duration::from_secs(timeout_secs);
        let max_bytes = self.max_output_bytes;
        let sandbox_mode = sandbox.mode();

        // Attached administration is deliberately foreground-only. The
        // recognizer rejects shell composition, then the exact running Yolop
        // executable exchanges one typed request over anonymous pipes. No
        // endpoint or capability reaches the ordinary shell environment.
        if sink.is_none()
            && let Some(control) = &self.control
            && let Some(output) = crate::control::invoke_attached(command, control.clone()).await
        {
            return Ok(BashRunOutput {
                stdout_text: output.stdout,
                stderr_text: output.stderr,
                exit_code: output.exit_code,
                out_truncated: false,
                err_truncated: false,
                duration: Duration::ZERO,
                sandbox_mode,
            });
        }

        if let Some(sink) = &sink {
            let _ = sink.status("Running bash command").await;
        }

        // kill_on_drop ensures a timed-out or canceled background command is
        // reaped when the owning future is dropped.
        let mut process = sandbox
            .command(&cwd, command)
            .map_err(|e| ToolExecutionResult::tool_error(format!("sandbox setup failed: {e:#}")))?;
        crate::exec::sandbox::configure_stdio(&mut process);
        let mut child = process
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
                        timeout_secs
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
            sandbox_mode,
        })
    }

    async fn request_approval(
        &self,
        command: &str,
        reason: String,
        full_access: bool,
    ) -> Result<(), ToolExecutionResult> {
        let request = ApprovalRequest {
            command: command.to_string(),
            reason,
            full_access,
        };
        if self.approval_gate.approve(request).await {
            Ok(())
        } else {
            Err(ToolExecutionResult::tool_error(
                "shell command was not approved",
            ))
        }
    }

    async fn execute_with_policy(
        &self,
        command: &str,
        sink: Option<Arc<dyn BackgroundEventSink>>,
        request_full_access: bool,
        justification: Option<&str>,
    ) -> Result<BashRunOutput, ToolExecutionResult> {
        if sink.is_none()
            && self.control.is_some()
            && self.sandbox.mode() != SandboxMode::DangerFullAccess
            && crate::control::requires_host_approval(
                command,
                self.control.as_ref().expect("checked above").as_ref(),
            )
        {
            if self.approval_policy == ApprovalPolicy::Never {
                return Err(ToolExecutionResult::tool_error(
                    "approval_policy=never forbids attached administration outside the shell sandbox",
                ));
            }
            self.request_approval(
                command,
                justification
                    .filter(|value| !value.trim().is_empty())
                    .unwrap_or(
                        "typed Yolop administration writes or executes outside the arbitrary-shell sandbox",
                    )
                    .to_string(),
                false,
            )
            .await?;
            return self.run_command(command, sink, &self.sandbox).await;
        }

        if self.sandbox.mode() == SandboxMode::DangerFullAccess {
            if self.approval_policy == ApprovalPolicy::Untrusted && !trusted_command(command) {
                self.request_approval(
                    command,
                    "command is outside the trusted read-only set".into(),
                    false,
                )
                .await?;
            }
            return self.run_command(command, sink, &self.sandbox).await;
        }

        match self.approval_policy {
            ApprovalPolicy::Untrusted => {
                if !trusted_command(command) {
                    self.request_approval(
                        command,
                        "command is outside the trusted read-only set".into(),
                        false,
                    )
                    .await?;
                }
                self.run_command(command, sink, &self.sandbox).await
            }
            ApprovalPolicy::OnRequest if request_full_access => {
                let reason = justification
                    .filter(|value| !value.trim().is_empty())
                    .ok_or_else(|| {
                        ToolExecutionResult::tool_error(
                            "require_escalated requires a non-empty justification",
                        )
                    })?
                    .to_string();
                self.request_approval(command, reason, true).await?;
                self.run_command(
                    command,
                    sink,
                    &crate::exec::sandbox::provider(SandboxMode::DangerFullAccess),
                )
                .await
            }
            ApprovalPolicy::Never if request_full_access => Err(ToolExecutionResult::tool_error(
                "approval_policy=never forbids danger-full-access escalation",
            )),
            ApprovalPolicy::OnFailure => {
                let first = self
                    .run_command(command, sink.clone(), &self.sandbox)
                    .await?;
                if likely_sandbox_denial(first.sandbox_mode, first.exit_code, &first.stderr_text) {
                    self.request_approval(
                        command,
                        "command failed because the sandbox likely blocked it; retry with danger-full-access"
                            .into(),
                        true,
                    )
                    .await?;
                    self.run_command(
                        command,
                        sink,
                        &crate::exec::sandbox::provider(SandboxMode::DangerFullAccess),
                    )
                    .await
                } else {
                    Ok(first)
                }
            }
            ApprovalPolicy::OnRequest | ApprovalPolicy::Never => {
                self.run_command(command, sink, &self.sandbox).await
            }
        }
    }
}

fn trusted_command(command: &str) -> bool {
    if command.contains(['>', '<', ';', '&', '|', '`', '\n']) || command.contains("$(") {
        return false;
    }
    let words: Vec<&str> = command.split_whitespace().collect();
    let Some(program) = words.first().copied() else {
        return true;
    };
    match program {
        "pwd" | "ls" | "cat" | "head" | "tail" | "wc" | "rg" | "grep" | "stat" | "file"
        | "which" => true,
        "git" => words
            .get(1)
            .is_some_and(|subcommand| *subcommand == "status"),
        _ => false,
    }
}

#[async_trait]
impl Tool for BashTool {
    fn narrate(
        &self,
        tool_call: &everruns_provider::tool_types::ToolCall,
        phase: ToolNarrationPhase,
        locale: Option<&str>,
        _ctx: everruns_core::tool_narration::ToolNarrationContext<'_>,
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
        #[cfg(windows)]
        {
            "Run a PowerShell command. Each call is a fresh non-interactive shell \
             already rooted at the workspace, so no state persists between calls: \
             the working directory and shell variables reset every time. A bare \
             `cd` is pointless — you are already at the workspace root; use paths \
             relative to it, or chain within one call (`cd sub; cmd`). Captures \
             stdout/stderr with configurable verbosity. 120s timeout; run commands \
             that wait on external events (CI runs, deploys) detached via \
             `spawn_background` instead."
        }
        #[cfg(not(windows))]
        {
            "Run a bash command. Each call is a fresh non-interactive shell already \
             rooted at the workspace, so no state persists between calls: the working \
             directory, shell variables, and exports reset every time. A bare `cd` is \
             pointless — you are already at the workspace root; use paths relative to \
             it, or chain within one call (`cd sub && cmd`). Captures stdout/stderr \
             with configurable verbosity. 120s timeout; run commands that wait on \
             external events (CI runs, deploys) detached via `spawn_background` \
             instead."
        }
    }
    fn parameters_schema(&self) -> Value {
        #[cfg(windows)]
        let command_description = "Shell command to run via PowerShell.";
        #[cfg(not(windows))]
        let command_description = "Shell command to run via bash -lc.";
        json!({
            "type": "object",
            "properties": {
                "command": {"type": "string", "description": command_description},
                "sandbox_permissions": {
                    "type": "string",
                    "enum": ["use_default", "require_escalated"],
                    "description": "Use require_escalated only when the command must run with danger-full-access. The active approval policy decides whether Yolop may ask."
                },
                "justification": {"type": "string", "description": "Short user-facing reason for require_escalated."},
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
        let request_full_access = arguments.get("sandbox_permissions").and_then(Value::as_str)
            == Some("require_escalated");
        let justification = arguments.get("justification").and_then(Value::as_str);
        let output = match self
            .execute_with_policy(&command, None, request_full_access, justification)
            .await
        {
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

        let mut result = json!({
            "command": command,
            "exit_code": exit_code,
            "success": success,
            "stdout": stdout,
            "stderr": stderr,
            "truncated": truncated || output.out_truncated || output.err_truncated,
            "total_lines": total_lines,
            "output_limited": output.out_truncated || output.err_truncated,
            "sandbox": output.sandbox_mode.as_str(),
        });
        if likely_sandbox_denial(output.sandbox_mode, exit_code, &output.stderr_text) {
            result["sandbox_denial"] = json!("likely");
        }
        if let Some(hint) = command_failure_hint(exit_code, &output.stderr_text) {
            result["hint"] = json!(hint);
        }

        ToolExecutionResult::success_with_raw_output(result, raw_output)
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
        let request_full_access = arguments.get("sandbox_permissions").and_then(Value::as_str)
            == Some("require_escalated");
        let justification = arguments.get("justification").and_then(Value::as_str);
        let output = self
            .execute_with_policy(
                &command,
                Some(sink.clone()),
                request_full_access,
                justification,
            )
            .await?;
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
        let sandbox_denial =
            likely_sandbox_denial(output.sandbox_mode, exit_code, &output.stderr_text);
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
            "sandbox": output.sandbox_mode.as_str(),
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
            let hint = command_failure_hint(exit_code, &output.stderr_text)
                .map(|hint| format!(" {hint}"))
                .unwrap_or_default();
            let sandbox_hint = if sandbox_denial {
                " Native sandbox likely blocked this operation."
            } else {
                ""
            };
            Err(ToolExecutionResult::tool_error(format!(
                "Bash command exited with code {exit_code} after {} ms.{sandbox_hint}{hint}",
                output.duration.as_millis(),
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use everruns_builtins::ToolOutputPersistenceCapability;
    use everruns_core::Capability;
    use everruns_core::ToolContext;
    use everruns_host::RealDiskFileStore;
    use everruns_provider::ToolCall;
    use everruns_provider::typed_id::SessionId;
    use std::sync::Mutex;

    #[cfg(target_os = "macos")]
    fn sandbox_test_root() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("yolop-sandbox-test-")
            .tempdir_in(std::env::current_dir().unwrap())
            .expect("sandbox test root outside shared temp")
    }

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

    #[test]
    fn sandbox_denial_classifier_requires_native_mode_and_failed_os_denial() {
        use crate::config::SandboxMode;

        assert!(likely_sandbox_denial(
            SandboxMode::WorkspaceWrite,
            1,
            "touch: Operation not permitted"
        ));
        assert!(likely_sandbox_denial(
            SandboxMode::WorkspaceWrite,
            1,
            "open: Permission denied"
        ));
        assert!(!likely_sandbox_denial(
            SandboxMode::DangerFullAccess,
            1,
            "touch: Operation not permitted"
        ));
        assert!(!likely_sandbox_denial(
            SandboxMode::WorkspaceWrite,
            0,
            "Permission denied"
        ));
    }

    #[test]
    fn untrusted_policy_allowlist_is_conservative() {
        for command in ["pwd", "rg sandbox src", "git status --short"] {
            assert!(trusted_command(command), "expected trusted: {command}");
        }
        for command in [
            "cargo test",
            "git commit -m test",
            "git diff",
            "cat file > copy",
            "rg foo | head",
            "sed -i s/a/b/ file",
            "curl example.com",
        ] {
            assert!(!trusted_command(command), "expected untrusted: {command}");
        }
    }

    #[tokio::test]
    async fn never_policy_rejects_requested_escalation() {
        let dir = tempfile::tempdir().unwrap();
        let tool = BashTool::with_policy(
            Workspace::from_path(dir.path().to_path_buf()),
            crate::exec::sandbox::provider(SandboxMode::WorkspaceWrite),
            ApprovalPolicy::Never,
            ApprovalGate::deny(),
        );
        let result = tool
            .execute(json!({
                "command": "pwd",
                "sandbox_permissions": "require_escalated",
                "justification": "test"
            }))
            .await;
        assert!(
            matches!(result, ToolExecutionResult::ToolError(message) if message.contains("forbids"))
        );
    }

    #[tokio::test]
    async fn on_request_policy_gates_then_runs_full_access() {
        let dir = tempfile::tempdir().unwrap();
        let (gate, mut approvals) = ApprovalGate::channel();
        let tool = BashTool::with_policy(
            Workspace::from_path(dir.path().to_path_buf()),
            crate::exec::sandbox::provider(SandboxMode::ReadOnly),
            ApprovalPolicy::OnRequest,
            gate,
        );
        let execute = tool.execute(json!({
            "command": "printf approved > result.txt",
            "sandbox_permissions": "require_escalated",
            "justification": "write the requested result"
        }));
        let approve = async move {
            let (request, reply) = approvals.recv().await.unwrap();
            assert!(request.full_access);
            assert_eq!(request.reason, "write the requested result");
            reply
                .send(crate::sandbox_approval::ApprovalDecision::ApproveOnce)
                .unwrap();
        };
        let (result, ()) = tokio::join!(execute, approve);
        let ToolExecutionResult::Success(result) = result else {
            panic!("expected approved execution");
        };
        assert_eq!(result["sandbox"], "danger-full-access");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("result.txt")).unwrap(),
            "approved"
        );
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

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn native_sandbox_allows_workspace_writes_and_denies_outside_writes() {
        let parent = sandbox_test_root();
        let workspace = parent.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let outside = parent.path().join("outside.txt");
        let tool = BashTool::new(Workspace::from_path(workspace.clone()));

        let ToolExecutionResult::Success(inside) = tool
            .execute(json!({ "command": "printf inside > allowed.txt" }))
            .await
        else {
            panic!("expected structured result");
        };
        assert_eq!(inside["exit_code"], 0, "{inside}");
        assert_eq!(
            std::fs::read_to_string(workspace.join("allowed.txt")).unwrap(),
            "inside"
        );

        let script = format!("printf escaped > '{}'", outside.display());
        let ToolExecutionResult::Success(escaped) =
            tool.execute(json!({ "command": script })).await
        else {
            panic!("expected structured result");
        };
        assert_ne!(escaped["exit_code"], 0, "{escaped}");
        assert!(!outside.exists(), "sandbox wrote outside the workspace");
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn read_only_sandbox_denies_workspace_writes() {
        let parent = sandbox_test_root();
        let workspace = parent.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let tool = BashTool::with_policy(
            Workspace::from_path(workspace.clone()),
            crate::exec::sandbox::provider(SandboxMode::ReadOnly),
            ApprovalPolicy::Never,
            ApprovalGate::deny(),
        );

        let ToolExecutionResult::Success(result) = tool
            .execute(json!({ "command": "printf blocked > denied.txt" }))
            .await
        else {
            panic!("expected structured result");
        };
        assert_ne!(result["exit_code"], 0, "{result}");
        assert!(!workspace.join("denied.txt").exists());
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn native_sandbox_allows_shared_slash_tmp_writes() {
        let parent = sandbox_test_root();
        let workspace = parent.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let shared_temp = tempfile::Builder::new()
            .prefix("yolop-shared-tmp-test-")
            .tempdir_in("/tmp")
            .unwrap();
        let target = shared_temp.path().join("allowed.txt");
        let outside = parent.path().join("outside-via-tmp.txt");
        std::os::unix::fs::symlink(&outside, shared_temp.path().join("escape-link")).unwrap();
        let tool = BashTool::new(Workspace::from_path(workspace));

        let ToolExecutionResult::Success(result) = tool
            .execute(json!({
                "command": format!(
                    "printf shared > '{}' && ! printf escaped > '{}'",
                    target.display(),
                    shared_temp.path().join("escape-link").display()
                )
            }))
            .await
        else {
            panic!("expected structured result");
        };

        assert_eq!(result["exit_code"], 0, "{result}");
        assert_eq!(std::fs::read_to_string(target).unwrap(), "shared");
        assert!(
            !outside.exists(),
            "shared /tmp symlink escaped writable roots"
        );
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn native_sandbox_denies_git_metadata_and_path_alias_escapes() {
        let parent = sandbox_test_root();
        let workspace = parent.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::create_dir(workspace.join(".git")).unwrap();
        let outside = parent.path().join("outside.txt");
        std::fs::write(&outside, "safe").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, workspace.join("escape-link")).unwrap();
        let tool = BashTool::new(Workspace::from_path(workspace.clone()));

        let script = format!(
            "if printf git > .git/config; then exit 91; fi; if printf symlink > escape-link; then exit 92; fi; if ln '{}' hard-link 2>/dev/null; then exit 93; fi; exit 0",
            outside.display()
        );
        let ToolExecutionResult::Success(result) = tool.execute(json!({ "command": script })).await
        else {
            panic!("expected structured result");
        };
        assert_eq!(result["exit_code"], 0, "{result}");
        assert!(!workspace.join(".git/config").exists());
        assert!(!workspace.join("hard-link").exists());
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "safe");
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn native_sandbox_denies_network_connections() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let workspace = tempfile::tempdir().unwrap();
        let tool = BashTool::new(Workspace::from_path(workspace.path().to_path_buf()));
        let script = format!("exec 3<>/dev/tcp/127.0.0.1/{port}");
        let ToolExecutionResult::Success(result) = tool.execute(json!({ "command": script })).await
        else {
            panic!("expected structured result");
        };
        assert_ne!(result["exit_code"], 0, "{result}");
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn native_sandbox_follows_active_worktree_per_command() {
        let parent = sandbox_test_root();
        let root = parent.path().join("root");
        let worktree = parent.path().join("worktree");
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(&worktree).unwrap();
        let active = Arc::new(std::sync::RwLock::new(root.clone()));
        let host = Arc::new(WorkspaceHost::new(active.clone(), root.clone()).expect("host"));
        let tool = BashTool::new(Workspace::new(host));

        let ToolExecutionResult::Success(first) = tool
            .execute(json!({ "command": "printf root > marker" }))
            .await
        else {
            panic!("expected root command result");
        };
        assert_eq!(first["exit_code"], 0, "{first}");

        *active.write().expect("lock") = worktree.clone();
        let ToolExecutionResult::Success(second) = tool
            .execute(json!({ "command": "printf worktree > marker" }))
            .await
        else {
            panic!("expected worktree command result");
        };
        assert_eq!(second["exit_code"], 0, "{second}");
        assert_eq!(
            std::fs::read_to_string(root.join("marker")).unwrap(),
            "root"
        );
        assert_eq!(
            std::fs::read_to_string(worktree.join("marker")).unwrap(),
            "worktree"
        );

        let ToolExecutionResult::Success(stale) = tool
            .execute(json!({
                "command": format!("printf stale > '{}'", root.join("stale").display())
            }))
            .await
        else {
            panic!("expected stale-root command result");
        };
        assert_ne!(stale["exit_code"], 0, "{stale}");
        assert!(!root.join("stale").exists());
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
    async fn bash_command_not_found_suggests_available_search_paths() {
        let dir = tempfile::tempdir().unwrap();
        let tool = BashTool::new(Workspace::from_path(dir.path().to_path_buf()));

        let ToolExecutionResult::Success(result) = tool
            .execute(json!({"command":"yolop-command-that-does-not-exist"}))
            .await
        else {
            panic!("expected structured command result");
        };

        assert_eq!(result["exit_code"], 127);
        let hint = result["hint"].as_str().expect("command-not-found hint");
        assert!(hint.contains("grep_files first"));
        assert!(hint.contains("grep -R"));
        assert!(hint.contains("only after confirming a Git worktree"));
        assert!(hint.contains("PATH"));
    }

    #[tokio::test]
    async fn bash_background_executable_uses_detached_timeout_and_streams_output() {
        #[derive(Default)]
        struct RecordingSink {
            output: Mutex<Vec<(String, String)>>,
            statuses: Mutex<Vec<String>>,
        }

        #[async_trait]
        impl BackgroundEventSink for RecordingSink {
            async fn status(&self, message: &str) -> everruns_provider::error::Result<()> {
                self.statuses.lock().unwrap().push(message.to_string());
                Ok(())
            }

            async fn output(
                &self,
                stream: &str,
                delta: &str,
            ) -> everruns_provider::error::Result<()> {
                self.output
                    .lock()
                    .unwrap()
                    .push((stream.to_string(), delta.to_string()));
                Ok(())
            }

            async fn progress(
                &self,
                _progress: BackgroundProgress,
            ) -> everruns_provider::error::Result<()> {
                Ok(())
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let mut tool = BashTool::new(Workspace::from_path(dir.path().to_path_buf()));
        // A zero foreground timeout proves the background entry point selects
        // its independent deadline instead of inheriting the interactive one.
        tool.foreground_timeout_secs = 0;
        let sink = Arc::new(RecordingSink::default());
        let outcome = tool
            .as_background_executable()
            .expect("background executor")
            .execute_background(
                json!({ "command": "sleep 0.05; printf stdout-line; printf stderr-line >&2" }),
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
            async fn status(&self, _message: &str) -> everruns_provider::error::Result<()> {
                Ok(())
            }

            async fn output(
                &self,
                _stream: &str,
                _delta: &str,
            ) -> everruns_provider::error::Result<()> {
                Ok(())
            }

            async fn progress(
                &self,
                _progress: BackgroundProgress,
            ) -> everruns_provider::error::Result<()> {
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
            async fn status(&self, _message: &str) -> everruns_provider::error::Result<()> {
                Ok(())
            }

            async fn output(
                &self,
                stream: &str,
                delta: &str,
            ) -> everruns_provider::error::Result<()> {
                self.output
                    .lock()
                    .unwrap()
                    .push((stream.to_string(), delta.to_string()));
                Ok(())
            }

            async fn progress(
                &self,
                _progress: BackgroundProgress,
            ) -> everruns_provider::error::Result<()> {
                Ok(())
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let mut tool = BashTool::new(Workspace::from_path(dir.path().to_path_buf()));
        tool.max_output_bytes = 5;
        let sink = Arc::new(RecordingSink::default());
        let output = tool
            .run_command("printf 1234567890", Some(sink.clone()), &tool.sandbox)
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
        let expected_output = std::fs::canonicalize(dir.path())
            .expect("canonical tempdir")
            .join("outputs/call-persist.stdout");
        assert_eq!(
            output_files[0].as_str(),
            Some(expected_output.to_string_lossy().as_ref())
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

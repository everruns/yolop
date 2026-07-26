//! The one workspace tool agentyk cannot ship: a sandboxed `bash`.
//!
//! `bash` is custom for the same reason it is custom on the everruns backend —
//! real child processes need yolop's containment, timeout, and output policy,
//! and no library tool can supply those. Everything else this backend needs
//! (`read_file`, `write_file`, `edit_file`, `grep_files`, `stat_file`,
//! `list_directory`, `delete_file`) now comes from agentyk's
//! `FileSystemCapability`; the hand-written `edit_file`/`grep_files` this
//! module used to carry went upstream, which was the point of writing them
//! here first.
//!
//! The tool is also the backend's proof of the library's newer seams: it
//! streams progress while the command runs, hands back structured result data
//! for the host, and is dropped mid-command when the turn is cancelled — which
//! is what kills the child, since the command is spawned with `kill_on_drop`.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use agentyk::{
    Capability, Result as AgentykResult, Tool, ToolContext, ToolDefinition, ToolOutput,
    ToolProgress,
};
use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, BufReader};

use crate::config::SandboxMode;
use crate::exec::sandbox::{SandboxProvider, provider};

/// Ceiling on how much of a command's output goes back to the model.
const MAX_OUTPUT_BYTES: usize = 64 * 1024;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(120);
/// Report the first output line, then every Nth — enough to show a long build
/// is alive without turning the transcript into the build log.
const PROGRESS_EVERY_LINES: usize = 20;

/// The metadata key carrying risk hints, matching what agentyk's bundled
/// tools now declare for themselves.
pub const HINTS_KEY: &str = "hints";

/// Whether a tool definition declares itself read-only via the hints hatch.
/// Unknown (hint-less) tools are treated as *not* read-only — failing closed
/// is the only safe default for a tool nobody classified.
pub fn is_readonly(definition: &ToolDefinition) -> bool {
    definition.metadata[HINTS_KEY]["readonly"]
        .as_bool()
        .unwrap_or(false)
}

/// yolop's sandboxed shell, attached as a capability.
pub struct WorkspaceToolsCapability {
    tools: Vec<Arc<dyn Tool>>,
    sandbox: SandboxMode,
}

impl WorkspaceToolsCapability {
    pub fn new(workspace: PathBuf, sandbox: SandboxMode) -> Self {
        let tools: Vec<Arc<dyn Tool>> = vec![Arc::new(BashTool {
            workspace,
            sandbox: provider(sandbox),
        })];
        Self { tools, sandbox }
    }
}

#[async_trait]
impl Capability for WorkspaceToolsCapability {
    fn id(&self) -> &str {
        "workspace_tools"
    }

    fn description(&self) -> &str {
        "A sandboxed shell rooted at the workspace."
    }

    async fn system_prompt_contribution(
        &self,
        _context: &agentyk::capability::SystemPromptContext,
    ) -> Option<String> {
        Some(format!(
            "Shell commands run under containment mode `{}`.",
            self.sandbox.as_str()
        ))
    }

    async fn tools(&self) -> AgentykResult<Vec<Arc<dyn Tool>>> {
        Ok(self.tools.clone())
    }
}

struct BashTool {
    workspace: PathBuf,
    sandbox: Arc<dyn SandboxProvider>,
}

#[async_trait]
impl Tool for BashTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            "bash",
            "Run a shell command in the workspace directory. Use for git, \
             builds, tests, and anything the file tools cannot do.",
            json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "The shell command line to run."
                    }
                },
                "required": ["command"]
            }),
        )
        .with_metadata(json!({ HINTS_KEY: { "readonly": false, "destructive": true } }))
    }

    async fn execute(&self, arguments: Value, context: &ToolContext) -> ToolOutput {
        let Some(command) = arguments["command"].as_str() else {
            return ToolOutput::error("`command` is required and must be a string");
        };
        match self.run(command, context).await {
            Ok(output) => output,
            Err(error) => ToolOutput::error(format!("failed to run command: {error}")),
        }
    }
}

impl BashTool {
    async fn run(&self, command: &str, context: &ToolContext) -> anyhow::Result<ToolOutput> {
        let started = Instant::now();
        let mut spawn = self.sandbox.command(&self.workspace, command)?;
        spawn
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // The turn's cancellation drops this future; `kill_on_drop` is
            // what turns that into a dead process rather than an orphan.
            .kill_on_drop(true);
        let mut child = spawn.spawn()?;
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");

        let collect = async {
            let mut out = String::new();
            let mut err = String::new();
            let mut lines = BufReader::new(stdout).lines();
            let mut seen = 0usize;
            // Reading line by line (rather than to_end) is what makes progress
            // possible at all: the host hears about a long build while it
            // builds, not after.
            while let Some(line) = lines.next_line().await? {
                if seen.is_multiple_of(PROGRESS_EVERY_LINES) {
                    context
                        .report_progress(
                            ToolProgress::message(clip(&line))
                                .with_detail(json!({"lines": seen + 1})),
                        )
                        .await;
                }
                seen += 1;
                out.push_str(&line);
                out.push('\n');
            }
            let mut error_lines = BufReader::new(stderr).lines();
            while let Some(line) = error_lines.next_line().await? {
                err.push_str(&line);
                err.push('\n');
            }
            let status = child.wait().await?;
            Ok::<_, std::io::Error>((out, err, status))
        };

        let (out, err, status) = match tokio::time::timeout(COMMAND_TIMEOUT, collect).await {
            Ok(result) => result?,
            Err(_) => {
                return Ok(ToolOutput::error(format!(
                    "command timed out after {}s",
                    COMMAND_TIMEOUT.as_secs()
                )));
            }
        };

        let mut body = truncate(out);
        let stderr_text = truncate(err);
        if !stderr_text.trim().is_empty() {
            if !body.is_empty() && !body.ends_with('\n') {
                body.push('\n');
            }
            body.push_str(&stderr_text);
        }
        if body.trim().is_empty() {
            body = "(no output)".to_string();
        }

        let exit_code = status.code();
        let metadata = json!({
            "exit_code": exit_code,
            "duration_ms": started.elapsed().as_millis() as u64,
            "sandbox": self.sandbox.mode().as_str(),
        });

        // A non-zero exit is a tool error so the model sees the failure
        // instead of inferring it from the text.
        Ok(match status.success() {
            true => ToolOutput::text(body).with_metadata(metadata),
            false => ToolOutput::error(format!(
                "exit status {}\n{body}",
                exit_code.map_or_else(|| "signal".to_string(), |code| code.to_string())
            ))
            .with_metadata(metadata),
        })
    }
}

fn clip(line: &str) -> String {
    let line = line.trim_end();
    match line.chars().count() > 120 {
        true => format!("{}…", line.chars().take(120).collect::<String>()),
        false => line.to_string(),
    }
}

fn truncate(mut text: String) -> String {
    if text.len() <= MAX_OUTPUT_BYTES {
        return text;
    }
    let cut = (0..=MAX_OUTPUT_BYTES)
        .rev()
        .find(|index| text.is_char_boundary(*index))
        .unwrap_or(0);
    text.truncate(cut);
    text.push_str("\n… output truncated");
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentyk::{SessionId, ToolProgressSink, TurnId};
    use std::sync::Mutex;

    #[derive(Default)]
    struct Reports(Mutex<Vec<String>>);

    #[async_trait]
    impl ToolProgressSink for Reports {
        async fn progress(&self, progress: ToolProgress) {
            self.0.lock().expect("reports").push(progress.message);
        }
    }

    fn tool(workspace: &std::path::Path) -> BashTool {
        BashTool {
            workspace: workspace.to_path_buf(),
            // Tests run in the libtest harness; Linux's native provider
            // re-execs the yolop binary, which only the integration tests can
            // do. Containment itself is covered there.
            sandbox: provider(SandboxMode::DangerFullAccess),
        }
    }

    #[tokio::test]
    async fn output_and_exit_code_reach_the_model_and_the_host() {
        let dir = tempfile::tempdir().unwrap();
        let context = ToolContext::new(SessionId::new(), TurnId::new());

        let ok = tool(dir.path())
            .run("echo out; echo err >&2", &context)
            .await
            .unwrap();
        assert!(!ok.is_error, "{ok:?}");
        assert!(ok.content.contains("out") && ok.content.contains("err"));
        assert_eq!(ok.metadata["exit_code"], 0);

        let failed = tool(dir.path()).run("exit 3", &context).await.unwrap();
        assert!(failed.is_error);
        assert!(failed.content.contains("exit status 3"));
        assert_eq!(failed.metadata["exit_code"], 3);
    }

    #[tokio::test]
    async fn long_output_is_narrated_while_it_runs() {
        let dir = tempfile::tempdir().unwrap();
        let reports = Arc::new(Reports::default());
        let context = ToolContext::new(SessionId::new(), TurnId::new())
            .with_progress(reports.clone() as Arc<dyn ToolProgressSink>);

        tool(dir.path())
            .run("for i in $(seq 1 45); do echo line-$i; done", &context)
            .await
            .unwrap();

        let seen = reports.0.lock().unwrap().clone();
        // First line, then every twentieth.
        assert_eq!(seen, ["line-1", "line-21", "line-41"], "{seen:?}");
    }

    #[tokio::test]
    async fn commands_run_in_the_workspace() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("marker.txt"), "hi").unwrap();
        let context = ToolContext::new(SessionId::new(), TurnId::new());
        let output = tool(dir.path()).run("ls", &context).await.unwrap();
        assert!(output.content.contains("marker.txt"), "{output:?}");
    }

    #[test]
    fn hints_classify_the_shell_as_mutating() {
        let definition = tool(std::path::Path::new("/tmp")).definition();
        assert!(!is_readonly(&definition));
    }
}

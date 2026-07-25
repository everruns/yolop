//! The workspace tools agentyk does not ship: a sandboxed `bash`, plus
//! `edit_file` and `grep_files`.
//!
//! `bash` is custom for the same reason it is custom on the everruns backend —
//! real child processes need yolop's containment, timeout, and output policy,
//! and no library tool can supply those. `edit_file` and `grep_files` are here
//! because agentyk's `FileSystemCapability` stops at read/write/list/delete;
//! a coding agent without a targeted-edit tool rewrites whole files, and one
//! without repository search shells out for every lookup.
//!
//! Every definition carries a `hints` object in `ToolDefinition.metadata` —
//! the metadata hatch standing in for everruns' typed `ToolHints`. The
//! approval middleware reads it; the model never sees it.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use agentyk::{Capability, Result as AgentykResult, Tool, ToolContext, ToolDefinition, ToolOutput};
use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::io::AsyncReadExt;

use crate::config::SandboxMode;
use crate::exec::sandbox::{SandboxProvider, provider};

/// Ceiling on how much of a command's output goes back to the model.
const MAX_OUTPUT_BYTES: usize = 64 * 1024;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(120);
/// Bounded so one `grep_files` cannot fill the context window.
const MAX_GREP_MATCHES: usize = 200;

/// The metadata key the approval middleware reads. Mirrors everruns'
/// `ToolHints` shape closely enough to port back.
pub const HINTS_KEY: &str = "hints";

fn hints(readonly: bool, destructive: bool) -> Value {
    json!({ HINTS_KEY: { "readonly": readonly, "destructive": destructive } })
}

/// Whether a tool definition declares itself read-only via the hints hatch.
/// Unknown (hint-less) tools are treated as *not* read-only — failing closed
/// is the only safe default for a tool nobody classified.
pub fn is_readonly(definition: &ToolDefinition) -> bool {
    definition.metadata[HINTS_KEY]["readonly"]
        .as_bool()
        .unwrap_or(false)
}

/// yolop's workspace tools, attached as one capability.
pub struct WorkspaceToolsCapability {
    tools: Vec<Arc<dyn Tool>>,
    sandbox: SandboxMode,
}

impl WorkspaceToolsCapability {
    pub fn new(workspace: PathBuf, sandbox: SandboxMode) -> Self {
        let tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(BashTool {
                workspace: workspace.clone(),
                sandbox: provider(sandbox),
            }),
            Arc::new(EditFileTool {
                workspace: workspace.clone(),
            }),
            Arc::new(GrepFilesTool { workspace }),
        ];
        Self { tools, sandbox }
    }
}

#[async_trait]
impl Capability for WorkspaceToolsCapability {
    fn id(&self) -> &str {
        "workspace_tools"
    }

    fn description(&self) -> &str {
        "Sandboxed shell plus targeted file edit and repository search."
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
        .with_metadata(hints(false, true))
    }

    async fn execute(&self, arguments: Value, _context: &ToolContext) -> ToolOutput {
        let Some(command) = arguments["command"].as_str() else {
            return ToolOutput::error("`command` is required and must be a string");
        };
        match self.run(command).await {
            Ok(output) => output,
            Err(error) => ToolOutput::error(format!("failed to run command: {error}")),
        }
    }
}

impl BashTool {
    async fn run(&self, command: &str) -> anyhow::Result<ToolOutput> {
        let mut spawn = self.sandbox.command(&self.workspace, command)?;
        spawn
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = spawn.spawn()?;
        let mut stdout = child.stdout.take().expect("piped stdout");
        let mut stderr = child.stderr.take().expect("piped stderr");

        let collect = async {
            let mut out = Vec::new();
            let mut err = Vec::new();
            let (a, b) = tokio::join!(stdout.read_to_end(&mut out), stderr.read_to_end(&mut err));
            a?;
            b?;
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

        let mut body = truncate(String::from_utf8_lossy(&out).into_owned());
        let stderr_text = truncate(String::from_utf8_lossy(&err).into_owned());
        if !stderr_text.trim().is_empty() {
            if !body.is_empty() && !body.ends_with('\n') {
                body.push('\n');
            }
            body.push_str(&stderr_text);
        }
        if body.trim().is_empty() {
            body = "(no output)".to_string();
        }

        // A non-zero exit is a tool error so the model sees the failure
        // instead of inferring it from the text.
        Ok(match status.success() {
            true => ToolOutput::text(body),
            false => ToolOutput::error(format!(
                "exit status {}\n{body}",
                status
                    .code()
                    .map_or_else(|| "signal".to_string(), |code| code.to_string())
            )),
        })
    }
}

struct EditFileTool {
    workspace: PathBuf,
}

#[async_trait]
impl Tool for EditFileTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            "edit_file",
            "Replace an exact string in a file. `old_string` must appear \
             exactly once unless `replace_all` is true. Prefer this over \
             rewriting a whole file with write_file.",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Workspace-relative file path." },
                    "old_string": { "type": "string", "description": "Exact text to replace." },
                    "new_string": { "type": "string", "description": "Replacement text." },
                    "replace_all": { "type": "boolean", "description": "Replace every occurrence." }
                },
                "required": ["path", "old_string", "new_string"]
            }),
        )
        .with_metadata(hints(false, true))
    }

    async fn execute(&self, arguments: Value, _context: &ToolContext) -> ToolOutput {
        let (Some(path), Some(old), Some(new)) = (
            arguments["path"].as_str(),
            arguments["old_string"].as_str(),
            arguments["new_string"].as_str(),
        ) else {
            return ToolOutput::error("`path`, `old_string`, and `new_string` are required");
        };
        let replace_all = arguments["replace_all"].as_bool().unwrap_or(false);
        let resolved = match resolve(&self.workspace, path) {
            Ok(resolved) => resolved,
            Err(error) => return ToolOutput::error(error),
        };
        let contents = match tokio::fs::read_to_string(&resolved).await {
            Ok(contents) => contents,
            Err(error) => return ToolOutput::error(format!("read {path}: {error}")),
        };
        let occurrences = contents.matches(old).count();
        if occurrences == 0 {
            return ToolOutput::error(format!("`old_string` not found in {path}"));
        }
        if occurrences > 1 && !replace_all {
            return ToolOutput::error(format!(
                "`old_string` appears {occurrences} times in {path}; add more context or set \
                 replace_all"
            ));
        }
        let updated = match replace_all {
            true => contents.replace(old, new),
            false => contents.replacen(old, new, 1),
        };
        match tokio::fs::write(&resolved, updated).await {
            Ok(()) => ToolOutput::text(format!("edited {path} ({occurrences} replacement(s))")),
            Err(error) => ToolOutput::error(format!("write {path}: {error}")),
        }
    }
}

struct GrepFilesTool {
    workspace: PathBuf,
}

#[async_trait]
impl Tool for GrepFilesTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            "grep_files",
            "Search the workspace for a regular expression. Returns \
             `path:line: text` matches. Cheaper than shelling out to grep and \
             never needs approval.",
            json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Regular expression." },
                    "path": { "type": "string", "description": "Workspace-relative subdirectory to search." }
                },
                "required": ["pattern"]
            }),
        )
        .with_metadata(hints(true, false))
    }

    async fn execute(&self, arguments: Value, _context: &ToolContext) -> ToolOutput {
        let Some(pattern) = arguments["pattern"].as_str() else {
            return ToolOutput::error("`pattern` is required and must be a string");
        };
        let regex = match regex::Regex::new(pattern) {
            Ok(regex) => regex,
            Err(error) => return ToolOutput::error(format!("invalid pattern: {error}")),
        };
        let root = match arguments["path"].as_str() {
            Some(path) => match resolve(&self.workspace, path) {
                Ok(resolved) => resolved,
                Err(error) => return ToolOutput::error(error),
            },
            None => self.workspace.clone(),
        };
        let workspace = self.workspace.clone();
        // `ignore` walks synchronously and honors .gitignore, which is what
        // keeps target/ and node_modules/ out of the results.
        let matches = tokio::task::spawn_blocking(move || search(&workspace, &root, &regex)).await;
        match matches {
            Ok(lines) if lines.is_empty() => ToolOutput::text("(no matches)"),
            Ok(lines) => ToolOutput::text(lines.join("\n")),
            Err(error) => ToolOutput::error(format!("search failed: {error}")),
        }
    }
}

fn search(workspace: &Path, root: &Path, regex: &regex::Regex) -> Vec<String> {
    let mut results = Vec::new();
    for entry in ignore::WalkBuilder::new(root).build().flatten() {
        if results.len() >= MAX_GREP_MATCHES {
            results.push(format!(
                "… more than {MAX_GREP_MATCHES} matches, refine the pattern"
            ));
            break;
        }
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        let Ok(contents) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        let display = entry
            .path()
            .strip_prefix(workspace)
            .unwrap_or(entry.path())
            .display()
            .to_string();
        for (number, line) in contents.lines().enumerate() {
            if regex.is_match(line) {
                results.push(format!("{display}:{}: {}", number + 1, line.trim_end()));
                if results.len() >= MAX_GREP_MATCHES {
                    break;
                }
            }
        }
    }
    results
}

/// Resolve a model-supplied path inside the workspace, rejecting `..`
/// structurally rather than asking the OS whether the result escaped.
fn resolve(workspace: &Path, path: &str) -> Result<PathBuf, String> {
    let candidate = Path::new(path);
    let mut resolved = workspace.to_path_buf();
    for component in candidate.components() {
        match component {
            std::path::Component::Normal(part) => resolved.push(part),
            std::path::Component::CurDir => {}
            std::path::Component::RootDir if candidate.is_absolute() => {
                return Err(format!("`{path}` must be workspace-relative"));
            }
            _ => return Err(format!("`{path}` escapes the workspace")),
        }
    }
    Ok(resolved)
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
    use agentyk::{SessionId, TurnId};

    fn context() -> ToolContext {
        ToolContext::new(SessionId::new(), TurnId::new())
    }

    #[test]
    fn paths_cannot_escape_the_workspace() {
        let workspace = Path::new("/tmp/ws");
        assert!(resolve(workspace, "../etc/passwd").is_err());
        assert!(resolve(workspace, "/etc/passwd").is_err());
        assert_eq!(
            resolve(workspace, "src/main.rs").unwrap(),
            workspace.join("src/main.rs")
        );
    }

    #[tokio::test]
    async fn edit_requires_a_unique_match() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "x\nx\n").unwrap();
        let tool = EditFileTool {
            workspace: dir.path().to_path_buf(),
        };
        let ambiguous = tool
            .execute(
                json!({"path": "a.txt", "old_string": "x", "new_string": "y"}),
                &context(),
            )
            .await;
        assert!(ambiguous.is_error, "{ambiguous:?}");

        let all = tool
            .execute(
                json!({"path": "a.txt", "old_string": "x", "new_string": "y", "replace_all": true}),
                &context(),
            )
            .await;
        assert!(!all.is_error, "{all:?}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "y\ny\n"
        );
    }

    #[tokio::test]
    async fn grep_reports_path_and_line() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "alpha\nbeta\n").unwrap();
        let tool = GrepFilesTool {
            workspace: dir.path().to_path_buf(),
        };
        let found = tool.execute(json!({"pattern": "^bet"}), &context()).await;
        assert!(found.content.contains("a.txt:2: beta"), "{found:?}");
    }

    #[test]
    fn hints_classify_read_only_tools() {
        let grep = GrepFilesTool {
            workspace: PathBuf::from("/tmp"),
        };
        assert!(is_readonly(&grep.definition()));
        let edit = EditFileTool {
            workspace: PathBuf::from("/tmp"),
        };
        assert!(!is_readonly(&edit.definition()));
        // A tool with no hints at all is treated as unclassified, not safe.
        assert!(!is_readonly(&ToolDefinition::new("x", "", json!({}))));
    }
}

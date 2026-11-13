//! Session worktree initialization through slash command and attached CLI.

use crate::control::{
    CliCapability, ControlCapability, ControlRequest, ControlResponse, ControlRoute,
};
use crate::exec::worktree::WorktreeManager;
use async_trait::async_trait;
use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};
use everruns_contracts::{ToolCall, ToolDefinition};
use everruns_core::ToolContext;
use everruns_core::capabilities::{Capability, CapabilityStatus, SystemPromptContext};
use everruns_core::command::{
    CommandArg, CommandDescriptor, CommandExecutionContext, CommandResult, CommandSource,
    ExecuteCommandRequest,
};
use everruns_core::tool_hooks::{PreToolUseDecision, PreToolUseHook};
use everruns_core::tools::ToolExecutionResult;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;

pub(crate) const WORKTREE_CONTROL_ROUTE: ControlRoute = ControlRoute {
    resource: "worktree",
    cli_subcommand: "worktree",
    read_only_operations: &["status", "list"],
    summary: "initialize and inspect the running session worktree",
};

#[derive(Debug, Clone, Parser)]
#[command(
    name = "worktree",
    about = "Control this session's Yolop worktree",
    after_help = "Examples:\n  Preview cleanup of worktrees no longer referenced by saved sessions:\n    yolop worktree prune --dry-run\n\n  Inspect the active session's policy and workspace path:\n    yolop worktree status"
)]
struct WorktreeCommandLine {
    #[command(subcommand)]
    command: WorktreeCliCommand,
}

#[derive(Debug, Clone, Subcommand)]
enum WorktreeCliCommand {
    /// Ensure this session has a Yolop-owned worktree and make it active.
    #[command(
        after_help = "Examples:\n  Move this session into an isolated worktree:\n    yolop worktree init"
    )]
    Init,
    /// Show this session's worktree policy and active workspace.
    #[command(
        after_help = "Examples:\n  Check which directory this session edits:\n    yolop worktree status"
    )]
    Status,
    /// Stay in the current checkout without worktree isolation.
    #[command(
        after_help = "Examples:\n  Stay in the current checkout without isolation:\n    yolop worktree stay"
    )]
    Stay,
    /// List Yolop worktree directories on disk.
    #[command(
        after_help = "Examples:\n  See every worktree yolop created:\n    yolop worktree list"
    )]
    List,
    /// Remove worktrees no longer referenced by saved sessions.
    #[command(
        after_help = "Examples:\n  Preview what would be removed:\n    yolop worktree prune --dry-run"
    )]
    Prune {
        /// Preview removals without deleting anything.
        #[arg(long)]
        dry_run: bool,
        /// Session storage parent directory (default: platform data dir).
        #[arg(long)]
        session_dir: Option<std::path::PathBuf>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
enum WorktreeAction {
    Init,
    Stay,
    Status,
    List,
    Prune {
        dry_run: bool,
        session_dir: Option<std::path::PathBuf>,
    },
}

impl From<WorktreeCliCommand> for WorktreeAction {
    fn from(value: WorktreeCliCommand) -> Self {
        match value {
            WorktreeCliCommand::Init => Self::Init,
            WorktreeCliCommand::Stay => Self::Stay,
            WorktreeCliCommand::Status => Self::Status,
            WorktreeCliCommand::List => Self::List,
            WorktreeCliCommand::Prune {
                dry_run,
                session_dir,
            } => Self::Prune {
                dry_run,
                session_dir,
            },
        }
    }
}

impl WorktreeAction {
    fn request(&self) -> ControlRequest {
        ControlRequest::new(WORKTREE_CONTROL_ROUTE.resource, self)
            .expect("worktree action serializes")
    }
}

pub struct WorktreeCommandCapability {
    manager: Option<Arc<WorktreeManager>>,
}

impl WorktreeCommandCapability {
    pub fn new(manager: Arc<WorktreeManager>) -> Self {
        Self {
            manager: Some(manager),
        }
    }

    pub fn detached() -> Self {
        Self { manager: None }
    }

    fn manager(&self) -> Result<&WorktreeManager, ToolExecutionResult> {
        self.manager.as_deref().ok_or_else(|| {
            ToolExecutionResult::tool_error("worktree commands require a running Yolop session")
        })
    }

    fn status_value(&self, created: Option<bool>) -> Value {
        let manager = self.manager.as_deref().expect("session manager required");
        let info = manager.worktree_info();
        json!({
            "mode": manager.mode().as_str(),
            "state": if info.is_some() { "active" } else { "starting_checkout" },
            "created": created,
            "active_root": manager.active_root(),
            "branch": info.as_ref().map(|value| value.branch.as_str()),
            "checkpointing_available": info.is_some(),
            "needs_init": manager.needs_explicit_init(),
        })
    }

    fn execute_action(&self, action: &WorktreeAction) -> ToolExecutionResult {
        let manager = match self.manager() {
            Ok(manager) => manager,
            Err(error) => return error,
        };
        match action {
            WorktreeAction::Init => match manager.ensure_initialized(None) {
                Ok(created) => ToolExecutionResult::success(self.status_value(Some(created))),
                Err(error) => ToolExecutionResult::tool_error(error.to_string()),
            },
            WorktreeAction::Stay => {
                manager.dismiss();
                let mut value = self.status_value(Some(false));
                value["stayed"] = json!(true);
                ToolExecutionResult::success(value)
            }
            WorktreeAction::Status => ToolExecutionResult::success(self.status_value(None)),
            WorktreeAction::List | WorktreeAction::Prune { .. } => ToolExecutionResult::tool_error(
                "list and prune are standalone maintenance commands",
            ),
        }
    }
}

#[async_trait]
impl Capability for WorktreeCommandCapability {
    fn id(&self) -> &str {
        "worktree"
    }
    fn name(&self) -> &str {
        "Session Worktree"
    }
    fn description(&self) -> &str {
        "Initializes and reports the Yolop-owned worktree for the current session."
    }
    fn status(&self) -> CapabilityStatus {
        CapabilityStatus::Available
    }
    fn category(&self) -> Option<&str> {
        Some("Workspace")
    }

    fn commands(&self) -> Vec<CommandDescriptor> {
        vec![CommandDescriptor {
            name: "worktree".to_string(),
            description: "initialize or inspect this session's Yolop-owned worktree".to_string(),
            source: CommandSource::System,
            args: vec![CommandArg {
                name: "action".to_string(),
                description: "init (default), stay, or status".to_string(),
                required: false,
                suggestions: vec!["init".to_string(), "stay".to_string(), "status".to_string()],
            }],
        }]
    }

    async fn execute_command(
        &self,
        request: &ExecuteCommandRequest,
        _ctx: &CommandExecutionContext,
    ) -> everruns_contracts::error::Result<CommandResult> {
        let action = match request.arguments.as_deref().map(str::trim) {
            None | Some("") | Some("init") => WorktreeAction::Init,
            Some("stay") => WorktreeAction::Stay,
            Some("status") => WorktreeAction::Status,
            Some(_) => {
                return Ok(CommandResult {
                    success: false,
                    message: "usage: /worktree [init|stay|status]".to_string(),
                    error_code: None,
                    error_fields: None,
                });
            }
        };
        let response = ControlResponse::from_tool_result(self.execute_action(&action));
        Ok(CommandResult {
            success: response.ok,
            message: self.render_control(
                &serde_json::to_value(action).unwrap_or(Value::Null),
                &response,
            ),
            error_code: None,
            error_fields: None,
        })
    }

    fn pre_tool_use_hooks(&self) -> Vec<Arc<dyn PreToolUseHook>> {
        match self.manager.clone() {
            Some(manager) => vec![Arc::new(WorktreeGateHook { manager })],
            None => Vec::new(),
        }
    }

    async fn system_prompt_contribution(&self, _ctx: &SystemPromptContext) -> Option<String> {
        let manager = self.manager.as_deref()?;
        if !manager.needs_explicit_init() {
            return None;
        }
        Some(
            "<capability id=\"session_worktree\">\n\
Before the first repository mutation, decide: run `yolop worktree init` in Bash to work isolated, \
or `yolop worktree stay` to deliberately keep working in the current checkout. \
A host gate blocks file edits and shell commands until one of the two runs. \
Do not initialize for read-only investigation, explanation, review, or planning. \
After initialization, continue in the active workspace reported by the command. \
Do not create, switch, move, or remove the session worktree with raw `git worktree` commands.\n\
</capability>"
                .to_string(),
        )
    }
}

#[async_trait]
impl ControlCapability for WorktreeCommandCapability {
    fn control_route(&self) -> ControlRoute {
        WORKTREE_CONTROL_ROUTE
    }

    async fn execute_control(&self, action: &Value) -> ToolExecutionResult {
        match serde_json::from_value::<WorktreeAction>(action.clone()) {
            Ok(action) => self.execute_action(&action),
            Err(error) => {
                ToolExecutionResult::tool_error(format!("invalid worktree action: {error}"))
            }
        }
    }

    fn render_control(&self, _action: &Value, response: &ControlResponse) -> String {
        if !response.ok {
            return response.render_default();
        }
        let value = response.value.as_ref().unwrap_or(&Value::Null);
        if value.get("stayed").and_then(serde_json::Value::as_bool) == Some(true) {
            return "staying in the current checkout without worktree isolation".to_string();
        }
        let state = value["state"].as_str().unwrap_or("unknown");
        let root = value["active_root"].as_str().unwrap_or("unknown");
        match value["created"].as_bool() {
            Some(true) => format!("initialized session worktree at {root}"),
            Some(false) => format!("session worktree already active at {root}"),
            None => format!(
                "worktree mode: {}; state: {state}; active root: {root}",
                value["mode"].as_str().unwrap_or("unknown")
            ),
        }
    }
}

#[async_trait]
impl CliCapability for WorktreeCommandCapability {
    fn cli_command(&self) -> clap::Command {
        WorktreeCommandLine::command()
    }

    fn control_request_from_cli(
        &self,
        matches: &clap::ArgMatches,
    ) -> anyhow::Result<ControlRequest> {
        let parsed = WorktreeCommandLine::from_arg_matches(matches)?;
        Ok(WorktreeAction::from(parsed.command).request())
    }

    async fn execute_cli(&self, request: &ControlRequest) -> anyhow::Result<()> {
        let action: WorktreeAction = serde_json::from_value(request.action.clone())?;
        match action {
            WorktreeAction::List => {
                for entry in crate::exec::worktree::list_worktree_paths_on_disk()? {
                    println!("{}", entry.display());
                }
                Ok(())
            }
            WorktreeAction::Prune {
                dry_run,
                session_dir,
            } => {
                let sessions_dir = match session_dir {
                    Some(path) => path,
                    None => crate::runtime::session_log::default_sessions_dir()?,
                };
                let report = crate::exec::worktree::prune_orphan_worktrees(&sessions_dir, dry_run)?;
                for path in &report.removed {
                    println!(
                        "{} {}",
                        if dry_run { "would remove" } else { "removed" },
                        path.display()
                    );
                }
                if report.errors.is_empty() {
                    Ok(())
                } else {
                    anyhow::bail!("{} worktree removal(s) failed", report.errors.len())
                }
            }
            WorktreeAction::Init | WorktreeAction::Stay | WorktreeAction::Status => {
                anyhow::bail!("worktree init, stay, and status require a running Yolop session")
            }
        }
    }
}

/// Deterministic first-mutation gate for `auto` mode.
///
/// Owned by the worktree capability so no new registration is needed: the same
/// capability that runs `init`/`stay` also blocks file edits and shell commands
/// while `needs_explicit_init` holds. Bash that only drives the worktree
/// decision itself (`yolop worktree ...`) passes through to avoid deadlock.
struct WorktreeGateHook {
    manager: Arc<WorktreeManager>,
}

/// Tools that count as a first repository mutation in `auto` mode.
const WORKTREE_GATED_TOOLS: &[&str] = &[
    "edit_file",
    "write_file",
    "delete_file",
    "apply_patch",
    "ast_edit",
    "bash",
];

impl WorktreeGateHook {
    fn is_worktree_command(tool_call: &ToolCall) -> bool {
        tool_call
            .arguments
            .get("command")
            .and_then(serde_json::Value::as_str)
            .map(|command| command.contains("yolop worktree"))
            .unwrap_or(false)
    }

    fn block_message(manager: &WorktreeManager) -> (String, String) {
        let repo = manager.active_root().display().to_string();
        let reason = format!(
            "session is in repo_root {repo} without worktree isolation; decide init vs stay before the first mutation"
        );
        let user_message = format!(
            "You are working directly in the repository checkout at {repo} with no worktree yet. Before the first file edit or shell command, make the isolation decision: run `yolop worktree init` in Bash to move into an isolated worktree, or `yolop worktree stay` to deliberately keep editing {repo} directly. If the request is read-only (investigation, explanation, review, planning), use read-only tools instead and skip both."
        );
        (reason, user_message)
    }
}

#[async_trait::async_trait]
impl PreToolUseHook for WorktreeGateHook {
    async fn before_exec(
        &self,
        tool_call: ToolCall,
        _tool_def: &ToolDefinition,
        _context: &ToolContext,
    ) -> PreToolUseDecision {
        if !self.manager.needs_explicit_init() {
            return PreToolUseDecision::Continue(tool_call);
        }
        if !WORKTREE_GATED_TOOLS.contains(&tool_call.name.as_str()) {
            return PreToolUseDecision::Continue(tool_call);
        }
        if tool_call.name == "bash" && Self::is_worktree_command(&tool_call) {
            return PreToolUseDecision::Continue(tool_call);
        }
        let (reason, user_message) = Self::block_message(&self.manager);
        PreToolUseDecision::Block {
            reason,
            tool_call,
            user_message: Some(user_message),
        }
    }
}

pub(crate) type WorktreeCapability = WorktreeCommandCapability;

#[cfg(test)]
mod gate_tests {
    use super::*;
    use crate::capabilities::apply_patch::ApplyPatchTool;
    use crate::config::WorktreesMode;
    use everruns_contracts::typed_id::SessionId;
    use everruns_core::Tool as _;

    fn test_manager() -> Arc<WorktreeManager> {
        let dir = tempfile::tempdir().expect("temp dir");
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).expect("repo dir");
        let session_dir = dir.path().join("session");
        std::fs::create_dir_all(&session_dir).expect("session dir");
        Arc::new(WorktreeManager::new(
            WorktreesMode::Auto,
            Some(repo.clone()),
            repo,
            SessionId::new(),
            session_dir,
            None,
        ))
    }

    fn tool_call(name: &str, arguments: serde_json::Value) -> ToolCall {
        ToolCall {
            id: format!("call-{name}"),
            name: name.to_string(),
            arguments,
        }
    }

    fn tool_def() -> ToolDefinition {
        ApplyPatchTool.to_definition()
    }

    fn test_context() -> ToolContext {
        ToolContext::new(SessionId::new())
    }

    #[tokio::test]
    async fn blocks_first_edit_until_init_or_stay() {
        let manager = test_manager();
        let hook = WorktreeGateHook {
            manager: manager.clone(),
        };
        let context = test_context();

        assert!(manager.needs_explicit_init());
        let decision = hook
            .before_exec(
                tool_call("edit_file", serde_json::json!({"path": "a.rs"})),
                &tool_def(),
                &context,
            )
            .await;
        assert!(matches!(decision, PreToolUseDecision::Block { .. }));

        manager.dismiss();
        assert!(!manager.needs_explicit_init());
        let decision = hook
            .before_exec(
                tool_call("edit_file", serde_json::json!({"path": "a.rs"})),
                &tool_def(),
                &context,
            )
            .await;
        assert!(matches!(decision, PreToolUseDecision::Continue(..)));
    }

    #[tokio::test]
    async fn allows_reads_and_worktree_command_bash() {
        let manager = test_manager();
        let hook = WorktreeGateHook { manager };
        let context = test_context();

        let decision = hook
            .before_exec(
                tool_call("read_file", serde_json::json!({"path": "a.rs"})),
                &tool_def(),
                &context,
            )
            .await;
        assert!(matches!(decision, PreToolUseDecision::Continue(..)));

        let decision = hook
            .before_exec(
                tool_call(
                    "bash",
                    serde_json::json!({"command": "yolop worktree init"}),
                ),
                &tool_def(),
                &context,
            )
            .await;
        assert!(matches!(decision, PreToolUseDecision::Continue(..)));

        let decision = hook
            .before_exec(
                tool_call("bash", serde_json::json!({"command": "cargo test"})),
                &tool_def(),
                &context,
            )
            .await;
        assert!(matches!(decision, PreToolUseDecision::Block { .. }));
    }

    #[test]
    fn capability_registers_gate_hook_when_attached() {
        let manager = test_manager();
        let attached = WorktreeCommandCapability::new(manager);
        assert_eq!(attached.pre_tool_use_hooks().len(), 1);
        assert!(
            WorktreeCommandCapability::detached()
                .pre_tool_use_hooks()
                .is_empty()
        );
    }

    #[test]
    fn cli_surface_offers_init_and_stay() {
        use clap::CommandFactory;
        let names: Vec<String> = WorktreeCommandLine::command()
            .get_subcommands()
            .map(|sub| sub.get_name().to_string())
            .collect();
        assert!(names.contains(&"init".to_string()), "{names:?}");
        assert!(names.contains(&"stay".to_string()), "{names:?}");
    }
}

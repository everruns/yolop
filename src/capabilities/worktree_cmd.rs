//! Session worktree initialization through slash command and attached CLI.

use crate::control::{
    CliCapability, ControlCapability, ControlRequest, ControlResponse, ControlRoute,
};
use crate::exec::worktree::WorktreeManager;
use async_trait::async_trait;
use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};
use everruns_core::capabilities::{Capability, CapabilityStatus, SystemPromptContext};
use everruns_core::command::{
    CommandArg, CommandDescriptor, CommandExecutionContext, CommandResult, CommandSource,
    ExecuteCommandRequest,
};
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
    Init,
    /// Show this session's worktree policy and active workspace.
    Status,
    /// List Yolop worktree directories on disk.
    List,
    /// Remove worktrees no longer referenced by saved sessions.
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
                description: "init (default) or status".to_string(),
                required: false,
                suggestions: vec!["init".to_string(), "status".to_string()],
            }],
        }]
    }

    async fn execute_command(
        &self,
        request: &ExecuteCommandRequest,
        _ctx: &CommandExecutionContext,
    ) -> everruns_provider::error::Result<CommandResult> {
        let action = match request.arguments.as_deref().map(str::trim) {
            None | Some("") | Some("init") => WorktreeAction::Init,
            Some("status") => WorktreeAction::Status,
            Some(_) => {
                return Ok(CommandResult {
                    success: false,
                    message: "usage: /worktree [init|status]".to_string(),
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

    async fn system_prompt_contribution(&self, _ctx: &SystemPromptContext) -> Option<String> {
        let manager = self.manager.as_deref()?;
        if !manager.needs_explicit_init() {
            return None;
        }
        Some(
            "<capability id=\"session_worktree\">\n\
Before the first repository mutation, run `yolop worktree init` as a direct foreground Bash command. \
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
            WorktreeAction::Init | WorktreeAction::Status => {
                anyhow::bail!("worktree init and status require a running Yolop session")
            }
        }
    }
}

pub(crate) type WorktreeCapability = WorktreeCommandCapability;

//! Session checkpoint commands and the conversational restore tool.

use crate::session_state::checkpoint::{CheckpointManager, RestoreMode, safe_display};
use async_trait::async_trait;
use everruns_core::command::{
    CommandArg, CommandDescriptor, CommandExecutionContext, CommandResult, CommandSource,
    ExecuteCommandRequest,
};
use everruns_core::{Capability, CapabilityStatus};
use everruns_core::{Tool, ToolExecutionResult};
use everruns_provider::{ToolCall, ToolHints};
use serde_json::{Value, json};
use std::sync::Arc;

pub(crate) const CHECKPOINT_CAPABILITY_ID: &str = "yolop_checkpoint";

pub(crate) struct CheckpointCapability {
    pub(crate) manager: Arc<CheckpointManager>,
    pub(crate) workspace_only: bool,
}

#[async_trait]
impl Capability for CheckpointCapability {
    fn id(&self) -> &str {
        CHECKPOINT_CAPABILITY_ID
    }

    fn name(&self) -> &str {
        "Session Checkpoints"
    }

    fn description(&self) -> &str {
        "List, preview, confirm, undo, redo, or rewind durable session checkpoints."
    }

    fn status(&self) -> CapabilityStatus {
        CapabilityStatus::Available
    }

    fn category(&self) -> Option<&str> {
        Some("Sessions")
    }

    fn system_prompt_addition(&self) -> Option<&str> {
        // `manage_checkpoint`'s description carries the preview → confirm → queue
        // contract, including the worktree limit on workspace restore.
        None
    }

    fn commands(&self) -> Vec<CommandDescriptor> {
        vec![
            command(
                "rewind",
                "list or restore an earlier turn",
                self.workspace_only,
            ),
            command(
                "undo",
                "restore the state before the latest turn",
                self.workspace_only,
            ),
            command(
                "redo",
                "restore the most recently abandoned branch",
                self.workspace_only,
            ),
        ]
    }

    fn tools(&self) -> Vec<Box<dyn Tool>> {
        vec![Box::new(ManageCheckpointTool {
            manager: self.manager.clone(),
            workspace_only: self.workspace_only,
        })]
    }

    async fn execute_command(
        &self,
        request: &ExecuteCommandRequest,
        _ctx: &CommandExecutionContext,
    ) -> everruns_provider::error::Result<CommandResult> {
        let result = execute_command(
            &self.manager,
            &request.name,
            request.arguments.as_deref(),
            self.workspace_only,
        )
        .await
        .map_err(|error| {
            everruns_provider::error::AgentLoopError::config(safe_display(&format!("{error:#}")))
        })?;
        Ok(CommandResult {
            success: true,
            message: result,
            error_code: None,
            error_fields: None,
        })
    }
}

fn command(name: &str, description: &str, workspace_only: bool) -> CommandDescriptor {
    CommandDescriptor {
        name: name.to_string(),
        description: description.to_string(),
        source: CommandSource::System,
        args: vec![CommandArg {
            name: "checkpoint, mode, or confirmation token".to_string(),
            description: if workspace_only {
                "ACP restores workspace state only. Use `confirm <token>` after preview."
                    .to_string()
            } else {
                "Modes: conversation, workspace, both. Use `confirm <token>` after preview."
                    .to_string()
            },
            required: false,
            suggestions: if workspace_only {
                vec!["workspace".to_string()]
            } else {
                vec![
                    "conversation".to_string(),
                    "workspace".to_string(),
                    "both".to_string(),
                ]
            },
        }],
    }
}

async fn execute_command(
    manager: &CheckpointManager,
    name: &str,
    arguments: Option<&str>,
    workspace_only: bool,
) -> anyhow::Result<String> {
    let words = arguments
        .unwrap_or_default()
        .split_whitespace()
        .collect::<Vec<_>>();
    if words.first().is_some_and(|word| *word == "confirm") {
        let token = words
            .get(1)
            .ok_or_else(|| anyhow::anyhow!("confirmation token is required"))?;
        return manager.confirm(token).await;
    }
    match name {
        "rewind" if words.is_empty() => Ok(render_checkpoints(manager, workspace_only)),
        "rewind" => {
            let checkpoint = words[0];
            let mode = resolve_mode(
                words.get(1).copied(),
                manager.default_rewind_mode(checkpoint),
                workspace_only,
            )?;
            Ok(manager.prepare_rewind(checkpoint, mode)?.render())
        }
        "undo" => {
            let mode = resolve_mode(
                words.first().copied(),
                manager.default_undo_mode(),
                workspace_only,
            )?;
            Ok(manager.prepare_undo(mode)?.render())
        }
        "redo" => {
            let mode = resolve_mode(
                words.first().copied(),
                manager.default_redo_mode(),
                workspace_only,
            )?;
            Ok(manager.prepare_redo(mode)?.render())
        }
        _ => anyhow::bail!("unknown checkpoint command `/{name}`"),
    }
}

fn resolve_mode(
    requested: Option<&str>,
    default: RestoreMode,
    workspace_only: bool,
) -> anyhow::Result<RestoreMode> {
    let mode = requested
        .map(|mode| RestoreMode::parse(Some(mode)))
        .transpose()?
        .unwrap_or(if workspace_only {
            RestoreMode::Workspace
        } else {
            default
        });
    if workspace_only && mode != RestoreMode::Workspace {
        anyhow::bail!(
            "ACP can restore workspace state only; conversation restore is unavailable because ACP cannot replace the client transcript"
        );
    }
    Ok(mode)
}

fn render_checkpoints(manager: &CheckpointManager, workspace_only: bool) -> String {
    let checkpoints = manager.list();
    if checkpoints.is_empty() {
        return "no checkpoints yet".to_string();
    }
    let mut lines = vec!["checkpoints (newest first):".to_string()];
    lines.extend(checkpoints.into_iter().map(|checkpoint| {
        let workspace = if checkpoint.workspace_available && workspace_only {
            "workspace"
        } else if checkpoint.workspace_available {
            "conversation + workspace"
        } else if workspace_only {
            "workspace unavailable"
        } else {
            "conversation only"
        };
        let reason = checkpoint
            .workspace_error
            .map(|error| format!(" ({error})"))
            .unwrap_or_default();
        format!(
            "  {} · {}{} · {}",
            checkpoint.id, workspace, reason, checkpoint.prompt
        )
    }));
    lines.push(if workspace_only {
        "preview with `/rewind <id> workspace`".to_string()
    } else {
        "preview with `/rewind <id> [conversation|workspace|both]`".to_string()
    });
    lines.join("\n")
}

struct ManageCheckpointTool {
    manager: Arc<CheckpointManager>,
    workspace_only: bool,
}

#[async_trait]
impl Tool for ManageCheckpointTool {
    fn name(&self) -> &str {
        "manage_checkpoint"
    }

    fn description(&self) -> &str {
        if self.workspace_only {
            "List or preview workspace undo/redo/rewind. ACP cannot replace its client transcript, \
             so conversation and both-state restore modes are unavailable. Preview returns a \
             confirmation token; describe it and only confirm after the user explicitly agrees. \
             Workspace restore is available only in a Yolop-owned worktree."
        } else {
            "List or preview session undo/redo/rewind — use this when the user asks to undo, redo, \
             rewind, or roll back the session. Preview returns a confirmation token; describe the \
             preview and only use operation=confirm after the user explicitly confirms. Confirmation \
             is queued until the current turn ends so model history is never mutated mid-turn. \
             Workspace restore is available only in a Yolop-owned worktree."
        }
    }

    fn parameters_schema(&self) -> Value {
        let modes = if self.workspace_only {
            json!(["workspace"])
        } else {
            json!(["conversation", "workspace", "both"])
        };
        json!({
            "type": "object",
            "properties": {
                "operation": {
                    "type": "string",
                    "enum": ["list", "undo", "redo", "rewind", "confirm"]
                },
                "checkpoint_id": { "type": "string" },
                "mode": {
                    "type": "string",
                    "enum": modes
                },
                "token": { "type": "string" }
            },
            "required": ["operation"],
            "additionalProperties": false
        })
    }

    fn hints(&self) -> ToolHints {
        ToolHints::default().with_idempotent(false)
    }

    fn narrate(
        &self,
        tool_call: &ToolCall,
        phase: everruns_core::tool_narration::ToolNarrationPhase,
        _locale: Option<&str>,
        _ctx: everruns_core::tool_narration::ToolNarrationContext<'_>,
    ) -> Option<String> {
        let operation = tool_call
            .arguments
            .get("operation")
            .and_then(Value::as_str)
            .unwrap_or("checkpoint");
        Some(match phase {
            everruns_core::tool_narration::ToolNarrationPhase::Started => {
                format!("Checkpoint · {operation}")
            }
            _ => format!("Checkpoint · {operation} complete"),
        })
    }

    async fn execute(&self, arguments: Value) -> ToolExecutionResult {
        let result = (|| -> anyhow::Result<Value> {
            let operation = arguments
                .get("operation")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("operation is required"))?;
            if operation == "list" {
                return Ok(json!({
                    "message": render_checkpoints(&self.manager, self.workspace_only)
                }));
            }
            if operation == "confirm" {
                let token = arguments
                    .get("token")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("token is required"))?;
                self.manager.queue_confirmation(token)?;
                return Ok(json!({
                    "queued": true,
                    "message": "restore queued for after this turn"
                }));
            }
            let preview = match operation {
                "undo" => {
                    let mode = resolve_mode(
                        arguments.get("mode").and_then(Value::as_str),
                        self.manager.default_undo_mode(),
                        self.workspace_only,
                    )?;
                    self.manager.prepare_undo(mode)?
                }
                "redo" => {
                    let mode = resolve_mode(
                        arguments.get("mode").and_then(Value::as_str),
                        self.manager.default_redo_mode(),
                        self.workspace_only,
                    )?;
                    self.manager.prepare_redo(mode)?
                }
                "rewind" => {
                    let checkpoint = arguments
                        .get("checkpoint_id")
                        .and_then(Value::as_str)
                        .ok_or_else(|| anyhow::anyhow!("checkpoint_id is required"))?;
                    let mode = resolve_mode(
                        arguments.get("mode").and_then(Value::as_str),
                        self.manager.default_rewind_mode(checkpoint),
                        self.workspace_only,
                    )?;
                    self.manager.prepare_rewind(checkpoint, mode)?
                }
                other => anyhow::bail!("unknown operation `{other}`"),
            };
            Ok(json!({
                "token": preview.token,
                "checkpoint_id": preview.checkpoint_id,
                "mode": format!("{:?}", preview.mode).to_lowercase(),
                "changed_paths": preview.changed_paths,
                "message": preview.render()
            }))
        })();
        match result {
            Ok(value) => ToolExecutionResult::success(value),
            Err(error) => ToolExecutionResult::tool_error(safe_display(&format!("{error:#}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_descriptors_expose_restore_modes() {
        let descriptor = command("undo", "undo", false);
        assert_eq!(descriptor.source, CommandSource::System);
        assert!(descriptor.args[0].suggestions.contains(&"both".to_string()));
    }

    #[test]
    fn acp_checkpoint_surface_is_workspace_only() {
        let descriptor = command("undo", "undo", true);
        assert_eq!(descriptor.args[0].suggestions, vec!["workspace"]);
        assert_eq!(
            resolve_mode(None, RestoreMode::Both, true).unwrap(),
            RestoreMode::Workspace
        );
        let error = resolve_mode(Some("conversation"), RestoreMode::Both, true).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("cannot replace the client transcript")
        );
    }
}

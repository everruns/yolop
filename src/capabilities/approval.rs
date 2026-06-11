// The `approval` capability — yolop's soft-approval (spoken-consent) layer.
//
// "Soft approval" is prompt-engineering, not a hard permission gate. Rather
// than block each tool call behind an interactive yes/no, this capability
// injects guidance into the system prompt that asks the model to:
//
//   * batch safe / read-only work and run it without interruption;
//   * recognize the small set of critical actions (destructive, irreversible,
//     or outward-facing) and, before those, state a brief justification and
//     ask the user for approval in plain language;
//   * treat an affirmative chat reply ("yes", "approved", "go ahead") as the
//     approval — there is no separate UI to click;
//   * record each granted approval with `record_approval`, which lands a
//     `tool.completed` line in the per-session `events.jsonl` audit log.
//
// The paranoia level is central configuration (`approval_mode` in
// settings.toml, see `crate::settings::ApprovalMode`), surfaced in the status
// bar, switchable with `/setup approval <level>` and — because users address
// yolop in natural language ("yolop, be more careful") — with the
// `set_approval_mode` tool.

use crate::config_service::ConfigService;
use crate::settings::{ApprovalMode, SettingsStore};
use async_trait::async_trait;
use everruns_core::capabilities::{Capability, CapabilityStatus, SystemPromptContext};
use everruns_core::tools::{Tool, ToolExecutionResult};
use serde_json::{Value, json};
use std::sync::Arc;

pub(crate) const APPROVAL_CAPABILITY_ID: &str = "yolop_approval";

/// Render the `<soft_approval>` system-prompt block for a given level.
/// Pure so the per-mode branch logic is unit-testable without a
/// `SystemPromptContext`. Returns `None` for [`ApprovalMode::Off`], which
/// contributes nothing to the prompt.
fn render_approval_block(mode: ApprovalMode) -> Option<String> {
    let threshold = match mode {
        ApprovalMode::Off => return None,
        ApprovalMode::Protective => {
            "PROTECTIVE — the bar is low. Ask before ANY action that changes \
             state on the host: writing or deleting files, `git` commits/pushes, \
             installing or removing packages, network calls with side effects, or \
             running a `bash` command that is not plainly read-only."
        }
        ApprovalMode::Normal => {
            "NORMAL — ask only before clearly DANGEROUS actions: destructive or \
             irreversible operations (deleting files, `rm -rf`, dropping data, \
             `git reset --hard`, force-push, history rewrites) and outward-facing \
             ones (pushing, publishing, opening PRs, sending mail, deploying). \
             Ordinary edits and local commits proceed without asking."
        }
    };

    Some(format!(
        "<soft_approval>\n\
Soft-approval is active at level {level}.\n\
\n\
{threshold}\n\
\n\
How to operate:\n\
- Plan first, then BATCH the safe steps and run them without pausing. Do NOT \
ask for approval before every tool call — that defeats the purpose. Read-only \
inspection (reading, listing, grepping, status checks) never needs approval.\n\
- When you reach a critical action, STOP before running it. Briefly justify it \
to the user (what you will do, why, and what is at risk — the \"proof\"), then \
ask for approval in one short question and wait.\n\
- A plain affirmative reply (\"yes\", \"approved\", \"go ahead\", \"do it\") is \
the approval; a negative or hesitant reply is not. There is no separate \
approval UI — consent is spoken in chat.\n\
- Immediately after the user approves, call `record_approval` with a concise \
description of exactly what was approved, then carry it out. This writes the \
approval to the session audit log.\n\
- One approval covers the specific action described, not unrelated later \
actions. If the user pre-authorizes a category (\"you don't need to ask for \
git commits\"), honor it for that category without re-asking.\n\
- If the user asks to change how cautious you are (\"be more careful\", \"stop \
asking\", \"yolo mode\"), call `set_approval_mode` to update the level.\n\
</soft_approval>",
        level = mode.as_str(),
    ))
}

pub(crate) struct ApprovalCapability {
    /// Reads the paranoia level through the shared config service each turn.
    pub(crate) config: Arc<dyn ConfigService>,
    /// Concrete store for the `set_approval_mode` write tool.
    pub(crate) settings: Arc<SettingsStore>,
}

#[async_trait]
impl Capability for ApprovalCapability {
    fn id(&self) -> &str {
        APPROVAL_CAPABILITY_ID
    }
    fn name(&self) -> &str {
        "Soft Approval"
    }
    fn description(&self) -> &str {
        "Spoken-consent approval for critical actions, tuned by a central paranoia level."
    }
    fn status(&self) -> CapabilityStatus {
        CapabilityStatus::Available
    }
    fn category(&self) -> Option<&str> {
        Some("Safety")
    }

    async fn system_prompt_contribution(&self, _ctx: &SystemPromptContext) -> Option<String> {
        // Read the level live each turn through the config service so
        // `/setup approval`, `set_approval_mode`, and `set_config approval_mode`
        // all take effect on the very next turn.
        render_approval_block(self.config.approval_mode())
    }

    fn system_prompt_preview(&self) -> Option<String> {
        // Show the normal-level block; `off` would contribute nothing.
        render_approval_block(ApprovalMode::Normal)
    }

    fn tools(&self) -> Vec<Box<dyn Tool>> {
        vec![
            Box::new(RecordApprovalTool),
            Box::new(SetApprovalModeTool {
                settings: self.settings.clone(),
            }),
        ]
    }
}

// ---------- tools ----------

/// Records that the user verbally approved a specific critical action. The
/// tool itself only echoes the record back; the durable audit entry is the
/// `tool.completed` event this call produces in the per-session `events.jsonl`
/// log, which captures the arguments (what was approved) and a timestamp.
struct RecordApprovalTool;

#[async_trait]
impl Tool for RecordApprovalTool {
    fn name(&self) -> &str {
        "record_approval"
    }
    fn display_name(&self) -> Option<&str> {
        Some("Record approval")
    }
    fn description(&self) -> &str {
        "Record that the user just gave spoken approval for a critical action, for the audit \
         trail. Call this immediately after the user says yes/approved and before carrying the \
         action out. Pass a concise, specific description of exactly what was approved. These \
         arguments are written to the session log, so do NOT include secrets (API keys, tokens, \
         passwords); describe the action and redact any sensitive values."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "description": "The specific action the user approved, e.g. \"force-push branch feature/x to origin\". Do not embed secrets."
                },
                "detail": {
                    "type": "string",
                    "description": "Optional extra context: the command, affected paths, or scope of the approval. Redact any secrets (keys/tokens/passwords) before passing them — this is logged."
                }
            },
            "required": ["action"],
            "additionalProperties": false
        })
    }
    async fn execute(&self, arguments: Value) -> ToolExecutionResult {
        let action = match arguments.get("action").and_then(Value::as_str) {
            Some(a) if !a.trim().is_empty() => a.trim(),
            _ => {
                return ToolExecutionResult::tool_error(
                    "'action' is required and must describe what was approved",
                );
            }
        };
        let detail = arguments
            .get("detail")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|d| !d.is_empty());
        ToolExecutionResult::success(json!({
            "ok": true,
            "recorded": true,
            "action": action,
            "detail": detail,
            "message": format!("approval recorded: {action}"),
        }))
    }
}

/// Switches the central soft-approval level. Backs natural-language requests
/// ("yolop, be more careful", "stop asking me") so the user can tune yolop's
/// paranoia without remembering the `/setup` form.
struct SetApprovalModeTool {
    settings: Arc<SettingsStore>,
}

#[async_trait]
impl Tool for SetApprovalModeTool {
    fn name(&self) -> &str {
        "set_approval_mode"
    }
    fn display_name(&self) -> Option<&str> {
        Some("Set approval level")
    }
    fn description(&self) -> &str {
        "Set yolop's soft-approval paranoia level. Use when the user asks you to be more or less \
         cautious about confirming actions. `protective` asks before any state change, `normal` \
         asks only before destructive or outward-facing actions, `off` never asks."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                // Kept as a free string (not an `enum`) so the lenient
                // `ApprovalMode::parse` aliases — the same ones `/setup
                // approval` and settings.toml accept — are reachable here too;
                // a hard enum would silently shadow them. Unknown values are
                // rejected in `execute`.
                "mode": {
                    "type": "string",
                    "description": "The new approval level: 'protective', 'normal', or 'off' (common synonyms like 'paranoid' or 'yolo' are also accepted)."
                }
            },
            "required": ["mode"],
            "additionalProperties": false
        })
    }
    async fn execute(&self, arguments: Value) -> ToolExecutionResult {
        let raw = match arguments.get("mode").and_then(Value::as_str) {
            Some(m) => m,
            None => return ToolExecutionResult::tool_error("'mode' is required"),
        };
        let mode = match ApprovalMode::parse(raw) {
            Some(mode) => mode,
            None => {
                return ToolExecutionResult::tool_error(format!(
                    "unknown approval level '{raw}'; expected protective, normal, or off"
                ));
            }
        };
        match self.settings.set_approval_mode(mode) {
            Ok(()) => ToolExecutionResult::success(json!({
                "ok": true,
                "mode": mode.as_str(),
                "message": format!("approval level set to {mode}"),
            })),
            Err(e) => {
                ToolExecutionResult::tool_error(format!("could not save approval level: {e}"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store_in_tmp() -> (tempfile::TempDir, Arc<SettingsStore>) {
        let tmp = tempfile::tempdir().expect("tmp");
        let store = Arc::new(SettingsStore::open(tmp.path().join("settings.toml")));
        (tmp, store)
    }

    #[test]
    fn off_contributes_no_prompt() {
        assert!(render_approval_block(ApprovalMode::Off).is_none());
    }

    #[test]
    fn normal_and_protective_render_expected_guidance() {
        let normal = render_approval_block(ApprovalMode::Normal).expect("normal block");
        assert!(normal.starts_with("<soft_approval>"));
        assert!(normal.contains("level normal"));
        assert!(normal.contains("NORMAL"));
        // The core behaviors must be spelled out.
        assert!(normal.contains("BATCH"));
        assert!(normal.contains("record_approval"));
        assert!(normal.ends_with("</soft_approval>"));

        let protective = render_approval_block(ApprovalMode::Protective).expect("protective block");
        assert!(protective.contains("level protective"));
        assert!(protective.contains("PROTECTIVE"));
    }

    #[test]
    fn capability_exposes_both_tools() {
        let (_tmp, settings) = store_in_tmp();
        let cap = ApprovalCapability {
            config: settings.clone(),
            settings,
        };
        let names: Vec<String> = cap.tools().iter().map(|t| t.name().to_string()).collect();
        assert_eq!(names, vec!["record_approval", "set_approval_mode"]);
        assert!(cap.commands().is_empty());
    }

    #[tokio::test]
    async fn contribution_follows_settings() {
        let (_tmp, settings) = store_in_tmp();
        let cap = ApprovalCapability {
            config: settings.clone(),
            settings: settings.clone(),
        };
        let ctx =
            SystemPromptContext::without_file_store(everruns_core::typed_id::SessionId::new());

        // Default (normal) contributes a block.
        assert!(cap.system_prompt_contribution(&ctx).await.is_some());

        // Off suppresses it.
        settings
            .set_approval_mode(ApprovalMode::Off)
            .expect("set off");
        assert!(cap.system_prompt_contribution(&ctx).await.is_none());
    }

    #[tokio::test]
    async fn record_approval_requires_action_and_echoes_it() {
        let tool = RecordApprovalTool;
        assert!(tool.execute(json!({ "action": "  " })).await.is_error());

        let res = tool
            .execute(json!({ "action": "force-push feature/x", "detail": "git push -f" }))
            .await;
        assert!(res.is_success());
        let text = format!("{res:?}");
        assert!(text.contains("force-push feature/x"));
    }

    #[tokio::test]
    async fn set_approval_mode_updates_settings_and_rejects_garbage() {
        let (_tmp, settings) = store_in_tmp();
        let tool = SetApprovalModeTool {
            settings: settings.clone(),
        };

        let res = tool.execute(json!({ "mode": "off" })).await;
        assert!(res.is_success());
        assert_eq!(settings.snapshot().approval_mode(), ApprovalMode::Off);

        // Aliases accepted by `ApprovalMode::parse` reach the tool too, since
        // the schema is a lenient string rather than a canonical-only enum.
        let res = tool.execute(json!({ "mode": "paranoid" })).await;
        assert!(res.is_success());
        assert_eq!(
            settings.snapshot().approval_mode(),
            ApprovalMode::Protective
        );

        assert!(tool.execute(json!({ "mode": "whenever" })).await.is_error());
        // Unchanged after a rejected value.
        assert_eq!(
            settings.snapshot().approval_mode(),
            ApprovalMode::Protective
        );
    }
}

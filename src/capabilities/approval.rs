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
// The pause itself is a tool call, `request_approval`, not just prose. A model
// that ends its turn on a sentence like "Squash-merging." has paused as far as
// the loop is concerned, but nothing distinguishes that from a finished answer:
// the user sees a turn that stopped mid-thought and has to guess that yolop is
// waiting on them. Routing the pause through a tool gives the host a fact to
// render, and the audit log a record of what was asked, not only of what was
// granted.
//
// The paranoia level is central configuration (`approval_mode` in
// settings.toml, see `crate::config::ApprovalMode`), surfaced in the status
// bar, switchable with `/setup approval <level>` and — because users address
// yolop in natural language ("yolop, be more careful") — with the
// `set_approval_mode` tool.

use crate::capabilities::narration::stable_labeled;
use crate::config::service::ConfigService;
use crate::config::{ApprovalMode, SettingsStore};
use async_trait::async_trait;
use everruns_core::tool_narration::{ToolNarrationPhase, arg_str, truncate};
use everruns_core::{Capability, CapabilityStatus, SystemPromptContext};
use everruns_core::{Tool, ToolExecutionResult};
use everruns_provider::{BuiltinTool, DeferrablePolicy, ToolCall, ToolDefinition};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

pub(crate) const APPROVAL_CAPABILITY_ID: &str = "yolop_approval";

/// A critical action yolop has stopped in front of, waiting for the user to
/// say yes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingApproval {
    /// What yolop will do once approved.
    pub action: String,
    /// The question put to the user, already phrased for display.
    pub question: String,
}

/// The one pending approval a session can be holding.
///
/// Shared between the capability's tools and the host: `request_approval`
/// sets it, `record_approval` clears it, and the host reads it when a turn
/// ends so a pause is visible rather than looking like a turn that died.
#[derive(Clone, Default)]
pub struct PendingApprovalStore {
    inner: Arc<Mutex<Option<PendingApproval>>>,
}

impl PendingApprovalStore {
    /// Raise a pause. `request_approval` owns this in production; the TUI's
    /// own tests use it to stand in for a model that paused.
    pub(crate) fn set(&self, pending: PendingApproval) {
        *self.inner.lock().expect("pending approval lock") = Some(pending);
    }

    fn clear(&self) {
        *self.inner.lock().expect("pending approval lock") = None;
    }

    /// What the session is waiting on, without consuming it. A pause outlives
    /// the turn that raised it: it is answered by the user's next message, not
    /// by the turn ending.
    pub fn peek(&self) -> Option<PendingApproval> {
        self.inner.lock().expect("pending approval lock").clone()
    }

    /// Drop a pause the user has now answered, whichever way they answered.
    pub fn resolve(&self) {
        self.clear();
    }
}

/// Render the `<soft_approval>` system-prompt block for a given level.
/// Pure so the per-mode branch logic is unit-testable without a
/// `SystemPromptContext`. Returns `None` for [`ApprovalMode::Off`], which
/// contributes nothing to the prompt.
pub(crate) fn render_approval_block(mode: ApprovalMode) -> Option<String> {
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
- Plan, then BATCH the safe steps and run them without pausing. Do NOT ask \
before every tool call; read-only inspection never needs approval.\n\
- At a critical action, STOP: say briefly what you will do, why, and what is \
at risk, then call `request_approval` with that action and one short question, \
and end your turn. That call IS the pause, and the only signal to the user \
that you are waiting.\n\
- NEVER announce a critical action and then stop without it. \"Squash-merging.\" \
as your last words reads as a turn that died, not as a question. Ask, or act; \
never narrate and halt.\n\
- A plain affirmative (\"yes\", \"go ahead\", \"do it\") is the approval; a \
negative or hesitant reply is not. Consent is spoken in chat.\n\
- Immediately after they approve, call `record_approval` with what was \
approved, then carry it out. This writes it to the session audit log.\n\
- One approval covers that action, not later ones. Honor a pre-authorized \
category (\"no need to ask for commits\") without re-asking, and a workflow the \
user invoked by name pre-authorizes the actions it exists to perform: run \
those without pausing and note the grant once with `record_approval`.\n\
- If the user asks you to be more or less cautious, call `set_approval_mode`.\n\
</soft_approval>",
        level = mode.as_str(),
    ))
}

pub(crate) struct ApprovalCapability {
    /// Reads the paranoia level through the shared config service each turn.
    pub(crate) config: Arc<dyn ConfigService>,
    /// Concrete store for the `set_approval_mode` write tool.
    pub(crate) settings: Arc<SettingsStore>,
    /// Shared with the host so a pause is rendered, not merely spoken.
    pub(crate) pending: PendingApprovalStore,
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
            Box::new(RequestApprovalTool {
                pending: self.pending.clone(),
            }),
            Box::new(RecordApprovalTool {
                pending: self.pending.clone(),
            }),
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
/// log, which captures the arguments, output, and timestamp.
struct RecordApprovalTool {
    pending: PendingApprovalStore,
}

const FALLBACK_APPROVAL_ACTION: &str = "the pending critical action approved in the conversation";

fn non_deferrable_builtin(tool: &dyn Tool) -> ToolDefinition {
    ToolDefinition::Builtin(BuiltinTool {
        name: tool.name().to_string(),
        display_name: tool.display_name().map(str::to_string),
        description: tool.description().to_string(),
        parameters: tool.parameters_schema(),
        policy: tool.policy(),
        category: None,
        deferrable: DeferrablePolicy::Never,
        hints: tool.hints(),
        full_parameters: None,
    })
}

fn non_empty_str(value: Option<&Value>) -> Option<&str> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn approval_action(arguments: &Value) -> &str {
    if let Some(action) = arguments.as_str().map(str::trim).filter(|s| !s.is_empty()) {
        return action;
    }

    let Some(object) = arguments.as_object() else {
        return FALLBACK_APPROVAL_ACTION;
    };

    non_empty_str(object.get("action")).unwrap_or(FALLBACK_APPROVAL_ACTION)
}

#[async_trait]
impl Tool for RecordApprovalTool {
    fn narrate(
        &self,
        tool_call: &ToolCall,
        phase: ToolNarrationPhase,
        locale: Option<&str>,
        _ctx: everruns_core::tool_narration::ToolNarrationContext<'_>,
    ) -> Option<String> {
        let _ = locale;
        let action = arg_str(&tool_call.arguments, &["action"]).map(|value| truncate(value, 48));
        Some(stable_labeled("Record approval", action, phase))
    }

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
            "additionalProperties": false
        })
    }

    fn to_definition(&self) -> ToolDefinition {
        non_deferrable_builtin(self)
    }

    async fn execute(&self, arguments: Value) -> ToolExecutionResult {
        let action = approval_action(&arguments);
        let detail = non_empty_str(arguments.get("detail"));
        // Consent has been given, so the session is no longer waiting on the
        // user. Clearing here rather than on the next turn keeps the host from
        // showing a pause that has already been answered.
        self.pending.resolve();
        ToolExecutionResult::success(json!({
            "ok": true,
            "recorded": true,
            "action": action,
            "detail": detail,
            "message": format!("approval recorded: {action}"),
        }))
    }
}

/// Pauses in front of a critical action and asks the user for approval.
///
/// Calling this is what makes the pause legible: the host reads
/// [`PendingApprovalStore`] when the turn ends and tells the user it is
/// waiting on them, instead of leaving a turn that stopped mid-sentence.
struct RequestApprovalTool {
    pending: PendingApprovalStore,
}

const FALLBACK_APPROVAL_QUESTION: &str = "Go ahead?";

#[async_trait]
impl Tool for RequestApprovalTool {
    fn narrate(
        &self,
        tool_call: &ToolCall,
        phase: ToolNarrationPhase,
        locale: Option<&str>,
        _ctx: everruns_core::tool_narration::ToolNarrationContext<'_>,
    ) -> Option<String> {
        let _ = locale;
        let action = arg_str(&tool_call.arguments, &["action"]).map(|value| truncate(value, 48));
        Some(stable_labeled("Ask approval", action, phase))
    }

    fn name(&self) -> &str {
        "request_approval"
    }
    fn display_name(&self) -> Option<&str> {
        Some("Ask approval")
    }
    fn description(&self) -> &str {
        // Deliberately terse: this tool can never be deferred behind
        // `tool_search` (the model has to find it at the instant it decides to
        // pause), so every byte here is paid on every turn.
        "Pause before a critical action and ask the user to approve it. Call this INSTEAD of \
         announcing it and stopping: the call is what tells the user you are waiting. End your \
         turn right after. Logged; no secrets."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "description": "The action awaiting approval, e.g. \"squash-merge PR #677\"."
                },
                "question": {
                    "type": "string",
                    "description": "One short question for the user, e.g. \"CI is green. Merge it?\""
                }
            },
            "required": ["action"],
            "additionalProperties": false
        })
    }

    fn to_definition(&self) -> ToolDefinition {
        non_deferrable_builtin(self)
    }

    async fn execute(&self, arguments: Value) -> ToolExecutionResult {
        let action = approval_action(&arguments).to_string();
        let question = arguments
            .get("question")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or(FALLBACK_APPROVAL_QUESTION)
            .to_string();
        self.pending.set(PendingApproval {
            action: action.clone(),
            question: question.clone(),
        });
        ToolExecutionResult::success(json!({
            "ok": true,
            "awaiting_approval": true,
            "action": action,
            "question": question,
            "message": "waiting for the user to approve; end your turn now",
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
    fn narrate(
        &self,
        tool_call: &ToolCall,
        phase: ToolNarrationPhase,
        locale: Option<&str>,
        _ctx: everruns_core::tool_narration::ToolNarrationContext<'_>,
    ) -> Option<String> {
        let _ = locale;
        let mode =
            arg_str(&tool_call.arguments, &["mode", "level"]).map(|value| truncate(value, 24));
        Some(stable_labeled("Set approval level", mode, phase))
    }

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

    fn to_definition(&self) -> ToolDefinition {
        non_deferrable_builtin(self)
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
        // The pause is a tool call, and announcing instead of asking is the
        // failure this block exists to prevent.
        assert!(normal.contains("request_approval"));
        assert!(normal.contains("NEVER announce a critical action"));
        // A workflow the user invoked can carry the pre-authorization, which
        // is what keeps `/ship` from stopping in front of its own merge.
        assert!(normal.contains("a workflow the user invoked by name"));
        assert!(normal.ends_with("</soft_approval>"));

        let protective = render_approval_block(ApprovalMode::Protective).expect("protective block");
        assert!(protective.contains("level protective"));
        assert!(protective.contains("PROTECTIVE"));
    }

    #[test]
    fn capability_exposes_its_tools() {
        let (_tmp, settings) = store_in_tmp();
        let cap = ApprovalCapability {
            config: settings.clone(),
            settings,
            pending: PendingApprovalStore::default(),
        };
        let tools = cap.tools();
        let names: Vec<String> = tools.iter().map(|t| t.name().to_string()).collect();
        assert_eq!(
            names,
            vec!["request_approval", "record_approval", "set_approval_mode"]
        );
        assert!(
            tools.iter().all(|tool| {
                matches!(tool.to_definition().deferrable(), DeferrablePolicy::Never)
            })
        );
        assert!(cap.commands().is_empty());
    }

    #[tokio::test]
    async fn contribution_follows_settings() {
        let (_tmp, settings) = store_in_tmp();
        let cap = ApprovalCapability {
            config: settings.clone(),
            settings: settings.clone(),
            pending: PendingApprovalStore::default(),
        };
        let ctx =
            SystemPromptContext::without_file_store(everruns_provider::typed_id::SessionId::new());

        // Default (normal) contributes a block.
        assert!(cap.system_prompt_contribution(&ctx).await.is_some());

        // Off suppresses it.
        settings
            .set_approval_mode(ApprovalMode::Off)
            .expect("set off");
        assert!(cap.system_prompt_contribution(&ctx).await.is_none());
    }

    #[tokio::test]
    async fn record_approval_echoes_action() {
        let tool = RecordApprovalTool {
            pending: PendingApprovalStore::default(),
        };

        let res = tool
            .execute(json!({ "action": "force-push feature/x", "detail": "git push -f" }))
            .await;
        let ToolExecutionResult::Success(value) = res else {
            panic!("expected success");
        };
        assert_eq!(value["action"], "force-push feature/x");
        assert_eq!(value["detail"], "git push -f");
    }

    #[tokio::test]
    async fn record_approval_accepts_empty_arguments_after_spoken_consent() {
        let tool = RecordApprovalTool {
            pending: PendingApprovalStore::default(),
        };
        let res = tool.execute(json!({})).await;
        let ToolExecutionResult::Success(value) = res else {
            panic!("expected success");
        };
        assert_eq!(value["action"], FALLBACK_APPROVAL_ACTION);
        assert!(value["detail"].is_null());
    }

    #[tokio::test]
    async fn request_approval_publishes_the_pause_for_the_host() {
        let pending = PendingApprovalStore::default();
        let tool = RequestApprovalTool {
            pending: pending.clone(),
        };
        assert_eq!(pending.peek(), None);

        let res = tool
            .execute(json!({
                "action": "squash-merge PR #677",
                "question": "CI is green and no comments are open. Merge it?",
            }))
            .await;
        let ToolExecutionResult::Success(value) = res else {
            panic!("expected success");
        };
        assert_eq!(value["awaiting_approval"], true);
        assert_eq!(
            pending.peek(),
            Some(PendingApproval {
                action: "squash-merge PR #677".into(),
                question: "CI is green and no comments are open. Merge it?".into(),
            })
        );
    }

    #[tokio::test]
    async fn request_approval_without_a_question_still_reads_as_a_question() {
        let pending = PendingApprovalStore::default();
        let tool = RequestApprovalTool {
            pending: pending.clone(),
        };
        tool.execute(json!({ "action": "deploy to production" }))
            .await;
        assert_eq!(
            pending.peek().map(|p| p.question),
            Some(FALLBACK_APPROVAL_QUESTION.to_string())
        );
    }

    /// Consent ends the pause: the host must not go on telling the user it is
    /// waiting for an answer they have already given.
    #[tokio::test]
    async fn recording_an_approval_clears_the_pause() {
        let pending = PendingApprovalStore::default();
        RequestApprovalTool {
            pending: pending.clone(),
        }
        .execute(json!({ "action": "squash-merge PR #677" }))
        .await;
        assert!(pending.peek().is_some());

        RecordApprovalTool {
            pending: pending.clone(),
        }
        .execute(json!({ "action": "squash-merge PR #677" }))
        .await;
        assert_eq!(pending.peek(), None);
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

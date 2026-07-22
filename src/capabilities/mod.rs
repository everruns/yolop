// CLI-owned capabilities for yolop.
//
// These are host/example behavior rather than runtime primitives. Keep the
// module boundary here small; capability implementations live in submodules.

pub(crate) mod approval;
pub(crate) mod ast_grep;
pub(crate) mod attribution;
pub(crate) mod background;
pub(crate) mod checkpoint;
pub(crate) mod client_commands;
pub(crate) mod config;
pub(crate) mod context_cost_control;
pub(crate) mod edit_file_override;
pub(crate) mod free_search;
pub(crate) mod goal;
pub(crate) mod herdr;
pub(crate) mod hooks;
mod host;
pub(crate) mod lsp;
pub(crate) mod mcp;
pub(crate) mod memory;
pub(crate) mod model_discovery;
pub(crate) mod model_ranking;
pub(crate) mod narration;
pub(crate) mod progress_guard;
pub(crate) mod repo_map;
pub(crate) mod session_history;
pub(crate) mod session_tasks_override;
pub mod skills;
pub(crate) mod tool_approval;
pub(crate) mod user_ask;
pub(crate) mod worktree_cmd;
pub(crate) mod yolop;

pub(crate) use crate::session_state::goal::GOAL_CAPABILITY_ID;
pub(crate) use crate::session_state::user_ask::USER_ASK_CAPABILITY_ID;
pub(crate) use approval::{APPROVAL_CAPABILITY_ID, ApprovalCapability};
pub(crate) use ast_grep::{AST_GREP_CAPABILITY_ID, AstEditCapability, AstGrepCapability};
pub(crate) use attribution::{ATTRIBUTION_CAPABILITY_ID, AttributionCapability};
pub(crate) use background::{
    BACKGROUND_CAPABILITY_ID, BackgroundCapability, NarratedBackgroundExecutionCapability,
};
pub(crate) use checkpoint::{CHECKPOINT_CAPABILITY_ID, CheckpointCapability};
pub(crate) use client_commands::{CLIENT_COMMANDS_CAPABILITY_ID, ClientCommandsCapability};
pub(crate) use config::{CONFIG_CAPABILITY_ID, ConfigCapability};
pub(crate) use context_cost_control::{
    CONTEXT_COST_CONTROL_CAPABILITY_ID, ContextCostControlCapability,
};
pub(crate) use free_search::FreeSearchCapability;
pub(crate) use goal::GoalCapability;
pub(crate) use herdr::{HERDR_CAPABILITY_ID, HerdrCapability};
pub(crate) use hooks::{HOOKS_CAPABILITY_ID, HooksCapability};
pub(crate) use host::{
    CODING_BASH_CAPABILITY_ID, ClientUiContext, CodingBashCapability,
    CodingCliEnvironmentCapability, ENVIRONMENT_CONTEXT_CAPABILITY_ID, EnvironmentContextRegistry,
    SETUP_CAPABILITY_ID, SetupCapability,
};
pub(crate) use lsp::LspCapability;
pub(crate) use progress_guard::{PROGRESS_GUARD_CAPABILITY_ID, ProgressGuardCapability};
pub(crate) use repo_map::{REPO_MAP_CAPABILITY_ID, RepoMapCapability};
pub(crate) use session_history::{SESSION_HISTORY_CAPABILITY_ID, SessionHistoryCapability};
pub(crate) use tool_approval::{ApprovalDecision, ToolApprovalCapability, ToolApprover};
pub(crate) use user_ask::UserAskCapability;
pub(crate) use worktree_cmd::WorktreeCapability;

// CLI-owned capabilities for yolop.
//
// These are host/example behavior rather than runtime primitives. Keep the
// module boundary here small; capability implementations live in submodules.

pub(crate) mod approval;
pub(crate) mod ast_grep;
pub(crate) mod attribution;
pub(crate) mod background;
pub(crate) mod client_commands;
pub(crate) mod config;
pub(crate) mod edit_file_override;
pub(crate) mod goal;
pub(crate) mod hooks;
mod host;
pub(crate) mod memory;
pub(crate) mod model_discovery;
pub(crate) mod model_ranking;
pub(crate) mod narration;
pub(crate) mod repo_map;
pub mod skills;
pub(crate) mod worktree_cmd;
pub(crate) mod yolop;

pub(crate) use crate::goal::GOAL_CAPABILITY_ID;
pub(crate) use approval::{APPROVAL_CAPABILITY_ID, ApprovalCapability};
pub(crate) use ast_grep::{AST_GREP_CAPABILITY_ID, AstGrepCapability};
pub(crate) use attribution::{ATTRIBUTION_CAPABILITY_ID, AttributionCapability};
pub(crate) use background::{
    AgentRunResult, AgentSpawner, BACKGROUND_CAPABILITY_ID, BackgroundCapability,
    BackgroundRegistry,
};
pub(crate) use client_commands::{CLIENT_COMMANDS_CAPABILITY_ID, ClientCommandsCapability};
pub(crate) use config::{CONFIG_CAPABILITY_ID, ConfigCapability};
pub(crate) use goal::GoalCapability;
pub(crate) use hooks::{HOOKS_CAPABILITY_ID, HooksCapability};
pub(crate) use host::{
    CODING_BASH_CAPABILITY_ID, CodingBashCapability, CodingCliEnvironmentCapability,
    ENVIRONMENT_CONTEXT_CAPABILITY_ID, SETUP_CAPABILITY_ID, SetupCapability,
};
pub(crate) use repo_map::{REPO_MAP_CAPABILITY_ID, RepoMapCapability};
pub(crate) use worktree_cmd::WorktreeCapability;

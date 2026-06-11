// CLI-owned capabilities for yolop.
//
// These are host/example behavior rather than runtime primitives. Keep the
// module boundary here small; capability implementations live in submodules.

pub(crate) mod approval;
pub(crate) mod client_commands;
mod host;
pub(crate) mod model_discovery;
pub mod skills;
pub(crate) mod tool_search;
pub(crate) mod your;

pub(crate) use approval::{APPROVAL_CAPABILITY_ID, ApprovalCapability};
pub(crate) use client_commands::{CLIENT_COMMANDS_CAPABILITY_ID, ClientCommandsCapability};
pub(crate) use host::{
    ATTRIBUTION_CAPABILITY_ID, AttributionCapability, CodingBashCapability,
    CodingCliEnvironmentCapability, ENVIRONMENT_CONTEXT_CAPABILITY_ID, SETUP_CAPABILITY_ID,
    SetupCapability,
};
pub(crate) use tool_search::{TOOL_SEARCH_CAPABILITY_ID, ToolSearchCapability};

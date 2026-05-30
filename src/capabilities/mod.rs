// CLI-owned capabilities for yolop.
//
// These are host/example behavior rather than runtime primitives. Keep the
// module boundary here small; capability implementations live in submodules.

mod host;
pub mod skills;
pub(crate) mod your;

pub(crate) use host::{
    CodingBashCapability, CodingCliEnvironmentCapability, ENVIRONMENT_CONTEXT_CAPABILITY_ID,
    SETUP_CAPABILITY_ID, SetupCapability,
};

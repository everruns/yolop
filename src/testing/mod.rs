//! Crate-internal test support and end-to-end test suites.
//!
//! Compiled only under `cfg(test)`. `test_env` is the process-wide env mutex
//! shared by unit tests across the crate; the remaining submodules are
//! black-box suites driving a real runtime.

pub mod test_env;

// `pub(crate)` so the ACP e2e suite can reuse the real MCP echo-server fixture
// helpers (see `editor::acp` tests).
pub(crate) mod mcp_e2e;
mod scenarios;
mod streaming;

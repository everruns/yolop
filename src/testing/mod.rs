//! Crate-internal test support and end-to-end test suites.
//!
//! Compiled only under `cfg(test)`. `test_env` is the process-wide env mutex
//! shared by unit tests across the crate; the remaining submodules are
//! black-box suites driving a real runtime.

pub mod test_env;

mod mcp_e2e;
mod scenarios;
mod streaming;

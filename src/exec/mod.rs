//! Command execution and workspace containment.
//!
//! The bash tool, subprocess execution that never corrupts the TUI's terminal,
//! pluggable sandboxing, the shared workspace host, and git worktree management.

pub(crate) mod group_ledger;
pub mod proc;
pub mod sandbox;
pub(crate) mod self_exe;
pub(crate) mod shell_policy;
pub mod tools;
pub mod workspace_host;
pub mod worktree;

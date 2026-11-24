//! Command execution and workspace containment.
//!
//! The bash tool, subprocess execution that never corrupts the TUI's terminal,
//! pluggable sandboxing, the shared workspace host, and git worktree management.

pub(crate) mod group_ledger;
pub mod proc;
pub(crate) mod readonly_git;
pub mod sandbox;
#[cfg(windows)]
pub(crate) mod sandbox_windows;
pub(crate) mod self_exe;
pub(crate) mod shell_policy;
pub mod tools;
pub(crate) mod workspace_evidence;
pub(crate) mod workspace_file;
pub mod workspace_host;
pub mod worktree;

pub(crate) mod private_artifact;

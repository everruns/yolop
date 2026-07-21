//! Command execution and workspace containment.
//!
//! The bash tool, subprocess execution that never corrupts the TUI's terminal,
//! pluggable sandboxing, the shared workspace host, and git worktree management.

pub mod proc;
pub mod sandbox;
pub mod tools;
pub mod workspace_host;
pub mod worktree;

//! Session-scoped state and durable turn history.
//!
//! Checkpoints and branch-aware rewind, bounded turn continuation, and
//! ATIF trajectory export.

pub mod atif;
pub mod checkpoint;
pub mod task_completion;

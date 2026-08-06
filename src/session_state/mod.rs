//! Session-scoped state and durable turn history.
//!
//! Checkpoints and branch-aware rewind, `/goal` and user-ask tracking, and
//! ATIF trajectory export.

pub mod atif;
pub mod checkpoint;
pub mod goal;
pub mod task_completion;
pub mod user_ask;

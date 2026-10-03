//! Provider LLM drivers.
//!
//! Each submodule implements a `ChatDriver` from
//! `everruns_core::driver_registry` for a specific provider/auth flavor.

pub mod codex;
#[cfg(feature = "local-inference")]
pub mod local;

/// Driver id for in-process inference. Declared unconditionally, unlike the
/// module behind it, so provider resolution stays feature-independent: a build
/// without `local-inference` resolves `--provider local` the same way and only
/// fails when the registry is asked for a driver nothing registered.
pub const LOCAL_DRIVER_ID: &str = "yolop-local";

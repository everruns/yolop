//! User-installable extensions — declarative contributions without recompilation.
//!
//! Extensions are directories with an `extension.toml` manifest installed under:
//! - **global**: `<config_dir>/yolop/extensions/<id>/`
//! - **workspace**: `<workspace>/.yolop/extensions/<id>/`
//!
//! Contributions mirror capability-level config (LSP servers, MCP entries, …)
//! and are merged at session build time. See `specs/extensions.md`.

#![allow(unused_imports)]

mod apply;
mod discovery;
mod install;
mod manifest;

pub use apply::{apply_extension_capability_contributions, apply_extension_mcp_contributions};
pub use discovery::{
    DiscoveredExtension, ExtensionScope, discover_extensions, global_extensions_dir,
    workspace_extensions_dir,
};
pub use install::{install_extension, uninstall_extension};
pub use manifest::{ExtensionManifest, MANIFEST_FILE};

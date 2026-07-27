//! `yolop-yep` — the yolop extension protocol (YEP): shared wire types plus a
//! small, zero-dependency-beyond-serde server SDK for authoring extension
//! capability servers in Rust.
//!
//! An extension server is a subprocess yolop spawns and talks to over
//! newline-delimited JSON on stdio. This crate is the source of truth for the
//! wire format (`protocol`) — the host depends on it too — and provides a
//! [`Server`] builder + [`Server::serve`] loop so an author writes handlers,
//! not a JSON-RPC dispatcher.
//!
//! ```no_run
//! use yolop_yep::{Server, ToolResponse};
//! use serde_json::json;
//!
//! # fn main() -> std::io::Result<()> {
//! Server::new("echo")
//!     .instructions("Use the echo tool to repeat text.")
//!     .tool("echo", |args| {
//!         let text = args.get("text").and_then(|v| v.as_str()).unwrap_or("");
//!         ToolResponse::ok(json!({ "echoed": text }))
//!     })
//!     .serve() // blocks, reading stdin until EOF
//! # }
//! ```
//!
//! See `knowledge/specs/extensions.md` in the yolop repo for the full protocol.

pub mod meta;
pub mod protocol;
#[cfg(feature = "schema")]
pub mod schema;
mod server;

pub use meta::{Meta, meta, meta_json};
pub use protocol::{PROTOCOL_VERSION, TraceEventParams};
#[cfg(feature = "schema")]
pub use schema::schema_json;
pub use server::{HookResponse, Notifier, Server, ToolResponse};

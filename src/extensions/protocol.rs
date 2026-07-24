//! YEP wire types for the host side. The definitions live in the `yolop-yep`
//! crate — the single source of truth shared with extension-server authors —
//! and are re-exported here so the rest of `src/extensions/` keeps its
//! `super::protocol::…` paths. See `knowledge/specs/extensions.md`.

pub use yolop_yep::protocol::*;

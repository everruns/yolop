//! `tuika` — a small retained-tree terminal UI toolkit.
//!
//! `tuika` is yolop's experimental full-screen renderer foundation. It is
//! designed to graduate into a standalone crate, so it depends only on
//! `ratatui`, `crossterm`, `textwrap`, and `unicode-width`, and knows nothing
//! about yolop's `App`, runtime, or presentation model. Yolop drives it from
//! `crate::app::fullscreen`; nothing here reaches back into the binary.
//!
//! # Why it exists
//!
//! yolop's default TUI is *inline*: the transcript lives in native terminal
//! scrollback and ratatui owns only a bottom composer. That keeps scrollback
//! and copy/paste native, but centered overlays and resize anchoring fight the
//! inline viewport (see the inline path's re-anchoring logic). `tuika` is the
//! opt-in *full-screen* alternative (`--fullscreen`): it owns the alternate
//! screen, so overlays, focus, and scrolling compose cleanly at the cost of
//! rebuilding scrollback in-app. This mirrors where Codex and Claude Code's
//! full-screen renderer landed.
//!
//! # Model
//!
//! - **Views** ([`view::View`]) are ephemeral, rebuilt from application state
//!   each frame; ratatui diffs the resulting cell buffer, so this is cheap.
//! - **State** that must persist across frames ([`components::ScrollState`],
//!   [`components::SelectState`], [`focus::FocusRegistry`]) lives in the host,
//!   in the `StatefulWidget` idiom.
//! - **Layout** is a flexbox subset ([`layout`]); **overlays** ([`overlay`])
//!   anchor over the base tree; the **host** ([`host`]) owns the alternate
//!   screen, translates crossterm input, and composites the frame.
//!
//! # Extending
//!
//! Add a component by implementing [`view::View`] in a new module under
//! [`components`]. No registration step; containers accept any boxed `View`.

// `tuika` is a self-contained toolkit staged for extraction into its own crate.
// Its public API is deliberately broader than yolop's current full-screen host
// consumes (Select, focus, overlays, alt themes, the full event enum), so the
// binary build sees parts of the surface as dead. Allow it here rather than
// trimming the API down to today's single call site.
#![allow(dead_code, unused_imports)]

pub mod anim;
pub mod components;
pub mod event;
pub mod focus;
pub mod geometry;
pub mod host;
pub mod layout;
pub mod native;
pub mod overlay;
pub mod style;
pub mod surface;
pub mod view;

// Curated top-level re-exports so callers write `tuika::Flex`, `tuika::Theme`,
// etc. for the common surface without deep paths. This is the toolkit's public
// API; not every item is used by yolop's current full-screen host, but they are
// part of the surface a future standalone crate exposes, so we keep them
// exported rather than trimming to today's call sites.
pub use components::{
    Boxed, Flex, Loader, Paragraph, ProgressBar, Scroll, ScrollState, SelectList, SelectOutcome,
    SelectState, Spacer, Spinner, SpinnerStyle, StatusBar, Text,
};
pub use event::{Event, EventFlow, Key, KeyCode, Mouse, MouseKind};
pub use focus::FocusRegistry;
pub use geometry::{Padding, Size};
pub use host::{AltScreen, Overlay, paint, translate_event};
pub use layout::{Align, Dimension, Direction, Justify, LayoutStyle};
pub use native::{ProgressState, TerminalProgress};
pub use overlay::{Anchor, Extent, OverlaySpec};
pub use style::{BorderStyle, Theme};
pub use view::{Element, RenderCtx, View, element};

#[cfg(test)]
mod tests;

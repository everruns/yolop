//! `tuika` — a small retained-tree terminal UI toolkit over
//! [`ratatui`](https://docs.rs/ratatui).
//!
//! `tuika` adds the pieces ratatui leaves to you — a flexbox-style layout
//! solver, anchored overlays, focus/input-ownership, an alternate-screen host,
//! and a set of components (text, boxes, scroll, select, spinner, progress) —
//! while letting ratatui keep ownership of the cell buffer and its diff against
//! the terminal. It depends only on `ratatui`, `crossterm`, `textwrap`, and
//! `unicode-width`.
//!
//! It was extracted from the [yolop](https://github.com/everruns/yolop) coding
//! agent, whose full-screen renderer is built on it, but it knows nothing about
//! any host application.
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

pub mod anim;
pub mod components;
pub mod event;
#[macro_use]
mod macros;
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
// etc. for the common surface without deep paths.
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
mod proptests;
#[cfg(test)]
mod snapshots;
#[cfg(test)]
mod tests;

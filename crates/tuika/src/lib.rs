//! `tuika` — a small composable terminal UI toolkit over
//! [`ratatui`](https://docs.rs/ratatui).
//!
//! `tuika` adds the pieces ratatui leaves to you — a flexbox-style layout
//! solver, anchored overlays, focus/input-ownership, an alternate-screen host,
//! and a set of components (text, boxes, scroll, select, spinner, progress) —
//! while letting ratatui keep ownership of the cell buffer and its diff against
//! the terminal. It depends only on `ratatui`, `crossterm`, `textwrap`,
//! `unicode-segmentation`, and `unicode-width`.
//!
//! It was extracted from the [yolop](https://github.com/everruns/yolop) coding
//! agent, whose full-screen renderer is built on it, but it knows nothing about
//! any host application.
//!
//! # Features
//!
//! A guided tour with animated demos lives in the
//! [feature guide](https://github.com/everruns/yolop/blob/main/crates/tuika/docs/features.md)
//! (progress bars, OSC 8 hyperlinks, overlays, mouse/clipboard, live data, and
//! more). The
//! [component gallery](https://github.com/everruns/yolop/blob/main/crates/tuika/docs/components.md)
//! catalogs each widget. On docs.rs, demos are also embedded on the relevant
//! types (for example [`ProgressBar`], [`Spinner`], [`HyperlinkBackend`]).
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
//!
//! Existing ratatui widgets should normally be wrapped in [`RatatuiView`],
//! which preserves Tuika clipping without exposing the frame buffer.
//! [`TerminalSession`] and [`Runner`] are optional host-side lifecycle helpers.

pub mod anim;
pub mod clipboard;
pub mod components;
pub mod event;
#[macro_use]
mod macros;
pub mod focus;
pub mod geometry;
pub mod host;
pub mod hyperlink;
pub mod layout;
pub mod live;
pub mod mouse;
pub mod native;
pub mod overlay;
pub mod probe;
pub mod ratatui_view;
pub mod runner;
pub mod style;
pub mod surface;
pub mod testing;
pub mod view;
pub mod width;

// Curated top-level re-exports so callers write `tuika::Flex`, `tuika::Theme`,
// etc. for the common surface without deep paths.
pub use clipboard::{osc52, write_clipboard};
pub use components::{
    Boxed, Constrained, Flex, KeyHints, Loader, Paragraph, ProgressBar, Responsive, Rule, Scroll,
    ScrollState, SelectList, SelectOutcome, SelectState, Spacer, Spinner, SpinnerStyle, StatusBar,
    Tabs, TabsState, Text, TextInput, TextInputState, Wrap, wrap_lines,
};
pub use event::{Event, EventFlow, Key, KeyCode, Mouse, MouseButton, MouseKind};
pub use focus::FocusRegistry;
pub use geometry::{Padding, Size};
pub use host::{AltScreen, Overlay, TerminalSession, paint, translate_event};
pub use hyperlink::{HyperlinkBackend, is_web_url, osc8, write_line};
pub use layout::{Align, Dimension, Direction, Justify, LayoutStyle};
pub use live::{Live, LiveView, RedrawHandle};
pub use mouse::{
    Click, ClickTracker, HitMap, SelectionRange, SelectionState, highlight, selected_text,
};
pub use native::{ProgressState, TerminalProgress};
pub use overlay::{Anchor, Extent, OverlaySpec};
pub use probe::{Probe, RectProbe};
pub use ratatui_view::RatatuiView;
pub use runner::{Runner, RunnerConfig};
pub use style::{BorderStyle, Theme};
pub use surface::Surface;
pub use view::{Element, RenderCtx, View, element};
pub use width::{grapheme_cols, str_cols};

#[cfg(test)]
mod proptests;
#[cfg(test)]
mod snapshots;
#[cfg(test)]
mod tests;

//! `tuika` — a small composable terminal UI toolkit over
//! [`ratatui`](https://docs.rs/ratatui).
//!
//! `tuika` adds the pieces ratatui leaves to you — a flexbox-style layout
//! solver, anchored overlays, focus/input-ownership, an alternate-screen host,
//! and a set of components (text, boxes, scroll, select, spinner, progress) —
//! while letting ratatui keep ownership of the cell buffer and its diff against
//! the terminal. It builds against `ratatui-core` (and `ratatui-crossterm` for
//! the backend) directly rather than the `ratatui` umbrella — it renders none of
//! ratatui's own widgets — so `ratatui-widgets` and `ratatui-macros` stay out of
//! its dependency tree. It otherwise depends only on `crossterm`, `textwrap`,
//! `unicode-segmentation`, and `unicode-width`.
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
//!
//! Existing ratatui widgets should normally be wrapped in [`RatatuiView`],
//! which preserves Tuika clipping without exposing the frame buffer.
//! [`TerminalSession`] and [`Runner`] are optional host-side lifecycle helpers.
//! With `feature = "async"`, `AsyncRunner` (in the `async_runner` module) is the
//! same loop for hosts that already run on Tokio.

#![warn(missing_docs)]
// On docs.rs (nightly, `--cfg docsrs`) annotate feature-gated items with the
// feature that enables them. A no-op on stable builds.
#![cfg_attr(docsrs, feature(doc_auto_cfg))]

pub mod anim;
#[cfg(feature = "async")]
pub mod async_runner;
pub mod capabilities;
pub mod clipboard;
pub mod components;
pub mod event;
#[macro_use]
mod macros;
pub mod focus;
pub mod geometry;
pub mod highlight;
pub mod host;
pub mod hyperlink;
pub mod image;
pub mod layout;
pub mod live;
pub mod markdown;
pub mod mouse;
pub mod native;
pub mod overlay;
pub mod probe;
pub mod ratatui_view;
pub mod runner;
pub mod style;
pub mod surface;
pub mod testing;
pub mod themes;
pub mod view;
pub mod width;

// Curated top-level re-exports so callers write `tuika::Flex`, `tuika::Theme`,
// etc. for the common surface without deep paths.
#[cfg(feature = "async")]
pub use async_runner::{AsyncRunner, Signal};
pub use capabilities::{Capabilities, DA1_REQUEST, DeviceAttributes};
pub use clipboard::{osc52, write_clipboard};
pub use components::{
    Boxed, CodeBlock, Column, Constrained, Flex, FocusScope, ImageResolver, KeyHints, Loader,
    Markdown, MarkdownImage, MarkdownState, Paragraph, ProgressBar, Responsive, Rule, Scroll,
    ScrollState, SelectList, SelectOutcome, SelectState, Spacer, Spinner, SpinnerStyle, StatusBar,
    Table, Tabs, TabsState, Text, TextInput, TextInputEvent, TextInputMode, TextInputState, Wrap,
    markdown_to_lines, markdown_to_linked_lines, wrap_lines,
};
pub use event::{Event, EventFlow, Key, KeyCode, Mouse, MouseButton, MouseKind};
pub use focus::FocusRegistry;
pub use geometry::{Padding, Size};
pub use highlight::{CodeHighlighter, Highlighter, PlainHighlighter};
pub use host::{AltScreen, Overlay, TerminalSession, paint, paint_with_sheet, translate_event};
pub use hyperlink::{
    BufferLink, HyperlinkBackend, LinkPolicy, apply_buffer_links, ctrl_click_url,
    ctrl_click_url_with, is_web_url, osc8, osc8_with, write_line, write_line_with,
};
pub use image::{Image, ImageData, ImageLayer, ImageSupport};
pub use layout::{Align, Dimension, Direction, Item, Justify, LayoutStyle, solve};
pub use live::{Live, LiveView, RedrawHandle};
pub use mouse::{
    Click, ClickTracker, HitMap, SelectionRange, SelectionState, highlight, selected_text,
};
pub use native::{ProgressState, TerminalProgress};
pub use overlay::{Anchor, Extent, OverlaySpec};
pub use probe::{Probe, RectProbe};
pub use ratatui_view::RatatuiView;
pub use runner::{Runner, RunnerConfig};
pub use style::{BorderStyle, CodeTheme, Role, StyleBundle, StyleSheet, Theme};
pub use surface::Surface;
pub use themes::{NamedTheme, by_name as theme_by_name};
pub use view::{Element, RenderCtx, View, element};
pub use width::{grapheme_cols, str_cols};

#[cfg(test)]
mod integration;
#[cfg(test)]
mod proptests;
#[cfg(test)]
mod snapshots;
#[cfg(test)]
mod test_support;

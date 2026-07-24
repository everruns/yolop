//! Component library.
//!
//! Every component implements [`View`](crate::view::View). Layout
//! containers ([`Flex`], [`Boxed`]) nest children; leaves ([`Text`],
//! [`Paragraph`], [`SelectList`], [`Table`], [`Scroll`], [`StatusBar`],
//! [`Spacer`]) paint content. Interactive leaves pair with a persisted `*State`
//! (see [`ScrollState`], [`SelectState`] — a [`Table`] reuses `SelectState`)
//! held by the host. To add a component, drop
//! a new module here and implement `View`; nothing else needs to change.

mod boxed;
pub(crate) mod code_block;
mod constrained;
mod flex;
mod focus_scope;
mod key_hints;
mod loader;
mod progress_bar;
mod responsive;
mod rule;
mod scroll;
mod select;
mod slider;
mod spacer;
mod spinner;
mod status_bar;
mod tab_select;
mod table;
mod tabs;
mod text;
mod textinput;

pub use crate::markdown::{
    ImageResolver, Markdown, MarkdownImage, MarkdownState, markdown_to_lines,
};
pub use boxed::Boxed;
pub use code_block::CodeBlock;
pub use constrained::Constrained;
pub use flex::Flex;
pub use focus_scope::FocusScope;
pub use key_hints::KeyHints;
pub use loader::Loader;
pub use progress_bar::ProgressBar;
pub use responsive::Responsive;
pub use rule::Rule;
pub use scroll::{Scroll, ScrollState};
pub use select::{SelectList, SelectOutcome, SelectState};
pub use slider::{Slider, SliderState};
pub use spacer::Spacer;
pub use spinner::{Spinner, SpinnerStyle};
pub use status_bar::StatusBar;
pub use tab_select::{TabSelect, TabSelectOutcome, TabSelectState};
pub use table::{Column, Table};
pub use tabs::{Tabs, TabsState};
pub use text::{Paragraph, Text, Wrap, line_width, wrap_lines};
pub use textinput::{TextInput, TextInputEvent, TextInputMode, TextInputState};

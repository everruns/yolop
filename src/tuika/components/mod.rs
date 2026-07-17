//! Component library.
//!
//! Every component implements [`View`](crate::tuika::view::View). Layout
//! containers ([`Flex`], [`Boxed`]) nest children; leaves ([`Text`],
//! [`Paragraph`], [`SelectList`], [`Scroll`], [`StatusBar`], [`Spacer`]) paint
//! content. Interactive leaves pair with a persisted `*State` (see
//! [`ScrollState`], [`SelectState`]) held by the host. To add a component, drop
//! a new module here and implement `View`; nothing else needs to change.

mod boxed;
mod flex;
mod scroll;
mod select;
mod spacer;
mod status_bar;
mod text;

pub use boxed::Boxed;
pub use flex::Flex;
pub use scroll::{Scroll, ScrollState};
pub use select::{SelectList, SelectOutcome, SelectState};
pub use spacer::Spacer;
pub use status_bar::StatusBar;
pub use text::{Paragraph, Text, line_width};

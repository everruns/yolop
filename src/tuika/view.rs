//! The `View` trait — the extensibility seam for components.
//!
//! Design note (why not a React-style reconciler): `tuika` rebuilds the view
//! tree from application state every frame, which is cheap because ratatui
//! already diffs the resulting cell buffer against the terminal. So views are
//! ephemeral *render descriptions* (immediate-mode structure), while the
//! interactive state that must survive across frames — scroll offset, current
//! selection, editor cursor — lives in host-persisted `State` structs (the
//! ratatui `StatefulWidget` idiom). Adding a component means implementing
//! `View` for its render, and, if interactive, a small state struct with an
//! event handler. No trait-object tree to reconcile.

use ratatui::layout::Rect;

use super::geometry::Size;
use super::style::Theme;
use super::surface::Surface;

/// Context threaded to every `View::render`.
pub struct RenderCtx<'a> {
    pub theme: &'a Theme,
    /// Whether the focused component of the frame is this one. Containers pass
    /// this down unchanged; focus-aware leaves use it to highlight borders.
    pub focused: bool,
}

impl<'a> RenderCtx<'a> {
    pub fn new(theme: &'a Theme) -> Self {
        Self {
            theme,
            focused: false,
        }
    }

    /// A child context with an explicit focus flag.
    pub fn with_focus(&self, focused: bool) -> RenderCtx<'a> {
        RenderCtx {
            theme: self.theme,
            focused,
        }
    }
}

/// A drawable, measurable UI element.
///
/// `measure` reports the intrinsic content size the view would like given
/// `available`; the flex solver uses it for `Dimension::Auto` sizing.
/// `render` paints into the `area` the layout assigned, through the clipped
/// `surface`.
pub trait View {
    fn measure(&self, available: Size) -> Size;
    fn render(&self, area: Rect, surface: &mut Surface, ctx: &RenderCtx);
}

/// Boxed view, the element type stored by containers.
pub type Element = Box<dyn View>;

impl View for Element {
    fn measure(&self, available: Size) -> Size {
        (**self).measure(available)
    }

    fn render(&self, area: Rect, surface: &mut Surface, ctx: &RenderCtx) {
        (**self).render(area, surface, ctx)
    }
}

/// Convenience to box any view into an [`Element`].
pub fn element<V: View + 'static>(view: V) -> Element {
    Box::new(view)
}

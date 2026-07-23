//! Focus-scoping container — override the render context's focus flag for a
//! subtree.
//!
//! Focus is carried on the [`RenderCtx`], and [`paint`](crate::paint) renders
//! the whole tree from one root context, so a plain [`Flex`](crate::Flex) can't
//! hand a single child `focused = true`. Multi-pane apps need exactly that: the
//! active pane's border should light up while the others stay dim. [`FocusScope`]
//! wraps a subtree and renders it with an explicit focus flag, so each pane can
//! be marked focused or not independently of the frame's root context — and any
//! focus-aware view inside it (notably [`Boxed`](crate::Boxed)) recolors
//! accordingly.

use ratatui::layout::Rect;

use crate::geometry::Size;
use crate::surface::Surface;
use crate::view::{Element, RenderCtx, View};

/// A layout-transparent wrapper that renders its child with an explicit focus
/// flag.
///
/// Measures and draws exactly as the wrapped view does — it only overrides
/// [`RenderCtx::focused`] for the subtree, so a focus-aware child (e.g.
/// [`Boxed`](crate::Boxed)) picks the focused or unfocused theme color even
/// though the surrounding frame shares one root context.
///
/// # Example
///
/// ```
/// use tuika::{Boxed, FocusScope, Text, Theme, element};
/// use tuika::testing::render;
///
/// // Two panes in one frame: the left one focused, the right one not — each
/// // Boxed border resolves its own color despite the shared root context.
/// let panes = tuika::Flex::row()
///     .grow(1, element(FocusScope::focused(element(Boxed::new(element(Text::raw("active")))))))
///     .grow(1, element(FocusScope::unfocused(element(Boxed::new(element(Text::raw("idle")))))));
///
/// let buffer = render(&panes, 24, 3, &Theme::default());
/// # let _ = buffer;
/// ```
pub struct FocusScope {
    focused: bool,
    child: Element,
}

impl FocusScope {
    /// Render `child` with the given `focused` flag.
    pub fn new(focused: bool, child: Element) -> Self {
        Self { focused, child }
    }

    /// Render `child` as focused. Shorthand for `FocusScope::new(true, child)`.
    pub fn focused(child: Element) -> Self {
        Self::new(true, child)
    }

    /// Render `child` as unfocused. Shorthand for `FocusScope::new(false, child)`.
    pub fn unfocused(child: Element) -> Self {
        Self::new(false, child)
    }
}

impl View for FocusScope {
    fn measure(&self, available: Size) -> Size {
        self.child.measure(available)
    }

    fn render(&self, area: Rect, surface: &mut Surface, ctx: &RenderCtx) {
        self.child
            .render(area, surface, &ctx.with_focus(self.focused));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Surface;
    use crate::components::{Boxed, Text};
    use crate::test_support::{buffer, rainbow_theme};
    use crate::view::{RenderCtx, element};

    /// The focus flag a `FocusScope` imposes on its subtree wins over the root
    /// context's flag — a `Boxed` inside picks the matching theme border color.
    fn corner_color(scope_focused: bool, root_focused: bool) -> ratatui::style::Color {
        let t = rainbow_theme();
        let mut buf = buffer(8, 3);
        let area = buf.area;
        let ctx = RenderCtx::new(&t).with_focus(root_focused);
        let view = FocusScope::new(scope_focused, element(Boxed::new(element(Text::raw("x")))));
        let mut surface = Surface::new(&mut buf, area);
        view.render(area, &mut surface, &ctx);
        buf[(0, 0)].fg
    }

    #[test]
    fn focus_scope_overrides_root_focus_for_its_subtree() {
        let t = rainbow_theme();
        // Scope says focused even though the root context is unfocused.
        assert_eq!(corner_color(true, false), t.border_focused);
        // Scope says unfocused even though the root context is focused.
        assert_eq!(corner_color(false, true), t.border);
    }

    #[test]
    fn focus_scope_is_layout_transparent() {
        // measure() passes straight through to the child.
        let inner = Boxed::new(element(Text::raw("hello")));
        let want = inner.measure(Size::new(40, 10));
        let scoped = FocusScope::focused(element(Boxed::new(element(Text::raw("hello")))));
        assert_eq!(scoped.measure(Size::new(40, 10)), want);
    }
}

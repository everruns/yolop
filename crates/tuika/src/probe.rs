//! Reading laid-out rects back out of the view tree.
//!
//! tuika's solver assigns every view a screen [`Rect`], but keeps it internal —
//! views render into the rect they're handed and return nothing. A host that
//! builds an app UI still needs some of those rects *after* the layout runs: to
//! place the terminal cursor inside a composer, to bound a mouse selection to
//! the transcript, or to register a clickable region. [`RectProbe`] closes that
//! gap: wrap a view in [`Probe`] and the rect it was painted into is recorded
//! into a shared handle the host can read once the frame is painted.
//!
//! ```ignore
//! let input = RectProbe::new();
//! let root = view! {
//!     col {
//!         grow(1) { node(transcript) }
//!         fixed(3) { node(input.wrap(composer)) }   // reserve + probe the input
//!     }
//! };
//! paint(frame.buffer_mut(), area, &theme, root.as_ref(), &[]);
//! // Now `input.rect()` is where the composer landed — place the cursor in it.
//! ```

use std::cell::Cell;
use std::rc::Rc;

use ratatui_core::layout::Rect;

use crate::geometry::Size;
use crate::surface::Surface;
use crate::view::{Element, RenderCtx, View, element};

/// A shared handle that receives the screen [`Rect`] tuika assigned to a
/// [`Probe`]-wrapped view during the last paint. Cheap to clone (shared cell);
/// reads back [`Rect::ZERO`] until the probed view has been painted at least
/// once, and updates every frame.
#[derive(Clone, Debug, Default)]
pub struct RectProbe(Rc<Cell<Rect>>);

impl RectProbe {
    /// Create a fresh handle reading back [`Rect::ZERO`] until its probed view
    /// is painted.
    pub fn new() -> Self {
        Self::default()
    }

    /// The rect the probed view was last painted into ([`Rect::ZERO`] if it has
    /// not been painted, e.g. it was laid out with zero size or clipped away).
    pub fn rect(&self) -> Rect {
        self.0.get()
    }

    /// Wrap `view` so its painted rect is reported here. Shorthand for
    /// [`Probe::new`].
    pub fn wrap(&self, view: Element) -> Element {
        element(Probe::new(view, self))
    }

    fn set(&self, rect: Rect) {
        self.0.set(rect);
    }
}

/// A view that records the area it is painted into via a [`RectProbe`], then
/// renders its inner view unchanged. Layout-transparent: it measures and draws
/// exactly as the wrapped view does.
pub struct Probe {
    inner: Element,
    probe: RectProbe,
}

impl Probe {
    /// Wrap `inner` so its painted rect is reported to `probe`.
    pub fn new(inner: Element, probe: &RectProbe) -> Self {
        Self {
            inner,
            probe: probe.clone(),
        }
    }
}

impl View for Probe {
    fn measure(&self, available: Size) -> Size {
        self.inner.measure(available)
    }

    fn render(&self, area: Rect, surface: &mut Surface, ctx: &RenderCtx) {
        self.probe.set(area);
        self.inner.render(area, surface, ctx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::{Flex, Text};
    use crate::style::Theme;
    use crate::view::element;
    use ratatui_core::layout::Rect;

    #[test]
    fn probe_reports_the_rect_a_view_was_painted_into() {
        let probe = RectProbe::new();
        assert_eq!(probe.rect(), Rect::ZERO, "an unpainted probe reads ZERO");
        let root = Flex::column()
            .fixed(1, element(Text::raw("header")))
            .grow(1, probe.wrap(element(Text::raw("body"))));
        let theme = Theme::default();
        let _ = crate::testing::render(&root, 10, 5, &theme);
        // The grown body lands below the 1-row header and fills the rest.
        assert_eq!(probe.rect(), Rect::new(0, 1, 10, 4));
    }
}

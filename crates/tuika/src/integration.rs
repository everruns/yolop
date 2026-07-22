//! Cross-cutting integration tests for the `tuika` toolkit.
//!
//! Per-module unit tests live in each module's own `#[cfg(test)] mod tests`.
//! What remains here are the checks that span several modules at once and have
//! no single owner: composing a real tree, and surviving tiny/degenerate
//! screens where scroll and overlay resolution interact.

use ratatui::text::{Line, Span};

use crate::components::{Boxed, Flex, Scroll, ScrollState, StatusBar, Text};
use crate::overlay::OverlaySpec;
use crate::style::Theme;
use crate::surface::Surface;
use crate::test_support::{buffer, row};
use crate::view::{RenderCtx, View, element};

#[test]
fn scroll_and_overlay_survive_tiny_screens() {
    use ratatui::layout::Rect;

    // A tall scroll region rendered into a 1×1 viewport.
    let lines: Vec<Line<'static>> = (0..50).map(|i| Line::from(format!("l{i}"))).collect();
    let mut st = ScrollState::new();
    st.clamp(50, 1);
    let theme = Theme::default();
    let ctx = RenderCtx::new(&theme);
    let mut buf = buffer(1, 1);
    let area = buf.area;
    let mut surface = Surface::new(&mut buf, area);
    Scroll::new(lines, &st).render(area, &mut surface, &ctx);

    // Overlay resolution on tiny/zero screens stays within bounds.
    for (w, h) in [(0u16, 0u16), (1, 1), (2, 3), (5, 5)] {
        let screen = Rect::new(0, 0, w, h);
        let rect = OverlaySpec::centered(80, 80).min_size(4, 4).resolve(screen);
        assert!(rect.right() <= screen.right(), "overlay exceeds {screen:?}");
        assert!(
            rect.bottom() <= screen.bottom(),
            "overlay exceeds {screen:?}"
        );
    }
}

#[test]
fn nested_flex_tree_lays_out_status_and_body() {
    let theme = Theme::default();
    let mut buf = buffer(24, 6);
    let area = buf.area;

    let body = Boxed::new(element(Text::raw("hi"))).title("Body");
    let status = StatusBar::new().left(vec![Span::raw("model: sim")]);
    let tree = Flex::column()
        .grow(1, element(body))
        .fixed(1, element(status));

    let ctx = RenderCtx::new(&theme);
    let mut surface = Surface::new(&mut buf, area);
    tree.render(area, &mut surface, &ctx);

    // Status bar occupies the last row.
    assert!(row(&buf, 5).starts_with("model: sim"));
    // Body box drew a rounded border on the top row with its title.
    assert!(row(&buf, 0).contains("Body"));
    assert_eq!(buf[(0, 0)].symbol(), "╭");
}

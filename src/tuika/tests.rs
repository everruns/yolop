//! Unit tests for the `tuika` toolkit. These exercise the layout math and
//! interactive state directly, and the components/compositor by rendering into
//! an in-memory ratatui [`Buffer`] and reading cells back — no real terminal.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};

use super::anim;
use super::components::{
    Loader, Paragraph, ProgressBar, Scroll, ScrollState, SelectList, SelectOutcome, SelectState,
    Spinner, Text,
};
use super::event::{Event, EventFlow, Key, KeyCode, Mouse, MouseKind};
use super::focus::FocusRegistry;
use super::geometry::{Padding, Size};
use super::host::{self, Overlay};
use super::layout::{Align, Dimension, Direction, Item, Justify, LayoutStyle, solve};
use super::native::{self, ProgressState};
use super::overlay::{Anchor, Extent, OverlaySpec};
use super::style::Theme;
use super::surface::Surface;
use super::view::{RenderCtx, View, element};

/// Read a buffer row into a trimmed string for assertions.
fn row(buffer: &Buffer, y: u16) -> String {
    let area = buffer.area;
    let mut s = String::new();
    for x in area.x..area.right() {
        s.push_str(buffer[(x, y)].symbol());
    }
    s.trim_end().to_string()
}

fn buffer(width: u16, height: u16) -> Buffer {
    Buffer::empty(Rect::new(0, 0, width, height))
}

fn item(dim: Dimension, w: u16, h: u16) -> Item {
    Item::new(dim, Size::new(w, h))
}

// ---- layout solver -------------------------------------------------------

#[test]
fn flex_distributes_leftover_to_grow_children() {
    let area = Rect::new(0, 0, 30, 1);
    let style = LayoutStyle::row();
    let items = [
        item(Dimension::Fixed(10), 10, 1),
        item(Dimension::Flex(1), 0, 1),
        item(Dimension::Flex(1), 0, 1),
    ];
    let rects = solve(area, &style, &items);
    assert_eq!(rects[0].width, 10);
    // 20 leftover split evenly.
    assert_eq!(rects[1].width, 10);
    assert_eq!(rects[2].width, 10);
    // Contiguous placement.
    assert_eq!(rects[1].x, 10);
    assert_eq!(rects[2].x, 20);
}

#[test]
fn flex_grow_weights_and_remainder_fill_exactly() {
    let area = Rect::new(0, 0, 10, 1);
    let style = LayoutStyle::row();
    let items = [
        item(Dimension::Flex(1), 0, 1),
        item(Dimension::Flex(2), 0, 1),
    ];
    let rects = solve(area, &style, &items);
    // Weighted 1:2 across 10 cells; the last flex child absorbs the remainder.
    assert_eq!(rects[0].width + rects[1].width, 10);
    assert_eq!(rects[0].width, 3);
    assert_eq!(rects[1].width, 7);
}

#[test]
fn flex_percent_and_gap() {
    let area = Rect::new(0, 0, 20, 1);
    let style = LayoutStyle::row().gap(2);
    let items = [
        item(Dimension::Percent(50), 0, 1),
        item(Dimension::Auto, 4, 1),
    ];
    let rects = solve(area, &style, &items);
    // space_for_children = 20 - gap(2) = 18; 50% = 9.
    assert_eq!(rects[0].width, 9);
    assert_eq!(rects[1].x, rects[0].x + 9 + 2);
}

#[test]
fn column_stretch_fills_cross_axis() {
    let area = Rect::new(0, 0, 12, 6);
    let style = LayoutStyle::column().align(Align::Stretch);
    let items = [
        item(Dimension::Fixed(2), 3, 2),
        item(Dimension::Fixed(2), 5, 2),
    ];
    let rects = solve(area, &style, &items);
    assert_eq!(rects[0].width, 12);
    assert_eq!(rects[1].width, 12);
    assert_eq!(rects[0].height, 2);
    assert_eq!(rects[1].y, 2);
}

#[test]
fn justify_center_and_end_offset_main_axis() {
    let area = Rect::new(0, 0, 20, 1);
    let items = [item(Dimension::Fixed(4), 4, 1)];
    let center = solve(area, &LayoutStyle::row().justify(Justify::Center), &items);
    assert_eq!(center[0].x, 8); // (20-4)/2
    let end = solve(area, &LayoutStyle::row().justify(Justify::End), &items);
    assert_eq!(end[0].x, 16);
}

#[test]
fn padding_shrinks_layout_area() {
    let area = Rect::new(0, 0, 20, 5);
    let style = LayoutStyle::column().padding(Padding::all(1));
    let items = [item(Dimension::Flex(1), 0, 0)];
    let rects = solve(area, &style, &items);
    assert_eq!(rects[0].x, 1);
    assert_eq!(rects[0].y, 1);
    assert_eq!(rects[0].width, 18);
    assert_eq!(rects[0].height, 3);
}

// ---- text / paragraph ----------------------------------------------------

#[test]
fn text_renders_and_clips_to_width() {
    let mut buf = buffer(6, 2);
    let text = Text::new(vec![Line::from("hello world"), Line::from("hi")]);
    let theme = Theme::default();
    let ctx = RenderCtx::new(&theme);
    let area = buf.area;
    let mut surface = Surface::new(&mut buf, area);
    text.render(area, &mut surface, &ctx);
    // Clipped to 6 columns ("hello " with a trailing space, which `row` trims).
    assert_eq!(row(&buf, 0), "hello");
    assert_eq!(row(&buf, 1), "hi");
}

#[test]
fn paragraph_wraps_to_width() {
    let p = Paragraph::new("the quick brown fox", Style::default());
    let size = p.measure(Size::new(10, 10));
    assert!(size.height >= 2, "expected wrap, got {size:?}");
    assert!(size.width <= 10);
}

// ---- scroll state --------------------------------------------------------

#[test]
fn scroll_sticks_to_bottom_until_scrolled_up() {
    let mut s = ScrollState::new();
    // content 100 rows, viewport 10 => bottom offset 90.
    s.clamp(100, 10);
    assert_eq!(s.offset(), 90);
    assert!(s.is_stuck_to_bottom());

    // Wheel up unsticks and moves up by 3.
    let up = Event::Mouse(Mouse {
        kind: MouseKind::ScrollUp,
        column: 0,
        row: 0,
    });
    assert_eq!(s.handle(&up, 100, 10), EventFlow::Consumed);
    assert!(!s.is_stuck_to_bottom());
    assert_eq!(s.offset(), 87);

    // Growing content no longer drags the view down while unstuck.
    s.clamp(200, 10);
    assert_eq!(s.offset(), 87);
}

#[test]
fn scroll_end_key_rearms_bottom_stick() {
    let mut s = ScrollState::new();
    s.clamp(100, 10);
    s.jump_to_top();
    assert_eq!(s.offset(), 0);
    assert!(!s.is_stuck_to_bottom());
    let end = Event::Key(Key::new(KeyCode::End));
    assert_eq!(s.handle(&end, 100, 10), EventFlow::Consumed);
    assert_eq!(s.offset(), 90);
    assert!(s.is_stuck_to_bottom());
}

#[test]
fn scroll_view_windows_content_and_draws_scrollbar() {
    let lines: Vec<Line<'static>> = (0..20).map(|i| Line::from(format!("line{i}"))).collect();
    let mut state = ScrollState::new();
    state.clamp(20, 5); // stuck to bottom => offset 15
    let scroll = Scroll::new(lines, &state);
    let mut buf = buffer(10, 5);
    let theme = Theme::default();
    let ctx = RenderCtx::new(&theme);
    let area = buf.area;
    let mut surface = Surface::new(&mut buf, area);
    scroll.render(area, &mut surface, &ctx);
    // Bottom-stuck: shows the last five lines (15..20).
    assert!(row(&buf, 0).starts_with("line15"));
    assert!(row(&buf, 4).starts_with("line19"));
    // Scrollbar drawn in the last column somewhere.
    let has_bar = (0..5).any(|y| {
        let c = buf[(9, y)].symbol().to_string();
        c == "█" || c == "│"
    });
    assert!(has_bar, "expected a scrollbar in the right column");
}

// ---- select --------------------------------------------------------------

#[test]
fn select_navigation_wraps_and_confirms() {
    let mut s = SelectState::new();
    let down = Event::Key(Key::new(KeyCode::Down));
    let up = Event::Key(Key::new(KeyCode::Up));
    assert_eq!(s.handle(&up, 3), SelectOutcome::Moved(EventFlow::Consumed));
    assert_eq!(s.selected(), 2); // wrapped from 0 to last
    assert_eq!(
        s.handle(&down, 3),
        SelectOutcome::Moved(EventFlow::Consumed)
    );
    assert_eq!(s.selected(), 0); // wrapped back
    let enter = Event::Key(Key::new(KeyCode::Enter));
    assert_eq!(s.handle(&enter, 3), SelectOutcome::Confirmed(0));
    let esc = Event::Key(Key::new(KeyCode::Esc));
    assert_eq!(s.handle(&esc, 3), SelectOutcome::Cancelled);
}

#[test]
fn select_highlights_current_row() {
    let items = vec![Line::from("alpha"), Line::from("beta")];
    let mut state = SelectState::new();
    state.handle(&Event::Key(Key::new(KeyCode::Down)), 2); // select beta
    let list = SelectList::new(items, &state);
    let mut buf = buffer(10, 2);
    let theme = Theme::default();
    let ctx = RenderCtx::new(&theme);
    let area = buf.area;
    let mut surface = Surface::new(&mut buf, area);
    list.render(area, &mut surface, &ctx);
    assert!(row(&buf, 1).contains("beta"));
    // Selected row carries the selection background.
    assert_eq!(buf[(0, 1)].bg, theme.selection_bg);
    assert_eq!(buf[(0, 0)].bg, ratatui::style::Color::Reset);
}

// ---- overlay -------------------------------------------------------------

#[test]
fn overlay_centered_percentage() {
    let screen = Rect::new(0, 0, 100, 40);
    let spec = OverlaySpec::centered(50, 50);
    let rect = spec.resolve(screen);
    assert_eq!(rect.width, 50);
    assert_eq!(rect.height, 20);
    assert_eq!(rect.x, 25);
    assert_eq!(rect.y, 10);
}

#[test]
fn overlay_anchors_to_corner_with_margin() {
    let screen = Rect::new(0, 0, 100, 40);
    let spec = OverlaySpec {
        anchor: Anchor::BottomRight,
        width: Extent::Cells(20),
        height: Extent::Cells(10),
        min_width: 0,
        min_height: 0,
        max_width: u16::MAX,
        max_height: u16::MAX,
        margin: 2,
    };
    let rect = spec.resolve(screen);
    assert_eq!(rect.width, 20);
    assert_eq!(rect.height, 10);
    // Bottom-right inside a 2-cell margin: right edge at 98, bottom at 38.
    assert_eq!(rect.right(), 98);
    assert_eq!(rect.bottom(), 38);
}

#[test]
fn overlay_clamps_to_max() {
    let screen = Rect::new(0, 0, 100, 40);
    let spec = OverlaySpec::centered(90, 90).max_size(40, 20);
    let rect = spec.resolve(screen);
    assert_eq!(rect.width, 40);
    assert_eq!(rect.height, 20);
}

// ---- focus ---------------------------------------------------------------

#[test]
fn focus_tab_cycles_registered_regions() {
    let mut f = FocusRegistry::new();
    f.begin_frame();
    f.register("a");
    f.register("b");
    f.register("c");
    assert!(f.is_focused("a"));
    let tab = Event::Key(Key::new(KeyCode::Tab));
    assert_eq!(f.handle(&tab), EventFlow::Consumed);
    assert!(f.is_focused("b"));
    let back = Event::Key(Key::new(KeyCode::BackTab));
    f.handle(&back);
    assert!(f.is_focused("a"));
    // Wrap backwards.
    f.handle(&back);
    assert!(f.is_focused("c"));
}

#[test]
fn overlay_owner_takes_input_and_blocks_tab() {
    let mut f = FocusRegistry::new();
    f.begin_frame();
    f.register("composer");
    f.set_owner("dialog");
    assert!(f.is_active("dialog"));
    assert!(!f.is_active("composer"));
    // Tab is swallowed while an overlay owns input.
    let tab = Event::Key(Key::new(KeyCode::Tab));
    assert_eq!(f.handle(&tab), EventFlow::Ignored);
    f.clear_owner();
    assert!(f.is_active("composer"));
}

// ---- compositor ----------------------------------------------------------

#[test]
fn paint_composites_background_root_and_overlay() {
    let theme = Theme::default();
    let mut buf = buffer(20, 5);
    let area = buf.area;
    let root = Text::new(vec![Line::from(Span::raw("base layer"))]);
    let dialog = Text::new(vec![Line::from("MODAL")]);
    let overlay_area = Rect::new(5, 2, 7, 1);
    let overlays = [Overlay {
        area: overlay_area,
        view: &dialog,
        clear: true,
    }];
    host::paint(&mut buf, area, &theme, &root, &overlays);
    // Root text on the top row.
    assert!(row(&buf, 0).starts_with("base layer"));
    // Background fill applied everywhere.
    assert_eq!(buf[(0, 0)].bg, theme.background);
    // Overlay painted last on its row with a surface background.
    assert!(row(&buf, 2).contains("MODAL"));
    assert_eq!(buf[(5, 2)].bg, theme.surface);
}

// ---- animation / progress ------------------------------------------------

#[test]
fn easing_endpoints_and_midpoints() {
    for f in [
        anim::linear,
        anim::ease_in,
        anim::ease_out,
        anim::ease_in_out,
    ] {
        assert!((f(0.0) - 0.0).abs() < 1e-6);
        assert!((f(1.0) - 1.0).abs() < 1e-6);
    }
    // Cubic ease-in-out is symmetric about 0.5.
    assert!((anim::ease_in_out(0.5) - 0.5).abs() < 1e-6);
    // Clamps out-of-range input.
    assert_eq!(anim::linear(2.0), 1.0);
    assert_eq!(anim::ease_out(-1.0), 0.0);
}

#[test]
fn ping_pong_and_sawtooth_shapes() {
    assert!((anim::ping_pong(0, 60) - 0.0).abs() < 1e-6);
    assert!((anim::ping_pong(30, 60) - 1.0).abs() < 1e-6); // peak at half period
    assert!((anim::ping_pong(60, 60) - 0.0).abs() < 1e-6); // back to start
    assert!((anim::sawtooth(0, 10) - 0.0).abs() < 1e-6);
    assert!((anim::sawtooth(5, 10) - 0.5).abs() < 1e-6);
    assert!((anim::sawtooth(10, 10) - 0.0).abs() < 1e-6); // wraps
}

#[test]
fn spinner_cycles_frames() {
    let frames = super::components::SpinnerStyle::Braille.frames();
    assert_eq!(Spinner::new(0).glyph(), frames[0]);
    assert_eq!(Spinner::new(1).glyph(), frames[1]);
    // Wraps at the end of the frame set.
    assert_eq!(Spinner::new(frames.len() as u64).glyph(), frames[0]);
}

#[test]
fn progress_bar_determinate_fills_by_fraction() {
    let bar = ProgressBar::determinate(0.5);
    assert_eq!(bar.percent_value(), Some(50));
    let mut buf = buffer(10, 1);
    let theme = Theme::default();
    let ctx = RenderCtx::new(&theme);
    let area = buf.area;
    let mut surface = Surface::new(&mut buf, area);
    bar.render(area, &mut surface, &ctx);
    // Half of 10 cells fully filled.
    let full = (0..10).filter(|&x| buf[(x, 0)].symbol() == "█").count();
    assert_eq!(full, 5);
}

#[test]
fn progress_bar_full_and_percent_label() {
    let bar = ProgressBar::determinate(1.0).percent(true);
    let mut buf = buffer(20, 1);
    let theme = Theme::default();
    let ctx = RenderCtx::new(&theme);
    let area = buf.area;
    let mut surface = Surface::new(&mut buf, area);
    bar.render(area, &mut surface, &ctx);
    assert!(row(&buf, 0).contains("100%"));
    // Bar area (minus the " 100%" suffix = 5 cols) is fully filled.
    let full = (0..15).filter(|&x| buf[(x, 0)].symbol() == "█").count();
    assert_eq!(full, 15);
}

#[test]
fn progress_bar_indeterminate_has_segment_and_track() {
    let bar = ProgressBar::indeterminate(0);
    let mut buf = buffer(12, 1);
    let theme = Theme::default();
    let ctx = RenderCtx::new(&theme);
    let area = buf.area;
    let mut surface = Surface::new(&mut buf, area);
    bar.render(area, &mut surface, &ctx);
    let seg = (0..12).filter(|&x| buf[(x, 0)].symbol() == "█").count();
    let track = (0..12).filter(|&x| buf[(x, 0)].symbol() == "░").count();
    assert!(seg > 0, "expected a bright segment");
    assert!(track > 0, "expected a dim track");
    assert_eq!(seg + track, 12);
}

#[test]
fn loader_renders_spinner_and_message() {
    let loader = Loader::new(0, "thinking").hint("esc to cancel");
    let mut buf = buffer(30, 1);
    let theme = Theme::default();
    let ctx = RenderCtx::new(&theme);
    let area = buf.area;
    let mut surface = Surface::new(&mut buf, area);
    loader.render(area, &mut surface, &ctx);
    let line = row(&buf, 0);
    assert!(line.contains("thinking"), "{line}");
    assert!(line.contains("esc to cancel"), "{line}");
}

#[test]
fn osc_progress_encoding() {
    // ESC ] 9 ; 4 ; state ; percent BEL
    assert_eq!(
        native::encode(ProgressState::Indeterminate, 0),
        "\x1b]9;4;3;0\x07"
    );
    assert_eq!(
        native::encode(ProgressState::Normal, 50),
        "\x1b]9;4;1;50\x07"
    );
    assert_eq!(native::encode(ProgressState::Clear, 0), "\x1b]9;4;0;0\x07");
    assert_eq!(
        native::encode(ProgressState::Error, 12),
        "\x1b]9;4;2;12\x07"
    );
    // Percent is clamped to 100.
    assert_eq!(
        native::encode(ProgressState::Normal, 200),
        "\x1b]9;4;1;100\x07"
    );
}

// ---- a small end-to-end tree ---------------------------------------------

#[test]
fn nested_flex_tree_lays_out_status_and_body() {
    use super::components::{Boxed, Flex, StatusBar};
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

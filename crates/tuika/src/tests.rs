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
    Spinner, Text, Wrap, line_width, wrap_lines,
};
use super::event::{Event, EventFlow, Key, KeyCode, Mouse, MouseButton, MouseKind};
use super::focus::FocusRegistry;
use super::geometry::{Padding, Size};
use super::host::{self, Overlay};
use super::layout::{Align, Dimension, Item, Justify, LayoutStyle, solve};
use super::mouse::{ClickTracker, HitMap, SelectionState, highlight, selected_text};
use super::native::{self, ProgressState};
use super::overlay::{Anchor, Extent, OverlaySpec};
use super::style::Theme;
use super::surface::Surface;
use super::view::{RenderCtx, View, element};

// ---- ratatui interoperability -------------------------------------------

#[test]
fn ratatui_rendering_preserves_clip_even_if_buffer_expands() {
    let mut buf = Buffer::filled(Rect::new(0, 0, 12, 3), ratatui::buffer::Cell::new("#"));
    let clip = Rect::new(4, 1, 4, 1);
    {
        let mut surface = Surface::new(&mut buf, clip);
        surface.render_ratatui(Rect::new(2, 0, 8, 3), |_area, scratch| {
            let expanded = Buffer::filled(Rect::new(0, 0, 12, 3), ratatui::buffer::Cell::new("x"));
            scratch.merge(&expanded);
        });
    }

    for y in 0..3 {
        for x in 0..12 {
            let expected = if y == 1 && (4..8).contains(&x) {
                "x"
            } else {
                "#"
            };
            assert_eq!(buf[(x, y)].symbol(), expected, "cell ({x}, {y})");
        }
    }
}

#[test]
fn ratatui_rendering_starts_with_existing_cells() {
    use ratatui::style::Color;

    let mut cell = ratatui::buffer::Cell::new(".");
    cell.set_bg(Color::Blue);
    let mut buf = Buffer::filled(Rect::new(0, 0, 4, 1), cell);
    let area = buf.area;
    {
        let mut surface = Surface::new(&mut buf, area);
        surface.render_ratatui(area, |_area, scratch| {
            scratch[(1, 0)].set_char('x');
        });
    }
    assert_eq!(buf[(1, 0)].symbol(), "x");
    assert_eq!(buf[(1, 0)].bg, Color::Blue);
}

#[test]
fn ratatui_rendering_limits_scratch_to_the_frame() {
    let mut buf = buffer(3, 2);
    let area = buf.area;
    let mut surface = Surface::new(&mut buf, area);
    surface.render_ratatui(Rect::new(0, 0, u16::MAX, u16::MAX), |actual, _| {
        assert_eq!(actual, Rect::new(0, 0, 3, 2));
    });
}

#[test]
fn ratatui_rendering_tolerates_a_callback_that_resizes_its_buffer() {
    let mut buf = Buffer::filled(Rect::new(0, 0, 4, 1), ratatui::buffer::Cell::new("#"));
    let area = buf.area;
    let mut surface = Surface::new(&mut buf, area);
    surface.render_ratatui(area, |_area, scratch| {
        scratch.resize(Rect::new(0, 0, 1, 1));
        scratch[(0, 0)].set_char('x');
    });
    assert_eq!(row(&buf, 0), "x###");
}

#[test]
fn ratatui_view_renders_borrowing_widget_data() {
    use ratatui::widgets::{Sparkline, Widget};

    let data = vec![1, 2, 4, 8];
    let view = crate::RatatuiView::fill(move |area, buffer| {
        Sparkline::default().data(&data).render(area, buffer);
    });
    let theme = Theme::default();
    let rendered = crate::testing::render(&view, 4, 1, &theme);
    assert_ne!(row(&rendered, 0), "");
}

// ---- responsive and data-driven views ----------------------------------

#[test]
fn responsive_selects_layout_from_render_width() {
    let view = crate::Responsive::new(
        10,
        element(Text::raw("compact")),
        element(Text::raw("wide")),
    );
    let theme = Theme::default();
    let compact = crate::testing::render(&view, 8, 1, &theme);
    let wide = crate::testing::render(&view, 12, 1, &theme);
    assert_eq!(row(&compact, 0), "compact");
    assert_eq!(row(&wide, 0), "wide");
}

#[test]
fn constrained_clamps_intrinsic_measurement() {
    let constrained = crate::Constrained::new(element(Text::raw("long content")))
        .min_size(4, 1)
        .max_size(6, 2);
    assert_eq!(constrained.measure(Size::new(20, 10)), Size::new(6, 1));
    assert_eq!(constrained.measure(Size::new(3, 1)), Size::new(3, 1));
}

#[test]
fn live_view_reads_updated_data_without_reconstruction() {
    let redraw = crate::RedrawHandle::default();
    let value = crate::Live::with_redraw(1u32, redraw.clone());
    let view = crate::LiveView::new(value.clone(), |value| {
        element(Text::raw(format!("count: {value}")))
    });
    let theme = Theme::default();
    assert_eq!(
        row(&crate::testing::render(&view, 12, 1, &theme), 0),
        "count: 1"
    );
    value.set(2);
    assert!(redraw.take());
    assert_eq!(
        row(&crate::testing::render(&view, 12, 1, &theme), 0),
        "count: 2"
    );
}

#[test]
fn tabs_state_wraps_and_tabs_render_selection() {
    let mut state = crate::TabsState::default();
    assert_eq!(
        state.handle(&Event::Key(Key::new(KeyCode::Left)), 3),
        EventFlow::Consumed
    );
    assert_eq!(state.selected(), 2);

    let tabs = crate::Tabs::new(
        vec![Line::from("one"), Line::from("two"), Line::from("three")],
        &state,
    );
    let theme = Theme::default();
    let rendered = crate::testing::render(&tabs, 20, 1, &theme);
    assert!(row(&rendered, 0).contains("one  two  three"));
    assert!(
        rendered[(10, 0)]
            .modifier
            .contains(ratatui::style::Modifier::UNDERLINED)
    );
}

#[test]
fn key_hints_measure_unicode_by_terminal_width() {
    let hints = crate::KeyHints::new([("⌘", "開く")]);
    assert_eq!(hints.measure(Size::new(20, 1)), Size::new(8, 1));
}

#[test]
fn public_testing_grid_is_stable_and_rectangular() {
    let theme = Theme::default();
    let rendered = crate::testing::render(&Text::raw("hi"), 3, 2, &theme);
    assert_eq!(crate::testing::grid(&rendered), "hi \n   ");
}

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

// ---- styled wrapping (wrap_lines / Wrap) ---------------------------------

/// Concatenated text of a line's spans.
fn line_text(line: &Line) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}

#[test]
fn wrap_lines_breaks_on_word_boundaries() {
    let out = wrap_lines(&[Line::from("the quick brown fox jumps")], 9);
    assert!(
        out.iter().all(|l| line_width(l) <= 9),
        "no output line may exceed the width: {out:?}"
    );
    // Every word survives, in order, un-split.
    let words: Vec<String> = out
        .iter()
        .flat_map(|l| {
            line_text(l)
                .split_whitespace()
                .map(String::from)
                .collect::<Vec<_>>()
        })
        .collect();
    assert_eq!(words, ["the", "quick", "brown", "fox", "jumps"]);
}

#[test]
fn wrap_lines_preserves_span_styles() {
    let red = Style::default().fg(Color::Red);
    let blue = Style::default().fg(Color::Blue);
    let line = Line::from(vec![
        Span::styled("red", red),
        Span::raw(" "),
        Span::styled("blue", blue),
    ]);
    // Wide enough that nothing wraps.
    let out = wrap_lines(&[line], 40);
    assert_eq!(out.len(), 1);
    let spans = &out[0].spans;
    assert!(
        spans
            .iter()
            .any(|s| s.content.starts_with("red") && s.style.fg == Some(Color::Red)),
        "red run lost its style: {spans:?}"
    );
    assert!(
        spans
            .iter()
            .any(|s| s.content.contains("blue") && s.style.fg == Some(Color::Blue)),
        "blue run lost its style: {spans:?}"
    );
}

#[test]
fn wrap_lines_style_survives_a_break() {
    let accent = Style::default()
        .fg(Color::Blue)
        .add_modifier(Modifier::UNDERLINED);
    // "aaaa bbbb", all accent, width 4 -> two lines, both still accent.
    let out = wrap_lines(&[Line::from(Span::styled("aaaa bbbb", accent))], 4);
    assert_eq!(out.len(), 2, "{out:?}");
    for l in &out {
        assert!(
            l.spans.iter().all(|s| s.style.fg == Some(Color::Blue)
                && s.style.add_modifier.contains(Modifier::UNDERLINED)),
            "wrapped line dropped styling: {l:?}"
        );
    }
}

#[test]
fn wrap_lines_hard_breaks_overlong_word() {
    let word = "x".repeat(20);
    let out = wrap_lines(&[Line::from(word.clone())], 8);
    assert!(out.len() >= 3, "a 20-col word at width 8 needs >=3 lines");
    assert!(out.iter().all(|l| line_width(l) <= 8));
    let joined: String = out.iter().map(|l| line_text(l)).collect();
    assert_eq!(joined, word, "hard-break must not lose characters");
}

#[test]
fn wrap_lines_counts_wide_glyphs() {
    // Each CJK glyph is 2 columns; no whitespace, so it hard-breaks at width 4.
    let out = wrap_lines(&[Line::from("你好世界")], 4);
    assert!(out.iter().all(|l| line_width(l) <= 4), "{out:?}");
    let joined: String = out.iter().map(|l| line_text(l)).collect();
    assert_eq!(joined, "你好世界");
}

#[test]
fn set_string_places_multi_scalar_emoji_in_one_wide_cell() {
    // A ZWJ family (multiple scalars) is a single grapheme: it must occupy one
    // cell and advance the cursor by 2, not scatter one component per cell.
    const FAMILY: &str = "👨\u{200D}👩\u{200D}👧";
    let mut buf = Buffer::filled(Rect::new(0, 0, 6, 1), ratatui::buffer::Cell::new("."));
    let end = {
        let mut surface = Surface::new(&mut buf, Rect::new(0, 0, 6, 1));
        surface.set_string(0, 0, &format!("{FAMILY}x"), Style::default())
    };
    assert_eq!(
        buf[(0, 0)].symbol(),
        FAMILY,
        "whole cluster lands in one cell"
    );
    assert_eq!(buf[(1, 0)].symbol(), " ", "trailing half of the wide glyph");
    assert_eq!(buf[(2, 0)].symbol(), "x", "next glyph starts at column 2");
    assert_eq!(end, 3, "cursor advanced 2 (emoji) + 1 (x)");
}

#[test]
fn wrap_lines_keeps_emoji_clusters_intact() {
    // "❤️" carries VS16 → width 2. A grapheme must never be split mid-cluster
    // by the wrapper, and each output line must respect the width budget.
    let out = wrap_lines(&[Line::from("❤\u{FE0F} 你 ok")], 4);
    assert!(out.iter().all(|l| line_width(l) <= 4), "{out:?}");
    let joined: String = out.iter().map(|l| line_text(l)).collect();
    assert!(
        joined.contains("❤\u{FE0F}"),
        "heart+VS16 survived: {joined:?}"
    );
}

#[test]
fn wrap_lines_keeps_blank_lines() {
    let lines = vec![Line::from("a"), Line::from(""), Line::from("b")];
    let out = wrap_lines(&lines, 10);
    assert_eq!(
        out.len(),
        3,
        "a blank line must stay one blank row: {out:?}"
    );
    assert_eq!(line_text(&out[1]), "");
}

#[test]
fn wrap_lines_zero_width_is_identity() {
    let out = wrap_lines(&[Line::from("hello world")], 0);
    assert_eq!(out.len(), 1);
    assert_eq!(line_text(&out[0]), "hello world");
}

#[test]
fn wrap_component_renders_reflowed() {
    // "aa bb cc" at width 5 wraps to "aa bb" / "cc".
    let mut buf = buffer(5, 3);
    let w = Wrap::new(vec![Line::from("aa bb cc")]);
    let theme = Theme::default();
    let ctx = RenderCtx::new(&theme);
    let area = buf.area;
    let mut surface = Surface::new(&mut buf, area);
    w.render(area, &mut surface, &ctx);
    assert_eq!(row(&buf, 0), "aa bb");
    assert_eq!(row(&buf, 1), "cc");
}

// ---- mouse: selection, hit-testing, clicks -------------------------------

fn down(col: u16, row: u16) -> Mouse {
    Mouse::at(MouseKind::Down(MouseButton::Left), col, row)
}
fn drag(col: u16, row: u16) -> Mouse {
    Mouse::at(MouseKind::Drag(MouseButton::Left), col, row)
}
fn up(col: u16, row: u16) -> Mouse {
    Mouse::at(MouseKind::Up(MouseButton::Left), col, row)
}

#[test]
fn selection_tracks_a_left_drag() {
    let mut sel = SelectionState::new();
    assert!(!sel.handle(&down(1, 0))); // fresh press: nothing to redraw yet
    assert!(sel.handle(&drag(4, 0)));
    assert!(sel.handle(&up(4, 0)));
    let range = sel.range().expect("a drag selects");
    assert_eq!(range.start, (1, 0));
    assert_eq!(range.end, (4, 0));
    assert!(sel.is_active());
}

#[test]
fn selection_normalizes_a_backwards_drag() {
    let mut sel = SelectionState::new();
    sel.handle(&down(5, 1));
    sel.handle(&drag(2, 0));
    sel.handle(&up(2, 0));
    let range = sel.range().expect("selection");
    // Reading order: (col=2,row=0) comes before (col=5,row=1).
    assert_eq!(range.start, (2, 0));
    assert_eq!(range.end, (5, 1));
}

#[test]
fn a_plain_click_leaves_no_selection() {
    let mut sel = SelectionState::new();
    sel.handle(&down(3, 0));
    sel.handle(&up(3, 0)); // released on the same cell, no drag
    assert!(sel.range().is_none());
    assert!(!sel.is_active());
}

#[test]
fn a_new_press_clears_the_previous_selection() {
    let mut sel = SelectionState::new();
    sel.handle(&down(0, 0));
    sel.handle(&drag(3, 0));
    sel.handle(&up(3, 0));
    assert!(sel.range().is_some());
    // Pressing again to start a new gesture must drop the old selection.
    assert!(sel.handle(&down(1, 1)));
    assert!(sel.range().is_none());
}

#[test]
fn selected_text_reads_one_row() {
    let theme = Theme::default();
    let buf = crate::testing::render(&Text::raw("hello world"), 11, 1, &theme);
    let mut sel = SelectionState::new();
    sel.handle(&down(0, 0));
    sel.handle(&drag(4, 0));
    sel.handle(&up(4, 0));
    let text = selected_text(&buf, buf.area, sel.range().unwrap());
    assert_eq!(text, "hello");
}

#[test]
fn selected_text_spans_rows_linearly() {
    let theme = Theme::default();
    let lines = vec![Line::from("hello"), Line::from("world")];
    let buf = crate::testing::render(&Text::new(lines), 5, 2, &theme);
    let mut sel = SelectionState::new();
    // From the "llo" of row 0 through the "wo" of row 1.
    sel.handle(&down(2, 0));
    sel.handle(&drag(1, 1));
    sel.handle(&up(1, 1));
    let text = selected_text(&buf, buf.area, sel.range().unwrap());
    assert_eq!(text, "llo\nwo");
}

#[test]
fn selected_text_trims_trailing_blanks() {
    let theme = Theme::default();
    let buf = crate::testing::render(&Text::raw("hi"), 10, 1, &theme);
    let mut sel = SelectionState::new();
    sel.handle(&down(0, 0));
    sel.handle(&drag(9, 0)); // drag well past the text into blank cells
    sel.handle(&up(9, 0));
    let text = selected_text(&buf, buf.area, sel.range().unwrap());
    assert_eq!(text, "hi");
}

#[test]
fn highlight_patches_selected_cells_only() {
    use ratatui::style::Color;
    let theme = Theme::default();
    let mut buf = crate::testing::render(&Text::raw("hello"), 5, 1, &theme);
    let area = buf.area;
    let mut sel = SelectionState::new();
    sel.handle(&down(0, 0));
    sel.handle(&drag(2, 0));
    sel.handle(&up(2, 0));
    highlight(
        &mut buf,
        area,
        sel.range().unwrap(),
        Style::default().bg(Color::Blue),
    );
    // Selected cells (0..=2) get the highlight bg; the glyph survives.
    for col in 0..=2 {
        assert_eq!(
            buf[(col, 0)].bg,
            Color::Blue,
            "cell {col} should be highlighted"
        );
    }
    assert_eq!(
        buf[(0, 0)].symbol(),
        "h",
        "highlight must not clobber the glyph"
    );
    // An unselected cell is untouched.
    assert_ne!(buf[(4, 0)].bg, Color::Blue);
}

#[test]
fn hit_map_last_region_wins_and_misses_return_none() {
    let mut hits: HitMap<&str> = HitMap::new();
    hits.push(Rect::new(0, 0, 10, 10), "background");
    hits.push(Rect::new(2, 2, 3, 3), "panel"); // pushed later -> wins on overlap
    hits.push(Rect::new(0, 0, 0, 0), "zero"); // zero-area never matches
    assert_eq!(hits.hit(3, 3), Some(&"panel"));
    assert_eq!(hits.hit(8, 8), Some(&"background"));
    assert_eq!(hits.hit(50, 50), None);
    // hit_event resolves an event's coordinates.
    assert_eq!(hits.hit_event(&down(3, 3)), Some(&"panel"));
}

#[test]
fn click_tracker_detects_a_click() {
    let mut clicks = ClickTracker::new();
    assert!(clicks.handle(&down(4, 2)).is_none()); // press alone is not a click
    let click = clicks
        .handle(&up(4, 2))
        .expect("down+up on one cell is a click");
    assert_eq!(
        (click.column, click.row, click.button),
        (4, 2, MouseButton::Left)
    );
}

#[test]
fn click_tracker_drag_cancels_the_click() {
    let mut clicks = ClickTracker::new();
    clicks.handle(&down(4, 2));
    clicks.handle(&drag(6, 2)); // moved -> this is a selection, not a click
    assert!(clicks.handle(&up(6, 2)).is_none());
}

#[test]
fn click_tracker_release_on_another_cell_is_not_a_click() {
    let mut clicks = ClickTracker::new();
    clicks.handle(&down(4, 2));
    assert!(clicks.handle(&up(9, 9)).is_none());
}

#[test]
fn translate_maps_button_modifiers_and_drag() {
    use crossterm::event::{
        Event as CtEvent, KeyModifiers, MouseButton as CtButton, MouseEvent, MouseEventKind,
    };
    let ct = CtEvent::Mouse(MouseEvent {
        kind: MouseEventKind::Down(CtButton::Right),
        column: 7,
        row: 3,
        modifiers: KeyModifiers::SHIFT | KeyModifiers::CONTROL,
    });
    let Some(Event::Mouse(m)) = host::translate_event(ct) else {
        panic!("expected a mouse event");
    };
    assert_eq!(m.kind, MouseKind::Down(MouseButton::Right));
    assert_eq!((m.column, m.row), (7, 3));
    assert!(m.shift && m.ctrl && !m.alt);
    assert!(!m.plain());
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
    let up = Event::Mouse(Mouse::at(MouseKind::ScrollUp, 0, 0));
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

// ---- resize / degenerate sizes -------------------------------------------
//
// Two properties: (1) rendering at any size — including 0×0, 1×1, and sizes
// smaller than a component's chrome — must never panic; (2) a component may
// never write a cell outside the clip it was given. The `Surface` clip is
// supposed to guarantee (2) by construction; these tests assert it.

/// A representative tree: a scrollable bordered body, a progress bar, and a
/// status bar — the shapes the full-screen renderer actually composes.
fn demo_tree(frame: u64) -> super::view::Element {
    use super::components::{Boxed, Flex, ProgressBar, Scroll, ScrollState, StatusBar};
    use ratatui::text::{Line, Span};
    let lines: Vec<Line<'static>> = (0..40).map(|i| Line::from(format!("row {i}"))).collect();
    let mut st = ScrollState::new();
    st.clamp(40, 8);
    element(
        Flex::column()
            .grow(
                1,
                element(Boxed::new(element(Scroll::new(lines, &st))).title(" body ")),
            )
            .fixed(1, element(ProgressBar::indeterminate(frame)))
            .fixed(1, element(StatusBar::new().left(vec![Span::raw("status")]))),
    )
}

#[test]
fn paint_survives_degenerate_and_swept_sizes() {
    let theme = Theme::default();
    // The test passing (no panic, no out-of-bounds index) is the assertion.
    for &w in &[0u16, 1, 2, 3, 5, 10, 17, 40, 200] {
        for &h in &[0u16, 1, 2, 3, 5, 8, 40] {
            let mut buf = Buffer::empty(Rect::new(0, 0, w, h));
            let area = buf.area;
            host::paint(&mut buf, area, &theme, demo_tree(w as u64).as_ref(), &[]);
            assert_eq!(buf.area, Rect::new(0, 0, w, h));
        }
    }
}

#[test]
fn surface_never_writes_outside_its_clip() {
    use super::components::{Boxed, Text};
    let theme = Theme::default();
    let ctx = RenderCtx::new(&theme);

    let mut buf = buffer(16, 8);
    for y in 0..8u16 {
        for x in 0..16u16 {
            buf[(x, y)].set_char('#');
        }
    }
    // Clip is a small window, but we hand the component a much larger area so
    // it *tries* to draw past the clip.
    let clip = Rect::new(3, 1, 6, 3);
    {
        let mut surface = Surface::new(&mut buf, clip);
        Boxed::new(element(Text::raw(
            "content far wider and taller than the clip window",
        )))
        .render(Rect::new(3, 1, 40, 20), &mut surface, &ctx);
    }
    for y in 0..8u16 {
        for x in 0..16u16 {
            let inside = x >= clip.x && x < clip.right() && y >= clip.y && y < clip.bottom();
            if !inside {
                assert_eq!(buf[(x, y)].symbol(), "#", "clip leaked at ({x},{y})");
            }
        }
    }
}

#[test]
fn boxed_border_closed_across_sizes() {
    use super::components::{Boxed, Text};
    let theme = Theme::default();
    let ctx = RenderCtx::new(&theme);
    for w in 2..=12u16 {
        for h in 2..=8u16 {
            let mut buf = buffer(w, h);
            let area = buf.area;
            let mut surface = Surface::new(&mut buf, area);
            Boxed::new(element(Text::raw("x"))).render(area, &mut surface, &ctx);
            assert_eq!(buf[(0, 0)].symbol(), "╭", "top-left at {w}x{h}");
            assert_eq!(buf[(w - 1, 0)].symbol(), "╮", "top-right at {w}x{h}");
            assert_eq!(buf[(0, h - 1)].symbol(), "╰", "bottom-left at {w}x{h}");
            assert_eq!(buf[(w - 1, h - 1)].symbol(), "╯", "bottom-right at {w}x{h}");
        }
    }
}

#[test]
fn progress_bar_is_responsive_to_width() {
    let theme = Theme::default();
    let ctx = RenderCtx::new(&theme);
    let filled = |w: u16| {
        let mut buf = buffer(w, 1);
        let area = buf.area;
        let mut surface = Surface::new(&mut buf, area);
        ProgressBar::determinate(0.5).render(area, &mut surface, &ctx);
        (0..w).filter(|&x| buf[(x, 0)].symbol() == "█").count()
    };
    assert_eq!(filled(4), 2, "half of 4");
    assert_eq!(filled(40), 20, "half of 40");
    assert!(filled(4) < filled(40), "wider bar fills more cells");
    // Degenerate widths must not panic.
    let _ = filled(0);
    let _ = filled(1);
}

#[test]
fn flex_solver_survives_degenerate_areas() {
    let items = [
        item(Dimension::Flex(1), 0, 0),
        item(Dimension::Fixed(5), 5, 1),
        item(Dimension::Percent(50), 0, 0),
    ];
    for (w, h) in [(0u16, 0u16), (1, 1), (2, 2), (3, 10), (4, 1), (60, 3)] {
        let area = Rect::new(0, 0, w, h);
        // A gap larger than the width exercises the saturating arithmetic.
        let style = LayoutStyle::row().gap(10);
        let rects = solve(area, &style, &items);
        assert_eq!(rects.len(), items.len());
        for r in &rects {
            assert!(r.right() <= area.right(), "{r:?} exceeds width of {area:?}");
            assert!(
                r.bottom() <= area.bottom(),
                "{r:?} exceeds height of {area:?}"
            );
        }
    }
}

#[test]
fn scroll_and_overlay_survive_tiny_screens() {
    use super::components::{Scroll, ScrollState};
    use ratatui::text::Line;

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
fn resize_reflows_body_and_status() {
    let theme = Theme::default();

    // Wide + tall: the body box title is visible on the top border.
    let mut big = buffer(80, 24);
    let a = big.area;
    host::paint(&mut big, a, &theme, demo_tree(0).as_ref(), &[]);
    assert!(
        row(&big, 0).contains("body"),
        "title at 80x24: {:?}",
        row(&big, 0)
    );

    // Then a small resize: must still render the status row at the bottom and
    // must not panic or leave the previous size's content behind (fresh buffer).
    let mut small = buffer(20, 6);
    let a2 = small.area;
    host::paint(&mut small, a2, &theme, demo_tree(0).as_ref(), &[]);
    assert!(
        row(&small, 5).contains("status"),
        "status row at 20x6: {:?}",
        row(&small, 5)
    );
}

// ---- palette / theme -----------------------------------------------------
//
// Every slot gets a unique indexed color so a rendered cell's fg/bg pins down
// exactly which theme slot the component read — not just "some non-default
// color". Swapping the theme must restyle the same view tree.

use ratatui::style::{Color, Modifier};

/// A theme whose slots are all distinct, identifiable colors.
fn rainbow_theme() -> Theme {
    Theme {
        background: Color::Indexed(1),
        surface: Color::Indexed(2),
        text: Color::Indexed(3),
        muted: Color::Indexed(4),
        dim: Color::Indexed(5),
        accent: Color::Indexed(6),
        accent_alt: Color::Indexed(7),
        border: Color::Indexed(8),
        border_focused: Color::Indexed(9),
        selection_bg: Color::Indexed(10),
        selection_fg: Color::Indexed(11),
    }
}

#[test]
fn theme_helper_styles_map_to_slots() {
    let t = rainbow_theme();
    assert_eq!(t.text_style().fg, Some(t.text));
    assert_eq!(t.muted_style().fg, Some(t.muted));
    assert_eq!(t.accent_style().fg, Some(t.accent));
    assert!(t.accent_style().add_modifier.contains(Modifier::BOLD));
    assert_eq!(t.border_color(false), t.border);
    assert_eq!(t.border_color(true), t.border_focused);
    let sel = t.selection_style();
    assert_eq!(sel.bg, Some(t.selection_bg));
    assert_eq!(sel.fg, Some(t.selection_fg));
    assert!(sel.add_modifier.contains(Modifier::BOLD));
}

#[test]
fn boxed_border_follows_theme_focus_color() {
    use super::components::{Boxed, Text};
    let t = rainbow_theme();

    let make = |focused: bool| {
        let mut buf = buffer(8, 3);
        let area = buf.area;
        let ctx = RenderCtx::new(&t).with_focus(focused);
        let boxed = Boxed::new(element(Text::raw("x")));
        let mut surface = Surface::new(&mut buf, area);
        boxed.render(area, &mut surface, &ctx);
        buf[(0, 0)].fg // the '╭' corner
    };

    assert_eq!(make(false), t.border, "unfocused border uses theme.border");
    assert_eq!(
        make(true),
        t.border_focused,
        "focused border uses theme.border_focused"
    );
}

#[test]
fn status_bar_background_is_theme_surface() {
    use super::components::StatusBar;
    use ratatui::text::Span;
    let t = rainbow_theme();
    let bar = StatusBar::new().left(vec![Span::raw("hi")]);
    let mut buf = buffer(10, 1);
    let area = buf.area;
    let ctx = RenderCtx::new(&t);
    let mut surface = Surface::new(&mut buf, area);
    bar.render(area, &mut surface, &ctx);
    // The whole row is filled with the surface background.
    assert_eq!(buf[(9, 0)].bg, t.surface);
}

#[test]
fn select_list_selection_uses_theme_slots() {
    use super::components::{SelectList, SelectState};
    use ratatui::text::Line;
    let t = rainbow_theme();
    let mut state = SelectState::new();
    state.handle(&Event::Key(Key::new(KeyCode::Down)), 2); // select row 1
    let list = SelectList::new(vec![Line::from("a"), Line::from("b")], &state);
    let mut buf = buffer(10, 2);
    let area = buf.area;
    let ctx = RenderCtx::new(&t);
    let mut surface = Surface::new(&mut buf, area);
    list.render(area, &mut surface, &ctx);
    assert_eq!(buf[(0, 1)].bg, t.selection_bg, "selected row bg");
    assert_eq!(buf[(0, 1)].fg, t.selection_fg, "selected caret fg");
    assert_ne!(
        buf[(0, 0)].bg,
        t.selection_bg,
        "unselected row not highlighted"
    );
}

#[test]
fn scrollbar_thumb_and_track_use_theme() {
    use super::components::{Scroll, ScrollState};
    use ratatui::text::Line;
    let t = rainbow_theme();
    let lines: Vec<Line<'static>> = (0..30).map(|i| Line::from(format!("l{i}"))).collect();
    let mut state = ScrollState::new();
    state.clamp(30, 5);
    let scroll = Scroll::new(lines, &state);
    let mut buf = buffer(10, 5);
    let area = buf.area;
    let ctx = RenderCtx::new(&t);
    let mut surface = Surface::new(&mut buf, area);
    scroll.render(area, &mut surface, &ctx);
    let col = 9; // scrollbar column
    let fgs: Vec<Color> = (0..5).map(|y| buf[(col, y)].fg).collect();
    assert!(fgs.contains(&t.muted), "thumb uses theme.muted: {fgs:?}");
    assert!(fgs.contains(&t.dim), "track uses theme.dim: {fgs:?}");
}

#[test]
fn progress_bar_default_colors_come_from_theme() {
    let t = rainbow_theme();
    let ctx = RenderCtx::new(&t);

    // Determinate: filled fg = accent, empty bg = dim.
    let bar = ProgressBar::determinate(0.5);
    let mut buf = buffer(10, 1);
    let area = buf.area;
    let mut surface = Surface::new(&mut buf, area);
    bar.render(area, &mut surface, &ctx);
    assert_eq!(buf[(0, 0)].fg, t.accent, "filled cell fg");
    assert_eq!(buf[(9, 0)].bg, t.dim, "empty cell bg");

    // Indeterminate: bright segment fg = accent, track fg = dim.
    let bar = ProgressBar::indeterminate(0);
    let mut buf = buffer(12, 1);
    let area = buf.area;
    let mut surface = Surface::new(&mut buf, area);
    bar.render(area, &mut surface, &ctx);
    let fgs: Vec<Color> = (0..12).map(|x| buf[(x, 0)].fg).collect();
    assert!(fgs.contains(&t.accent), "segment fg accent: {fgs:?}");
    assert!(fgs.contains(&t.dim), "track fg dim: {fgs:?}");
}

#[test]
fn spinner_default_color_is_theme_accent() {
    let t = rainbow_theme();
    let ctx = RenderCtx::new(&t);
    let mut buf = buffer(3, 1);
    let area = buf.area;
    let mut surface = Surface::new(&mut buf, area);
    Spinner::new(0).render(area, &mut surface, &ctx);
    assert_eq!(buf[(0, 0)].fg, t.accent);
}

#[test]
fn compositor_uses_theme_background_and_overlay_surface() {
    let t = rainbow_theme();
    let mut buf = buffer(12, 4);
    let area = buf.area;
    let root = Text::raw("base");
    let dialog = Text::raw("hi");
    let overlays = [Overlay {
        area: Rect::new(4, 1, 4, 1),
        view: &dialog,
        clear: true,
    }];
    host::paint(&mut buf, area, &t, &root, &overlays);
    assert_eq!(
        buf[(0, 3)].bg,
        t.background,
        "base fill uses theme.background"
    );
    assert_eq!(
        buf[(4, 1)].bg,
        t.surface,
        "overlay clear uses theme.surface"
    );
}

#[test]
fn swapping_theme_restyles_the_same_tree() {
    use super::components::{Boxed, Text};

    let tree = || Boxed::new(element(Text::raw("x")));
    let render_border = |theme: &Theme| {
        let mut buf = buffer(8, 3);
        let area = buf.area;
        let ctx = RenderCtx::new(theme);
        let mut surface = Surface::new(&mut buf, area);
        tree().render(area, &mut surface, &ctx);
        buf[(0, 0)].fg
    };

    let a = Theme {
        border: Color::Indexed(21),
        ..rainbow_theme()
    };
    let b = Theme {
        border: Color::Indexed(99),
        ..rainbow_theme()
    };
    assert_eq!(render_border(&a), Color::Indexed(21));
    assert_eq!(render_border(&b), Color::Indexed(99));
    assert_ne!(
        render_border(&a),
        render_border(&b),
        "theme swap must restyle"
    );
}

// ---- view! macro ---------------------------------------------------------

fn render_el(el: &super::view::Element, w: u16, h: u16) -> Vec<String> {
    let theme = Theme::default();
    let mut buf = buffer(w, h);
    let area = buf.area;
    let ctx = RenderCtx::new(&theme);
    let mut surface = Surface::new(&mut buf, area);
    el.render(area, &mut surface, &ctx);
    (0..h).map(|y| row(&buf, y)).collect()
}

#[test]
fn view_macro_matches_builder() {
    use super::components::{Boxed, Flex, Spacer, Text};

    // Hand-written builder tree.
    let built: super::view::Element = element(
        Flex::column()
            .gap(1)
            .auto(element(Boxed::new(element(Text::raw("hi"))).title(" t ")))
            .grow(1, element(Spacer)),
    );

    // The same tree via the declarative macro.
    let macroed: super::view::Element = crate::view! {
        col(gap = 1) {
            boxed(title = " t ") { text("hi") }
            grow(1) { spacer() }
        }
    };

    let a = render_el(&built, 20, 5);
    let b = render_el(&macroed, 20, 5);
    assert_eq!(a, b, "view! must render identically to the builder form");
    assert!(b.iter().any(|l| l.contains('t')), "{b:?}");
}

/// A `View` standing in for a component defined in some other crate.
struct Star;
impl super::view::View for Star {
    fn measure(&self, _available: Size) -> Size {
        Size::new(1, 1)
    }
    fn render(&self, area: Rect, surface: &mut Surface, _ctx: &RenderCtx) {
        surface.set(area.x, area.y, '★', Style::default());
    }
}

#[test]
fn view_macro_accepts_foreign_view_via_node() {
    // `node(expr)` splices any `impl View` — this is how a component from
    // another crate participates in the DSL.
    let tree: super::view::Element = crate::view! {
        col {
            node(Star)
        }
    };
    let out = render_el(&tree, 5, 2);
    assert!(out[0].contains('★'), "foreign view should render: {out:?}");
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

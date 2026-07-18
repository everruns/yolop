//! Golden-snapshot tests: render whole screens to a glyph grid and compare
//! against a checked-in `.txt` file. A layout regression shows up as a readable
//! diff instead of a subtle cell assertion. Snapshots capture glyphs only
//! (styles/colors are covered by the palette tests), so they stay stable and
//! human-reviewable.
//!
//! To (re)generate after an intentional change:
//! `UPDATE_SNAPSHOTS=1 cargo test --all-features tuika::snapshots`.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};

use super::components::{
    Boxed, Flex, ProgressBar, Scroll, ScrollState, SelectList, SelectState, Spinner, SpinnerStyle,
    StatusBar, Text,
};
use super::host::{self, Overlay};
use super::overlay::OverlaySpec;
use super::style::Theme;
use super::surface::Surface;
use super::view::{Element, RenderCtx, element};

/// A buffer rendered to a newline-joined glyph grid, trailing spaces trimmed.
fn grid(buf: &Buffer) -> String {
    let area = buf.area;
    let mut out = String::new();
    for y in area.y..area.bottom() {
        let mut row = String::new();
        for x in area.x..area.right() {
            row.push_str(buf[(x, y)].symbol());
        }
        out.push_str(row.trim_end());
        out.push('\n');
    }
    out
}

/// Render a single view (no compositor background) to a glyph grid.
fn view_grid(w: u16, h: u16, view: Element) -> String {
    let theme = Theme::default();
    let ctx = RenderCtx::new(&theme);
    let mut buf = Buffer::empty(Rect::new(0, 0, w, h));
    let area = buf.area;
    {
        let mut surface = Surface::new(&mut buf, area);
        view.render(area, &mut surface, &ctx);
    }
    grid(&buf)
}

/// Compare `actual` against the checked-in snapshot `name`, or write it when
/// `UPDATE_SNAPSHOTS` is set.
fn assert_snapshot(name: &str, actual: &str) {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src/tuika/snapshots")
        .join(format!("{name}.txt"));
    if std::env::var_os("UPDATE_SNAPSHOTS").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, actual).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(&path).unwrap_or_else(|_| {
        panic!("missing snapshot `{name}`; run with UPDATE_SNAPSHOTS=1 to create it")
    });
    assert!(
        expected == actual,
        "snapshot `{name}` mismatch (run UPDATE_SNAPSHOTS=1 to refresh)\n\
         --- expected ---\n{expected}\n--- actual ---\n{actual}",
    );
}

#[test]
fn snapshot_progress_dashboard() {
    let lines: Vec<Line<'static>> = (0..30).map(|i| Line::from(format!("row {i}"))).collect();
    let mut st = ScrollState::new();
    st.clamp(30, 6);
    let tree = element(
        Flex::column()
            .grow(
                1,
                element(Boxed::new(element(Scroll::new(lines, &st))).title(" body ")),
            )
            .fixed(1, element(ProgressBar::determinate(0.5).percent(true)))
            .fixed(1, element(StatusBar::new().left(vec![Span::raw("ready")]))),
    );
    assert_snapshot("progress_dashboard", &view_grid(40, 12, tree));
}

#[test]
fn snapshot_spinner_and_progress_row() {
    let tree = element(
        Flex::column()
            .fixed(
                1,
                element(
                    Flex::row()
                        .gap(2)
                        .fixed(1, element(Spinner::new(0).style(SpinnerStyle::Braille)))
                        .fixed(1, element(Spinner::new(0).style(SpinnerStyle::Line)))
                        .fixed(1, element(Spinner::new(0).style(SpinnerStyle::Dots))),
                ),
            )
            .fixed(1, element(ProgressBar::determinate(0.25).percent(true)))
            .fixed(1, element(ProgressBar::determinate(1.0).percent(true)))
            .fixed(1, element(ProgressBar::indeterminate(0))),
    );
    assert_snapshot("spinner_and_progress", &view_grid(24, 4, tree));
}

#[test]
fn snapshot_select_list() {
    let mut state = SelectState::new();
    // Move to the middle entry deterministically.
    state.handle(
        &super::event::Event::Key(super::event::Key::new(super::event::KeyCode::Down)),
        3,
    );
    let list = Boxed::new(element(SelectList::new(
        vec![Line::from("Alpha"), Line::from("Beta"), Line::from("Gamma")],
        &state,
    )))
    .title(" pick ");
    assert_snapshot("select_list", &view_grid(18, 5, element(list)));
}

#[test]
fn snapshot_centered_overlay() {
    let theme = Theme::default();
    let mut buf = Buffer::empty(Rect::new(0, 0, 32, 11));
    let area = buf.area;
    let base = Text::new(vec![
        Line::from("base line one"),
        Line::from("base line two"),
        Line::from("base line three"),
    ]);
    let dialog = Boxed::new(element(Text::new(vec![
        Line::from("Choose one:"),
        Line::from("1) first"),
        Line::from("2) second"),
    ])))
    .title(" menu ");
    let rect = OverlaySpec::centered(70, 70).min_size(16, 6).resolve(area);
    let overlays = [Overlay {
        area: rect,
        view: &dialog,
        clear: true,
    }];
    host::paint(&mut buf, area, &theme, &base, &overlays);
    assert_snapshot("centered_overlay", &grid(&buf));
}

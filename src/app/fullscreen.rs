//! Full-screen renderer, composed with the `tuika` view tree.
//!
//! Where the inline renderer ([`super::render::draw_shared`]) lays out the
//! chrome with a hand-rolled ratatui split, the full-screen mode builds the
//! same vertical composition — transcript, the blue message separator, the
//! composer, the gold status separator, the session status — as a `tuika`
//! `Flex` column and paints it through `tuika::paint`. Content is the *same*
//! styled lines the inline renderer uses (from `super::render`), so the design
//! (colors, separators, status) cannot drift.
//!
//! Two regions are drawn by the shared ratatui renderers into rects `tuika`
//! reserved for them (via [`tuika::RectProbe`]): the composer, which is the
//! `ratatui-textarea` widget plus the real terminal cursor, and the streaming
//! preview. Full-screen mouse text selection (see [`super::App`]) is applied
//! over the transcript rect the same probe reports.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;

use tuika::{Element, Flex, Padding, RectProbe, Spacer, Text, element};

use super::render;
use super::{ACCENT_BLUE, ACCENT_GOLD, App};

pub(crate) fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();

    // Overlays own the whole viewport; reuse the shared sheet renderers.
    if app.pending_ask.is_some() {
        render::draw_ask_overlay(f, area, app);
        return;
    }
    if app.setup.is_some() {
        render::draw_setup_overlay(f, area, app);
        return;
    }
    if app.background_panel.is_some() {
        render::draw_background_panel(f, area, app);
        return;
    }
    if area.width < 4 || area.height == 0 {
        return;
    }

    let state = app.view_state();
    let input_width = area.width.saturating_sub(2);
    let desired_input_height = app.input_height(input_width);
    let status_rows = state.status_row_count();
    let preview_visible = render::chrome_preview_visible(&state);
    let (chrome_height, input_height) = render::chrome_dimensions(
        area.height,
        desired_input_height,
        status_rows,
        preview_visible,
    );
    let preview_height = u16::from(input_height == 1 && preview_visible);
    let status_sep_height = u16::from(input_height < 3);
    let transcript_height = area.height.saturating_sub(chrome_height);

    // Content as styled lines — the exact builders the inline renderer uses.
    let inner_w = area.width.saturating_sub(2) as usize;
    let transcript =
        render::recent_transcript_lines(app, inner_w, transcript_height.max(1) as usize);
    let message_separator = render::separator_line(
        render::message_separator_title(&state),
        area.width,
        Style::default().fg(ACCENT_BLUE),
    );
    let status_separator =
        render::separator_line(Line::from(""), area.width, Style::default().fg(ACCENT_GOLD));
    let status_lines = render::session_status_lines(&state);

    // Probes recover the rects tuika assigns to regions the shared ratatui
    // renderers finish drawing (composer, preview) and the transcript.
    let transcript_probe = RectProbe::new();
    let preview_probe = RectProbe::new();
    let input_probe = RectProbe::new();

    let mut col = Flex::column().grow(1, transcript_view(transcript, &transcript_probe));
    if preview_height > 0 {
        col = col.fixed(preview_height, preview_probe.wrap(element(Spacer)));
    }
    col = col.fixed(1, element(Text::new(vec![message_separator])));
    col = col.fixed(input_height, input_probe.wrap(element(Spacer)));
    if status_sep_height > 0 {
        col = col.fixed(
            status_sep_height,
            element(Text::new(vec![status_separator])),
        );
    }
    if status_rows > 0 {
        col = col.fixed(status_rows, element(Text::new(status_lines)));
    }
    let root = element(col);

    // Transparent background so the styled lines' own colors show through.
    let theme = tuika::Theme {
        background: Color::Reset,
        ..tuika::Theme::default()
    };
    tuika::paint(f.buffer_mut(), area, &theme, root.as_ref(), &[]);

    // Mouse text selection over the transcript. Its selectable inner rect is the
    // transcript region inset one column (matching `transcript_view`'s padding).
    let t = transcript_probe.rect();
    let selection = Rect {
        x: t.x.saturating_add(1),
        y: t.y,
        width: t.width.saturating_sub(2),
        height: t.height,
    };
    app.set_selection_area(selection);
    if let Some(range) = app.selection_range() {
        if app.take_pending_copy() {
            let text = tuika::selected_text(f.buffer_mut(), selection, range);
            if !text.is_empty() {
                let _ = tuika::write_clipboard(&mut std::io::stdout(), &text);
            }
        }
        tuika::highlight(
            f.buffer_mut(),
            selection,
            range,
            Style::default().add_modifier(Modifier::REVERSED),
        );
    }

    // Ratatui-native regions, into the rects tuika reserved for them.
    if preview_height > 0 {
        render::draw_preview_slot(f, preview_probe.rect(), &state);
    }
    render::draw_input(f, input_probe.rect(), app);
}

/// The transcript: its styled lines bottom-aligned within a one-column inset,
/// probed so the host can bound mouse selection to it.
fn transcript_view(lines: Vec<Line<'static>>, probe: &RectProbe) -> Element {
    let height = (lines.len() as u16).max(1);
    let inner = Flex::column()
        .padding(Padding {
            left: 1,
            right: 1,
            top: 0,
            bottom: 0,
        })
        .grow(1, element(Spacer))
        .fixed(height, element(Text::new(lines)));
    probe.wrap(element(inner))
}

//! Experimental full-screen renderer (opt-in via `--fullscreen`).
//!
//! Where the inline renderer (`super::render::draw`) pushes the transcript into
//! native terminal scrollback and owns only a bottom composer, this path owns
//! the whole alternate screen and rebuilds the frame from `App` state every
//! draw through the [`crate::tuika`] toolkit: a scrollable transcript, a
//! bordered composer, and a status bar, with setup rendered as a centered
//! overlay. The transcript lives in `App::lines` and is windowed by
//! `App::scroll` rather than handed to the terminal — the deliberate trade for
//! clean overlays and resize behavior.
//!
//! This is intentionally a subset of the inline renderer's affordances; it is
//! the foundation for the full-screen mode, not yet at parity.

use ratatui::Frame;
use ratatui::layout::Position;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::tuika::{
    self, Boxed, Flex, Overlay, OverlaySpec, Paragraph, Scroll, StatusBar, Text, Theme, element,
};

use super::App;

/// Border (2) + horizontal padding (2) a [`Boxed`] adds around its child.
const BOX_H_CHROME: u16 = 4;
/// Border rows (top + bottom) a [`Boxed`] adds around its child.
const BOX_V_CHROME: u16 = 2;
const STATUS_ROWS: u16 = 1;
/// Cap the composer so a huge paste cannot swallow the transcript.
const MAX_COMPOSER_ROWS: u16 = 10;

pub(crate) fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();
    let theme = Theme::default();
    if area.width < 4 || area.height < 6 {
        return;
    }

    let transcript_inner_w = area.width.saturating_sub(BOX_H_CHROME).max(1);

    // Whole transcript, wrapped through the shared presenter so full-screen and
    // inline render identical line content.
    let mut lines: Vec<Line<'static>> = Vec::new();
    for chat in &app.lines {
        super::append_chat_lines(&mut lines, chat, transcript_inner_w as usize);
    }
    let content_h = lines.len() as u16;

    // Composer geometry.
    let composer_text = app.input.lines().join("\n");
    let composer_rows = (app.input.lines().len() as u16).clamp(1, MAX_COMPOSER_ROWS);
    let composer_box_h = composer_rows + BOX_V_CHROME;

    // Transcript box gets whatever height is left after the composer + status.
    let transcript_box_h = area
        .height
        .saturating_sub(STATUS_ROWS + composer_box_h)
        .max(BOX_V_CHROME + 1);
    let transcript_inner_h = transcript_box_h.saturating_sub(BOX_V_CHROME).max(1);

    // Reconcile scroll with the freshly measured content, and record metrics so
    // the mouse-wheel handler can scroll without re-laying out.
    app.scroll.clamp(content_h, transcript_inner_h);
    app.scroll_metrics = (content_h, transcript_inner_h);
    let scroll_state = app.scroll;

    // Status bar segments.
    let ps = app.presentation_state();
    let mut left = vec![
        Span::styled(
            format!(" {} ", ps.model_id),
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("· {} ", ps.provider_name), theme.muted_style()),
    ];
    if app.busy {
        left.push(Span::styled("· working…", theme.muted_style()));
    }
    let mut right = Vec::new();
    if let Some(tokens) = ps.session_tokens {
        right.push(Span::styled(format!("{tokens} tok  "), theme.muted_style()));
    }
    right.push(Span::styled(
        format!("{}  ", ps.approval_mode),
        Style::default().fg(theme.accent_alt),
    ));

    // Compose the base tree.
    let transcript = Boxed::new(element(Scroll::new(lines, &scroll_state))).title(Line::from(
        Span::styled(" transcript ", theme.accent_style()),
    ));
    let composer_title = if scroll_state.is_stuck_to_bottom() {
        " message "
    } else {
        " message · scrolled up (End to resume) "
    };
    let composer = Boxed::new(element(Paragraph::new(composer_text, theme.text_style()))).title(
        Line::from(Span::styled(composer_title, theme.muted_style())),
    );
    let status = StatusBar::new()
        .left(left)
        .right(right)
        .background(Style::default().bg(theme.surface));

    let root = Flex::column()
        .grow(1, element(transcript))
        .fixed(composer_box_h, element(composer))
        .fixed(STATUS_ROWS, element(status));

    // Setup renders as a centered overlay; declared before `overlays` so it
    // outlives the borrow held in the `Overlay`.
    let setup_view = if app.setup.is_some() {
        let (setup_lines, _cursor) = super::setup_overlay_content(app);
        Some(
            Boxed::new(element(Text::new(setup_lines)))
                .title(Line::from(Span::styled(" setup ", theme.accent_style()))),
        )
    } else {
        None
    };
    let mut overlays: Vec<Overlay> = Vec::new();
    if let Some(view) = setup_view.as_ref() {
        let rect = OverlaySpec::centered(60, 60).min_size(32, 8).resolve(area);
        overlays.push(Overlay {
            area: rect,
            view,
            clear: true,
        });
    }

    tuika::paint(f.buffer_mut(), area, &theme, &root, &overlays);

    // Park the terminal cursor in the composer (only when no overlay owns input).
    if app.setup.is_none() {
        let cursor = app.input.cursor();
        let (crow, ccol) = (cursor.0, cursor.1);
        let inner_x = area.x + 2; // border + left pad
        let inner_y = area.y + transcript_box_h + 1; // composer border top
        let cursor_x = (inner_x + ccol as u16).min(area.right().saturating_sub(1));
        let max_y = inner_y + composer_rows.saturating_sub(1);
        let cursor_y = (inner_y + crow as u16).min(max_y);
        f.set_cursor_position(Position {
            x: cursor_x,
            y: cursor_y,
        });
    }
}

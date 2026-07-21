//! Full-screen renderer, built as a native `tuika` view tree.
//!
//! This is the tuika-native counterpart to the inline renderer
//! ([`super::render::draw_shared`]). Where the inline mode paints the chrome
//! with hand-rolled ratatui splits and widgets, the full-screen mode composes
//! the whole frame — transcript, the blue message separator, the composer, the
//! gold status separator, the session status, and the preview popup row — as a
//! `tuika` [`view!`] tree of real components and paints it with
//! [`tuika::paint`]:
//!
//! - **separators** are [`tuika::Rule`]s (blue message rule, gold status rule);
//! - **the composer** is a [`tuika::TextInput`] fed from a snapshot of the
//!   shared composer text model (`app.input`) — no ratatui-textarea widget;
//! - **the preview row** (reverse-search / suggestions / streaming preview) and
//!   **the status** are [`tuika::Text`] views built from the same pure line
//!   builders the inline chrome uses, so the two modes cannot visually drift;
//! - **the transcript** is a [`tuika::Scroll`] over the *full* history, bound to
//!   [`App`]'s persisted `ScrollState`, inside a probed region so full-screen
//!   mouse text selection (see [`super::App`]) can be bounded to it.
//!
//! Crucially, the full-screen frame calls **no** `render::draw_*` painter — not
//! for its chrome and not for the setup / ask / background overlays (which
//! composite as tuika [`Overlay`]s, see below). It uses only the shared *content*
//! builders, which return styled [`Line`]s.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use tuika::{
    Boxed, Element, Overlay, Padding, RectProbe, Rule, Scroll, Spacer, Text, TextInput,
    TextInputState, element, view,
};

use super::render;
use super::{ACCENT_BLUE, ACCENT_GOLD, App, DIFF_META, PANEL_BG, TEXT_PRIMARY};

pub(crate) fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();

    // Overlays own the whole viewport, composited as tuika overlays over a blank
    // base (same "sheet" behavior as inline, but tuika-native).
    if app.pending_ask.is_some() {
        draw_ask_overlay(f, area, app);
        return;
    }
    if app.setup.is_some() {
        draw_setup_overlay(f, area, app);
        return;
    }
    if app.background_panel.is_some() {
        draw_background_overlay(f, area, app);
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
    // NOTE (layout policy, item 2): `chrome_dimensions` stays hand-rolled on
    // purpose. tuika's Flex solver places the rows of the `view!` tree below, but
    // *how tall the composer may grow* and *whether the preview/status-separator
    // rows appear at all* is yolop UX policy (shrink the composer to keep the
    // transcript visible on a short viewport; mirror Codex's composer behavior) —
    // not something a generic constraint solver can infer. We compute those
    // heights here and hand the solver fixed sizes; the solver still owns
    // placement, wrapping, and the transcript's `grow(1)` fill.
    let (chrome_height, input_height) = render::chrome_dimensions(
        area.height,
        desired_input_height,
        status_rows,
        preview_visible,
    );
    let preview_height = u16::from(input_height == 1 && preview_visible);
    let status_sep_height = u16::from(input_height < 3);
    let transcript_height = area.height.saturating_sub(chrome_height).max(1);

    // Content as styled lines — the exact pure builders the inline renderer uses,
    // so colors, separators, and status can never drift between the two modes.
    let inner_w = area.width.saturating_sub(2) as usize;
    let status_lines = render::session_status_lines(&state);
    let preview_line = render::preview_slot_line(&state, area.width);

    // The transcript is a real Scroll over the *full* history, bound to the
    // persisted ScrollState. Short content is top-padded so it rests at the
    // bottom (the inline scrollback feel); taller content scrolls, and
    // stick-to-bottom keeps the newest line visible as the turn streams.
    let scroll_lines = pad_to_bottom(
        render::full_transcript_lines(app, inner_w),
        transcript_height,
    );
    let scroll_content_h = scroll_lines.len() as u16;
    // Record metrics + reconcile the offset before rendering, so the mouse-wheel
    // / paging handlers (which reuse these metrics) clamp correctly.
    app.scroll_metrics = (scroll_content_h, transcript_height);
    app.scroll.clamp(scroll_content_h, transcript_height);

    // The composer is a real tuika TextInput. Its text is a snapshot of the
    // shared composer model (`app.input`); inline mode keeps owning that model,
    // full-screen mode only reads it here.
    let mut composer = TextInputState::new();
    composer.set_text(&app.input.lines().join("\n"));
    let cursor = app.input.cursor();
    composer.set_cursor(cursor.0, cursor.1);

    // Probes recover the rects tuika assigns so the host can place the terminal
    // cursor inside the composer and bound mouse selection to the transcript.
    let transcript_probe = RectProbe::new();
    let input_probe = RectProbe::new();

    let transcript = Scroll::new(scroll_lines, &app.scroll);

    let root = view! {
        col {
            grow(1) { node(transcript_view(transcript, &transcript_probe)) }
            fixed(preview_height) { node(preview_view(preview_line)) }
            fixed(1) { node(message_rule(&state)) }
            fixed(input_height) { node(composer_row(&composer, &input_probe)) }
            fixed(status_sep_height) { node(status_rule()) }
            fixed(status_rows) { node(Text::new(status_lines)) }
        }
    };

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

    // Place the real terminal cursor inside the composer's TextInput rect, unless
    // a turn is running (input disabled).
    if !app.busy {
        let input_rect = input_probe.rect();
        if input_rect.width > 0 && input_rect.height > 0 {
            let (x, y) = composer.cursor_screen(input_rect);
            f.set_cursor_position((x, y));
        }
    }
}

/// Top-pad `lines` with blanks so a history shorter than the viewport rests at
/// the bottom of the scroll region (matching the inline scrollback feel). A
/// history taller than the viewport is returned unchanged and simply scrolls.
fn pad_to_bottom(lines: Vec<Line<'static>>, viewport_h: u16) -> Vec<Line<'static>> {
    let deficit = (viewport_h as usize).saturating_sub(lines.len());
    if deficit == 0 {
        return lines;
    }
    let mut padded = vec![Line::from(""); deficit];
    padded.extend(lines);
    padded
}

/// The transcript region: a [`Scroll`] within a one-column inset, probed so the
/// host can bound mouse selection to it.
fn transcript_view(scroll: Scroll, probe: &RectProbe) -> Element {
    let inner = view! {
        col(padding = Padding { left: 1, right: 1, top: 0, bottom: 0 }) {
            grow(1) { node(scroll) }
        }
    };
    probe.wrap(inner)
}

/// The preview popup row: one line (reverse-search / suggestions / stream tail)
/// or empty when the row is hidden.
fn preview_view(line: Option<Line<'static>>) -> Element {
    element(Text::new(line.map(|l| vec![l]).unwrap_or_default()))
}

/// The blue message separator — a [`Rule`] whose title is the composer hint (or
/// the busy "thinking" indicator), filled out with `─` in accent blue.
fn message_rule(state: &super::ViewState) -> Element {
    element(
        Rule::new()
            .title(render::message_separator_title(state))
            .style(Style::default().fg(ACCENT_BLUE)),
    )
}

/// The gold status separator — a full-width [`Rule`] with no title.
fn status_rule() -> Element {
    element(Rule::new().style(Style::default().fg(ACCENT_GOLD)))
}

/// The composer row: the blue `> ` prompt then the [`TextInput`], the latter
/// probed so the host can place the terminal cursor in it.
fn composer_row(composer: &TextInputState, probe: &RectProbe) -> Element {
    let prompt = Line::from(Span::styled(
        "> ",
        Style::default()
            .fg(ACCENT_BLUE)
            .add_modifier(Modifier::BOLD),
    ));
    view! {
        row {
            fixed(2) { node(Text::new(vec![prompt])) }
            grow(1) { node(probe.wrap(element(TextInput::new(composer)))) }
        }
    }
}

// ---- overlays (setup / ask / background) as tuika overlays ----------------

/// Paint `lines` inside a centered bordered panel, composited as a tuika
/// [`Overlay`] over a blank base — the "sheet" that owns the viewport. Places
/// the terminal cursor at `cursor` (a `(row, col)` inside the panel interior)
/// when given.
///
/// The panel uses the shared [`render::setup_panel_rect`] geometry, and
/// [`Boxed`]'s default border + `symmetric(1, 0)` padding reproduces the inline
/// overlay's interior rect exactly, so the two modes place content identically.
fn draw_panel_overlay(
    f: &mut Frame,
    area: Rect,
    lines: Vec<Line<'static>>,
    cursor: Option<(usize, usize)>,
) {
    let panel = render::setup_panel_rect(area);
    if panel.width == 0 || panel.height == 0 {
        return;
    }
    let boxed = Boxed::new(element(Text::new(lines)))
        .background(Style::default().bg(PANEL_BG).fg(TEXT_PRIMARY));
    let theme = tuika::Theme {
        background: Color::Reset,
        surface: PANEL_BG,
        ..tuika::Theme::default()
    };
    let overlay = Overlay {
        area: panel,
        view: &boxed,
        clear: true,
    };
    tuika::paint(f.buffer_mut(), area, &theme, &Spacer, &[overlay]);

    if let Some((row, col)) = cursor {
        let inner_x = panel.x.saturating_add(2);
        let inner_y = panel.y.saturating_add(1);
        let inner_w = panel.width.saturating_sub(4);
        let inner_h = panel.height.saturating_sub(2);
        if inner_w > 0 && inner_h > row as u16 {
            f.set_cursor_position((
                inner_x.saturating_add((col as u16).min(inner_w.saturating_sub(1))),
                inner_y.saturating_add(row as u16),
            ));
        }
    }
}

fn draw_setup_overlay(f: &mut Frame, area: Rect, app: &App) {
    if app.setup.is_none() || area.width == 0 || area.height == 0 {
        return;
    }
    let (lines, cursor) = render::setup_overlay_content(app);
    draw_panel_overlay(f, area, lines, cursor);
}

fn draw_ask_overlay(f: &mut Frame, area: Rect, app: &App) {
    let Some(ask) = app.pending_ask.as_ref() else {
        return;
    };
    if area.width == 0 || area.height == 0 {
        return;
    }
    let (lines, cursor) = render::ask_overlay_content(ask);
    draw_panel_overlay(f, area, lines, Some(cursor));
}

fn draw_background_overlay(f: &mut Frame, area: Rect, app: &App) {
    let Some(offset) = app.background_panel else {
        return;
    };
    if area.width == 0 || area.height == 0 {
        return;
    }
    let panel = render::setup_panel_rect(area);
    let inner_h = panel.height.saturating_sub(2) as usize;
    let body = app.background_panel_body();
    let texts = render::background_panel_lines(&body, offset, inner_h);
    let lines: Vec<Line<'static>> = texts
        .into_iter()
        .enumerate()
        .map(|(i, text)| {
            if i == 0 {
                Line::from(Span::styled(
                    text,
                    Style::default().fg(DIFF_META).add_modifier(Modifier::BOLD),
                ))
            } else {
                Line::raw(text)
            }
        })
        .collect();
    draw_panel_overlay(f, area, lines, None);
}

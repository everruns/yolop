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
//! - **the transcript** is bottom-aligned styled lines inside a probed region so
//!   full-screen mouse text selection (see [`super::App`]) can be bounded to it.
//!
//! Crucially, the full-screen frame calls **no** `render::draw_*` painter for
//! its own chrome — only the shared *content* builders (which return styled
//! [`Line`]s). The setup / ask / background overlays still borrow the inline
//! sheet renderers; they move onto tuika in a follow-up.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use tuika::{Element, Padding, RectProbe, Rule, Text, TextInput, TextInputState, element, view};

use super::render;
use super::{ACCENT_BLUE, ACCENT_GOLD, App};

pub(crate) fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();

    // Overlays own the whole viewport; borrow the shared sheet renderers until
    // they move onto tuika.
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

    // Content as styled lines — the exact pure builders the inline renderer uses,
    // so colors, separators, and status can never drift between the two modes.
    let inner_w = area.width.saturating_sub(2) as usize;
    let transcript_lines =
        render::recent_transcript_lines(app, inner_w, transcript_height.max(1) as usize);
    let status_lines = render::session_status_lines(&state);
    let preview_line = render::preview_slot_line(&state, area.width);

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

    let root = view! {
        col {
            grow(1) { node(transcript_view(transcript_lines, &transcript_probe)) }
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

/// The transcript region: styled lines bottom-aligned within a one-column inset,
/// probed so the host can bound mouse selection to it.
fn transcript_view(lines: Vec<Line<'static>>, probe: &RectProbe) -> Element {
    let height = (lines.len() as u16).max(1);
    let inner = view! {
        col(padding = Padding { left: 1, right: 1, top: 0, bottom: 0 }) {
            grow(1) { spacer() }
            fixed(height) { node(Text::new(lines)) }
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

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
//! - **the composer** is a [`tuika::TextInput`] over full-screen's own model of
//!   record ([`App::composer`], a tuika `TextInputState` that handles its own
//!   key events) — no ratatui-textarea widget;
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
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use tuika::{
    Boxed, Element, Overlay, Padding, RectProbe, Rule, Scroll, SelectList, SelectState, Spacer,
    Text, TextInput, TextInputState, element, view,
};

use super::render;
use super::{
    ACCENT_BLUE, ACCENT_GOLD, App, CODE_BG, DIFF_ADD, DIFF_META, PANEL_BG, TEXT_DIM, TEXT_MUTED,
    TEXT_PRIMARY,
};

/// Startup-selected theme override. `None` (the default) renders yolop's own
/// palette ([`native_theme`]); a `--theme <name>` / config selection sets it
/// once to a bundled tuika preset before the TUI starts. Written a single time
/// at startup, then only read, so a plain `OnceLock` is enough — no locking on
/// the render path.
static THEME_OVERRIDE: std::sync::OnceLock<tuika::Theme> = std::sync::OnceLock::new();

/// Install the process-wide theme override. Call once, before the TUI runs;
/// later calls are ignored (the first selection wins).
pub(crate) fn set_theme_override(theme: tuika::Theme) {
    let _ = THEME_OVERRIDE.set(theme);
}

/// Resolve a theme name to a palette: a bundled tuika preset by name, or
/// `"yolop"` for yolop's own [`native_theme`]. Returns `None` for anything else
/// so the caller can report the valid choices.
pub(crate) fn resolve_theme(name: &str) -> Option<tuika::Theme> {
    if name.eq_ignore_ascii_case("yolop") {
        return Some(native_theme());
    }
    tuika::theme_by_name(name)
}

/// The names a `--theme` value may take: `yolop` plus every bundled preset.
pub(crate) fn theme_names() -> Vec<&'static str> {
    std::iter::once("yolop")
        .chain(tuika::themes::PRESETS.iter().map(|p| p.name))
        .collect()
}

/// The full-screen renderer's tuika [`Theme`](tuika::Theme): the startup
/// override if one was selected, otherwise yolop's own [`native_theme`].
pub(crate) fn yolop_theme() -> tuika::Theme {
    THEME_OVERRIDE.get().copied().unwrap_or_else(native_theme)
}

/// yolop's own palette.
///
/// NOTE (item 6): tuika's `Theme::default()` is the toolkit's neutral identity
/// (a red-on-dark look), deliberately not any host's brand. yolop owns its
/// colors here, so every theme-driven tuika component in full-screen — the
/// `Scroll` scrollbar, `Boxed` borders, `SelectList` selection — renders in
/// yolop's palette instead of the toolkit default. `background: Reset` lets the
/// terminal's own background own the alternate screen.
pub(crate) fn native_theme() -> tuika::Theme {
    tuika::Theme {
        background: Color::Reset,
        surface: PANEL_BG,
        text: TEXT_PRIMARY,
        muted: TEXT_MUTED,
        dim: TEXT_DIM,
        accent: ACCENT_BLUE,
        accent_alt: ACCENT_GOLD,
        border: TEXT_DIM,
        border_focused: ACCENT_BLUE,
        selection_bg: ACCENT_BLUE,
        selection_fg: TEXT_PRIMARY,
        // Markdown prose + syntax palette for tuika's Markdown/CodeBlock. These
        // mirror yolop's former hand-rolled highlighter so rendered code and
        // prose keep the same look now that both flow through tuika.
        code: tuika::CodeTheme {
            heading: TEXT_PRIMARY,
            link: ACCENT_BLUE,
            background: CODE_BG,
            text: TEXT_PRIMARY,
            label: TEXT_DIM,
            keyword: ACCENT_GOLD,
            function: ACCENT_BLUE,
            type_name: Color::Rgb(126, 170, 176),
            constant: Color::Rgb(184, 152, 120),
            string: DIFF_ADD,
            comment: TEXT_MUTED,
            punctuation: TEXT_DIM,
        },
    }
}

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
    let status = render::fullscreen_status_layout(&state, area.width);
    let status_rows = status.lines.len().try_into().unwrap_or(u16::MAX);
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

    // Transcript and compact chrome share the inline renderer's pure builders.
    // Expanded status is a fullscreen projection over the same presentation
    // fields so it can use responsive columns and typed hit targets.
    let inner_w = area.width.saturating_sub(2) as usize;
    let preview_line = render::preview_slot_line(&state, area.width);

    // The transcript is a real Scroll over the *full* history, bound to the
    // persisted ScrollState. Metrics + offset are reconciled before rendering so
    // the mouse-wheel / paging handlers (which reuse these metrics) clamp
    // correctly. Content height is `usize`: a long transcript wraps past
    // u16::MAX rows (see `ScrollState`).
    let viewport_h = transcript_height as usize;
    let content_h = app.refresh_transcript_cache(inner_w);
    app.scroll_metrics = (content_h, viewport_h);
    app.scroll.clamp(content_h, viewport_h);

    // Hand `Scroll` only what it paints. Short content is top-padded so it rests
    // at the bottom (the inline scrollback feel) and handed over whole — it's
    // tiny. Taller content is *windowed*: we clone just `[offset .. offset+h]`
    // out of the cache instead of the whole transcript, so a frame stays
    // O(viewport) rather than cloning (and dropping) every retained row.
    // stick-to-bottom keeps the newest line visible as the turn streams.
    let transcript = if content_h <= viewport_h {
        let all = pad_to_bottom(app.transcript_window(0, content_h), transcript_height);
        Scroll::new(all, &app.scroll)
    } else {
        let offset = app.scroll.offset();
        let window = app.transcript_window(offset, (offset + viewport_h).min(content_h));
        Scroll::windowed(window, content_h, &app.scroll)
    };

    // Probes recover the rects tuika assigns so the host can place the terminal
    // cursor inside the composer and bound mouse selection to the transcript.
    let transcript_probe = RectProbe::new();
    let input_probe = RectProbe::new();

    // The composer is a real tuika TextInput reading full-screen's own composer
    // model of record (`app.composer`) — no per-frame mirror from a
    // ratatui-textarea. `App` routes editing keys straight into it.
    let root = view! {
        col {
            grow(1) { node(transcript_view(transcript, &transcript_probe)) }
            fixed(preview_height) { node(preview_view(preview_line)) }
            fixed(1) { node(message_rule(&state)) }
            fixed(input_height) { node(composer_row(&app.composer, &input_probe)) }
            fixed(status_sep_height) { node(status_rule()) }
            fixed(status_rows) { node(Text::new(status.lines.clone())) }
        }
    };

    // yolop's own palette (see `yolop_theme`); a Reset background lets the
    // styled lines' own colors show through.
    let theme = yolop_theme();
    tuika::paint(f.buffer_mut(), area, &theme, root.as_ref(), &[]);
    app.set_status_hit_regions(
        Rect {
            y: area.bottom().saturating_sub(status_rows),
            height: status_rows,
            ..area
        },
        &status.hits,
    );

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
    // Embed OSC 8 for markdown links (including labeled `[text](url)`) that fall
    // inside the visible transcript window. Bare URLs are also covered when
    // HyperlinkBackend is enabled; this pass is what makes a non-URL label
    // Ctrl+clickable in Ghostty.
    {
        let content_h = app.scroll_metrics.0;
        let viewport_h = app.scroll_metrics.1;
        // When content fits, pad_to_bottom puts blanks above — visible row 0 of
        // content sits at selection.y + (viewport_h - content_h). When scrolled,
        // cache line `offset + i` maps to selection row i.
        let (origin_y, first_line) = if content_h <= viewport_h {
            (
                selection
                    .y
                    .saturating_add(viewport_h.saturating_sub(content_h) as u16),
                0usize,
            )
        } else {
            (selection.y, app.scroll.offset())
        };
        let last_line = first_line.saturating_add(viewport_h);
        let visible: Vec<tuika::BufferLink> = app
            .transcript_links()
            .iter()
            .filter(|l| (l.line as usize) >= first_line && (l.line as usize) < last_line)
            .map(|l| {
                let mut v = l.clone();
                v.line = (l.line as usize)
                    .saturating_sub(first_line)
                    .min(u16::MAX as usize) as u16;
                v
            })
            .collect();
        let policy = crate::hyperlink_policy();
        tuika::apply_buffer_links(
            f.buffer_mut(),
            Position {
                x: selection.x,
                y: origin_y,
            },
            &visible,
            policy,
        );
        let interactive: Vec<_> = visible
            .into_iter()
            .filter(|link| policy.allows(&link.url))
            .collect();
        app.set_visible_links(
            Position {
                x: selection.x,
                y: origin_y,
            },
            &interactive,
        );
    }
    app.resolve_selection(f.buffer_mut());
    // Copy reads the whole selection from the transcript cache (it may span rows
    // that are scrolled off-screen); highlight paints only the visible slice.
    if app.take_pending_copy() {
        let text = app.selection_copy_text();
        if !text.is_empty() {
            let _ = tuika::write_clipboard(&mut std::io::stdout(), &text);
        }
    }
    if let Some(range) = app.selection_range() {
        tuika::highlight(
            f.buffer_mut(),
            selection,
            range,
            Style::default().add_modifier(Modifier::REVERSED),
        );
    }

    // The composer remains editable while a turn runs so Enter can queue a
    // steering message for the next safe turn boundary.
    if app.setup.is_none() {
        let input_rect = input_probe.rect();
        if input_rect.width > 0 && input_rect.height > 0 {
            let (x, y) = app.composer.cursor_screen(input_rect);
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
    panel_overlay(f, area, element(Text::new(lines)), cursor);
}

/// Composite `content` inside the centered bordered panel as a tuika [`Overlay`]
/// over a blank base, placing the terminal cursor at `cursor` when given.
///
/// The panel uses the shared [`render::setup_panel_rect`] geometry, and
/// [`Boxed`]'s default border + `symmetric(1, 0)` padding reproduces the inline
/// overlay's interior rect exactly, so the two modes place content identically.
fn panel_overlay(f: &mut Frame, area: Rect, content: Element, cursor: Option<(usize, usize)>) {
    let panel = render::setup_panel_rect(area);
    if panel.width == 0 || panel.height == 0 {
        return;
    }
    let boxed = Boxed::new(content).background(Style::default().bg(PANEL_BG).fg(TEXT_PRIMARY));
    let theme = yolop_theme();
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
    // Bounded list steps (provider / credential / effort) render through a real
    // tuika SelectList (item 4); other steps (input fields, Codex login, the
    // windowed model picker) fall back to the styled-Line Text path.
    if let Some(picker) = render::setup_picker(app) {
        draw_setup_picker(f, area, picker);
    } else {
        let (lines, cursor) = render::setup_overlay_content(app);
        draw_panel_overlay(f, area, lines, cursor);
    }
}

/// Render a bounded setup step as `header · SelectList · footer` inside the
/// panel. The `SelectState`'s index is driven from the step's own `selected`
/// field for rendering; the arrow-key transitions in `App::handle_setup_key`
/// step that field through `SelectState`'s (clamping) `move_up`/`move_down`, so
/// the pickers share one tested navigation source.
fn draw_setup_picker(f: &mut Frame, area: Rect, picker: render::SetupPicker) {
    let render::SetupPicker {
        header,
        options,
        selected,
        footer,
        viewport,
    } = picker;
    let mut state = SelectState::new();
    state.select(selected);
    let mut list = SelectList::new(options, &state);
    if let Some(rows) = viewport {
        list = list.viewport(rows);
    }
    let content = view! {
        col {
            node(Text::new(header))
            node(list)
            node(Text::new(footer))
        }
    };
    panel_overlay(f, area, content, None);
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

#[cfg(test)]
mod theme_tests {
    use super::*;

    #[test]
    fn resolve_theme_maps_yolop_and_presets() {
        // `yolop` is yolop's own palette, not a tuika preset.
        assert_eq!(resolve_theme("yolop"), Some(native_theme()));
        assert_eq!(resolve_theme("YOLOP"), Some(native_theme()));
        // Every bundled preset resolves to its tuika struct.
        for p in tuika::themes::PRESETS {
            assert_eq!(resolve_theme(p.name), Some(p.theme));
        }
        assert_eq!(resolve_theme("no-such-theme"), None);
    }

    #[test]
    fn theme_names_lists_yolop_first_then_presets() {
        let names = theme_names();
        assert_eq!(names.first(), Some(&"yolop"));
        for p in tuika::themes::PRESETS {
            assert!(names.contains(&p.name), "missing {}", p.name);
        }
    }
}

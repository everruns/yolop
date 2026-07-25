//! All terminal rendering: the inline chrome (stream preview, input, status),
//! the recent-transcript viewport, the setup overlay, and the markdown/diff
//! line formatting helpers. Pure rendering over `&App` / `&ViewState`; state
//! mutation lives elsewhere.

use super::*;
use tuika::width::str_cols;

// Color presentation for the transcript view-model types defined in
// `crate::tui::transcript`. Labels and status semantics live in `crate::tui::presentation`
// so they can be tested without a terminal renderer.
impl Author {
    pub fn color(&self) -> Color {
        match self {
            Author::User => ACCENT_BLUE,
            Author::Assistant => ACCENT_GOLD,
            Author::Narration => TEXT_MUTED,
            Author::Tool => TEXT_MUTED,
            Author::ToolDetail => TEXT_MUTED,
            Author::Stderr | Author::Sandbox => ERROR_RED,
            Author::Diff => ACCENT_BLUE,
            Author::System => TEXT_DIM,
        }
    }
}

impl StreamKind {
    fn color(self) -> Color {
        match self {
            StreamKind::Assistant => ACCENT_GOLD,
            StreamKind::Tool => TEXT_MUTED,
        }
    }
}

pub(crate) fn draw(f: &mut ratatui::Frame, app: &mut App) {
    if app.render_mode.is_fullscreen() {
        super::fullscreen::draw(f, app);
    } else {
        draw_shared(f, app);
    }
}

pub(super) fn draw_shared(f: &mut ratatui::Frame, app: &mut App) {
    let area = f.area();
    // Inline viewports cannot place a true full-screen modal above terminal
    // scrollback. Treat overlays as sheets that own this complete viewport so
    // the composer and status chrome never bleed through around their edges.
    if app.pending_ask.is_some() {
        draw_ask_overlay(f, area, app);
        return;
    }
    if app.setup.is_some() {
        draw_setup_overlay(f, area, app);
        return;
    }
    if app.background_panel.is_some() {
        draw_background_panel(f, area, app);
        return;
    }

    // Match `draw_input`: the `> ` prompt consumes two columns.
    let input_width = area.width.saturating_sub(2);
    let desired_input_height = app.input_height(input_width);
    let state = app.view_state();
    let layout = TuiLayout::new(
        area,
        desired_input_height,
        state.status_row_count(),
        chrome_preview_visible(&state),
    );

    // Chrome renders the non-input rows; we then layer the input field
    // on top into the chrome-reserved input slot.
    clear_transcript_viewport(f, layout.transcript);
    draw_recent_transcript(f, layout.transcript, app);
    draw_chrome_layout(f, layout.chrome, &state);
    draw_input(f, layout.chrome.input, app);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TuiLayout {
    pub frame: Rect,
    pub transcript: Rect,
    pub chrome: ChromeLayout,
}

impl TuiLayout {
    pub(crate) fn new(
        frame: Rect,
        desired_input_height: u16,
        status_rows: u16,
        preview_visible: bool,
    ) -> Self {
        let (chrome_height, input_height) = chrome_dimensions(
            frame.height,
            desired_input_height,
            status_rows,
            preview_visible,
        );
        let chrome_area = bottom_rect(frame, chrome_height);
        let transcript = Rect {
            height: frame.height.saturating_sub(chrome_area.height),
            ..frame
        };
        Self {
            frame,
            transcript,
            chrome: ChromeLayout::new(chrome_area, input_height, status_rows, preview_visible),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ChromeLayout {
    pub area: Rect,
    pub preview: Rect,
    pub message_separator: Rect,
    pub input: Rect,
    pub status_separator: Rect,
    pub session_status: Rect,
    pub input_height: u16,
}

impl ChromeLayout {
    pub(crate) fn new(
        area: Rect,
        input_height: u16,
        status_rows: u16,
        preview_visible: bool,
    ) -> Self {
        let preview_height = u16::from(input_height == 1 && preview_visible);
        let status_height = u16::from(input_height < 3);
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(preview_height),
                Constraint::Length(1),
                Constraint::Length(input_height),
                Constraint::Length(status_height),
                Constraint::Length(status_rows),
            ])
            .split(area);
        Self {
            area,
            preview: chunks[0],
            message_separator: chunks[1],
            input: chunks[2],
            status_separator: chunks[3],
            session_status: chunks[4],
            input_height,
        }
    }
}

pub(crate) fn bottom_rect(area: Rect, height: u16) -> Rect {
    let height = height.min(area.height);
    Rect {
        y: area.y + area.height.saturating_sub(height),
        height,
        ..area
    }
}

pub(crate) fn chrome_height(input_height: u16, status_rows: u16, preview_visible: bool) -> u16 {
    input_height.saturating_add(chrome_fixed_rows(
        input_height,
        status_rows,
        preview_visible,
    ))
}

pub(crate) fn chrome_dimensions(
    frame_height: u16,
    desired_input_height: u16,
    status_rows: u16,
    preview_visible: bool,
) -> (u16, u16) {
    if frame_height == 0 {
        return (0, 0);
    }

    let desired_input_height = desired_input_height.clamp(1, MAX_INPUT_HEIGHT);
    let chrome_height =
        chrome_height(desired_input_height, status_rows, preview_visible).min(frame_height);
    let mut input_height = desired_input_height.min(chrome_height);
    while input_height > 1
        && input_height.saturating_add(chrome_fixed_rows(
            input_height,
            status_rows,
            preview_visible,
        )) > chrome_height
    {
        input_height -= 1;
    }
    (chrome_height, input_height)
}

pub(crate) fn app_layout_for_frame(
    frame: Rect,
    desired_input_height: u16,
    status_rows: u16,
    preview_visible: bool,
) -> TuiLayout {
    TuiLayout::new(frame, desired_input_height, status_rows, preview_visible)
}

fn chrome_fixed_rows(input_height: u16, status_rows: u16, preview_visible: bool) -> u16 {
    u16::from(input_height == 1 && preview_visible)
        .saturating_add(1)
        .saturating_add(u16::from(input_height < 3))
        .saturating_add(status_rows)
}

pub(crate) fn clear_transcript_viewport(f: &mut ratatui::Frame, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    f.render_widget(Clear, area);
}

pub(crate) fn draw_recent_transcript(f: &mut ratatui::Frame, area: Rect, app: &App) {
    if area.width < 4 || area.height == 0 {
        return;
    }

    let inner = Rect {
        x: area.x.saturating_add(1),
        y: area.y,
        width: area.width.saturating_sub(2),
        height: area.height,
    };
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let rendered = recent_transcript_lines(app, inner.width as usize, inner.height as usize);
    if rendered.is_empty() {
        return;
    }

    let rendered_height = (rendered.len() as u16).min(inner.height);
    let render_area = Rect {
        y: inner.y + inner.height.saturating_sub(rendered_height),
        height: rendered_height,
        ..inner
    };
    f.render_widget(Paragraph::new(rendered), render_area);
}

pub(crate) fn recent_transcript_lines(
    app: &App,
    width: usize,
    max_lines: usize,
) -> Vec<Line<'static>> {
    if max_lines == 0 {
        return Vec::new();
    }

    let mut chunks = Vec::new();
    let mut total_lines = 0;
    let mut newer_author: Option<Author> = None;

    // Keep the recent tail visible above the composer even after lines are
    // flushed into native scrollback.
    let mirror_lines: Vec<&ChatLine> = app
        .lines
        .iter()
        .rev()
        .take(RECENT_TRANSCRIPT_SOURCE_LINES)
        .collect();

    for chat in mirror_lines {
        let chat = bounded_recent_chat_line(chat);
        let mut chunk = Vec::new();
        append_chat_lines(&mut chunk, &chat, width);
        if should_insert_chat_gap(&chat.author, newer_author.as_ref()) {
            chunk.push(Line::from(""));
        }

        if total_lines + chunk.len() > max_lines {
            let remaining = max_lines.saturating_sub(total_lines);
            if remaining > 0 {
                chunks.push(chunk.split_off(chunk.len().saturating_sub(remaining)));
            }
            break;
        }

        total_lines += chunk.len();
        newer_author = Some(chat.author);
        chunks.push(chunk);
    }

    chunks.reverse();
    chunks.into_iter().flatten().collect()
}

/// Wrap the **full**-screen transcript for `lines[start..]` onto `out`,
/// oldest-first, for the full-screen `tuika::components::Scroll` viewport (which owns the
/// alternate screen and can scroll the entire history, unlike the inline
/// recent-tail mirror above). Uses the same [`append_chat_lines`] formatting and
/// [`should_insert_chat_gap`] spacing as [`recent_transcript_lines`], but
/// assembles history forward with no tail bound and no `bounded_recent_chat_line`
/// truncation, because the full-screen viewport scrolls rather than clipping to a
/// fixed window — the traversal differs by design, so they are not worth
/// collapsing into one function.
///
/// `prev_author` is threaded across the `start` boundary and the author of the
/// last emitted chat is returned, so a later call can resume where this one
/// stopped. That resume is what lets the full-screen renderer memoize wrapping:
/// because `App::lines` only grows at the tail between resets, the wrap cache
/// re-runs this over just the newly-appended lines instead of the whole history
/// every frame. Pass `start = 0`, `prev_author = None` to wrap everything.
pub(crate) fn append_transcript_range(
    out: &mut Vec<Line<'static>>,
    links: &mut Vec<BufferLink>,
    lines: &[ChatLine],
    start: usize,
    width: usize,
    mut prev_author: Option<Author>,
) -> Option<Author> {
    for chat in lines.iter().skip(start) {
        if let Some(prev) = &prev_author
            && should_insert_chat_gap(prev, Some(&chat.author))
        {
            out.push(Line::from(""));
        }
        links.extend(append_chat_lines(out, chat, width));
        prev_author = Some(chat.author.clone());
    }
    prev_author
}

pub(crate) fn bounded_recent_chat_line(chat: &ChatLine) -> ChatLine {
    if chat.text.len() <= RECENT_TRANSCRIPT_MAX_TEXT_BYTES {
        return chat.clone();
    }

    ChatLine {
        author: chat.author.clone(),
        text: truncate_tail_bytes(&chat.text, RECENT_TRANSCRIPT_MAX_TEXT_BYTES),
    }
}

/// Render the non-input chrome (command suggestions / stream preview,
/// message separator, status separator, session status) into `area` using `state`, and
/// return the `Rect` where the caller should render the input widget
/// (which needs `&mut` and so cannot be driven through `ViewState`).
///
/// Snapshot tests call this against a `TestBackend` and ignore the
/// returned input rect — the buffer's other rows are what they assert
/// against.
#[cfg(test)]
pub(crate) fn draw_chrome(
    f: &mut ratatui::Frame,
    area: Rect,
    input_height: u16,
    state: &ViewState,
) -> Rect {
    let layout = ChromeLayout::new(
        area,
        input_height,
        state.status_row_count(),
        chrome_preview_visible(state),
    );
    draw_chrome_layout(f, layout, state);
    layout.input
}

pub(crate) fn chrome_preview_visible(state: &ViewState) -> bool {
    state.history_search.is_some()
        || state.presentation.stream_preview.is_some()
        || !state.command_suggestions.is_empty()
}

/// The preview row multiplexes (in priority order) the Ctrl+R reverse-search
/// prompt, the `@`/command suggestions, and the streaming preview. Shared by the
/// inline chrome and the full-screen renderer so both show the same popups.
pub(crate) fn draw_preview_slot(f: &mut ratatui::Frame, area: Rect, state: &ViewState) {
    if let Some(search) = &state.history_search {
        draw_history_search(f, area, search);
    } else if state.command_suggestions.is_empty() {
        draw_stream_preview(f, area, state);
    } else {
        draw_suggestions(f, area, &state.command_suggestions);
    }
}

/// The multiplexed preview-row content, as one styled line, for the full-screen
/// tuika renderer. Same priority order as [`draw_preview_slot`] — reverse
/// search, else command suggestions, else the streaming preview — but pure, so
/// the full-screen renderer paints it through a tuika view instead of a ratatui
/// widget. `None` when the row is empty.
pub(crate) fn preview_slot_line(state: &ViewState, width: u16) -> Option<Line<'static>> {
    if let Some(search) = &state.history_search {
        Some(history_search_preview_line(search, width))
    } else if !state.command_suggestions.is_empty() {
        Some(suggestion_preview_line(&state.command_suggestions, width))
    } else {
        stream_preview_line(state, width)
    }
}

pub(crate) fn draw_chrome_layout(f: &mut ratatui::Frame, layout: ChromeLayout, state: &ViewState) {
    draw_preview_slot(f, layout.preview, state);
    draw_message_separator(f, layout.message_separator, state);
    draw_status_separator(f, layout.status_separator);
    draw_session_status(f, layout.session_status, state);
}

pub(crate) fn draw_setup_overlay(f: &mut ratatui::Frame, area: Rect, app: &App) {
    if app.setup.is_none() || area.width == 0 || area.height == 0 {
        return;
    }
    let panel = setup_panel_rect(area);
    if panel.width == 0 || panel.height == 0 {
        return;
    }
    f.render_widget(Clear, area);
    f.render_widget(Clear, panel);
    let block = Block::default()
        .borders(Borders::ALL)
        .style(Style::default().bg(PANEL_BG).fg(TEXT_PRIMARY));
    f.render_widget(block, panel);
    let inner = Rect {
        x: panel.x.saturating_add(2),
        y: panel.y.saturating_add(1),
        width: panel.width.saturating_sub(4),
        height: panel.height.saturating_sub(2),
    };
    let (lines, cursor) = setup_overlay_content(app);
    f.render_widget(
        Paragraph::new(lines).style(Style::default().bg(PANEL_BG)),
        inner,
    );
    if let Some((row, col)) = cursor
        && inner.height > 0
        && inner.width > 0
    {
        f.set_cursor_position((
            inner
                .x
                .saturating_add((col as u16).min(inner.width.saturating_sub(1))),
            inner
                .y
                .saturating_add((row as u16).min(inner.height.saturating_sub(1))),
        ));
    }
}

/// The `ui/ask` overlay body: the question plus a live echo of the answer being
/// typed, and the `(row, col)` cursor cell within the panel interior. Pure, so
/// both the inline sheet renderer and the full-screen tuika overlay show the
/// same content.
pub(crate) fn ask_overlay_content(ask: &PendingAsk) -> (Vec<Line<'static>>, (usize, usize)) {
    let mut lines = vec![
        Line::from(Span::styled(
            "An extension is asking:",
            Style::default()
                .fg(TEXT_PRIMARY)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(ask.prompt.clone()),
        Line::from(""),
    ];
    // Selector mode: a highlighted list of options.
    if !ask.options.is_empty() {
        for (i, option) in ask.options.iter().enumerate() {
            let selected = i == ask.selected;
            let marker = if selected { "▶ " } else { "  " };
            let style = if selected {
                Style::default()
                    .fg(TEXT_PRIMARY)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().add_modifier(Modifier::DIM)
            };
            lines.push(Line::from(Span::styled(format!("{marker}{option}"), style)));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "↑/↓ to choose · Enter to select · Esc to cancel",
            Style::default().add_modifier(Modifier::DIM),
        )));
        // No text cursor in selector mode; park it on the selected row.
        return (lines, (4 + ask.selected, 0));
    }
    // Text / secret input. Secret input is masked; the placeholder (shown when
    // empty) is never masked.
    let field = if ask.value.is_empty() {
        ask.placeholder
            .as_ref()
            .map(|p| format!("({p})"))
            .unwrap_or_default()
    } else if ask.secret {
        "•".repeat(ask.value.chars().count())
    } else {
        ask.value.clone()
    };
    lines.push(Line::from(format!("> {field}")));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Enter to answer · Esc to cancel",
        Style::default().add_modifier(Modifier::DIM),
    )));
    // The typed answer is on row 4, after the "> " prompt.
    let cursor = (4, "> ".len() + ask.value.chars().count());
    (lines, cursor)
}

/// Overlay for an extension `ui/ask` prompt: the question plus a live echo of
/// the answer being typed. Owns the viewport like the setup overlay.
pub(crate) fn draw_ask_overlay(f: &mut ratatui::Frame, area: Rect, app: &App) {
    let Some(ask) = app.pending_ask.as_ref() else {
        return;
    };
    if area.width == 0 || area.height == 0 {
        return;
    }
    let panel = setup_panel_rect(area);
    if panel.width == 0 || panel.height == 0 {
        return;
    }
    f.render_widget(Clear, area);
    f.render_widget(Clear, panel);
    let block = Block::default()
        .borders(Borders::ALL)
        .style(Style::default().bg(PANEL_BG).fg(TEXT_PRIMARY));
    f.render_widget(block, panel);
    let inner = Rect {
        x: panel.x.saturating_add(2),
        y: panel.y.saturating_add(1),
        width: panel.width.saturating_sub(4),
        height: panel.height.saturating_sub(2),
    };
    let (lines, (cursor_row, cursor_col)) = ask_overlay_content(ask);
    f.render_widget(
        Paragraph::new(lines).style(Style::default().bg(PANEL_BG)),
        inner,
    );
    // Park the cursor after the typed value.
    if inner.width > 0 && inner.height > cursor_row as u16 {
        f.set_cursor_position((
            inner
                .x
                .saturating_add((cursor_col as u16).min(inner.width.saturating_sub(1))),
            inner.y.saturating_add(cursor_row as u16),
        ));
    }
}

/// Interactive task-tree panel overlay (toggled with Ctrl+B). Reuses the
/// `/background` tree rendering, adding a selected row and cancellation key.
pub(crate) fn draw_background_panel(f: &mut ratatui::Frame, area: Rect, app: &App) {
    let Some(offset) = app.background_panel else {
        return;
    };
    if area.width == 0 || area.height == 0 {
        return;
    }
    let panel = setup_panel_rect(area);
    if panel.width == 0 || panel.height == 0 {
        return;
    }
    f.render_widget(Clear, area);
    f.render_widget(Clear, panel);
    let block = Block::default()
        .borders(Borders::ALL)
        .style(Style::default().bg(PANEL_BG).fg(TEXT_PRIMARY));
    f.render_widget(block, panel);
    let inner = Rect {
        x: panel.x.saturating_add(2),
        y: panel.y.saturating_add(1),
        width: panel.width.saturating_sub(4),
        height: panel.height.saturating_sub(2),
    };
    let body = app.background_panel_body();
    let texts = background_panel_lines(&body, offset, inner.height as usize);
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(texts.len());
    for (i, text) in texts.into_iter().enumerate() {
        if i == 0 {
            lines.push(Line::from(Span::styled(
                text,
                Style::default().fg(DIFF_META).add_modifier(Modifier::BOLD),
            )));
        } else {
            lines.push(Line::raw(text));
        }
    }
    f.render_widget(
        Paragraph::new(lines).style(Style::default().bg(PANEL_BG)),
        inner,
    );
}

/// Lines for the background panel: a fixed header followed by the scrolled body
/// (the `/background` text), clipped to `height` rows. Pure for testability.
pub(crate) fn background_panel_lines(body: &str, offset: usize, height: usize) -> Vec<String> {
    if height == 0 {
        return Vec::new();
    }
    let mut out = vec!["Task tree — ↑/↓ select · x cancel · Ctrl+B/Esc close".to_string()];
    let body_rows = height.saturating_sub(1);
    for line in body.lines().skip(offset).take(body_rows) {
        out.push(line.to_string());
    }
    out
}

pub(crate) fn setup_panel_rect(area: Rect) -> Rect {
    let width = area.width.saturating_sub(4).min(104).max(area.width.min(1));
    let height = area
        .height
        .saturating_sub(2)
        .min(18)
        .max(area.height.min(1));
    Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    }
}

pub(crate) fn setup_overlay_content(app: &App) -> (Vec<Line<'static>>, Option<(usize, usize)>) {
    let mut lines = Vec::new();
    let mut cursor = None;
    match app.setup.as_ref() {
        Some(SetupStep::Provider { selected }) => {
            lines.push(setup_title("Set Up Yolop"));
            lines.push(setup_hint(
                "Connected providers jump straight to model selection.",
            ));
            lines.push(Line::from(""));
            let current = app.current_provider_name();
            let snapshot = app.settings.snapshot();
            for (idx, option) in PROVIDER_OPTIONS.iter().enumerate() {
                let (_, status) = App::provider_status(&snapshot, option.name);
                let mut hint = format!("{} · {status}", option.hint);
                if option.name == current {
                    hint.push_str(" · current");
                }
                lines.push(setup_row(idx == *selected, idx + 1, option.label, &hint));
            }
            lines.push(Line::from(""));
            lines.push(setup_footer(
                "Enter select · c configure key/URL · ↑/↓ move · Esc cancel",
            ));
        }
        Some(SetupStep::BaseUrlInput { value, error }) => {
            lines.push(setup_title("Custom OpenAI-Compatible Endpoint"));
            lines.push(setup_hint(
                "Base URL of the API, e.g. http://localhost:8000/v1 — saved to settings.toml.",
            ));
            lines.push(Line::from(""));
            let input = format!("› {value}");
            cursor = Some((3, input.chars().count()));
            lines.push(Line::from(vec![
                Span::styled(
                    "› ",
                    Style::default()
                        .fg(ACCENT_BLUE)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(value.clone(), Style::default().fg(TEXT_PRIMARY)),
            ]));
            push_setup_error(&mut lines, error.as_deref());
            lines.push(setup_footer("Enter save · Esc back"));
        }
        Some(SetupStep::Credential {
            provider,
            selected,
            error,
            ..
        }) => {
            lines.push(setup_title(&format!(
                "API Key for {}",
                App::provider_label(provider)
            )));
            lines.push(setup_hint(
                "Choose how yolop should authenticate this provider.",
            ));
            lines.push(Line::from(""));
            for (idx, option) in App::credential_options(provider).iter().enumerate() {
                lines.push(setup_row(
                    idx == *selected,
                    idx + 1,
                    &option.label,
                    &option.hint,
                ));
            }
            push_setup_error(&mut lines, error.as_deref());
            lines.push(setup_footer("Enter confirm · ↑/↓ move · Esc back"));
        }
        Some(SetupStep::CodexLogin {
            method,
            device_code,
            ..
        }) => {
            lines.push(setup_title("Sign in to Codex subscription"));
            match method {
                CodexLoginMethod::Browser => {
                    lines.push(setup_hint(
                        "Waiting for the browser to finish authentication.",
                    ));
                    lines.push(Line::from(""));
                    lines.push(setup_hint(
                        "If the browser was closed, cancel here and try again.",
                    ));
                }
                CodexLoginMethod::Device => {
                    if let Some((verification_uri, user_code)) = device_code {
                        lines.push(setup_hint(&format!("Open {verification_uri}")));
                        lines.push(Line::from(""));
                        lines.push(setup_hint(&format!("Enter code: {user_code}")));
                    } else {
                        lines.push(setup_hint("Requesting a device code…"));
                    }
                }
            }
            lines.push(Line::from(""));
            lines.push(setup_footer("Esc cancel · Ctrl+C twice exit"));
        }
        Some(SetupStep::TokenInput {
            provider,
            token,
            error,
            ..
        }) => {
            let secret_label = if provider == "codex" {
                "Access Token"
            } else {
                "API Key"
            };
            lines.push(setup_title(&format!(
                "Paste {secret_label} for {}",
                App::provider_label(provider)
            )));
            lines.push(setup_hint(
                "The secret is masked and is never written to the transcript.",
            ));
            lines.push(Line::from(""));
            let masked = if token.is_empty() {
                String::new()
            } else {
                "•".repeat(token.chars().count())
            };
            let input = format!("› {masked}");
            cursor = Some((3, input.chars().count()));
            lines.push(Line::from(vec![
                Span::styled(
                    "› ",
                    Style::default()
                        .fg(ACCENT_BLUE)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(masked, Style::default().fg(TEXT_PRIMARY)),
            ]));
            push_setup_error(&mut lines, error.as_deref());
            lines.push(setup_footer("Enter save · Esc back"));
        }
        Some(SetupStep::PickModel {
            provider,
            selected,
            custom,
            error,
        }) => {
            lines.push(setup_title("Select Model"));
            lines.push(setup_hint(&if provider == "custom" {
                "Model id served by your endpoint. Applies to this session and future sessions."
                    .to_string()
            } else {
                format!(
                    "{} models. Applies to this session and future sessions.",
                    App::provider_label(provider)
                )
            }));
            lines.push(Line::from(""));
            let options = app.model_options(provider);
            if let Some(value) = custom {
                let input = format!("› {value}");
                cursor = Some((3, input.chars().count()));
                lines.push(Line::from(vec![
                    Span::styled(
                        "› ",
                        Style::default()
                            .fg(ACCENT_BLUE)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(value.clone(), Style::default().fg(TEXT_PRIMARY)),
                ]));
            } else {
                if app.is_fetching_models(provider) {
                    lines.push(setup_hint("fetching models from the provider API…"));
                }
                let total = options.len();
                let recommended_count = app.model_recommended_count(provider);
                let (start, end) = model_window(*selected, total, MAX_VISIBLE_MODEL_ROWS);
                if start > 0 {
                    lines.push(setup_hint(&format!("↑ {start} more")));
                }
                // Specs are provider-relative; compare against the bare model
                // id (the same anchor `model_index_for_label` uses).
                let current = app.model.model_id();
                for (idx, option) in options.iter().enumerate().take(end).skip(start) {
                    if recommended_count > 0
                        && idx == recommended_count
                        && recommended_count < total.saturating_sub(1)
                    {
                        lines.push(setup_divider("─── more models ───"));
                    }
                    let mut hint = option.hint.to_string();
                    if option.spec.as_deref() == Some(current.as_str()) {
                        hint.push_str(" · current");
                    }
                    lines.push(setup_row(idx == *selected, idx + 1, &option.label, &hint));
                }
                if end < total {
                    lines.push(setup_hint(&format!("↓ {} more", total - end)));
                }
            }
            push_setup_error(&mut lines, error.as_deref());
            lines.push(setup_footer("Enter confirm · ↑/↓ move · Esc back"));
        }
        Some(SetupStep::PickEffort { selected, error }) => {
            lines.push(setup_title("Select Reasoning Effort"));
            lines.push(setup_hint(
                "Profile-defined options for the current model — this session and future sessions.",
            ));
            lines.push(Line::from(""));
            let options = app.model.reasoning_effort_options();
            let current = app.model.reasoning_effort();
            let default = app.model.default_reasoning_effort();
            if options.is_empty() {
                lines.push(setup_hint(
                    "No reasoning effort options in this model profile.",
                ));
            }
            for (idx, option) in options.iter().enumerate() {
                let mut hint = String::new();
                if Some(option.value.as_str()) == current.as_deref() {
                    hint.push_str("current");
                }
                if Some(option.value.as_str()) == default.as_deref() {
                    if !hint.is_empty() {
                        hint.push_str(" · ");
                    }
                    hint.push_str("profile default");
                }
                lines.push(setup_row(idx == *selected, idx + 1, &option.label, &hint));
            }
            push_setup_error(&mut lines, error.as_deref());
            lines.push(setup_footer("Enter confirm · ↑/↓ move · Esc cancel"));
        }
        None => {}
    }
    (lines, cursor)
}

/// A setup step's navigable option list, split from its chrome, for rendering as
/// a tuika `SelectList` in the full-screen overlay.
pub(crate) struct SetupPicker {
    pub header: Vec<Line<'static>>,
    pub options: Vec<Line<'static>>,
    pub selected: usize,
    pub footer: Vec<Line<'static>>,
    /// Max visible option rows, for a `SelectList` viewport; `None` shows all.
    pub viewport: Option<u16>,
}

/// The option rows + surrounding chrome for the list-selection steps (provider,
/// credential method, reasoning effort, and the model list), so full-screen can
/// render them through a tuika `SelectList` instead of hand-highlighted
/// [`setup_row`] lines (item 4). The model list sets [`SetupPicker::viewport`] so
/// the `SelectList` windows its (possibly huge) options with a scrollbar.
///
/// NOTE (duplication): this intentionally mirrors the per-step option iteration
/// in [`setup_overlay_content`] rather than refactoring it, so the inline sheet
/// renderer stays byte-identical. The two share [`setup_option_line`] for the row
/// formatting. The model picker's **custom-id input** sub-mode (`custom: Some`)
/// returns `None` here so it falls back to the shared text-input panel path —
/// that sub-mode is a `TextInput`, not a list. Other input steps and Codex login
/// return `None` too (nothing to select). The inline `setup_overlay_content` also
/// draws a "─── more models ───" divider between recommended and other models;
/// the `SelectList` path omits it (the scrollbar conveys position instead).
pub(crate) fn setup_picker(app: &App) -> Option<SetupPicker> {
    match app.setup.as_ref()? {
        SetupStep::Provider { selected } => {
            let header = vec![
                setup_title("Set Up Yolop"),
                setup_hint("Connected providers jump straight to model selection."),
                Line::from(""),
            ];
            let current = app.current_provider_name();
            let snapshot = app.settings.snapshot();
            let options = PROVIDER_OPTIONS
                .iter()
                .enumerate()
                .map(|(idx, option)| {
                    let (_, status) = App::provider_status(&snapshot, option.name);
                    let mut hint = format!("{} · {status}", option.hint);
                    if option.name == current {
                        hint.push_str(" · current");
                    }
                    setup_option_line(idx + 1, option.label, &hint)
                })
                .collect();
            let footer = vec![setup_footer(
                "Enter select · c configure key/URL · ↑/↓ move · Esc cancel",
            )];
            Some(SetupPicker {
                header,
                options,
                selected: *selected,
                footer,
                viewport: None,
            })
        }
        SetupStep::Credential {
            provider,
            selected,
            error,
            ..
        } => {
            let header = vec![
                setup_title(&format!("API Key for {}", App::provider_label(provider))),
                setup_hint("Choose how yolop should authenticate this provider."),
                Line::from(""),
            ];
            let options = App::credential_options(provider)
                .iter()
                .enumerate()
                .map(|(idx, option)| setup_option_line(idx + 1, &option.label, &option.hint))
                .collect();
            let mut footer = Vec::new();
            push_setup_error(&mut footer, error.as_deref());
            footer.push(setup_footer("Enter confirm · ↑/↓ move · Esc back"));
            Some(SetupPicker {
                header,
                options,
                selected: *selected,
                footer,
                viewport: None,
            })
        }
        SetupStep::PickEffort { selected, error } => {
            let effort_options = app.model.reasoning_effort_options();
            if effort_options.is_empty() {
                // No options — the Text path shows the explanatory hint instead.
                return None;
            }
            let header = vec![
                setup_title("Select Reasoning Effort"),
                setup_hint(
                    "Profile-defined options for the current model — this session and future sessions.",
                ),
                Line::from(""),
            ];
            let current = app.model.reasoning_effort();
            let default = app.model.default_reasoning_effort();
            let options = effort_options
                .iter()
                .enumerate()
                .map(|(idx, option)| {
                    let mut hint = String::new();
                    if Some(option.value.as_str()) == current.as_deref() {
                        hint.push_str("current");
                    }
                    if Some(option.value.as_str()) == default.as_deref() {
                        if !hint.is_empty() {
                            hint.push_str(" · ");
                        }
                        hint.push_str("profile default");
                    }
                    setup_option_line(idx + 1, &option.label, &hint)
                })
                .collect();
            let mut footer = Vec::new();
            push_setup_error(&mut footer, error.as_deref());
            footer.push(setup_footer("Enter confirm · ↑/↓ move · Esc cancel"));
            Some(SetupPicker {
                header,
                options,
                selected: *selected,
                footer,
                viewport: None,
            })
        }
        // The model list is a windowed SelectList. Its custom-id sub-mode
        // (`custom: Some`) is a text input, so it falls back to the Text path.
        SetupStep::PickModel {
            provider,
            selected,
            custom: None,
            error,
        } => {
            let header = vec![
                setup_title("Select Model"),
                setup_hint(&if provider == "custom" {
                    "Model id served by your endpoint. Applies to this session and future sessions."
                        .to_string()
                } else {
                    format!(
                        "{} models. Applies to this session and future sessions.",
                        App::provider_label(provider)
                    )
                }),
                Line::from(""),
            ];
            let current = app.model.model_id();
            let options = app
                .model_options(provider)
                .iter()
                .enumerate()
                .map(|(idx, option)| {
                    let mut hint = option.hint.to_string();
                    if option.spec.as_deref() == Some(current.as_str()) {
                        hint.push_str(" · current");
                    }
                    setup_option_line(idx + 1, &option.label, &hint)
                })
                .collect();
            let mut footer = Vec::new();
            push_setup_error(&mut footer, error.as_deref());
            footer.push(setup_footer("Enter confirm · ↑/↓ move · Esc back"));
            Some(SetupPicker {
                header,
                options,
                selected: *selected,
                footer,
                viewport: Some(MAX_VISIBLE_MODEL_ROWS as u16),
            })
        }
        _ => None,
    }
}

/// One picker option row — `index. label   hint` — with no selection marker or
/// highlight (the `SelectList` adds the caret and applies the selection style).
/// Shares the label column width with [`setup_row`].
pub(crate) fn setup_option_line(index: usize, label: &str, hint: &str) -> Line<'static> {
    let pad = 28usize.saturating_sub(label.chars().count()).max(2);
    Line::from(vec![
        Span::styled(
            format!("{index}. "),
            Style::default().fg(TEXT_MUTED).bg(PANEL_BG),
        ),
        Span::styled(
            format!("{label}{}", " ".repeat(pad)),
            Style::default().fg(TEXT_PRIMARY).bg(PANEL_BG),
        ),
        Span::styled(
            hint.to_string(),
            Style::default().fg(TEXT_MUTED).bg(PANEL_BG),
        ),
    ])
}

pub(crate) fn setup_title(text: &str) -> Line<'static> {
    Line::from(Span::styled(
        text.to_string(),
        Style::default()
            .fg(TEXT_PRIMARY)
            .bg(PANEL_BG)
            .add_modifier(Modifier::BOLD),
    ))
}

pub(crate) fn setup_hint(text: &str) -> Line<'static> {
    Line::from(Span::styled(
        text.to_string(),
        Style::default().fg(TEXT_MUTED).bg(PANEL_BG),
    ))
}

pub(crate) fn setup_divider(text: &str) -> Line<'static> {
    Line::from(Span::styled(
        text.to_string(),
        Style::default().fg(ACCENT_BLUE).bg(PANEL_BG),
    ))
}

pub(crate) fn setup_footer(text: &str) -> Line<'static> {
    Line::from(Span::styled(
        text.to_string(),
        Style::default().fg(TEXT_MUTED).bg(PANEL_BG),
    ))
}

pub(crate) fn setup_row(selected: bool, index: usize, label: &str, hint: &str) -> Line<'static> {
    let marker = if selected { "›" } else { " " };
    let marker_style = if selected {
        Style::default()
            .fg(ACCENT_BLUE)
            .bg(PANEL_BG)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(TEXT_DIM).bg(PANEL_BG)
    };
    let label_style = if selected {
        Style::default()
            .fg(Color::Rgb(135, 220, 205))
            .bg(PANEL_BG)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(TEXT_PRIMARY).bg(PANEL_BG)
    };
    Line::from(vec![
        Span::styled(format!("{marker} "), marker_style),
        Span::styled(
            format!("{index}. "),
            Style::default().fg(TEXT_MUTED).bg(PANEL_BG),
        ),
        // Pad to a 28-col label column so hints align, but always keep at
        // least a 2-space gap: labels like "Use OPENAI_API_KEY from
        // environment" overflow the column, and a bare `{:<28}` would let the
        // hint butt right against them ("environmentnot detected yet").
        Span::styled(
            {
                let pad = 28usize.saturating_sub(label.chars().count()).max(2);
                format!("{label}{}", " ".repeat(pad))
            },
            label_style,
        ),
        Span::styled(
            hint.to_string(),
            Style::default().fg(TEXT_MUTED).bg(PANEL_BG),
        ),
    ])
}

pub(crate) fn push_setup_error(lines: &mut Vec<Line<'static>>, error: Option<&str>) {
    if let Some(error) = error {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!("error: {error}"),
            Style::default().fg(Color::Rgb(220, 120, 90)).bg(PANEL_BG),
        )));
    } else {
        lines.push(Line::from(""));
    }
}

pub(crate) fn draw_suggestions(
    f: &mut ratatui::Frame,
    area: Rect,
    suggestions: &[CommandSuggestion],
) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    f.render_widget(
        Paragraph::new(suggestion_preview_line(suggestions, area.width)),
        area,
    );
}

pub(crate) fn draw_history_search(f: &mut ratatui::Frame, area: Rect, search: &HistorySearchView) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    f.render_widget(
        Paragraph::new(history_search_preview_line(search, area.width)),
        area,
    );
}

/// The chrome row for an active reverse search: `(reverse-search)'query'` plus a
/// `no match` marker when nothing matched. The matched entry itself previews in
/// the composer below.
pub(crate) fn history_search_preview_line(search: &HistorySearchView, width: u16) -> Line<'static> {
    let prefix = "(reverse-search) ";
    let query = format!("'{}'", search.query);
    // Flag a non-empty query that matched nothing; a bare prompt stays quiet.
    let suffix = if !search.matched && !search.query.is_empty() {
        "  no match".to_string()
    } else {
        String::new()
    };
    let budget = (width as usize).saturating_sub(prefix.chars().count() + 1);
    let query = truncate_end_chars(&query, budget.max(4));
    let mut spans = vec![
        Span::styled(
            prefix,
            Style::default()
                .fg(ACCENT_BLUE)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(query, Style::default().fg(TEXT_PRIMARY)),
    ];
    if !suffix.is_empty() {
        spans.push(Span::styled(suffix, Style::default().fg(DIFF_DELETE)));
    }
    Line::from(spans)
}

pub(crate) fn suggestion_preview_line(
    suggestions: &[CommandSuggestion],
    width: u16,
) -> Line<'static> {
    let body = suggestions
        .iter()
        .map(|suggestion| suggestion.label.as_str())
        .collect::<Vec<_>>()
        .join("  ·  ");
    let prefix = "Tab ";
    let max_body = (width as usize)
        .saturating_sub(prefix.chars().count() + 1)
        .max(8);
    Line::from(vec![
        Span::styled(
            prefix,
            Style::default()
                .fg(ACCENT_BLUE)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            truncate_end_chars(&body, max_body),
            Style::default().fg(TEXT_MUTED),
        ),
    ])
}

/// The streaming-preview row: `label › …tail`, showing the most recent non-empty
/// line of the accumulated stream so the eye tracks the live tail. `None` when
/// nothing is streaming or `width` is zero. Pure so both the inline chrome and
/// the full-screen tuika renderer draw the identical line.
pub(crate) fn stream_preview_line(state: &ViewState, width: u16) -> Option<Line<'static>> {
    let preview = state.presentation.stream_preview.as_ref()?;
    let inner_width = width as usize;
    if inner_width == 0 {
        return None;
    }
    let tail = preview
        .text
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("");
    let label = preview.kind.label();
    let prefix = format!("{label} › ");
    let prefix_w = prefix.chars().count();
    let max_text = inner_width.saturating_sub(prefix_w + 1).max(8);
    let truncated = truncate_tail_chars(tail, max_text);
    Some(Line::from(vec![
        Span::styled(
            prefix,
            Style::default()
                .fg(preview.kind.color())
                .add_modifier(Modifier::DIM),
        ),
        Span::styled(truncated, Style::default().fg(TEXT_MUTED)),
    ]))
}

pub(crate) fn draw_stream_preview(f: &mut ratatui::Frame, area: Rect, state: &ViewState) {
    if area.height == 0 {
        return;
    }
    let Some(line) = stream_preview_line(state, area.width) else {
        return;
    };
    f.render_widget(Paragraph::new(line), area);
}

/// Keep the last `max_chars` of `text`. Streaming preview reads better
/// when the cursor (tail of the stream) is what's visible.
pub(crate) fn truncate_tail_chars(text: &str, max_chars: usize) -> String {
    let count = text.chars().count();
    if count <= max_chars {
        return text.to_string();
    }
    let skip = count - max_chars.saturating_sub(1);
    let mut out = String::with_capacity(max_chars);
    out.push('…');
    out.extend(text.chars().skip(skip));
    out
}

pub(crate) fn truncate_tail_bytes(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    if max_bytes == 0 {
        return String::new();
    }
    if max_bytes <= '…'.len_utf8() {
        return "…".to_string();
    }

    let mut start = text.len().saturating_sub(max_bytes - '…'.len_utf8());
    while start < text.len() && !text.is_char_boundary(start) {
        start += 1;
    }
    format!("…{}", &text[start..])
}

pub(crate) fn truncate_end_chars(text: &str, max_chars: usize) -> String {
    let count = text.chars().count();
    if count <= max_chars {
        return text.to_string();
    }
    if max_chars == 0 {
        return String::new();
    }
    if max_chars == 1 {
        return "…".to_string();
    }
    let mut out = String::with_capacity(max_chars);
    out.extend(text.chars().take(max_chars - 1));
    out.push('…');
    out
}

pub(crate) fn should_insert_chat_gap(current: &Author, next: Option<&Author>) -> bool {
    let Some(next) = next else {
        return false;
    };

    !matches!(
        (current, next),
        (&Author::Tool, &Author::Tool)
            | (&Author::Tool, &Author::ToolDetail)
            | (&Author::ToolDetail, &Author::Tool)
            | (&Author::ToolDetail, &Author::ToolDetail)
            | (&Author::Tool, &Author::Stderr)
            | (&Author::ToolDetail, &Author::Stderr)
            | (&Author::Stderr, &Author::ToolDetail)
            | (&Author::Stderr, &Author::Stderr)
            | (&Author::Stderr, &Author::Sandbox)
            | (&Author::Diff, &Author::Diff)
    )
}

pub(crate) fn append_chat_lines<'a>(
    lines: &mut Vec<Line<'a>>,
    chat: &ChatLine,
    inner_width: usize,
) -> Vec<BufferLink> {
    let presented = present_transcript_line(chat);
    if matches!(presented.author, Author::ToolDetail) {
        append_wrapped_plain(
            lines,
            "           ",
            Style::default().fg(TEXT_MUTED),
            &presented.text,
            inner_width,
        );
        return Vec::new();
    }
    if matches!(presented.author, Author::Stderr) {
        append_wrapped_plain(
            lines,
            "         ● ",
            Style::default().fg(ERROR_RED),
            &presented.text,
            inner_width,
        );
        return Vec::new();
    }

    let header_text = format!("{} › ", presented.label.unwrap_or_default());
    let header_style = Style::default()
        .fg(presented.author.color())
        .add_modifier(Modifier::BOLD);
    if matches!(presented.author, Author::Assistant) {
        append_markdown_lines(
            lines,
            &header_text,
            header_style,
            &presented.text,
            inner_width,
        )
    } else if matches!(presented.author, Author::Narration) {
        append_wrapped_styled(
            lines,
            &header_text,
            header_style,
            &presented.text,
            inner_width,
            Style::default().fg(TEXT_MUTED),
        );
        Vec::new()
    } else if matches!(presented.author, Author::Diff) {
        append_wrapped_diff(
            lines,
            &header_text,
            header_style,
            &presented.text,
            inner_width,
        );
        Vec::new()
    } else {
        append_wrapped_plain(
            lines,
            &header_text,
            header_style,
            &presented.text,
            inner_width,
        );
        Vec::new()
    }
}

pub(crate) fn append_wrapped_plain<'a>(
    lines: &mut Vec<Line<'a>>,
    first_prefix: &str,
    prefix_style: Style,
    text: &str,
    inner_width: usize,
) {
    append_wrapped_styled(
        lines,
        first_prefix,
        prefix_style,
        text,
        inner_width,
        Style::default(),
    );
}

pub(crate) fn append_wrapped_styled<'a>(
    lines: &mut Vec<Line<'a>>,
    first_prefix: &str,
    prefix_style: Style,
    text: &str,
    inner_width: usize,
    content_style: Style,
) {
    let continuation = " ".repeat(first_prefix.chars().count());
    let wrap_width = inner_width
        .saturating_sub(first_prefix.chars().count())
        .max(1);
    let mut emitted = false;
    for raw in text.lines() {
        let wrapped = textwrap::wrap(
            raw,
            textwrap::Options::new(wrap_width)
                .break_words(true)
                .word_separator(textwrap::WordSeparator::AsciiSpace),
        );
        if wrapped.is_empty() {
            let prefix = if emitted {
                continuation.as_str()
            } else {
                first_prefix
            };
            lines.push(Line::from(vec![Span::styled(
                prefix.to_string(),
                prefix_style,
            )]));
            emitted = true;
            continue;
        }
        for piece in wrapped {
            let prefix = if emitted {
                continuation.as_str()
            } else {
                first_prefix
            };
            lines.push(Line::from(vec![
                Span::styled(prefix.to_string(), prefix_style),
                Span::styled(piece.into_owned(), content_style),
            ]));
            emitted = true;
        }
    }
    if !emitted {
        lines.push(Line::from(vec![Span::styled(
            first_prefix.to_string(),
            prefix_style,
        )]));
    }
}

pub(crate) fn append_wrapped_diff<'a>(
    lines: &mut Vec<Line<'a>>,
    first_prefix: &str,
    prefix_style: Style,
    text: &str,
    inner_width: usize,
) {
    let continuation = " ".repeat(first_prefix.chars().count());
    let wrap_width = inner_width
        .saturating_sub(first_prefix.chars().count())
        .max(1);
    let mut emitted = false;
    for raw in text.lines() {
        let content_style = diff_line_style(raw);
        let wrapped = textwrap::wrap(
            raw,
            textwrap::Options::new(wrap_width)
                .break_words(true)
                .word_separator(textwrap::WordSeparator::AsciiSpace),
        );
        if wrapped.is_empty() {
            let prefix = if emitted {
                continuation.as_str()
            } else {
                first_prefix
            };
            lines.push(Line::from(vec![Span::styled(
                prefix.to_string(),
                prefix_style,
            )]));
            emitted = true;
            continue;
        }
        for piece in wrapped {
            let prefix = if emitted {
                continuation.as_str()
            } else {
                first_prefix
            };
            lines.push(Line::from(vec![
                Span::styled(prefix.to_string(), prefix_style),
                Span::styled(piece.into_owned(), content_style),
            ]));
            emitted = true;
        }
    }
    if !emitted {
        lines.push(Line::from(vec![Span::styled(
            first_prefix.to_string(),
            prefix_style,
        )]));
    }
}

pub(crate) fn diff_line_style(line: &str) -> Style {
    let color = if line.starts_with('+') {
        DIFF_ADD
    } else if line.starts_with('-') {
        DIFF_DELETE
    } else if line.starts_with("@@") || line.starts_with('\\') {
        DIFF_META
    } else {
        TEXT_PRIMARY
    };
    Style::default().fg(color)
}

/// `tuika-mermaid` with a transcript-width guard.
///
/// mmdflux lays a diagram out at its natural size and ignores the width tuika
/// offers, so a wide flowchart in a narrow terminal would be painted clipped —
/// half a box, no way to read the rest. Falling back to `None` there hands the
/// block back to tuika's themed code block, which keeps the Mermaid source
/// itself on screen.
struct MermaidFencedBlocks;

impl tuika::components::markdown::FencedBlockRenderer for MermaidFencedBlocks {
    fn render(
        &self,
        language: &str,
        source: &str,
        width: u16,
        theme: &tuika::style::Theme,
    ) -> Option<Vec<Line<'static>>> {
        let rendered =
            tuika_mermaid::MermaidRenderer::new().render(language, source, width, theme)?;
        rendered
            .iter()
            .all(|line| line_width(line) <= width as usize)
            .then_some(rendered)
    }
}

/// Render assistant markdown to transcript lines via tuika's streaming markdown
/// renderer + tree-sitter code highlighting (see the `tuika` and
/// `tuika-codeformatters` crates).
///
/// ` ```mermaid ` fences go through [`MermaidFencedBlocks`], which paints them
/// as Unicode cell diagrams; unsupported, malformed, or too-wide diagrams keep
/// the ordinary code-block fallback so the source stays readable.
///
/// The tuika renderer word-wraps prose and lays code/tables out to a width; we
/// render at `inner_width` minus the header prefix, then prepend the header to
/// the first row and matching indentation to the continuation rows so the
/// author label (`agent › `) still owns the left gutter.
///
/// Returns [`BufferLink`]s for every hyperlink run in the rendered block
/// (labeled `[text](url)` and bare URLs), with coordinates relative to the
/// lines appended to `lines` — including the gutter offset — so the host can
/// [`hyperlink::apply_buffer_links`] after painting.
pub(crate) fn append_markdown_lines<'a>(
    lines: &mut Vec<Line<'a>>,
    first_prefix: &str,
    prefix_style: Style,
    text: &str,
    inner_width: usize,
) -> Vec<BufferLink> {
    let prefix_cols = first_prefix.chars().count();
    let width = inner_width.saturating_sub(prefix_cols).max(1) as u16;
    let theme = super::fullscreen::yolop_theme();
    let mut sheet = tuika::StyleSheet::from_theme(&theme);
    // Leave underlining to the terminal's native OSC 8 hover/modifier state.
    // Keeping it permanently underlined masks Ghostty's clickability feedback.
    sheet.link = tuika::style::StyleBundle::new().fg(theme.code.link);
    let highlighter = tuika_codeformatters::TreeSitterHighlighter::new();
    let mermaid = MermaidFencedBlocks;
    let (rendered, md_links) = tuika::components::markdown::to_linked_lines_with_renderer(
        text,
        width,
        &theme,
        &sheet,
        tuika::highlight::CodeHighlighter::With(&highlighter),
        &mermaid,
    );

    let base_line = lines.len() as u16;
    let gutter = prefix_cols as u16;
    let continuation = " ".repeat(prefix_cols);
    let mut first = true;
    if rendered.is_empty() {
        // Preserve the header row even when the message body is empty.
        push_markdown_line(
            lines,
            first_prefix,
            &continuation,
            prefix_style,
            &mut first,
            vec![],
        );
        return Vec::new();
    }
    for line in rendered {
        push_markdown_line(
            lines,
            first_prefix,
            &continuation,
            prefix_style,
            &mut first,
            line.spans,
        );
    }
    md_links
        .into_iter()
        .map(|mut link| {
            link.line = link.line.saturating_add(base_line);
            link.start_col = link.start_col.saturating_add(gutter);
            link.end_col = link.end_col.saturating_add(gutter);
            link
        })
        .collect()
}

pub(crate) fn push_markdown_line<'a>(
    lines: &mut Vec<Line<'a>>,
    first_prefix: &str,
    continuation: &str,
    prefix_style: Style,
    first: &mut bool,
    mut spans: Vec<Span<'a>>,
) {
    let prefix = if *first { first_prefix } else { continuation };
    let mut line_spans = vec![Span::styled(prefix.to_string(), prefix_style)];
    line_spans.append(&mut spans);
    lines.push(Line::from(line_spans));
    *first = false;
}

/// Concatenate spans' plain text. Only used by rendering tests.
#[cfg(test)]
pub(crate) fn spans_plain_text(spans: &[Span]) -> String {
    spans.iter().map(|span| span.content.as_ref()).collect()
}

pub(crate) fn digit_index(ch: char, len: usize) -> Option<usize> {
    let digit = ch.to_digit(10)? as usize;
    if digit == 0 || digit > len {
        None
    } else {
        Some(digit - 1)
    }
}

pub(crate) fn model_index_for_label(label: &str, options: &[ModelOption]) -> usize {
    options
        .iter()
        .position(|option| option.spec.as_deref() == Some(label))
        .unwrap_or(0)
}

pub(crate) fn custom_model_option() -> ModelOption {
    ModelOption {
        spec: None,
        label: "Custom...".to_string(),
        hint: "paste a model id".to_string(),
    }
}

/// Convert models discovered from a provider's models API (already enriched
/// with core profile names/descriptions) into picker options, keeping the
/// trailing "Custom..." escape hatch.
pub(crate) fn model_options_from_discovered(
    _provider: &str,
    models: Vec<crate::capabilities::model_discovery::DiscoveredProviderModel>,
    recommended_count: usize,
) -> ModelPickerCatalog {
    /// OpenRouter descriptions run to paragraphs; the hint shares one row
    /// with the model id, so keep it short.
    const MAX_HINT_CHARS: usize = 72;

    let mut options: Vec<ModelOption> = models
        .into_iter()
        .map(|model| {
            let mut hint = match (model.display_name, model.description) {
                (Some(name), Some(description)) => format!("{name} · {description}"),
                (Some(name), None) => name,
                (None, Some(description)) => description,
                (None, None) => String::new(),
            };
            if hint.chars().count() > MAX_HINT_CHARS {
                hint = hint.chars().take(MAX_HINT_CHARS - 1).collect::<String>() + "…";
            }
            ModelOption {
                spec: Some(model.model_id.clone()),
                label: model.model_id,
                hint,
            }
        })
        .collect();
    options.push(custom_model_option());
    ModelPickerCatalog {
        recommended_count,
        options,
    }
}

/// Discovered model lists can be hundreds of entries (OpenRouter), far more
/// than the setup panel can show. Window the list around the selection.
pub(crate) const MAX_VISIBLE_MODEL_ROWS: usize = 8;

pub(crate) fn model_window(selected: usize, total: usize, max_rows: usize) -> (usize, usize) {
    if total <= max_rows {
        return (0, total);
    }
    let start = selected.saturating_sub(max_rows / 2).min(total - max_rows);
    (start, start + max_rows)
}

pub(crate) fn inset_x(area: Rect, pad: u16) -> Rect {
    let total = pad.saturating_mul(2);
    if area.width <= total {
        return area;
    }
    Rect {
        x: area.x.saturating_add(pad),
        width: area.width.saturating_sub(total),
        ..area
    }
}

pub(crate) fn line_width(line: &Line) -> usize {
    line.spans
        .iter()
        .map(|span| span.content.chars().count())
        .sum()
}

pub(crate) fn separator_line(mut title: Line<'static>, width: u16, style: Style) -> Line<'static> {
    let fill_width = (width as usize).saturating_sub(line_width(&title));
    title
        .spans
        .push(Span::styled("─".repeat(fill_width), style));
    title
}

pub(crate) fn draw_separator(
    f: &mut ratatui::Frame,
    area: Rect,
    title: Line<'static>,
    style: Style,
) {
    if area.height == 0 {
        return;
    }
    f.render_widget(
        Paragraph::new(separator_line(title, area.width, style)),
        area,
    );
}

pub(crate) fn draw_input(f: &mut ratatui::Frame, area: Rect, app: &mut App) {
    let area = inset_x(area, 0);
    let prompt_width = area.width.min(2);
    let prompt_area = Rect {
        width: prompt_width,
        ..area
    };
    let input_area = Rect {
        x: area.x.saturating_add(prompt_width),
        width: area.width.saturating_sub(prompt_width),
        ..area
    };
    f.render_widget(
        Paragraph::new(Span::styled(
            "> ",
            Style::default()
                .fg(ACCENT_BLUE)
                .add_modifier(Modifier::BOLD),
        )),
        prompt_area,
    );
    // Render the shared composer through tuika's TextInput view — the same
    // component and word-wrap the full-screen renderer uses, so the two modes
    // stay identical.
    let theme = crate::tui::fullscreen::yolop_theme();
    let view = tuika::element(
        tuika::components::TextInput::new(&app.composer).style(Style::default().fg(TEXT_PRIMARY)),
    );
    tuika::paint(f.buffer_mut(), input_area, &theme, view.as_ref(), &[]);
    draw_input_cursor(f, input_area, app);
}

pub(crate) fn draw_input_cursor(f: &mut ratatui::Frame, area: Rect, app: &App) {
    if app.setup.is_some() {
        return;
    }
    if area.width == 0 || area.height == 0 {
        return;
    }
    // `cursor_screen` derives the same scroll-to-cursor offset the TextInput
    // rendered with, and clamps into `area`.
    let (x, y) = app.composer.cursor_screen(area);
    f.set_cursor_position((x, y));
}

pub(crate) fn message_separator_title(state: &ViewState) -> Line<'static> {
    if let Some(activity) = state.presentation.activity_text() {
        return thinking_title(
            state.busy_frame,
            activity,
            state.presentation.turn_elapsed_secs,
            state.presentation.queued_messages,
        );
    }
    Line::from(vec![
        Span::styled("─── ", Style::default().fg(ACCENT_BLUE)),
        Span::styled(
            format!("(Enter to send, {} for newline) ", newline_shortcut_hint()),
            Style::default().fg(TEXT_MUTED),
        ),
    ])
}

pub(crate) fn newline_shortcut_hint() -> &'static str {
    "Shift-Enter"
}

pub(crate) fn thinking_title(
    frame: u64,
    activity: &str,
    elapsed_secs: Option<u64>,
    queued_messages: usize,
) -> Line<'static> {
    const SPINNER: [&str; 4] = ["-", "\\", "|", "/"];
    let spinner = SPINNER[((frame / 2) as usize) % SPINNER.len()];
    let text = format!("{activity}...");
    let text_style = Style::default().fg(TEXT_MUTED).add_modifier(Modifier::BOLD);
    let mut spans = vec![
        Span::styled("─── ", Style::default().fg(ACCENT_BLUE)),
        Span::styled(spinner.to_string(), Style::default().fg(ACCENT_GOLD)),
        Span::raw(" "),
        Span::styled(text, text_style),
    ];
    // Live elapsed timer, like Codex's working indicator.
    if let Some(secs) = elapsed_secs {
        spans.push(Span::styled(
            format!(" {}", format_elapsed(secs)),
            Style::default().fg(TEXT_DIM),
        ));
    }
    let queue_hint = if queued_messages == 0 {
        " (Enter to queue · Esc twice to cancel) ".to_string()
    } else {
        format!(" ({queued_messages} queued · Enter to queue · Esc twice to cancel) ")
    };
    spans.push(Span::styled(queue_hint, Style::default().fg(TEXT_DIM)));
    Line::from(spans)
}

/// Compact wall-clock formatting for the busy timer: `8s`, `1m03s`, `1h02m`.
pub(crate) fn format_elapsed(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
    }
}

pub(crate) fn draw_message_separator(f: &mut ratatui::Frame, area: Rect, state: &ViewState) {
    draw_separator(
        f,
        area,
        message_separator_title(state),
        Style::default().fg(ACCENT_BLUE),
    );
}

pub(crate) fn draw_status_separator(f: &mut ratatui::Frame, area: Rect) {
    draw_separator(f, area, Line::from(""), Style::default().fg(ACCENT_GOLD));
}

pub(crate) fn session_status_lines(state: &ViewState) -> Vec<Line<'static>> {
    state
        .presentation
        .status_lines()
        .iter()
        .map(status_line)
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct StatusHit {
    pub row: u16,
    pub start_col: u16,
    pub end_col: u16,
    pub action: StatusAction,
}

pub(crate) struct FullscreenStatusLayout {
    pub lines: Vec<Line<'static>>,
    pub hits: Vec<StatusHit>,
}

pub(crate) fn fullscreen_status_layout(state: &ViewState, width: u16) -> FullscreenStatusLayout {
    if state.presentation.status_layout == StatusLayout::Compact {
        return linear_status_layout(&state.presentation.status_lines(), width);
    }
    section_status_layout(&state.presentation.expanded_status_sections(), width)
}

pub(crate) fn draw_session_status(f: &mut ratatui::Frame, area: Rect, state: &ViewState) {
    f.render_widget(Paragraph::new(session_status_lines(state)), area);
}

fn linear_status_layout(lines: &[StatusLine], width: u16) -> FullscreenStatusLayout {
    let mut rendered = Vec::with_capacity(lines.len());
    let mut hits = Vec::new();
    for (row, line) in lines.iter().enumerate() {
        let mut spans = vec![Span::styled(" ", Style::default().fg(TEXT_MUTED))];
        let mut col: u16 = 1;
        for (index, field) in line.fields.iter().enumerate() {
            if index > 0 {
                spans.push(Span::styled("  ·  ", Style::default().fg(TEXT_DIM)));
                col = col.saturating_add(5);
            }
            let start = col;
            if let Some(label) = field.label {
                let label = format!("{label} ");
                col = col.saturating_add(str_cols(&label));
                spans.push(Span::styled(label, Style::default().fg(TEXT_DIM)));
            }
            col = col.saturating_add(str_cols(&field.value));
            spans.push(Span::styled(
                field.value.clone(),
                Style::default().fg(TEXT_MUTED),
            ));
            if let Some(action) = field.action
                && start < width
            {
                hits.push(StatusHit {
                    row: row as u16,
                    start_col: start,
                    end_col: col.min(width),
                    action,
                });
            }
        }
        rendered.push(Line::from(spans));
    }
    FullscreenStatusLayout {
        lines: rendered,
        hits,
    }
}

struct StatusCell {
    lines: Vec<Line<'static>>,
    hits: Vec<StatusHit>,
}

fn section_status_layout(sections: &[StatusSection], width: u16) -> FullscreenStatusLayout {
    if width == 0 {
        return FullscreenStatusLayout {
            lines: Vec::new(),
            hits: Vec::new(),
        };
    }
    let column_count = if width >= 96 { 3 } else { 2 }.min(sections.len()).max(1);
    let mut lines = Vec::new();
    let mut hits = Vec::new();
    for group in sections.chunks(column_count) {
        let group_columns = group.len();
        let separators = group_columns.saturating_sub(1) as u16;
        let columns_width = width.saturating_sub(separators);
        let base_width = columns_width / group_columns as u16;
        let extra = columns_width % group_columns as u16;
        let widths = (0..group_columns)
            .map(|index| base_width + u16::from((index as u16) < extra))
            .collect::<Vec<_>>();
        let cells = group
            .iter()
            .zip(widths.iter().copied())
            .map(|(section, cell_width)| status_cell(section, cell_width))
            .collect::<Vec<_>>();
        let height = cells.iter().map(|cell| cell.lines.len()).max().unwrap_or(0);
        let row_offset = lines.len() as u16;
        for row in 0..height {
            let mut spans = Vec::new();
            for (column, &cell_width) in widths.iter().enumerate() {
                if let Some(line) = cells.get(column).and_then(|cell| cell.lines.get(row)) {
                    spans.extend(line.spans.clone());
                    let line_width = line
                        .spans
                        .iter()
                        .map(|span| str_cols(span.content.as_ref()))
                        .fold(0, u16::saturating_add);
                    if line_width < cell_width {
                        spans.push(Span::raw(" ".repeat((cell_width - line_width) as usize)));
                    }
                } else {
                    spans.push(Span::raw(" ".repeat(cell_width as usize)));
                }
                if column + 1 < group_columns {
                    spans.push(Span::styled("│", Style::default().fg(TEXT_DIM)));
                }
            }
            lines.push(Line::from(spans));
        }

        let mut column_offset: u16 = 0;
        for (column, cell) in cells.iter().enumerate() {
            hits.extend(cell.hits.iter().map(|hit| StatusHit {
                row: row_offset.saturating_add(hit.row),
                start_col: column_offset.saturating_add(hit.start_col),
                end_col: column_offset.saturating_add(hit.end_col),
                action: hit.action,
            }));
            column_offset = column_offset
                .saturating_add(widths[column])
                .saturating_add(1);
        }
    }

    FullscreenStatusLayout { lines, hits }
}

fn status_cell(section: &StatusSection, width: u16) -> StatusCell {
    let mut lines = Vec::with_capacity(section.fields.len() + 1);
    let mut hits = Vec::new();
    let title_text = fit_status_text(section.title, width.saturating_sub(2));
    let title = format!(" {title_text} ");
    let title_width = str_cols(&title).min(width);
    lines.push(Line::from(vec![
        Span::styled(title, Style::default().fg(ACCENT_GOLD)),
        Span::styled(
            "─".repeat(width.saturating_sub(title_width) as usize),
            Style::default().fg(TEXT_DIM),
        ),
    ]));
    for (row, field) in section.fields.iter().enumerate() {
        let label = field
            .label
            .map(|label| format!("{label} "))
            .unwrap_or_default();
        let prefix_width = 1u16.saturating_add(str_cols(&label));
        let value_width = width.saturating_sub(prefix_width);
        let value = fit_status_text(&field.value, value_width);
        let end_col = prefix_width.saturating_add(str_cols(&value)).min(width);
        let mut spans = vec![Span::raw(" ")];
        if !label.is_empty() {
            spans.push(Span::styled(label, Style::default().fg(TEXT_DIM)));
        }
        spans.push(Span::styled(value, Style::default().fg(TEXT_MUTED)));
        lines.push(Line::from(spans));
        if let Some(action) = field.action
            && end_col > 1
        {
            hits.push(StatusHit {
                row: row as u16 + 1,
                start_col: 1,
                end_col,
                action,
            });
        }
    }
    StatusCell { lines, hits }
}

fn fit_status_text(text: &str, width: u16) -> String {
    if width == 0 {
        return String::new();
    }
    if str_cols(text) <= width {
        return text.to_string();
    }
    if width == 1 {
        return "…".to_string();
    }
    let wrapped = tuika::components::text::wrap_lines(&[Line::from(text.to_string())], width - 1);
    let prefix = wrapped
        .first()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .unwrap_or_default();
    format!("{}…", prefix.trim_end())
}

fn status_line(line: &StatusLine) -> Line<'static> {
    let mut spans = vec![Span::styled(" ", Style::default().fg(TEXT_MUTED))];
    let mut first = true;
    for field in &line.fields {
        if !first {
            spans.push(Span::styled("  ·  ", Style::default().fg(TEXT_DIM)));
        }
        first = false;
        if let Some(label) = field.label {
            spans.push(Span::styled(
                format!("{label} "),
                Style::default().fg(TEXT_DIM),
            ));
        }
        spans.push(Span::styled(
            field.value.clone(),
            Style::default().fg(TEXT_MUTED),
        ));
    }
    spans.push(Span::styled(" ", Style::default().fg(TEXT_MUTED)));
    Line::from(spans)
}

//! All terminal rendering: the inline chrome (stream preview, input, status),
//! the recent-transcript viewport, the setup overlay, and the markdown/diff
//! line formatting helpers. Pure rendering over `&App` / `&ViewState`; state
//! mutation lives elsewhere.

use super::*;

// Presentation for the transcript view-model types defined in
// `crate::transcript`. The data lives there; colors/labels live here so the
// translation layer stays free of `ratatui`.
impl Author {
    pub fn label(&self) -> &'static str {
        match self {
            Author::User => "you",
            Author::Assistant => "agent",
            Author::Narration => "note",
            Author::Tool => "tool",
            Author::ToolDetail => "",
            Author::Diff => "diff",
            Author::System => "system",
        }
    }
    pub fn color(&self) -> Color {
        match self {
            Author::User => theme().accent_blue,
            Author::Assistant => theme().accent_gold,
            Author::Narration => theme().text_muted,
            Author::Tool => theme().text_muted,
            Author::ToolDetail => theme().text_muted,
            Author::Diff => theme().accent_blue,
            Author::System => theme().text_dim,
        }
    }
}

impl StreamKind {
    fn label(self) -> &'static str {
        match self {
            StreamKind::Assistant => "agent",
            StreamKind::Tool => "tool",
        }
    }

    fn color(self) -> Color {
        match self {
            StreamKind::Assistant => theme().accent_gold,
            StreamKind::Tool => theme().text_muted,
        }
    }
}

pub(crate) fn draw(f: &mut ratatui::Frame, app: &mut App) {
    let area = f.area();
    // Match `draw_input`: the `> ` prompt consumes two columns.
    let input_width = area.width.saturating_sub(2);
    let desired_input_height = app.input_height(input_width);
    let state = app.view_state();
    let layout = TuiLayout::new(
        area,
        desired_input_height,
        state.status_layout,
        chrome_preview_visible(&state),
    );

    // Chrome renders the non-input rows; we then layer the input field
    // on top into the chrome-reserved input slot.
    clear_transcript_viewport(f, layout.transcript);
    draw_recent_transcript(f, layout.transcript, app);
    draw_chrome_layout(f, layout.chrome, &state);
    draw_input(f, layout.chrome.input, app);
    draw_setup_overlay(f, area, app);
    draw_background_panel(f, area, app);
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
        status_layout: StatusLayout,
        preview_visible: bool,
    ) -> Self {
        let (chrome_height, input_height) = chrome_dimensions(
            frame.height,
            desired_input_height,
            status_layout,
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
            chrome: ChromeLayout::new(chrome_area, input_height, status_layout, preview_visible),
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
        status_layout: StatusLayout,
        preview_visible: bool,
    ) -> Self {
        let preview_height = u16::from(input_height == 1 && preview_visible);
        let status_height = u16::from(input_height < 3);
        let status_rows = status_layout.row_count();
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

pub(crate) fn chrome_height(
    input_height: u16,
    status_layout: StatusLayout,
    preview_visible: bool,
) -> u16 {
    let preview_height = u16::from(input_height == 1 && preview_visible);
    let status_separator_height = u16::from(input_height < 3);
    let fixed_rows = preview_height
        .saturating_add(1)
        .saturating_add(status_separator_height)
        .saturating_add(status_layout.row_count());
    input_height.saturating_add(fixed_rows)
}

pub(crate) fn chrome_dimensions(
    frame_height: u16,
    desired_input_height: u16,
    status_layout: StatusLayout,
    preview_visible: bool,
) -> (u16, u16) {
    if frame_height == 0 {
        return (0, 0);
    }

    let desired_input_height = desired_input_height.clamp(1, MAX_INPUT_HEIGHT);
    let chrome_height =
        chrome_height(desired_input_height, status_layout, preview_visible).min(frame_height);
    let mut input_height = desired_input_height.min(chrome_height);
    while input_height > 1
        && input_height.saturating_add(chrome_fixed_rows(
            input_height,
            status_layout,
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
    status_layout: StatusLayout,
    preview_visible: bool,
) -> TuiLayout {
    TuiLayout::new(frame, desired_input_height, status_layout, preview_visible)
}

fn chrome_fixed_rows(input_height: u16, status_layout: StatusLayout, preview_visible: bool) -> u16 {
    u16::from(input_height == 1 && preview_visible)
        .saturating_add(1)
        .saturating_add(u16::from(input_height < 3))
        .saturating_add(status_layout.row_count())
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

    // Already-flushed lines live in native scrollback; mirroring them here
    // makes prompts and outputs appear twice until the next redraw.
    let pending_start = app.printed_lines.min(app.lines.len());
    for chat in app.lines[pending_start..]
        .iter()
        .rev()
        .filter(|line| !matches!(line.author, Author::System))
        .take(RECENT_TRANSCRIPT_SOURCE_LINES)
    {
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
        state.status_layout,
        chrome_preview_visible(state),
    );
    draw_chrome_layout(f, layout, state);
    layout.input
}

pub(crate) fn chrome_preview_visible(state: &ViewState) -> bool {
    state.stream_preview.is_some() || !state.command_suggestions.is_empty()
}

pub(crate) fn draw_chrome_layout(f: &mut ratatui::Frame, layout: ChromeLayout, state: &ViewState) {
    if state.command_suggestions.is_empty() {
        draw_stream_preview(f, layout.preview, state);
    } else {
        draw_suggestions(f, layout.preview, &state.command_suggestions);
    }
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
    f.render_widget(Clear, panel);
    let block = Block::default().borders(Borders::ALL).style(
        Style::default()
            .bg(theme().panel_bg)
            .fg(theme().text_primary),
    );
    f.render_widget(block, panel);
    let inner = Rect {
        x: panel.x.saturating_add(2),
        y: panel.y.saturating_add(1),
        width: panel.width.saturating_sub(4),
        height: panel.height.saturating_sub(2),
    };
    let (lines, cursor) = setup_overlay_content(app);
    f.render_widget(
        Paragraph::new(lines).style(Style::default().bg(theme().panel_bg)),
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

/// Read-only background-tasks panel overlay (toggled with Ctrl+B). Reuses the
/// registry's `/background` rendering for the body, scrolled by the stored
/// offset, inside the same centered panel as the setup overlay.
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
    f.render_widget(Clear, panel);
    let block = Block::default().borders(Borders::ALL).style(
        Style::default()
            .bg(theme().panel_bg)
            .fg(theme().text_primary),
    );
    f.render_widget(block, panel);
    let inner = Rect {
        x: panel.x.saturating_add(2),
        y: panel.y.saturating_add(1),
        width: panel.width.saturating_sub(4),
        height: panel.height.saturating_sub(2),
    };
    let body = app.background.render_task_list();
    let texts = background_panel_lines(&body, offset, inner.height as usize);
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(texts.len());
    for (i, text) in texts.into_iter().enumerate() {
        if i == 0 {
            lines.push(Line::from(Span::styled(
                text,
                Style::default()
                    .fg(theme().diff_meta)
                    .add_modifier(Modifier::BOLD),
            )));
        } else {
            lines.push(Line::raw(text));
        }
    }
    f.render_widget(
        Paragraph::new(lines).style(Style::default().bg(theme().panel_bg)),
        inner,
    );
}

/// Lines for the background panel: a fixed header followed by the scrolled body
/// (the `/background` text), clipped to `height` rows. Pure for testability.
pub(crate) fn background_panel_lines(body: &str, offset: usize, height: usize) -> Vec<String> {
    if height == 0 {
        return Vec::new();
    }
    let mut out = vec!["Background tasks — ↑/↓ scroll · Ctrl+B/Esc to close".to_string()];
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
                        .fg(theme().accent_blue)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(value.clone(), Style::default().fg(theme().text_primary)),
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
                        .fg(theme().accent_blue)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(masked, Style::default().fg(theme().text_primary)),
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
                            .fg(theme().accent_blue)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(value.clone(), Style::default().fg(theme().text_primary)),
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

pub(crate) fn setup_title(text: &str) -> Line<'static> {
    Line::from(Span::styled(
        text.to_string(),
        Style::default()
            .fg(theme().text_primary)
            .bg(theme().panel_bg)
            .add_modifier(Modifier::BOLD),
    ))
}

pub(crate) fn setup_hint(text: &str) -> Line<'static> {
    Line::from(Span::styled(
        text.to_string(),
        Style::default().fg(theme().text_muted).bg(theme().panel_bg),
    ))
}

pub(crate) fn setup_divider(text: &str) -> Line<'static> {
    Line::from(Span::styled(
        text.to_string(),
        Style::default()
            .fg(theme().accent_blue)
            .bg(theme().panel_bg),
    ))
}

pub(crate) fn setup_footer(text: &str) -> Line<'static> {
    Line::from(Span::styled(
        text.to_string(),
        Style::default().fg(theme().text_muted).bg(theme().panel_bg),
    ))
}

pub(crate) fn setup_row(selected: bool, index: usize, label: &str, hint: &str) -> Line<'static> {
    let marker = if selected { "›" } else { " " };
    let marker_style = if selected {
        Style::default()
            .fg(theme().accent_blue)
            .bg(theme().panel_bg)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme().text_dim).bg(theme().panel_bg)
    };
    let label_style = if selected {
        Style::default()
            .fg(theme().accent_cyan)
            .bg(theme().panel_bg)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
            .fg(theme().text_primary)
            .bg(theme().panel_bg)
    };
    Line::from(vec![
        Span::styled(format!("{marker} "), marker_style),
        Span::styled(
            format!("{index}. "),
            Style::default().fg(theme().text_muted).bg(theme().panel_bg),
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
            Style::default().fg(theme().text_muted).bg(theme().panel_bg),
        ),
    ])
}

pub(crate) fn push_setup_error(lines: &mut Vec<Line<'static>>, error: Option<&str>) {
    if let Some(error) = error {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!("error: {error}"),
            Style::default()
                .fg(theme().accent_warn)
                .bg(theme().panel_bg),
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
                .fg(theme().accent_blue)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            truncate_end_chars(&body, max_body),
            Style::default().fg(theme().text_muted),
        ),
    ])
}

pub(crate) fn draw_stream_preview(f: &mut ratatui::Frame, area: Rect, state: &ViewState) {
    if area.height == 0 {
        return;
    }
    let Some(preview) = state.stream_preview.as_ref() else {
        return;
    };
    let inner_width = area.width as usize;
    if inner_width == 0 {
        return;
    }
    // Show the most recent line of the accumulated stream so the eye
    // tracks the live tail rather than the start of a long response.
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
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                prefix,
                Style::default()
                    .fg(preview.kind.color())
                    .add_modifier(Modifier::DIM),
            ),
            Span::styled(truncated, Style::default().fg(theme().text_muted)),
        ])),
        area,
    );
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
    )
}

pub(crate) fn append_chat_lines<'a>(
    lines: &mut Vec<Line<'a>>,
    chat: &ChatLine,
    inner_width: usize,
) {
    if matches!(chat.author, Author::ToolDetail) {
        append_wrapped_plain(
            lines,
            "           ",
            Style::default().fg(theme().text_muted),
            &chat.text,
            inner_width,
        );
        return;
    }

    let header_text = format!("{} › ", chat.author.label());
    let header_style = Style::default()
        .fg(chat.author.color())
        .add_modifier(Modifier::BOLD);
    if matches!(chat.author, Author::Assistant) {
        append_markdown_lines(lines, &header_text, header_style, &chat.text, inner_width);
    } else if matches!(chat.author, Author::Narration) {
        append_wrapped_styled(
            lines,
            &header_text,
            header_style,
            &chat.text,
            inner_width,
            Style::default().fg(theme().text_muted),
        );
    } else if matches!(chat.author, Author::Diff) {
        append_wrapped_diff(lines, &header_text, header_style, &chat.text, inner_width);
    } else {
        append_wrapped_plain(lines, &header_text, header_style, &chat.text, inner_width);
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
        theme().diff_add
    } else if line.starts_with('-') {
        theme().diff_delete
    } else if line.starts_with("@@") || line.starts_with('\\') {
        theme().diff_meta
    } else {
        theme().text_primary
    };
    Style::default().fg(color)
}

pub(crate) fn append_markdown_lines<'a>(
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
    let mut first = true;
    let mut in_code = false;
    let source_lines = text.lines().collect::<Vec<_>>();
    let mut index = 0;

    while index < source_lines.len() {
        let raw = source_lines[index];
        let trimmed = raw.trim_end();

        if let Some(lang) = trimmed.trim_start().strip_prefix("```") {
            in_code = !in_code;
            let code_lang = lang.trim();
            let label = if in_code {
                if code_lang.is_empty() {
                    "code".to_string()
                } else {
                    format!("code: {code_lang}")
                }
            } else {
                String::new()
            };
            push_markdown_line(
                lines,
                first_prefix,
                &continuation,
                prefix_style,
                &mut first,
                vec![Span::styled(
                    label,
                    Style::default()
                        .fg(theme().text_dim)
                        .add_modifier(Modifier::ITALIC),
                )],
            );
            index += 1;
            continue;
        }

        if !in_code
            && let Some((table, next)) =
                super::markdown_table::try_parse_table_at(&source_lines, index)
            && super::markdown_table::append_markdown_table_lines(
                lines,
                first_prefix,
                prefix_style,
                &table,
                inner_width,
                &mut first,
            )
        {
            index = next;
            continue;
        }

        let content_spans = if in_code {
            markdown_code_spans(trimmed)
        } else {
            markdown_text_spans(trimmed)
        };
        let plain = spans_plain_text(&content_spans);
        let wrapped = textwrap::wrap(
            &plain,
            textwrap::Options::new(wrap_width)
                .break_words(true)
                .word_separator(textwrap::WordSeparator::AsciiSpace),
        );
        if wrapped.is_empty() {
            push_markdown_line(
                lines,
                first_prefix,
                &continuation,
                prefix_style,
                &mut first,
                vec![],
            );
            index += 1;
            continue;
        }
        let mut search_from = 0;
        for piece in wrapped {
            let piece = piece.into_owned();
            let piece_spans =
                styled_piece_for_wrapped_text(&plain, &content_spans, &piece, &mut search_from)
                    .unwrap_or_else(|| vec![Span::raw(piece)]);
            push_markdown_line(
                lines,
                first_prefix,
                &continuation,
                prefix_style,
                &mut first,
                piece_spans,
            );
        }
        index += 1;
    }
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

pub(crate) fn markdown_text_spans(text: &str) -> Vec<Span<'static>> {
    let trimmed = text.trim_start();
    if trimmed.starts_with('#') {
        let heading = trimmed.trim_start_matches('#').trim_start();
        return vec![Span::styled(
            heading.to_string(),
            Style::default()
                .fg(theme().text_primary)
                .add_modifier(Modifier::BOLD),
        )];
    }
    if let Some(rest) = trimmed.strip_prefix("> ") {
        return vec![
            Span::styled("| ", Style::default().fg(theme().accent_blue)),
            Span::styled(rest.to_string(), Style::default().fg(theme().text_muted)),
        ];
    }
    if let Some(rest) = trimmed
        .strip_prefix("- ")
        .or_else(|| trimmed.strip_prefix("* "))
    {
        return vec![
            Span::styled("- ", Style::default().fg(theme().accent_gold)),
            Span::raw(rest.to_string()),
        ];
    }
    if let Some((marker, rest)) = numbered_marker(trimmed) {
        return vec![
            Span::styled(marker, Style::default().fg(theme().accent_gold)),
            Span::raw(rest.to_string()),
        ];
    }
    inline_code_spans(text)
}

pub(crate) fn markdown_code_spans(text: &str) -> Vec<Span<'static>> {
    let mut spans = vec![Span::styled("    ", Style::default().fg(theme().text_dim))];
    spans.extend(simple_code_highlight(text));
    spans
}

pub(crate) fn inline_code_spans(text: &str) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut rest = text;
    let mut code = false;
    while let Some((before, after_tick)) = rest.split_once('`') {
        if !before.is_empty() {
            spans.push(Span::raw(before.to_string()));
        }
        if let Some((inside, after)) = after_tick.split_once('`') {
            spans.push(Span::styled(
                inside.to_string(),
                Style::default()
                    .fg(theme().text_primary)
                    .bg(theme().code_bg),
            ));
            rest = after;
            code = true;
        } else {
            spans.push(Span::raw("`".to_string()));
            rest = after_tick;
            break;
        }
    }
    if !rest.is_empty() {
        spans.push(Span::raw(rest.to_string()));
    }
    if spans.is_empty() || !code {
        vec![Span::raw(text.to_string())]
    } else {
        spans
    }
}

pub(crate) fn simple_code_highlight(text: &str) -> Vec<Span<'static>> {
    let keywords = [
        "async", "await", "const", "enum", "fn", "impl", "let", "match", "pub", "return", "struct",
        "use",
    ];
    let mut spans = Vec::new();
    let mut token = String::new();
    for ch in text.chars() {
        if ch.is_alphanumeric() || ch == '_' {
            token.push(ch);
            continue;
        }
        if !token.is_empty() {
            let style = if keywords.contains(&token.as_str()) {
                Style::default()
                    .fg(theme().accent_gold)
                    .add_modifier(Modifier::BOLD)
            } else if token.chars().all(|c| c.is_ascii_digit()) {
                Style::default().fg(theme().text_muted)
            } else {
                Style::default().fg(theme().accent_blue)
            };
            spans.push(Span::styled(std::mem::take(&mut token), style));
        }
        spans.push(Span::styled(
            ch.to_string(),
            Style::default().fg(theme().text_dim),
        ));
    }
    if !token.is_empty() {
        let style = if keywords.contains(&token.as_str()) {
            Style::default()
                .fg(theme().accent_gold)
                .add_modifier(Modifier::BOLD)
        } else if token.chars().all(|c| c.is_ascii_digit()) {
            Style::default().fg(theme().text_muted)
        } else {
            Style::default().fg(theme().accent_blue)
        };
        spans.push(Span::styled(token, style));
    }
    spans
}

pub(crate) fn numbered_marker(text: &str) -> Option<(String, &str)> {
    let dot = text.find(". ")?;
    if dot == 0 || !text[..dot].chars().all(|ch| ch.is_ascii_digit()) {
        return None;
    }
    Some((text[..dot + 2].to_string(), &text[dot + 2..]))
}

pub(crate) fn spans_plain_text(spans: &[Span]) -> String {
    spans.iter().map(|span| span.content.as_ref()).collect()
}

pub(crate) fn styled_piece_for_wrapped_text(
    plain: &str,
    spans: &[Span],
    piece: &str,
    search_from: &mut usize,
) -> Option<Vec<Span<'static>>> {
    let offset = plain.get(*search_from..)?.find(piece)?;
    let start_byte = search_from.saturating_add(offset);
    let start_char = plain.get(..start_byte)?.chars().count();
    *search_from = start_byte.saturating_add(piece.len());
    Some(spans_for_char_range(
        spans,
        start_char,
        piece.chars().count(),
    ))
}

pub(crate) fn spans_for_char_range(
    spans: &[Span],
    start_char: usize,
    len_chars: usize,
) -> Vec<Span<'static>> {
    if len_chars == 0 {
        return Vec::new();
    }

    let end_char = start_char.saturating_add(len_chars);
    let mut span_start = 0usize;
    let mut out = Vec::new();
    for span in spans {
        let content = span.content.as_ref();
        let span_len = content.chars().count();
        let span_end = span_start.saturating_add(span_len);
        let overlap_start = start_char.max(span_start);
        let overlap_end = end_char.min(span_end);
        if overlap_start < overlap_end {
            let local_start = overlap_start.saturating_sub(span_start);
            let local_len = overlap_end.saturating_sub(overlap_start);
            out.push(Span::styled(
                content
                    .chars()
                    .skip(local_start)
                    .take(local_len)
                    .collect::<String>(),
                span.style,
            ));
        }
        span_start = span_end;
        if span_start >= end_char {
            break;
        }
    }
    out
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
                .fg(theme().accent_blue)
                .add_modifier(Modifier::BOLD),
        )),
        prompt_area,
    );
    app.input.set_block(ratatui::widgets::Block::default());
    f.render_widget(&app.input, input_area);
    draw_input_cursor(f, input_area, app);
}

pub(crate) fn draw_input_cursor(f: &mut ratatui::Frame, area: Rect, app: &App) {
    if app.busy || app.setup.is_some() {
        return;
    }

    let inner_width = area.width;
    let inner_height = area.height;
    if inner_width == 0 || inner_height == 0 {
        return;
    }

    let cursor = app.input.screen_cursor();
    let x = area
        .x
        .saturating_add((cursor.col as u16).min(inner_width.saturating_sub(1)));
    let y = area
        .y
        .saturating_add((cursor.row as u16).min(inner_height.saturating_sub(1)));
    f.set_cursor_position((x, y));
}

pub(crate) fn message_separator_title(state: &ViewState) -> Line<'static> {
    if state.busy {
        return thinking_title(
            state.busy_frame,
            state.turn_activity.as_deref().unwrap_or("thinking"),
        );
    }
    Line::from(vec![
        Span::styled("─── ", Style::default().fg(theme().accent_blue)),
        Span::styled(
            format!("(Enter to send, {} for newline) ", newline_shortcut_hint()),
            Style::default().fg(theme().text_muted),
        ),
    ])
}

pub(crate) fn newline_shortcut_hint() -> &'static str {
    "Shift-Enter"
}

pub(crate) fn thinking_title(frame: u64, activity: &str) -> Line<'static> {
    const SPINNER: [&str; 4] = ["-", "\\", "|", "/"];
    let spinner = SPINNER[((frame / 2) as usize) % SPINNER.len()];
    let text = format!("{activity}...");
    let text_style = Style::default()
        .fg(theme().text_muted)
        .add_modifier(Modifier::BOLD);
    let spans = vec![
        Span::styled("─── ", Style::default().fg(theme().accent_blue)),
        Span::styled(
            spinner.to_string(),
            Style::default().fg(theme().accent_gold),
        ),
        Span::raw(" "),
        Span::styled(text, text_style),
        Span::styled(" (input disabled) ", Style::default().fg(theme().text_dim)),
    ];
    Line::from(spans)
}

pub(crate) fn draw_message_separator(f: &mut ratatui::Frame, area: Rect, state: &ViewState) {
    draw_separator(
        f,
        area,
        message_separator_title(state),
        Style::default().fg(theme().accent_blue),
    );
}

pub(crate) fn draw_status_separator(f: &mut ratatui::Frame, area: Rect) {
    draw_separator(
        f,
        area,
        Line::from(""),
        Style::default().fg(theme().accent_gold),
    );
}

pub(crate) fn draw_session_status(f: &mut ratatui::Frame, area: Rect, state: &ViewState) {
    let lines = match state.status_layout {
        StatusLayout::Compact => compact_status_lines(state),
        StatusLayout::Expanded => expanded_status_lines(state),
    };
    f.render_widget(Paragraph::new(lines), area);
}

#[derive(Clone, Debug)]
pub(crate) struct StatusContribution {
    compact: Vec<StatusField>,
}

#[derive(Clone, Debug)]
pub(crate) struct StatusField {
    label: Option<&'static str>,
    value: String,
}

impl StatusContribution {
    pub(crate) fn new(compact: Vec<StatusField>) -> Self {
        Self { compact }
    }
}

fn compact_status_lines(state: &ViewState) -> Vec<Line<'static>> {
    vec![status_line(
        status_contributions(state)
            .iter()
            .flat_map(|section| section.compact.iter()),
    )]
}

fn expanded_status_lines(state: &ViewState) -> Vec<Line<'static>> {
    let mut counts = vec![
        status_value(message_count_label(state.lines_count)),
        status_field("tokens", token_label(state.session_tokens)),
    ];
    if let Some(bg) = background_label(state.background) {
        counts.push(status_field("bg", bg));
    }

    let provider_model = [
        status_value("[collapse ↑]"),
        status_field("provider", state.provider_name.clone()),
        status_field("model", state.model_id.clone()),
    ];
    let controls = [
        status_field("effort", effort_label(state)),
        status_field("approval", state.approval_mode.clone()),
        status_field("hooks", state.hooks_summary.clone()),
        status_field(
            "goal",
            state.goal_indicator.clone().unwrap_or_else(|| "—".into()),
        ),
    ];
    let session = [status_field("session", state.session_id.to_string())];

    vec![
        status_line(provider_model.iter()),
        status_line(controls.iter()),
        status_line(counts.iter()),
        status_line(session.iter()),
    ]
}

fn status_contributions(state: &ViewState) -> Vec<StatusContribution> {
    let toggle_label = match state.status_layout {
        StatusLayout::Compact => "[expand ↓]",
        StatusLayout::Expanded => "[collapse ↑]",
    };
    vec![
        StatusContribution::new(vec![
            status_value(toggle_label),
            status_value(state.provider_name.clone()),
            status_value(state.model_id.clone()),
        ]),
        StatusContribution::new(vec![
            status_field("effort", effort_label(state)),
            status_field("approval", state.approval_mode.clone()),
        ]),
        StatusContribution::new(vec![status_field(
            "goal",
            state.goal_indicator.clone().unwrap_or_else(|| "—".into()),
        )]),
        StatusContribution::new({
            let mut compact = vec![status_value(message_count_label(state.lines_count))];
            if let Some(bg) = background_label(state.background) {
                compact.push(status_field("bg", bg));
            }
            compact
        }),
    ]
}

/// Status-bar label for background tasks, e.g. `2▸/3` (2 running of 3), or
/// `0▸/2` when none are running. `None` when there are no tasks (segment hidden).
fn background_label(counts: Option<(usize, usize)>) -> Option<String> {
    let (running, total) = counts?;
    if total == 0 {
        return None;
    }
    Some(format!("{running}▸/{total}"))
}

pub(crate) fn status_value(value: impl Into<String>) -> StatusField {
    StatusField {
        label: None,
        value: value.into(),
    }
}

pub(crate) fn status_field(label: &'static str, value: impl Into<String>) -> StatusField {
    StatusField {
        label: Some(label),
        value: value.into(),
    }
}

fn status_line<'a>(fields: impl IntoIterator<Item = &'a StatusField>) -> Line<'static> {
    let mut spans = vec![Span::styled(" ", Style::default().fg(theme().text_muted))];
    let mut first = true;
    for field in fields {
        if !first {
            spans.push(Span::styled("  ·  ", Style::default().fg(theme().text_dim)));
        }
        first = false;
        if let Some(label) = field.label {
            spans.push(Span::styled(
                format!("{label} "),
                Style::default().fg(theme().text_dim),
            ));
        }
        spans.push(Span::styled(
            field.value.clone(),
            Style::default().fg(theme().text_muted),
        ));
    }
    spans.push(Span::styled(" ", Style::default().fg(theme().text_muted)));
    Line::from(spans)
}

fn effort_label(state: &ViewState) -> String {
    state
        .reasoning_effort
        .clone()
        .unwrap_or_else(|| "n/a".to_string())
}

fn token_label(tokens: Option<u64>) -> String {
    tokens
        .map(|tokens| tokens.to_string())
        .unwrap_or_else(|| "n/a".to_string())
}

fn message_count_label(count: usize) -> String {
    format!("{count} msgs")
}

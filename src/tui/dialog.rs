//! Question sheets compose a scrollable prompt and a permanently visible answer.

use super::*;
use tuika::components::{
    Boxed, Scroll, SelectList, SelectState, SelectViewportState, Spacer, Text, TextInput,
};
use tuika::probe::RectProbe;
use tuika::term::terminal::Frame;
use tuika::{Overlay, Padding, element, view};

pub(super) fn draw_question(f: &mut Frame, area: Rect, app: &mut App) {
    let Some(ask) = app.pending_ask.as_ref() else {
        return;
    };
    app.dialog_answer_visible = false;
    if area.width == 0 || area.height == 0 {
        return;
    }
    let theme = fullscreen::yolop_theme();
    let panel = render::setup_panel_rect(area);
    if panel.width < 6 || panel.height < 4 {
        let text = tuika::components::text::wrap_lines(
            &[Line::from("Resize terminal to answer. Esc cancels.")],
            panel.width.max(1),
        );
        tuika::paint(f.buffer_mut(), panel, &theme, &Text::new(text), &[]);
        app.dialog_list_area = Rect::ZERO;
        app.dialog_answer_visible = false;
        return;
    }
    app.dialog_answer_visible = true;
    let width = panel.width.saturating_sub(4).max(1);
    let budget = panel.height.saturating_sub(2);
    let selector = !ask.options.is_empty() && !ask.custom;
    let footer_height = u16::from(budget >= 3);
    let answer_height = if selector {
        (budget / 3).clamp(1, 6)
    } else {
        1
    };
    let prompt_height = budget.saturating_sub(answer_height + footer_height).max(1);
    let model = render::ask_overlay_content(ask);
    let mut prompt = model.question;
    let footer = if width < 35 {
        "Esc/Enter · PgUp/PgDn".to_owned()
    } else {
        format!(
            "PgUp/PgDn question · {}",
            model
                .lines
                .last()
                .map(ToString::to_string)
                .unwrap_or_default()
        )
    };
    if selector && let Some(option) = ask.options.get(ask.selected) {
        prompt.push(Line::from(""));
        prompt.push(Line::from(format!("Selected option: {option}")));
    }
    let prompt = tuika::components::text::wrap_lines(&prompt, width.saturating_sub(1).max(1));
    app.dialog_scroll_metrics = (prompt.len(), usize::from(prompt_height));
    app.dialog_scroll
        .clamp(prompt.len(), usize::from(prompt_height));
    let input_probe = RectProbe::new();
    let list_probe = RectProbe::new();
    let mut selection = SelectState::new();
    selection.select(Some(ask.selected));
    let mut masked = TextInputState::from_text(&"•".repeat(ask.value.text().chars().count()));
    masked.set_cursor(0, model.cursor.1.saturating_sub(2));
    let input_state = if ask.secret {
        &masked
    } else {
        ask.value.as_text_input()
    };
    let answer = if selector {
        let mut viewport = SelectViewportState::new();
        viewport.select(Some(ask.selected));
        let window = viewport.resolve(ask.options.len(), usize::from(answer_height));
        app.dialog_list_start = window.start();
        list_probe.wrap(element(
            SelectList::new(
                ask.options
                    .iter()
                    .map(|option| Line::from(option.clone()))
                    .collect(),
                &selection,
            )
            .viewport(answer_height)
            .visible_window(window),
        ))
    } else {
        input_probe.wrap(element(
            TextInput::new(input_state)
                .style(Style::default().fg(theme.text))
                .placeholder(
                    ask.placeholder
                        .clone()
                        .unwrap_or_else(|| "Your answer…".into()),
                    Style::default().fg(theme.muted),
                ),
        ))
    };
    let content = view! { col {
        fixed(prompt_height) { node(Scroll::new(prompt, &app.dialog_scroll)) }
        fixed(answer_height) { node(answer) }
        fixed(footer_height) { node(Text::raw(&footer)) }
    }};
    let frame = Boxed::new(content)
        .title("Question · answer required")
        .padding(Padding::symmetric(1, 0))
        .background(Style::default().bg(theme.surface).fg(theme.text));
    tuika::paint(
        f.buffer_mut(),
        area,
        &theme,
        &Spacer,
        &[Overlay {
            area: panel,
            view: &frame,
            clear: true,
        }],
    );
    app.dialog_list_area = list_probe.rect();
    let input = input_probe.rect();
    if !selector && input.width > 0 && input.height > 0 {
        f.set_cursor_position(input_state.cursor_screen(input));
    }
}

pub(super) fn draw_setup_text(
    f: &mut Frame,
    area: Rect,
    app: &mut App,
    lines: Vec<Line<'static>>,
    cursor: Option<(usize, usize)>,
) {
    app.dialog_answer_visible = false;
    if area.width == 0 || area.height == 0 {
        return;
    }
    let panel = render::setup_panel_rect(area);
    let theme = fullscreen::yolop_theme();
    let title = lines
        .first()
        .map(ToString::to_string)
        .unwrap_or_else(|| "Setup".into());
    if app.dialog_title != title {
        app.dialog_title = title;
        app.dialog_scroll.jump_to_top();
        app.dialog_manual_scroll = false;
    }
    let tiny = panel.width < 6 || panel.height < 4;
    app.dialog_answer_visible = !tiny;
    app.dialog_list_area = Rect::ZERO;
    let width = if tiny {
        panel.width
    } else {
        panel.width.saturating_sub(5).max(1)
    };
    let body_height = if tiny {
        panel.height
    } else {
        panel.height.saturating_sub(3).max(1)
    };
    let mut wrapped = Vec::new();
    let mut anchor = None;
    let mut mapped_cursor = None;
    for (index, line) in lines.into_iter().enumerate() {
        let start = wrapped.len();
        let text = line.to_string();
        if text.starts_with('›')
            || text.starts_with('▶')
            || line.spans.iter().any(|span| {
                span.style.fg == Some(theme.semantic_color(tuika::style::SemanticRole::Danger))
            })
        {
            anchor = Some(start);
        }
        if let Some((row, column)) = cursor
            && index == row
        {
            let prefix: String = text.chars().take(column).collect();
            let prefix = tuika::components::text::wrap_lines(&[Line::from(prefix)], width.max(1));
            let row = start + prefix.len().saturating_sub(1);
            let column = prefix.last().map(|line| line.width()).unwrap_or(0);
            mapped_cursor = Some((row, column));
            anchor = Some(row);
        }
        wrapped.extend(tuika::components::text::wrap_lines(&[line], width.max(1)));
    }
    app.dialog_scroll_metrics = (wrapped.len(), usize::from(body_height));
    if !app.dialog_manual_scroll
        && let Some(row) = anchor
    {
        let offset = app.dialog_scroll.offset();
        if row < offset {
            app.dialog_scroll.set_offset(row);
        } else if row >= offset + usize::from(body_height) {
            app.dialog_scroll
                .set_offset(row + 1 - usize::from(body_height));
        }
    }
    app.dialog_scroll
        .clamp(wrapped.len(), usize::from(body_height));
    if tiny {
        let visible = wrapped
            .into_iter()
            .skip(app.dialog_scroll.offset())
            .take(usize::from(body_height))
            .collect();
        tuika::paint(f.buffer_mut(), panel, &theme, &Text::new(visible), &[]);
        return;
    }
    let content = view! { col {
        grow(1) { node(Scroll::new(wrapped, &app.dialog_scroll)) }
        fixed(1) { node(Text::raw(if app.ctrl_c_pending_exit() { "Ctrl+C again exits · Esc back" } else { "Esc back · PgUp/PgDn details" })) }
    }};
    let frame = Boxed::new(content)
        .title("Setup")
        .padding(Padding::symmetric(1, 0))
        .background(Style::default().bg(theme.surface).fg(theme.text));
    tuika::paint(
        f.buffer_mut(),
        area,
        &theme,
        &Spacer,
        &[Overlay {
            area: panel,
            view: &frame,
            clear: true,
        }],
    );
    if let Some((row, column)) = mapped_cursor {
        let offset = app.dialog_scroll.offset();
        if row >= offset && row < offset + usize::from(body_height) {
            f.set_cursor_position((
                panel.x + 2 + column.min(width.saturating_sub(1)),
                panel.y + 1 + (row - offset) as u16,
            ));
        }
    }
}

pub(super) fn draw_setup_picker(
    f: &mut Frame,
    area: Rect,
    app: &mut App,
    picker: render::SetupPicker,
) {
    let panel = render::setup_panel_rect(area);
    if panel.width < 6 || panel.height < 4 {
        draw_setup_text(
            f,
            area,
            app,
            vec![Line::from("Resize terminal to choose. Esc goes back.")],
            None,
        );
        return;
    }
    app.dialog_answer_visible = true;
    let theme = fullscreen::yolop_theme();
    let budget = panel.height.saturating_sub(2);
    let footer_height = u16::from(budget >= 3);
    let header_height = (picker.header.len() as u16).min(budget.saturating_sub(footer_height + 1));
    let list_height = budget
        .saturating_sub(header_height + footer_height)
        .min(picker.viewport.unwrap_or(picker.options.len() as u16))
        .max(1);
    let probe = RectProbe::new();
    let mut selection = SelectState::new();
    selection.select(Some(picker.selected));
    let mut viewport = SelectViewportState::new();
    viewport.select(Some(picker.selected));
    let window = viewport.resolve(picker.options.len(), usize::from(list_height));
    app.dialog_list_start = window.start();
    let title = picker
        .header
        .first()
        .map(ToString::to_string)
        .unwrap_or_default();
    if app.dialog_title != title {
        app.dialog_title = title;
        app.dialog_scroll.jump_to_top();
        app.dialog_manual_scroll = false;
    }
    let mut header = picker.header;
    if let Some(selected) = picker.options.get(picker.selected) {
        header.push(Line::from("Selected option details:"));
        header.push(selected.clone());
    }
    let mut error_row = None;
    for line in picker.footer.iter().filter(|line| {
        line.spans.iter().any(|span| {
            span.style.fg == Some(theme.semantic_color(tuika::style::SemanticRole::Danger))
        })
    }) {
        error_row = Some(header.len());
        header.push(line.clone());
    }
    let header = tuika::components::text::wrap_lines(&header, panel.width.saturating_sub(5).max(1));
    app.dialog_scroll_metrics = (header.len(), usize::from(header_height));
    if !app.dialog_manual_scroll && error_row.is_some() {
        app.dialog_scroll
            .jump_to_bottom(header.len(), usize::from(header_height));
    }
    app.dialog_scroll
        .clamp(header.len(), usize::from(header_height));
    let footer = format!(
        "{} · PgUp/PgDn details",
        picker
            .footer
            .last()
            .map(ToString::to_string)
            .unwrap_or_else(|| "Enter confirm · Esc back".into())
    );
    let footer = if app.ctrl_c_pending_exit() {
        "Ctrl+C again exits · Esc back/cancel".into()
    } else if panel.width < 70 {
        "Esc back/cancel · Enter · PgUp/PgDn".into()
    } else {
        footer
    };
    let content = view! { col {
        fixed(header_height) { node(Scroll::new(header, &app.dialog_scroll)) }
        fixed(list_height) { node(probe.wrap(element(SelectList::new(picker.options, &selection).viewport(list_height).visible_window(window)))) }
        fixed(footer_height) { node(Text::raw(&footer)) }
    }};
    let frame = Boxed::new(content)
        .title("Setup")
        .padding(Padding::symmetric(1, 0))
        .background(Style::default().bg(theme.surface).fg(theme.text));
    tuika::paint(
        f.buffer_mut(),
        area,
        &theme,
        &Spacer,
        &[Overlay {
            area: panel,
            view: &frame,
            clear: true,
        }],
    );
    app.dialog_list_area = probe.rect();
}

impl App {
    pub(super) fn scroll_dialog(&mut self, code: KeyCode) {
        self.dialog_manual_scroll = true;
        let (content, viewport) = self.dialog_scroll_metrics;
        let offset = self.dialog_scroll.offset();
        self.dialog_scroll.set_offset(match code {
            KeyCode::PageUp => offset.saturating_sub(viewport.saturating_sub(1).max(1)),
            KeyCode::PageDown => offset
                .saturating_add(viewport.saturating_sub(1).max(1))
                .min(ScrollState::max_offset(content, viewport)),
            _ => offset,
        });
    }

    pub(super) fn question_mouse(&mut self, mouse: MouseEvent) -> bool {
        if self.pending_ask.is_none() {
            return false;
        }
        match mouse.kind {
            MouseEventKind::ScrollUp => self.scroll_dialog(KeyCode::PageUp),
            MouseEventKind::ScrollDown => self.scroll_dialog(KeyCode::PageDown),
            MouseEventKind::Down(MouseButton::Left)
                if self
                    .dialog_list_area
                    .contains((mouse.column, mouse.row).into()) =>
            {
                if let Some(ask) = self.pending_ask.as_mut() {
                    ask.selected = (self.dialog_list_start
                        + usize::from(mouse.row.saturating_sub(self.dialog_list_area.y)))
                    .min(ask.options.len().saturating_sub(1));
                }
            }
            _ => {}
        }
        true
    }
}

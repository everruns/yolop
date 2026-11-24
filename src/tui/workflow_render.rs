//! Compose workflow sheets and the docked changes view with tuika components.

use super::*;
use tuika::components::{Boxed, Image, Scroll, SelectList, Spacer, Text, TextInput};
use tuika::probe::RectProbe;
use tuika::term::terminal::Frame;
use tuika::{Overlay, Padding, view};

pub(super) fn main_area(app: &App, area: Rect) -> Rect {
    if app.workflow.docked(area.width) {
        Rect {
            width: area.width.saturating_sub((area.width / 2).clamp(48, 80)),
            ..area
        }
    } else {
        area
    }
}

pub(super) fn panel_area(app: &App, area: Rect) -> Option<Rect> {
    app.workflow.panel?;
    if app.workflow.docked(area.width) {
        let main = main_area(app, area);
        Some(Rect {
            x: main.right(),
            width: area.width.saturating_sub(main.width),
            ..area
        })
    } else if app.workflow.focused {
        Some(area)
    } else {
        None
    }
}

pub(super) fn draw(f: &mut Frame, area: Rect, app: &mut App) -> bool {
    draw_themed(f, area, app, fullscreen::yolop_theme())
}

pub(super) fn draw_themed(f: &mut Frame, area: Rect, app: &mut App, theme: tuika::Theme) -> bool {
    let Some(panel) = panel_area(app, area) else {
        return false;
    };
    let Some(state) = app.workflow_presentation() else {
        return false;
    };
    app.workflow.actionable = panel.width >= 6 && panel.height >= 4;
    let exclusive = !app.workflow.docked(area.width);
    if panel.width == 0 || panel.height == 0 {
        return exclusive;
    }
    if panel.width < 6 || panel.height < 4 {
        let detail = wrapped(&state.detail, usize::from(panel.width).max(1), theme.text);
        app.workflow.scroll_metrics = (detail.len(), usize::from(panel.height));
        app.workflow
            .scroll
            .clamp(detail.len(), usize::from(panel.height));
        app.workflow.list_area = Rect::ZERO;
        let start = app.workflow.scroll.offset();
        let visible = detail
            .into_iter()
            .skip(start)
            .take(usize::from(panel.height))
            .collect();
        tuika::paint(f.buffer_mut(), panel, &theme, &Text::new(visible), &[]);
        return exclusive;
    }
    let width = usize::from(panel.width.saturating_sub(4)).max(1);
    let budget = panel.height.saturating_sub(2);
    let query_height = u16::from(!state.query_label.is_empty() && budget >= 2);
    let footer = wrapped(
        if budget < 12 {
            "Esc close · PgUp/PgDn detail"
        } else {
            &state.footer
        },
        width,
        theme.muted,
    );
    let footer_height = if budget >= 3 + query_height {
        (footer.len() as u16).min(if budget >= 12 { 3 } else { 1 })
    } else {
        0
    };
    let header = wrapped(
        &format!(
            "{}{}",
            state.subtitle,
            if state.loading { " · updating…" } else { "" },
        ),
        width,
        theme.muted,
    );
    let header_height = if budget >= 6 {
        (header.len() as u16).min(if budget >= 14 { 3 } else { 1 })
    } else {
        0
    };
    // Reserve a body row before assigning optional chrome or the file list.
    let rows_height = if state.rows.is_empty() || budget < 4 {
        0
    } else {
        (budget / 3)
            .clamp(1, 8)
            .min(budget.saturating_sub(header_height + footer_height + query_height + 1))
    };
    let detail_height =
        budget.saturating_sub(header_height + rows_height + footer_height + query_height);
    let detail = if app.workflow.panel == Some(super::workflow::WorkflowPanel::Changes)
        || state.title == "Confirm restore"
    {
        wrapped_diff(&state.detail, width.saturating_sub(1).max(1), theme)
    } else {
        wrapped(&state.detail, width.saturating_sub(1).max(1), theme.text)
    };
    app.workflow.scroll_metrics = (detail.len(), detail_height as usize);
    if app.workflow.panel == Some(super::workflow::WorkflowPanel::Tasks) && app.workflow.task_follow
    {
        app.workflow
            .scroll
            .jump_to_bottom(detail.len(), detail_height as usize);
    } else {
        app.workflow
            .scroll
            .clamp(detail.len(), detail_height as usize);
    }
    app.workflow
        .list_viewport
        .select((!state.rows.is_empty()).then_some(state.selected));
    let window = app
        .workflow
        .list_viewport
        .resolve(state.rows.len(), usize::from(rows_height));
    app.workflow.list_start = window.start();
    let list = SelectList::new(
        state
            .rows
            .iter()
            .map(|row| Line::from(row.clone()))
            .collect(),
        app.workflow.list_viewport.selection(),
    )
    .viewport(rows_height)
    .visible_window(window);
    let list_probe = RectProbe::new();
    let query_probe = RectProbe::new();
    let query = query_probe.wrap(tuika::element(
        TextInput::new(app.workflow.query.as_text_input())
            .style(Style::default().fg(theme.text))
            .placeholder(
                format!("{}…", state.query_label),
                Style::default().fg(theme.muted),
            ),
    ));
    let body = if let Some(image) = app.workflow.image_preview.clone() {
        tuika::element(
            Image::new(image, panel.width.saturating_sub(4), detail_height)
                .in_layer(&app.workflow.image_layer)
                .alt("Image preview: this terminal does not support graphics"),
        )
    } else {
        tuika::element(Scroll::new(detail, &app.workflow.scroll))
    };
    let content = view! {
        col {
            fixed(header_height) { node(Text::new(header)) }
            fixed(query_height) { node(query) }
            fixed(rows_height) { node(list_probe.wrap(tuika::element(list))) }
            grow(1) { node(body) }
            fixed(footer_height) { node(Text::new(footer)) }
        }
    };
    let frame = Boxed::new(content)
        .title(format!("{} · {}", state.title, state.focus_hint))
        .padding(Padding::symmetric(1, 0))
        .background(Style::default().bg(theme.surface).fg(theme.text));
    let overlay = Overlay {
        area: panel,
        view: &frame,
        clear: true,
    };
    tuika::paint(f.buffer_mut(), panel, &theme, &Spacer, &[overlay]);
    app.workflow.list_area = list_probe.rect();
    let query_area = query_probe.rect();
    if query_height > 0 && app.workflow.focused && query_area.width > 0 && query_area.height > 0 {
        f.set_cursor_position(app.workflow.query.as_text_input().cursor_screen(query_area));
    }
    exclusive
}

pub(super) fn draw_approval(f: &mut Frame, area: Rect, app: &mut App) -> bool {
    let Some(request) = app
        .pending_sandbox_approval
        .as_ref()
        .and_then(|pending| pending.request.as_ref())
    else {
        return false;
    };
    if area.width == 0 || area.height == 0 {
        app.approval_body_visible = false;
        return true;
    }
    if area.width < 20 || area.height < 5 {
        app.approval_body_visible = false;
        let theme = fullscreen::yolop_theme();
        tuika::paint(
            f.buffer_mut(),
            area,
            &theme,
            &Text::new(wrapped(
                "Resize to review this action. Esc denies.",
                usize::from(area.width).max(1),
                fullscreen::yolop_theme().text,
            )),
            &[],
        );
        return true;
    }
    app.approval_body_visible = true;
    let theme = fullscreen::yolop_theme();
    let width = usize::from(area.width.saturating_sub(4)).max(1);
    let footer = wrapped(
        "y Allow once · a Allow ALL requests in this scope for session · n / Esc Deny · PgUp/PgDn review",
        width,
        fullscreen::yolop_theme().accent_alt,
    );
    let footer_height = (footer.len() as u16)
        .min(area.height.saturating_sub(3))
        .min(4);
    let detail = wrapped(
        &workflow_presentation::approval_detail(request),
        width.saturating_sub(1).max(1),
        fullscreen::yolop_theme().text,
    );
    let height = area.height.saturating_sub(2 + footer_height) as usize;
    app.approval_scroll_metrics = (detail.len(), height);
    app.approval_scroll.clamp(detail.len(), height);
    let content = view! {col {
        grow(1) {node(Scroll::new(detail,&app.approval_scroll))}
        fixed(footer_height) {node(Text::new(footer))}
    }};
    let frame = Boxed::new(content)
        .title("Permission required")
        .padding(Padding::symmetric(1, 0))
        .background(Style::default().bg(theme.surface).fg(theme.text));
    tuika::paint(
        f.buffer_mut(),
        area,
        &theme,
        &Spacer,
        &[Overlay {
            area,
            view: &frame,
            clear: true,
        }],
    );
    true
}

fn wrapped(text: &str, width: usize, color: Color) -> Vec<Line<'static>> {
    let mut result = Vec::new();
    for raw in text.split('\n') {
        let mut safe = String::with_capacity(raw.len());
        for ch in raw.chars() {
            if ch.is_control() && ch != '\t' {
                safe.extend(ch.escape_default());
            } else {
                safe.push(ch);
            }
        }
        render::append_wrapped_styled(
            &mut result,
            "",
            Style::default(),
            &safe,
            width,
            Style::default().fg(color),
        );
    }
    result
}

fn wrapped_diff(text: &str, width: usize, theme: tuika::Theme) -> Vec<Line<'static>> {
    text.split('\n')
        .flat_map(|line| {
            wrapped(
                line,
                width,
                if line.starts_with('+') {
                    theme.semantic_color(tuika::style::SemanticRole::Success)
                } else if line.starts_with('-') {
                    theme.semantic_color(tuika::style::SemanticRole::Danger)
                } else {
                    theme.text
                },
            )
        })
        .collect()
}

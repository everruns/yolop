//! Compose workflow sheets and the docked changes view with tuika components.

use super::*;
use tuika::components::{Boxed, Image, Scroll, SelectList, Spacer, Text};
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
    let Some(panel) = panel_area(app, area) else {
        return false;
    };
    let Some(state) = app.workflow_presentation() else {
        return false;
    };
    let exclusive = !app.workflow.docked(area.width);
    if panel.width < 4 || panel.height < 3 {
        return exclusive;
    }
    let width = usize::from(panel.width.saturating_sub(4)).max(1);
    let rows_height = if state.rows.is_empty() {
        0
    } else {
        (panel.height.saturating_sub(9) / 3).clamp(1, 8)
    };
    let footer = wrapped(&state.footer, width, TEXT_MUTED);
    let footer_height = (footer.len() as u16)
        .min(4)
        .min(panel.height.saturating_sub(3));
    let header = wrapped(
        &format!(
            "{}{}\n{}",
            state.subtitle,
            if state.loading { " · updating…" } else { "" },
            if !state.query_label.is_empty() {
                format!("{}: {}", state.query_label, state.query)
            } else {
                String::new()
            }
        ),
        width,
        TEXT_MUTED,
    );
    let header_height = (header.len() as u16).min(4);
    let detail_height = panel
        .height
        .saturating_sub(2 + header_height + rows_height + footer_height)
        .max(1);
    let detail = wrapped(&state.detail, width, TEXT_PRIMARY);
    app.workflow.scroll_metrics = (detail.len(), detail_height as usize);
    app.workflow
        .scroll
        .clamp(detail.len(), detail_height as usize);
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
            fixed(rows_height) { node(list_probe.wrap(tuika::element(list))) }
            grow(1) { node(body) }
            fixed(footer_height) { node(Text::new(footer)) }
        }
    };
    let theme = fullscreen::yolop_theme();
    let frame = Boxed::new(content)
        .title(format!(
            "{}{}",
            state.title,
            if app.workflow.focused {
                " · focused"
            } else {
                " · F6 focus"
            }
        ))
        .padding(Padding::symmetric(1, 0))
        .background(Style::default().bg(theme.surface).fg(theme.text));
    let overlay = Overlay {
        area: panel,
        view: &frame,
        clear: true,
    };
    tuika::paint(f.buffer_mut(), panel, &theme, &Spacer, &[overlay]);
    app.workflow.list_area = list_probe.rect();
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
    if area.width < 4 || area.height < 3 {
        return true;
    }
    let theme = fullscreen::yolop_theme();
    let width = usize::from(area.width.saturating_sub(4)).max(1);
    let footer = wrapped(
        "y Allow once · a Allow ALL requests in this scope for session · n / Esc Deny · PgUp/PgDn review",
        width,
        ACCENT_GOLD,
    );
    let footer_height = (footer.len() as u16)
        .min(area.height.saturating_sub(2))
        .min(4);
    let detail = wrapped(
        &workflow_presentation::approval_detail(request),
        width,
        TEXT_PRIMARY,
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
            Style::default().fg(if raw.starts_with('+') {
                DIFF_ADD
            } else if raw.starts_with('-') {
                DIFF_DELETE
            } else {
                color
            }),
        );
    }
    result
}

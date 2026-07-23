//! Live component gallery. Run with `cargo run -p tuika --example gallery`
//! (press `q` or `Esc` to quit).
//!
//! Demonstrates the declarative `view!` DSL, the motion components, the native
//! OSC 9;4 progress indicator, and OSC 8 hyperlinks (the footer URL is
//! clickable in a supporting terminal) — the same building blocks yolop's
//! full-screen renderer uses, with no host application involved.

use std::io;
use std::time::Duration;

use crossterm::event::{self, Event as CtEvent, KeyCode, KeyEventKind};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::{Terminal, TerminalOptions, Viewport};

use tuika::{
    Column, FocusScope, HyperlinkBackend, Loader, ProgressBar, SelectState, Spinner, SpinnerStyle,
    Table, Text, Theme, view,
};

fn build(frame: u64, theme: &Theme) -> tuika::Element {
    let labeled = |style: SpinnerStyle, label: &str| -> tuika::Element {
        view! {
            row(gap = 1) {
                fixed(1) { node(Spinner::new(frame).style(style)) }
                text(label.to_string())
            }
        }
    };
    let animated = tuika::anim::ping_pong(frame, 120);

    view! {
        col(padding = tuika::Padding::all(1), gap = 1,
            background = ratatui::style::Style::default().bg(theme.background)) {
            fixed(5) {
                boxed(title = Line::from(Span::styled(" spinners ", theme.accent_style()))) {
                    col {
                        fixed(1) { node(labeled(SpinnerStyle::Braille, "Braille")) }
                        fixed(1) { node(labeled(SpinnerStyle::Line, "Line")) }
                        fixed(1) { node(labeled(SpinnerStyle::Dots, "Dots")) }
                    }
                }
            }
            fixed(6) {
                boxed(title = Line::from(Span::styled(" progress ", theme.accent_style()))) {
                    col {
                        fixed(1) { node(ProgressBar::determinate(0.25).percent(true)) }
                        fixed(1) { node(ProgressBar::determinate(0.60).percent(true)) }
                        fixed(1) { node(ProgressBar::determinate(animated).percent(true)) }
                        fixed(1) { node(ProgressBar::indeterminate(frame)) }
                    }
                }
            }
            fixed(3) {
                boxed(title = Line::from(Span::styled(" loader ", theme.accent_style())),
                      title_bottom = Line::from(Span::styled(" 3 of 3 ", theme.muted_style()))) {
                    node(Loader::new(frame, "working…").hint("esc to quit"))
                }
            }
            fixed(5) {
                boxed(title = Line::from(Span::styled(" alignment ", theme.accent_style()))) {
                    // Per-line alignment carried straight off each `Line`.
                    node(Text::new(vec![
                        Line::from(Span::styled("left", theme.muted_style())),
                        Line::from(Span::styled("centered", theme.accent_style())).centered(),
                        Line::from(Span::styled("right", theme.muted_style())).right_aligned(),
                    ]))
                }
            }
            fixed(3) {
                // FocusScope drives each pane's border independently of the root
                // context; the right pane also takes an explicit accent color.
                row(gap = 1) {
                    grow(1) {
                        node(FocusScope::focused(view! {
                            boxed(title = " focused ") { text("theme.border_focused") }
                        }))
                    }
                    grow(1) {
                        node(FocusScope::unfocused(view! {
                            boxed(title = " idle ") { text("theme.border") }
                        }))
                    }
                    grow(1) {
                        boxed(title = Line::from(Span::styled(" danger ", theme.accent_style())),
                              border_color = ratatui::style::Color::Red) {
                            text("explicit color")
                        }
                    }
                }
            }
            fixed(5) {
                boxed(title = Line::from(Span::styled(" table ", theme.accent_style()))) {
                    // Columned selectable table: fixed + auto + flex columns,
                    // full-row selection, caret gutter (flex solver owns widths).
                    node({
                        let mut sel = SelectState::new();
                        sel.select(1);
                        let columns = vec![
                            Column::fixed(Span::styled("branch", theme.muted_style()), 8),
                            Column::auto(Span::styled("ahead", theme.muted_style())),
                            Column::flex(Span::styled("subject", theme.muted_style()), 1),
                        ];
                        let rows = vec![
                            vec![Line::from("main"), Line::from("0"), Line::from("release cut")],
                            vec![Line::from("wip"), Line::from("3"), Line::from("table component")],
                            vec![Line::from("fix"), Line::from("1"), Line::from("scrollbar math")],
                        ];
                        // Custom caret + preserve column colors under the highlight.
                        Table::new(columns, rows, &sel)
                            .header(true)
                            .caret('▶')
                            .preserve_selection_fg(true)
                    })
                }
            }
            grow(1) { spacer() }
            fixed(1) {
                node(Text::new(vec![Line::from(vec![
                    Span::styled("docs ", theme.muted_style()),
                    Span::styled(
                        "https://github.com/everruns/yolop",
                        theme.accent_style().add_modifier(Modifier::UNDERLINED),
                    ),
                    Span::styled("  ·  press q to quit", theme.muted_style()),
                ])]))
            }
        }
    }
}

fn main() -> io::Result<()> {
    let _session = tuika::TerminalSession::enter()?;
    // Hyperlinks enabled so the footer URL demos as a real OSC 8 link.
    let mut terminal = Terminal::with_options(
        HyperlinkBackend::new(io::stdout(), true),
        TerminalOptions {
            viewport: Viewport::Fullscreen,
        },
    )?;
    let theme = Theme::default();
    let mut progress = tuika::TerminalProgress::new();
    progress.indeterminate();

    let mut frame = 0u64;
    loop {
        terminal.draw(|f| {
            let area = f.area();
            let root = build(frame, &theme);
            tuika::paint(f.buffer_mut(), area, &theme, root.as_ref(), &[]);
        })?;
        if event::poll(Duration::from_millis(80))?
            && let CtEvent::Key(key) = event::read()?
            && key.kind != KeyEventKind::Release
            && matches!(key.code, KeyCode::Char('q') | KeyCode::Esc)
        {
            break;
        }
        frame = frame.wrapping_add(1);
    }

    progress.clear();
    let _ = terminal.clear();
    drop(terminal);
    Ok(())
}

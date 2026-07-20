//! Overlay compositing + input routing. Run with
//! `cargo run -p tuika --example overlay` (o open · enter/esc close · q quit).
//!
//! Demonstrates [`OverlaySpec`] anchoring a centered dialog over the base tree
//! and [`paint`] compositing it last (clearing its rect first, so base content
//! leaks around a clean-bordered panel). While the dialog is open it owns
//! input — the same key means different things depending on overlay state.

use std::io;
use std::time::Duration;

use crossterm::event::{self};
use ratatui::backend::CrosstermBackend;
use ratatui::text::{Line, Span};
use ratatui::{Terminal, TerminalOptions, Viewport};

use tuika::{
    Element, Event, KeyCode, Overlay, OverlaySpec, Padding, TerminalSession, Text, Theme, paint,
    translate_event, view,
};

fn main() -> io::Result<()> {
    let mut open = false;
    let mut confirmed = 0u32;

    let _session = TerminalSession::enter()?;
    let mut terminal = Terminal::with_options(
        CrosstermBackend::new(io::stdout()),
        TerminalOptions {
            viewport: Viewport::Fullscreen,
        },
    )?;
    let theme = Theme::default();

    loop {
        terminal.draw(|f| {
            let area = f.area();
            let hint = if open {
                "dialog open — enter = confirm · esc = dismiss"
            } else {
                "o open dialog · q quit"
            };
            let base = view! {
                col(padding = Padding::all(1), gap = 1) {
                    node(Text::new(vec![Line::from(Span::styled(
                        "tuika overlay demo",
                        theme.accent_style(),
                    ))]))
                    fixed(4) {
                        boxed(title = Line::from(" base layer ")) {
                            node(Text::raw(format!(
                                "confirmed {confirmed} time(s) — base content stays visible around the panel"
                            )))
                        }
                    }
                    grow(1) { spacer() }
                    fixed(1) {
                        node(Text::new(vec![Line::from(Span::styled(hint, theme.muted_style()))]))
                    }
                }
            };

            // The dialog Element must outlive the `paint` borrow below.
            let dialog: Option<Element> = open.then(|| {
                view! {
                    boxed(
                        title = Line::from(Span::styled(" confirm ", theme.accent_style())),
                        padding = Padding::all(1)
                    ) {
                        col(gap = 1) {
                            node(Text::raw("Proceed with the action?"))
                            node(Text::new(vec![Line::from(Span::styled(
                                "enter = yes · esc = no",
                                theme.muted_style(),
                            ))]))
                        }
                    }
                }
            });
            let mut overlays: Vec<Overlay> = Vec::new();
            if let Some(d) = &dialog {
                let rect = OverlaySpec::centered(50, 40)
                    .min_size(34, 7)
                    .resolve(area);
                overlays.push(Overlay {
                    area: rect,
                    view: d.as_ref(),
                    clear: true,
                });
            }
            paint(f.buffer_mut(), area, &theme, base.as_ref(), &overlays);
        })?;

        if !event::poll(Duration::from_millis(150))? {
            continue;
        }
        let Some(Event::Key(k)) = translate_event(event::read()?) else {
            continue;
        };
        if !k.plain() {
            continue;
        }
        match (open, k.code) {
            (false, KeyCode::Char('q')) => break,
            (false, KeyCode::Char('o')) => open = true,
            (true, KeyCode::Enter) => {
                confirmed += 1;
                open = false;
            }
            (true, KeyCode::Esc) => open = false,
            _ => {}
        }
    }

    let _ = terminal.clear();
    drop(terminal);
    Ok(())
}

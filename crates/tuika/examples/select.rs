//! Interactive select list. Run with `cargo run -p tuika --example select`
//! (↑/↓ move · enter choose · q or esc quit).
//!
//! Demonstrates the stateful-widget idiom: [`SelectState`] persists the
//! highlighted index across frames while [`SelectList`] is rebuilt each frame,
//! and [`translate_event`] feeds real terminal input to the widget.

use std::io;
use std::time::Duration;

use crossterm::event::{self};
use ratatui::backend::CrosstermBackend;
use ratatui::text::{Line, Span};
use ratatui::{Terminal, TerminalOptions, Viewport};

use tuika::{
    Event, KeyCode, Padding, SelectList, SelectOutcome, SelectState, TerminalSession, Text, Theme,
    paint, translate_event, view,
};

const FRUITS: [&str; 8] = [
    "Apple",
    "Blueberry",
    "Cherry",
    "Date",
    "Elderberry",
    "Fig",
    "Grape",
    "Honeydew",
];

fn main() -> io::Result<()> {
    let mut state = SelectState::new();
    let mut chosen: Option<usize> = None;

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
            let items: Vec<Line> = FRUITS.iter().map(|s| Line::from(*s)).collect();
            let list = SelectList::new(items, &state);
            let status = match chosen {
                Some(i) => format!("chose: {}", FRUITS[i]),
                None => "↑/↓ move · enter choose · q quit".to_string(),
            };
            let root = view! {
                col(padding = Padding::all(1), gap = 1) {
                    fixed(FRUITS.len() as u16 + 2) {
                        boxed(title = Line::from(Span::styled(" fruit ", theme.accent_style()))) {
                            node(list)
                        }
                    }
                    fixed(1) {
                        node(Text::new(vec![Line::from(Span::styled(
                            status,
                            theme.muted_style(),
                        ))]))
                    }
                }
            };
            paint(f.buffer_mut(), area, &theme, root.as_ref(), &[]);
        })?;

        if !event::poll(Duration::from_millis(150))? {
            continue;
        }
        let Some(ev) = translate_event(event::read()?) else {
            continue;
        };
        // `q` quits globally; the list itself only cares about arrows/enter/esc.
        if let Event::Key(k) = &ev
            && k.plain()
            && matches!(k.code, KeyCode::Char('q'))
        {
            break;
        }
        match state.handle(&ev, FRUITS.len()) {
            SelectOutcome::Confirmed(i) => chosen = Some(i),
            SelectOutcome::Cancelled => break,
            SelectOutcome::Moved(_) => {}
        }
    }

    let _ = terminal.clear();
    drop(terminal);
    Ok(())
}

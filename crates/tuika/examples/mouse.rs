//! Live mouse demo. Run with `cargo run -p tuika --example mouse`.
//!
//! Drag across the text to select it — the selection highlights, and releasing
//! the button copies it to the system clipboard via OSC 52. Click the on-screen
//! buttons (`[ clear ]`, `[ quit ]`). Press `q`/`Esc` to quit.
//!
//! This exercises the whole `tuika::mouse` stack end to end: `translate_event`
//! feeding `SelectionState` / `ClickTracker` / `HitMap`, `highlight` +
//! `selected_text` over the rendered buffer, and `clipboard::write_clipboard`
//! (OSC 52). Chrome is drawn with plain ratatui widgets so the interaction rects
//! are exact — the star here is the mouse API, not layout.
//!
//! Shift-drag is intentionally *not* handled: most terminals use it to bypass
//! application mouse capture for their own native selection.

use std::io;
use std::time::Duration;

use crossterm::event::{self, Event as CtEvent, KeyCode, KeyEventKind};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};
use ratatui::{Terminal, TerminalOptions, Viewport};

use tuika::{
    AltScreen, ClickTracker, Event, HitMap, MouseButton, MouseKind, SelectionState, Theme,
    highlight, selected_text, translate_event, write_clipboard,
};

const SAMPLE: &[&str] = &[
    "Drag across this text to select it.",
    "Release the button to copy the selection",
    "to your clipboard via OSC 52.",
    "Wide glyphs work too: 你好 world.",
];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Btn {
    Clear,
    Quit,
}

fn main() -> io::Result<()> {
    enable_raw_mode()?;
    let mut alt = AltScreen::enter()?; // also enables mouse capture
    let mut term = Terminal::with_options(
        CrosstermBackend::new(io::stdout()),
        TerminalOptions {
            viewport: Viewport::Fullscreen,
        },
    )?;
    let theme = Theme::default();
    let mut sel = SelectionState::new();
    let mut clicks = ClickTracker::new();
    let mut status = String::from("drag to select · click a button · q to quit");
    let sel_style = Style::default()
        .bg(theme.selection_bg)
        .fg(theme.selection_fg);
    // Set when a drag is released; the copy happens in the next draw, where the
    // freshly rendered frame buffer is available to read the selected text from.
    let mut pending_copy = false;

    loop {
        // Rects are computed inside the draw and reused for hit-testing below.
        let mut text_area = Rect::default();
        let mut clear_btn = Rect::default();
        let mut quit_btn = Rect::default();

        term.draw(|f| {
            let area = f.area();
            text_area = Rect::new(
                2,
                2,
                area.width.saturating_sub(4).max(1),
                SAMPLE.len() as u16,
            );
            let btn_y = text_area.bottom().saturating_add(1);
            clear_btn = Rect::new(2, btn_y, 9, 1);
            quit_btn = Rect::new(12, btn_y, 8, 1);

            f.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    " tuika mouse demo ",
                    theme.accent_style(),
                ))),
                Rect::new(1, 0, area.width.saturating_sub(2), 1),
            );
            f.render_widget(
                Block::bordered(),
                Rect::new(1, 1, area.width.saturating_sub(2), SAMPLE.len() as u16 + 2),
            );
            f.render_widget(
                Paragraph::new(SAMPLE.iter().map(|s| Line::from(*s)).collect::<Vec<_>>()),
                text_area,
            );
            f.render_widget(
                Paragraph::new(Span::styled("[ clear ]", theme.muted_style())),
                clear_btn,
            );
            f.render_widget(
                Paragraph::new(Span::styled("[ quit ]", theme.muted_style())),
                quit_btn,
            );

            if let Some(range) = sel.range() {
                // Read the selected text off the freshly rendered frame *before*
                // highlighting (highlight only changes style, so order is cosmetic).
                if pending_copy {
                    let text = selected_text(f.buffer_mut(), text_area, range);
                    if let Ok(true) = write_clipboard(&mut io::stdout(), &text) {
                        status = format!("copied {} chars", text.chars().count());
                    }
                    pending_copy = false;
                }
                highlight(f.buffer_mut(), text_area, range, sel_style);
            } else {
                pending_copy = false;
            }

            f.render_widget(
                Paragraph::new(Span::styled(status.clone(), theme.muted_style())),
                Rect::new(
                    1,
                    area.height.saturating_sub(1),
                    area.width.saturating_sub(2),
                    1,
                ),
            );
        })?;

        let mut hits: HitMap<Btn> = HitMap::new();
        hits.push(clear_btn, Btn::Clear);
        hits.push(quit_btn, Btn::Quit);

        if !event::poll(Duration::from_millis(100))? {
            continue;
        }
        let ct = event::read()?;
        if let CtEvent::Key(k) = &ct
            && k.kind != KeyEventKind::Release
            && matches!(k.code, KeyCode::Char('q') | KeyCode::Esc)
        {
            break;
        }
        let Some(Event::Mouse(m)) = translate_event(ct) else {
            continue;
        };

        // Keep selection state consistent for every plain gesture.
        let selection_changed = m.plain() && sel.handle(&m);

        // A click that lands on a button wins over selection.
        if let Some(click) = clicks.handle(&m)
            && let Some(btn) = hits.hit(click.column, click.row)
        {
            match btn {
                Btn::Quit => break,
                Btn::Clear => {
                    sel.clear();
                    status = "cleared".into();
                }
            }
            continue;
        }

        // Releasing a drag arms the copy for the next frame.
        if selection_changed
            && matches!(m.kind, MouseKind::Up(MouseButton::Left))
            && sel.range().is_some()
        {
            pending_copy = true;
        }
    }

    let _ = term.clear();
    drop(term);
    alt.leave();
    disable_raw_mode()?;
    Ok(())
}

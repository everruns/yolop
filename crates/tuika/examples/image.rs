//! Terminal images via the Kitty graphics protocol. Run with
//! `cargo run -p tuika --example image` (q or esc to quit).
//!
//! Builds a synthetic RGBA gradient (no image-decoding dependency needed) and
//! shows it centered on screen. Capability is auto-detected: on Kitty, Ghostty,
//! WezTerm, or Konsole the real pixels are painted; anywhere else the same
//! [`Image`](tuika::Image) view degrades to its alt-text placeholder.
//!
//! The load-bearing detail is the two-step draw: [`paint`] reserves the image's
//! cells and records its placement into an [`ImageLayer`](tuika::ImageLayer),
//! then — *after* `terminal.draw()` flushes the frame — [`ImageLayer::emit`]
//! writes the graphics escapes over those cells and the layer is cleared for the
//! next frame.

use std::io::{self, Write};
use std::time::Duration;

use crossterm::event;
use ratatui::backend::CrosstermBackend;
use ratatui::style::Style;
use ratatui::text::Span;
use ratatui::{Terminal, TerminalOptions, Viewport};

use tuika::{
    Event, Image, ImageData, ImageLayer, ImageSupport, KeyCode, Padding, Spacer, StatusBar,
    TerminalSession, Theme, paint, translate_event, view,
};

/// A `w × h` RGBA gradient: red rises left-to-right, green top-to-bottom.
fn gradient(w: u32, h: u32) -> ImageData {
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            let r = (x * 255 / w.max(1)) as u8;
            let g = (y * 255 / h.max(1)) as u8;
            rgba.extend_from_slice(&[r, g, 128, 255]);
        }
    }
    ImageData::from_rgba(w, h, rgba).expect("gradient is well-formed")
}

fn main() -> io::Result<()> {
    // Cell footprint of the on-screen image, and its source pixel resolution.
    const COLS: u16 = 40;
    const ROWS: u16 = 20;
    let data = gradient(320, 320);
    let support = ImageSupport::detect();
    let layer = ImageLayer::new();

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
            let image = Image::new(data.clone(), COLS, ROWS)
                .support(support)
                .in_layer(&layer)
                .alt("a red/green gradient");
            let status = match support {
                ImageSupport::Kitty => " graphics: Kitty protocol detected ",
                ImageSupport::ITerm2 => " graphics: iTerm2 protocol detected ",
                ImageSupport::None => " graphics: none — showing text fallback ",
            };
            let bar = StatusBar::new()
                .left(vec![Span::styled(status, theme.selection_style())])
                .right(vec![Span::styled(" q quit ", theme.muted_style())])
                .background(Style::default().bg(theme.surface));
            let root = view! {
                col(padding = Padding::all(1), gap = 1) {
                    grow(1) { node(Spacer) }
                    fixed(ROWS) {
                        row {
                            grow(1) { node(Spacer) }
                            fixed(COLS) { node(image) }
                            grow(1) { node(Spacer) }
                        }
                    }
                    grow(1) { node(Spacer) }
                    fixed(1) { node(bar) }
                }
            };
            paint(f.buffer_mut(), area, &theme, root.as_ref(), &[]);
        })?;

        // After the cell frame is on screen, paint the pixels over the reserved
        // cells, then clear the layer so the next frame records fresh.
        layer.emit(&mut io::stdout())?;
        io::stdout().flush()?;
        layer.clear();

        if !event::poll(Duration::from_millis(100))? {
            continue;
        }
        if let Some(Event::Key(k)) = translate_event(event::read()?)
            && matches!(k.code, KeyCode::Char('q') | KeyCode::Esc)
        {
            break;
        }
    }

    let _ = terminal.clear();
    drop(terminal);
    Ok(())
}

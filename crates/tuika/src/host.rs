//! Terminal host: alternate-screen lifecycle, event translation, compositor.
//!
//! This is the seam between `tuika` and a real terminal. [`TerminalSession`]
//! owns the complete full-screen lifecycle; [`AltScreen`] is the narrower
//! guard for hosts that manage raw mode and cursor visibility themselves.
//! [`translate_event`] maps crossterm input to `tuika` [`Event`]s. [`paint`]
//! composites a root view plus any overlays into the frame buffer: background
//! fill, root, then each overlay last (clearing its rect first), which is why
//! overlays never leak surrounding chrome here.

use std::io::{self, Write};

use crossterm::cursor::{Hide, Show};
use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event as CtEvent, KeyCode as CtKeyCode, KeyEventKind,
    KeyModifiers, MouseEventKind as CtMouseKind,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
    is_raw_mode_enabled,
};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;

use super::event::{Event, Key, KeyCode, Mouse, MouseKind};
use super::style::Theme;
use super::surface::Surface;
use super::view::{RenderCtx, View};

/// RAII guard for the alternate screen + mouse capture.
pub struct AltScreen {
    active: bool,
}

impl AltScreen {
    /// Enter the alternate screen and enable mouse capture. Assumes raw mode is
    /// already enabled by the caller.
    pub fn enter() -> io::Result<Self> {
        let mut out = io::stdout();
        execute!(out, EnterAlternateScreen, EnableMouseCapture)?;
        out.flush()?;
        Ok(Self { active: true })
    }

    /// Explicitly leave (also runs on drop; idempotent).
    pub fn leave(&mut self) {
        if self.active {
            let mut out = io::stdout();
            let _ = execute!(out, DisableMouseCapture, LeaveAlternateScreen);
            let _ = out.flush();
            self.active = false;
        }
    }
}

impl Drop for AltScreen {
    fn drop(&mut self) {
        self.leave();
    }
}

/// Complete RAII ownership of a full-screen terminal session.
///
/// Construction enables raw mode, enters the alternate screen, enables mouse
/// capture, and hides the cursor. Drop restores the terminal, including when
/// unwinding from a panic. Pre-existing raw mode is preserved rather than
/// disabled. If construction fails partway through, it performs the same
/// best-effort rollback before returning.
pub struct TerminalSession {
    active: bool,
    raw_mode_owned: bool,
}

impl TerminalSession {
    /// Enter a full-screen terminal session, rolling back on failure.
    pub fn enter() -> io::Result<Self> {
        let raw_mode_owned = !is_raw_mode_enabled()?;
        if raw_mode_owned {
            enable_raw_mode()?;
        }
        let mut out = io::stdout();
        if let Err(error) =
            execute!(out, EnterAlternateScreen, EnableMouseCapture, Hide).and_then(|()| out.flush())
        {
            let _ = execute!(out, Show, DisableMouseCapture, LeaveAlternateScreen);
            if raw_mode_owned {
                let _ = disable_raw_mode();
            }
            return Err(error);
        }
        Ok(Self {
            active: true,
            raw_mode_owned,
        })
    }

    /// Restore the terminal immediately. Calling this more than once is safe.
    pub fn leave(&mut self) {
        if !self.active {
            return;
        }
        let mut out = io::stdout();
        let _ = execute!(out, Show, DisableMouseCapture, LeaveAlternateScreen);
        let _ = out.flush();
        if self.raw_mode_owned {
            let _ = disable_raw_mode();
        }
        self.active = false;
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        self.leave();
    }
}

/// An overlay to composite over the base tree at a resolved rect.
pub struct Overlay<'a> {
    pub area: Rect,
    pub view: &'a dyn View,
    /// Clear the rect (fill with the theme background) before painting.
    pub clear: bool,
}

/// Composite `root` and `overlays` into `buffer` over `area`.
pub fn paint(
    buffer: &mut Buffer,
    area: Rect,
    theme: &Theme,
    root: &dyn View,
    overlays: &[Overlay],
) {
    // Base background so the alternate screen is fully owned (no stale cells).
    {
        let mut surface = Surface::new(buffer, area);
        surface.fill(Style::default().bg(theme.background));
    }
    let ctx = RenderCtx::new(theme);
    {
        let mut surface = Surface::new(buffer, area);
        root.render(area, &mut surface, &ctx);
    }
    for overlay in overlays {
        let mut surface = Surface::new(buffer, overlay.area);
        if overlay.clear {
            surface.fill(Style::default().bg(theme.surface));
        }
        // An open overlay owns input, so render it focused.
        overlay
            .view
            .render(overlay.area, &mut surface, &ctx.with_focus(true));
    }
}

/// Translate a crossterm event into a `tuika` event, dropping ones `tuika`
/// does not model (key-release, unmapped codes).
pub fn translate_event(event: CtEvent) -> Option<Event> {
    match event {
        CtEvent::Key(k) => {
            if k.kind == KeyEventKind::Release {
                return None;
            }
            let code = match k.code {
                CtKeyCode::Char(c) => KeyCode::Char(c),
                CtKeyCode::Enter => KeyCode::Enter,
                CtKeyCode::Esc => KeyCode::Esc,
                CtKeyCode::Backspace => KeyCode::Backspace,
                CtKeyCode::Delete => KeyCode::Delete,
                CtKeyCode::Tab => KeyCode::Tab,
                CtKeyCode::BackTab => KeyCode::BackTab,
                CtKeyCode::Up => KeyCode::Up,
                CtKeyCode::Down => KeyCode::Down,
                CtKeyCode::Left => KeyCode::Left,
                CtKeyCode::Right => KeyCode::Right,
                CtKeyCode::Home => KeyCode::Home,
                CtKeyCode::End => KeyCode::End,
                CtKeyCode::PageUp => KeyCode::PageUp,
                CtKeyCode::PageDown => KeyCode::PageDown,
                _ => return None,
            };
            Some(Event::Key(Key {
                code,
                ctrl: k.modifiers.contains(KeyModifiers::CONTROL),
                alt: k.modifiers.contains(KeyModifiers::ALT),
                shift: k.modifiers.contains(KeyModifiers::SHIFT),
            }))
        }
        CtEvent::Mouse(m) => {
            let kind = match m.kind {
                CtMouseKind::Down(_) => MouseKind::Down,
                CtMouseKind::Up(_) => MouseKind::Up,
                CtMouseKind::ScrollUp => MouseKind::ScrollUp,
                CtMouseKind::ScrollDown => MouseKind::ScrollDown,
                CtMouseKind::Moved | CtMouseKind::Drag(_) => MouseKind::Moved,
                _ => return None,
            };
            Some(Event::Mouse(Mouse {
                kind,
                column: m.column,
                row: m.row,
            }))
        }
        CtEvent::Paste(text) => Some(Event::Paste(text)),
        CtEvent::Resize(width, height) => Some(Event::Resize { width, height }),
        _ => None,
    }
}

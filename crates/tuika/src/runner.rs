//! A small synchronous full-screen runner.
//!
//! Applications with an existing async runtime can keep their own event loop
//! and use [`paint`] directly. This runner is for conventional
//! dashboards and tools that want lifecycle, redraw scheduling, and event
//! translation in one place.

use std::io;
use std::ops::ControlFlow;
use std::time::{Duration, Instant};

use crossterm::event;
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::{Terminal, TerminalOptions, Viewport};

use crate::{Element, Event, RedrawHandle, TerminalSession, Theme, paint, translate_event};

#[derive(Clone, Copy, Debug)]
/// Scheduling options for [`Runner`].
pub struct RunnerConfig {
    /// Maximum time between frames and data-driven redraw checks.
    pub tick_rate: Duration,
}

impl Default for RunnerConfig {
    fn default() -> Self {
        Self {
            tick_rate: Duration::from_millis(100),
        }
    }
}

/// A synchronous Crossterm event and rendering loop.
pub struct Runner {
    config: RunnerConfig,
    redraw: RedrawHandle,
}

impl Runner {
    /// Create a runner without touching the terminal.
    pub fn new(mut config: RunnerConfig) -> Self {
        // A zero interval would busy-spin even when the application has no
        // events or updates. Keep the public config ergonomic while enforcing
        // a safe scheduling floor at the boundary.
        config.tick_rate = config.tick_rate.max(Duration::from_millis(1));
        Self {
            config,
            redraw: RedrawHandle::default(),
        }
    }

    /// Return a handle that background producers can use to request redraws.
    pub fn redraw_handle(&self) -> RedrawHandle {
        self.redraw.clone()
    }

    /// Run until `on_event` returns [`ControlFlow::Break`].
    pub fn run(
        &self,
        theme: &Theme,
        build: impl FnMut(u64) -> Element,
        on_event: impl FnMut(Event) -> ControlFlow<()>,
    ) -> io::Result<()> {
        self.run_with_backend(theme, CrosstermBackend::new(io::stdout()), build, on_event)
    }

    /// Run with a caller-provided backend, such as
    /// [`HyperlinkBackend`](crate::HyperlinkBackend).
    pub fn run_with_backend<B>(
        &self,
        theme: &Theme,
        backend: B,
        mut build: impl FnMut(u64) -> Element,
        mut on_event: impl FnMut(Event) -> ControlFlow<()>,
    ) -> io::Result<()>
    where
        B: Backend<Error = io::Error>,
    {
        let _session = TerminalSession::enter()?;
        let mut terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Fullscreen,
            },
        )?;
        let mut frame = 0u64;
        let mut last_draw = Instant::now();
        self.redraw.request();

        loop {
            let due = last_draw.elapsed() >= self.config.tick_rate;
            let requested = self.redraw.take();
            if due || requested {
                terminal.draw(|terminal_frame| {
                    let area = terminal_frame.area();
                    let root = build(frame);
                    paint(terminal_frame.buffer_mut(), area, theme, root.as_ref(), &[]);
                })?;
                frame = frame.wrapping_add(1);
                last_draw = Instant::now();
            }

            let timeout = self.config.tick_rate.saturating_sub(last_draw.elapsed());
            if event::poll(timeout)?
                && let Some(event) = translate_event(event::read()?)
                && on_event(event).is_break()
            {
                break;
            }
        }

        // Some terminal emulators do not answer the cursor-position query
        // used by `clear`. Session restoration must still succeed and a
        // cosmetic cleanup failure must not turn a completed run into an
        // application error.
        let _ = terminal.clear();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_tick_rate_is_clamped() {
        let runner = Runner::new(RunnerConfig {
            tick_rate: Duration::ZERO,
        });
        assert_eq!(runner.config.tick_rate, Duration::from_millis(1));
    }
}

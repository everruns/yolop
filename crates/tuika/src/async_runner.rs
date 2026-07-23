//! An asynchronous full-screen runner (`feature = "async"`).
//!
//! [`AsyncRunner`] ties tuika's public host primitives — [`TerminalSession`],
//! [`paint`], [`translate_event`] — to crossterm's async
//! [`EventStream`] and a tick timer in a single
//! `tokio::select!` loop. An application that already has a Tokio runtime (it is
//! doing network or disk I/O) gets lifecycle, redraw scheduling, and event
//! translation in one place, and its data becomes a plain local value the loop
//! owns — no `spawn_blocking`, no shared `RwLock`/`Notify`/stop flag bolted onto
//! the synchronous [`Runner`](crate::Runner) just to feed it.
//!
//! The loop threads a single `state` value through two callbacks: `view` builds
//! the frame from `&state`, and `update` mutates `&mut state` in response to a
//! [`Signal`] (a tick or an input event) and may `.await` while doing so. Both
//! borrow the same value at different times, which is why it is a runner
//! argument rather than a capture — a closure cannot hold `&state` and
//! `&mut state` at once.
//!
//! ```no_run
//! use std::ops::ControlFlow;
//! use std::time::Duration;
//!
//! use tuika::{AsyncRunner, Event, KeyCode, RunnerConfig, Signal, Text, Theme, element};
//!
//! # async fn fetch_stats() -> std::io::Result<u64> { Ok(0) }
//! // Call from inside your own `#[tokio::main]` (or any Tokio runtime).
//! async fn dashboard() -> std::io::Result<()> {
//!     let runner = AsyncRunner::new(RunnerConfig {
//!         tick_rate: Duration::from_secs(2),
//!     });
//!     let mut requests = 0u64;
//!
//!     runner
//!         .run(
//!             &Theme::default(),
//!             &mut requests,
//!             |requests, _frame| element(Text::raw(format!("requests: {requests}"))),
//!             async |requests, signal| match signal {
//!                 // Poll on every tick and on `r`; both may await.
//!                 Signal::Tick => {
//!                     *requests = fetch_stats().await.unwrap_or(*requests);
//!                     ControlFlow::Continue(())
//!                 }
//!                 Signal::Event(Event::Key(k)) if k.plain() => match k.code {
//!                     KeyCode::Char('q') | KeyCode::Esc => ControlFlow::Break(()),
//!                     KeyCode::Char('r') => {
//!                         *requests = fetch_stats().await.unwrap_or(*requests);
//!                         ControlFlow::Continue(())
//!                     }
//!                     _ => ControlFlow::Continue(()),
//!                 },
//!                 _ => ControlFlow::Continue(()),
//!             },
//!         )
//!         .await
//! }
//! ```

use std::io;
use std::ops::ControlFlow;
use std::time::Duration;

use crossterm::event::EventStream;
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::{Terminal, TerminalOptions, Viewport};
use tokio::time::{MissedTickBehavior, interval};
use tokio_stream::{Stream, StreamExt};

use crate::{Element, Event, RunnerConfig, TerminalSession, Theme, paint, translate_event};

/// A tick or an input event delivered to an [`AsyncRunner`]'s update callback.
#[derive(Clone, Debug)]
pub enum Signal {
    /// The periodic tick fired. Its interval is [`RunnerConfig::tick_rate`]; the
    /// first tick fires immediately after the initial frame is painted, so a
    /// data-polling app loads on start without a special case.
    Tick,
    /// An input event arrived, already translated to a tuika [`Event`]. Events
    /// crossterm reports but tuika does not model (key-release, unmapped codes)
    /// are dropped before they reach here.
    Event(Event),
}

/// An asynchronous Crossterm event and rendering loop.
///
/// See the [module documentation](self) for the full picture. Construct one with
/// a [`RunnerConfig`], then drive it with [`run`](Self::run) (real terminal),
/// [`run_with_backend`](Self::run_with_backend) (caller-supplied backend, such as
/// [`HyperlinkBackend`](crate::HyperlinkBackend)), or
/// [`run_with_events`](Self::run_with_events) (caller-supplied backend *and*
/// event stream, for tests and hosts that own the terminal lifecycle).
pub struct AsyncRunner {
    config: RunnerConfig,
}

impl AsyncRunner {
    /// Create a runner without touching the terminal.
    pub fn new(mut config: RunnerConfig) -> Self {
        // A zero interval would busy-spin the `select!` even when the app has no
        // events or updates. Enforce the same scheduling floor the synchronous
        // `Runner` does.
        config.tick_rate = config.tick_rate.max(Duration::from_millis(1));
        Self { config }
    }

    /// Run on the real terminal until `update` returns [`ControlFlow::Break`].
    ///
    /// Enters a [`TerminalSession`] (restored on return, including on error or
    /// panic) and reads input from crossterm's async
    /// [`EventStream`].
    pub async fn run<S, V, U>(
        &self,
        theme: &Theme,
        state: &mut S,
        view: V,
        update: U,
    ) -> io::Result<()>
    where
        V: FnMut(&S, u64) -> Element,
        U: AsyncFnMut(&mut S, Signal) -> ControlFlow<()>,
    {
        self.run_with_backend(
            theme,
            CrosstermBackend::new(io::stdout()),
            state,
            view,
            update,
        )
        .await
    }

    /// Run with a caller-provided backend, such as
    /// [`HyperlinkBackend`](crate::HyperlinkBackend). Otherwise identical to
    /// [`run`](Self::run): it owns the [`TerminalSession`] and the
    /// [`EventStream`].
    pub async fn run_with_backend<S, B, V, U>(
        &self,
        theme: &Theme,
        backend: B,
        state: &mut S,
        view: V,
        update: U,
    ) -> io::Result<()>
    where
        B: Backend<Error = io::Error>,
        V: FnMut(&S, u64) -> Element,
        U: AsyncFnMut(&mut S, Signal) -> ControlFlow<()>,
    {
        let _session = TerminalSession::enter()?;
        let mut terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Fullscreen,
            },
        )?;
        // Translate crossterm events to tuika events at the stream boundary,
        // dropping the ones tuika does not model and surfacing read errors.
        let events = EventStream::new().filter_map(|result| match result {
            Ok(raw) => translate_event(raw).map(Ok),
            Err(error) => Some(Err(error)),
        });
        let result = self
            .run_with_events(&mut terminal, theme, state, events, view, update)
            .await;

        // Some terminal emulators do not answer the cursor-position query used
        // by `clear`; a cosmetic cleanup failure must not turn a completed run
        // into an application error. (Session restoration is the `_session`
        // guard's job and happens regardless.)
        let _ = terminal.clear();
        result
    }

    /// The core loop: caller-owned `terminal` and `events`, no terminal
    /// lifecycle. This is what [`run`](Self::run) builds on, and the seam tests
    /// use to drive the runner against a
    /// [`TestBackend`](ratatui::backend::TestBackend) with a scripted event
    /// stream. Hosts that already own their terminal and event source (or want a
    /// non-crossterm one) can call it directly.
    ///
    /// The backend error and the event-stream error share one type `Er`, which
    /// is the run's error type: for the real terminal that is [`io::Error`], but
    /// leaving it generic lets an infallible backend
    /// ([`TestBackend`](ratatui::backend::TestBackend), whose error is
    /// [`Infallible`](std::convert::Infallible)) pair with an infallible stream.
    ///
    /// `events` yields already-translated tuika [`Event`]s. An `Err` item ends
    /// the run by propagating out; `None` (a finite stream running dry) stops
    /// event delivery but leaves the tick timer running, so the loop still exits
    /// only when `update` returns [`ControlFlow::Break`].
    pub async fn run_with_events<S, B, V, U, E, Er>(
        &self,
        terminal: &mut Terminal<B>,
        theme: &Theme,
        state: &mut S,
        mut events: E,
        mut view: V,
        mut update: U,
    ) -> Result<(), Er>
    where
        B: Backend<Error = Er>,
        E: Stream<Item = Result<Event, Er>> + Unpin,
        V: FnMut(&S, u64) -> Element,
        U: AsyncFnMut(&mut S, Signal) -> ControlFlow<()>,
    {
        let mut events_done = false;
        let mut frame = 0u64;

        let mut ticker = interval(self.config.tick_rate);
        // A slow `update` must not make the timer fire a burst of catch-up ticks
        // the moment it returns; skip the missed ticks and resume the cadence.
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

        // Paint once before waiting for the first signal so the UI is visible
        // immediately rather than after the first tick or keypress.
        draw(terminal, theme, &mut view, state, &mut frame)?;

        loop {
            let signal = tokio::select! {
                _ = ticker.tick() => Signal::Tick,
                // Once the stream is exhausted the guard disables this branch so
                // `select!` waits on the tick alone instead of spinning on a
                // stream that keeps returning `None`.
                item = events.next(), if !events_done => match item {
                    Some(Ok(event)) => Signal::Event(event),
                    Some(Err(error)) => return Err(error),
                    None => {
                        events_done = true;
                        continue;
                    }
                },
            };

            if update(state, signal).await.is_break() {
                break;
            }
            // Redraws are a ratatui buffer diff, so repainting after every
            // handled signal only flushes the cells that actually changed.
            draw(terminal, theme, &mut view, state, &mut frame)?;
        }

        Ok(())
    }
}

/// Paint one frame from the current `state` and advance the frame counter that
/// drives motion components (spinners, indeterminate bars).
fn draw<S, B, V, Er>(
    terminal: &mut Terminal<B>,
    theme: &Theme,
    view: &mut V,
    state: &S,
    frame: &mut u64,
) -> Result<(), Er>
where
    B: Backend<Error = Er>,
    V: FnMut(&S, u64) -> Element,
{
    terminal.draw(|terminal_frame| {
        let area = terminal_frame.area();
        let root = view(state, *frame);
        paint(terminal_frame.buffer_mut(), area, theme, root.as_ref(), &[]);
    })?;
    *frame = frame.wrapping_add(1);
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;

    use super::*;
    use crate::components::Text;
    use crate::event::{Key, KeyCode};
    use crate::view::element;
    use ratatui::backend::TestBackend;

    /// A key event as the infallible-stream item the `TestBackend` tests use.
    fn key(code: KeyCode) -> Result<Event, Infallible> {
        Ok(Event::Key(Key {
            code,
            ctrl: false,
            alt: false,
            shift: false,
        }))
    }

    fn terminal(width: u16, height: u16) -> Terminal<TestBackend> {
        Terminal::new(TestBackend::new(width, height)).expect("test terminal")
    }

    fn buffer_text(terminal: &Terminal<TestBackend>) -> String {
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    #[test]
    fn zero_tick_rate_is_clamped() {
        let runner = AsyncRunner::new(RunnerConfig {
            tick_rate: Duration::ZERO,
        });
        assert_eq!(runner.config.tick_rate, Duration::from_millis(1));
    }

    // Events flow through `update`, mutate the owned local state, and a quit key
    // breaks the loop with `Ok(())`. A far-off tick rate keeps the timer out of
    // this test so it stays timing-independent.
    #[tokio::test]
    async fn events_drive_state_and_quit_breaks() {
        let runner = AsyncRunner::new(RunnerConfig {
            tick_rate: Duration::from_secs(3600),
        });
        let mut terminal = terminal(24, 1);
        let mut count = 0u64;
        let events = tokio_stream::iter([
            key(KeyCode::Char('a')),
            key(KeyCode::Char('a')),
            key(KeyCode::Char('q')),
        ]);

        let result = runner
            .run_with_events(
                &mut terminal,
                &Theme::default(),
                &mut count,
                events,
                |count, _frame| element(Text::raw(format!("count={count}"))),
                async |count, signal| match signal {
                    Signal::Event(Event::Key(k)) if k.code == KeyCode::Char('q') => {
                        ControlFlow::Break(())
                    }
                    Signal::Event(Event::Key(_)) => {
                        *count += 1;
                        ControlFlow::Continue(())
                    }
                    _ => ControlFlow::Continue(()),
                },
            )
            .await;

        assert!(result.is_ok());
        assert_eq!(count, 2, "both 'a' presses counted, 'q' quit");
        assert!(
            buffer_text(&terminal).contains("count=2"),
            "final frame reflects state: {:?}",
            buffer_text(&terminal)
        );
    }

    // The initial frame is painted before any signal is handled, so a run that
    // quits on the very first event still shows state on screen.
    #[tokio::test]
    async fn initial_frame_paints_before_first_signal() {
        let runner = AsyncRunner::new(RunnerConfig::default());
        let mut terminal = terminal(16, 1);
        let mut state = "hello";
        let events = tokio_stream::iter([key(KeyCode::Esc)]);

        runner
            .run_with_events(
                &mut terminal,
                &Theme::default(),
                &mut state,
                events,
                |state, _frame| element(Text::raw(*state)),
                async |_state, _signal| ControlFlow::Break(()),
            )
            .await
            .unwrap();

        assert!(buffer_text(&terminal).contains("hello"));
    }

    // Ticks fire on the interval and can await. Under a paused clock the runtime
    // auto-advances time whenever it goes idle, so the interval fires
    // deterministically with no wall-clock wait; the empty event stream runs dry
    // and the tick-only loop keeps going until the third tick breaks it.
    #[tokio::test(start_paused = true)]
    async fn ticks_fire_and_can_await() {
        let runner = AsyncRunner::new(RunnerConfig {
            tick_rate: Duration::from_millis(50),
        });
        let mut terminal = terminal(20, 1);
        let mut ticks = 0u64;
        let events = tokio_stream::iter(Vec::<Result<Event, Infallible>>::new());

        runner
            .run_with_events(
                &mut terminal,
                &Theme::default(),
                &mut ticks,
                events,
                |ticks, _frame| element(Text::raw(format!("ticks={ticks}"))),
                async |ticks, signal| {
                    if let Signal::Tick = signal {
                        // Prove an await inside tick handling is fine.
                        tokio::task::yield_now().await;
                        *ticks += 1;
                    }
                    if *ticks >= 3 {
                        ControlFlow::Break(())
                    } else {
                        ControlFlow::Continue(())
                    }
                },
            )
            .await
            .unwrap();

        assert_eq!(ticks, 3);
        // The break tick is not painted (the loop exits before the redraw), so
        // the last frame on screen is the preceding tick's `ticks=2`. This is
        // the same "no redraw after Break" rule the event test relies on.
        assert!(
            buffer_text(&terminal).contains("ticks=2"),
            "last painted frame: {:?}",
            buffer_text(&terminal)
        );
    }

    // A read error from the event stream propagates out of the run. This uses a
    // real `io::Error` backend (crossterm over a `Vec` sink) so the run's error
    // type is `io::Error` rather than the `Infallible` of `TestBackend`. A fixed
    // viewport is required: `Terminal::new`/`Fullscreen` queries the terminal
    // size, which has no TTY to answer under CI and would panic; `Fixed` uses the
    // given rect and never touches the terminal.
    #[tokio::test]
    async fn stream_error_propagates() {
        use ratatui::backend::CrosstermBackend;
        use ratatui::layout::Rect;

        let runner = AsyncRunner::new(RunnerConfig {
            tick_rate: Duration::from_secs(3600),
        });
        let mut terminal = Terminal::with_options(
            CrosstermBackend::new(Vec::<u8>::new()),
            TerminalOptions {
                viewport: Viewport::Fixed(Rect::new(0, 0, 10, 1)),
            },
        )
        .expect("test terminal");
        let mut state = ();
        let events = tokio_stream::iter([Err::<Event, io::Error>(io::Error::other("boom"))]);

        let result = runner
            .run_with_events(
                &mut terminal,
                &Theme::default(),
                &mut state,
                events,
                |_state, _frame| element(Text::raw("x")),
                async |_state, _signal| ControlFlow::Continue(()),
            )
            .await;

        let error = result.expect_err("stream error should propagate");
        assert_eq!(error.to_string(), "boom");
    }
}

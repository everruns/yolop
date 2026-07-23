//! Async live dashboard driven by [`tuika::AsyncRunner`].
//!
//! The synchronous [`ratatui_dashboard`](ratatui_dashboard.rs) example spawns a
//! background thread and shares its metrics through `Live` so the blocking
//! `Runner` can read them. This one does the same job on a Tokio runtime with
//! **no** shared state: `Metrics` is a plain local value the loop owns, polled
//! on every tick (and on demand with `r`) by an `async` update closure. There is
//! no `Live`, no `Notify`, no stop flag, no `spawn_blocking` — the runner's
//! single `tokio::select!` is the whole event loop.
//!
//! Run with: `cargo run -p tuika --example async_dashboard --features async`.
//! Press `q`/`esc` to quit, `r` to refresh now.

use std::ops::ControlFlow;
use std::time::Duration;

use ratatui::layout::Constraint;
use ratatui::widgets::{Block, Borders, Row, Sparkline, Table, Widget};
use tuika::{
    AsyncRunner, Dimension, Element, Event, Flex, KeyCode, KeyHints, RatatuiView, RunnerConfig,
    Signal, Theme, element,
};

/// The dashboard's entire state — owned by the run loop, not shared.
struct Metrics {
    requests: u64,
    errors: u64,
    history: Vec<u64>,
}

impl Metrics {
    fn new() -> Self {
        Self {
            requests: 0,
            errors: 0,
            history: Vec::new(),
        }
    }

    /// Fold a freshly fetched sample into the rolling view.
    fn ingest(&mut self, requests: u64) {
        self.requests = requests;
        self.errors = requests / 7;
        self.history.push(requests % 20);
        if self.history.len() > 40 {
            self.history.remove(0);
        }
    }
}

/// Stand-in for a real network/database call: an `async` source of a monotonic
/// request count. A real app would `reqwest::get(&url).await?` here; the point
/// is that the update closure can simply `.await` it.
async fn fetch_stats(previous: u64) -> u64 {
    tokio::time::sleep(Duration::from_millis(20)).await;
    previous + 1
}

fn table(metrics: &Metrics) -> Element {
    let (requests, errors) = (metrics.requests, metrics.errors);
    element(RatatuiView::fill(move |area, buffer| {
        Table::new(
            [
                Row::new(["requests", &requests.to_string()]),
                Row::new(["errors", &errors.to_string()]),
            ],
            [Constraint::Percentage(60), Constraint::Percentage(40)],
        )
        .block(Block::bordered().title(" Metrics "))
        .render(area, buffer);
    }))
}

fn sparkline(metrics: &Metrics) -> Element {
    let history = metrics.history.clone();
    element(RatatuiView::fill(move |area, buffer| {
        Sparkline::default()
            .data(&history)
            .block(Block::default().borders(Borders::ALL).title(" Requests "))
            .render(area, buffer);
    }))
}

fn dashboard(metrics: &Metrics) -> Element {
    element(
        Flex::column()
            .gap(1)
            .child(Dimension::Fixed(4), table(metrics))
            .child(Dimension::Flex(1), sparkline(metrics))
            .child(
                Dimension::Fixed(1),
                element(KeyHints::new([("q", "quit"), ("r", "refresh")])),
            ),
    )
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> std::io::Result<()> {
    let runner = AsyncRunner::new(RunnerConfig {
        tick_rate: Duration::from_millis(500),
    });
    let mut metrics = Metrics::new();

    runner
        .run(
            &Theme::default(),
            &mut metrics,
            |metrics, _frame| dashboard(metrics),
            async |metrics, signal| match signal {
                // Poll on the tick and on `r`; both simply await the fetch.
                Signal::Tick => {
                    metrics.ingest(fetch_stats(metrics.requests).await);
                    ControlFlow::Continue(())
                }
                Signal::Event(Event::Key(key)) if key.plain() => match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => ControlFlow::Break(()),
                    KeyCode::Char('r') => {
                        metrics.ingest(fetch_stats(metrics.requests).await);
                        ControlFlow::Continue(())
                    }
                    _ => ControlFlow::Continue(()),
                },
                _ => ControlFlow::Continue(()),
            },
        )
        .await
}

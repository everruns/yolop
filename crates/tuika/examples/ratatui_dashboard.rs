//! Mixed Tuika/Ratatui dashboard.
//!
//! Demonstrates clipped widget adapters, responsive layouts, live data, the
//! terminal runner, and semantic key hints. Run with:
//! `cargo run -p tuika --example ratatui_dashboard`.

use std::ops::ControlFlow;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use ratatui::layout::Constraint;
use ratatui::text::Line;
use ratatui::widgets::{Bar, BarChart, BarGroup, Block, Borders, Row, Sparkline, Table, Widget};
use tuika::{
    Dimension, Element, Event, Flex, KeyCode, KeyHints, Live, LiveView, RatatuiView, Responsive,
    Runner, RunnerConfig, Theme, element,
};

#[derive(Clone)]
struct Metrics {
    requests: u64,
    errors: u64,
    history: Vec<u64>,
}

fn table(metrics: Metrics) -> Element {
    element(RatatuiView::fill(move |area, buffer| {
        Table::new(
            [
                Row::new(["requests", &metrics.requests.to_string()]),
                Row::new(["errors", &metrics.errors.to_string()]),
            ],
            [Constraint::Percentage(60), Constraint::Percentage(40)],
        )
        .block(Block::bordered().title(" Metrics "))
        .render(area, buffer);
    }))
}

fn sparkline(metrics: Metrics) -> Element {
    element(RatatuiView::fill(move |area, buffer| {
        Sparkline::default()
            .data(&metrics.history)
            .block(Block::bordered().title(" Requests "))
            .render(area, buffer);
    }))
}

fn bars(metrics: Metrics) -> Element {
    element(RatatuiView::fill(move |area, buffer| {
        let bars = [
            Bar::default()
                .value(metrics.requests)
                .label(Line::from("ok")),
            Bar::default()
                .value(metrics.errors)
                .label(Line::from("err")),
        ];
        BarChart::default()
            .data(BarGroup::default().bars(&bars))
            .block(Block::default().borders(Borders::ALL).title(" Totals "))
            .render(area, buffer);
    }))
}

fn dashboard(metrics: &Metrics) -> Element {
    let compact = Flex::column()
        .child(Dimension::Fixed(4), table(metrics.clone()))
        .child(Dimension::Flex(1), sparkline(metrics.clone()));
    let wide = Flex::row()
        .gap(1)
        .child(Dimension::Flex(1), table(metrics.clone()))
        .child(Dimension::Flex(2), sparkline(metrics.clone()))
        .child(Dimension::Flex(1), bars(metrics.clone()));

    element(
        Flex::column()
            .gap(1)
            .child(
                Dimension::Flex(1),
                element(Responsive::new(70, element(compact), element(wide))),
            )
            .child(
                Dimension::Fixed(1),
                element(KeyHints::new([("q", "quit"), ("resize", "reflow")])),
            ),
    )
}

fn main() -> std::io::Result<()> {
    let runner = Runner::new(RunnerConfig::default());
    let metrics = Live::with_redraw(
        Metrics {
            requests: 1,
            errors: 0,
            history: vec![1],
        },
        runner.redraw_handle(),
    );

    let stop = Arc::new(AtomicBool::new(false));
    let worker_stop = Arc::clone(&stop);
    let worker_metrics = metrics.clone();
    let worker = thread::spawn(move || {
        while !worker_stop.load(Ordering::Acquire) {
            thread::sleep(Duration::from_millis(500));
            worker_metrics.update(|metrics| {
                metrics.requests += 1;
                metrics.errors = metrics.requests / 7;
                metrics.history.push(metrics.requests % 20);
                if metrics.history.len() > 40 {
                    metrics.history.remove(0);
                }
            });
        }
    });

    let live_view = metrics.clone();
    let result = runner.run(
        &Theme::default(),
        move |_| element(LiveView::new(live_view.clone(), dashboard)),
        |event| match event {
            Event::Key(key)
                if key.plain() && matches!(key.code, KeyCode::Char('q') | KeyCode::Esc) =>
            {
                ControlFlow::Break(())
            }
            _ => ControlFlow::Continue(()),
        },
    );

    stop.store(true, Ordering::Release);
    let _ = worker.join();
    result
}

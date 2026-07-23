//! Rendering benchmarks for the windowed [`Scroll`] viewport.
//!
//! Run with `cargo bench -p tuika --bench scroll`. Save a baseline to compare
//! against later with `cargo bench -p tuika --bench scroll -- --save-baseline
//! <name>`, then `--baseline <name>` on a later run to see the delta. Criterion
//! writes machine-readable results under `target/criterion/**/estimates.json`.
//!
//! [`Scroll`] is the primitive that replaces native scrollback in the
//! full-screen renderer: it holds the *whole* pre-wrapped transcript but paints
//! only the `area.height` rows at the current offset. The defining property is
//! that a frame costs O(viewport), not O(transcript) — these benches exist to
//! keep it that way. Four groups:
//!
//! - `render` — paint one frame at the bottom (stick-to-bottom) offset across a
//!   sweep of transcript *sizes* at a fixed viewport. The times must stay flat
//!   as the transcript grows; a time that tracks the size is the regression
//!   (windowing quietly turned into a full walk).
//! - `render_offset` — paint one frame of a large transcript at the top vs the
//!   bottom offset. Where the visible window sits must not change the cost.
//! - `paging` — drive [`ScrollState`] from top to bottom one page at a time.
//!   Each event is O(1); the traversal is O(transcript / viewport), and this
//!   guards the event path against sneaking in per-row work.
//! - `frame` — the realistic per-frame cost of feeding [`Scroll`]: build the
//!   rows, construct, paint, drop. `full` clones the whole transcript into
//!   [`Scroll::new`] every frame (what the renderer paid before windowing);
//!   `windowed` clones only the visible slice into [`Scroll::windowed`]. The gap
//!   between them, widening with transcript size, is the win — `full` is
//!   O(transcript), `windowed` is O(viewport).

use std::hint::black_box;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use tuika::{Event, Key, KeyCode, RenderCtx, Scroll, ScrollState, Surface, Theme, View};

/// Fixed viewport the render benches paint into — a typical terminal transcript
/// pane. Only these `HEIGHT` rows are ever drawn, whatever the transcript size.
const WIDTH: u16 = 80;
const HEIGHT: u16 = 24;

/// Transcript sizes, in pre-wrapped rows. The point of the sweep is that `render`
/// time stays flat across it; `large` is past `u16::MAX` so it also covers the
/// long-session regime the offset/height arithmetic is now `usize` for.
const SIZES: &[(&str, usize)] = &[("small", 100), ("medium", 10_000), ("large", 100_000)];

/// Build a transcript of `rows` pre-wrapped lines. Deterministic (no randomness)
/// so runs compare, and each row carries a couple of styled spans — a gutter and
/// a body — so `render` does representative per-span painting rather than one
/// trivial write.
fn transcript(rows: usize) -> Vec<Line<'static>> {
    (0..rows)
        .map(|i| {
            Line::from(vec![
                Span::styled(format!("{i:>6} │ "), Style::default().fg(Color::DarkGray)),
                Span::styled(
                    "the quick brown fox jumps over the lazy dog once more",
                    Style::default().fg(Color::Gray),
                ),
            ])
        })
        .collect()
}

/// A `Scroll` over a fresh `rows`-row transcript, offset to the bottom (the
/// live-transcript default) unless `top` pins it to the first row.
fn scroll_at(rows: usize, top: bool) -> Scroll {
    let mut state = ScrollState::new();
    if top {
        state.jump_to_top();
    } else {
        state.clamp(rows, HEIGHT as usize);
    }
    Scroll::new(transcript(rows), &state)
}

/// Paint one frame. The `Surface` is rebuilt each call (it just re-borrows the
/// buffer — no allocation), and the buffer is reused, so the measured work is
/// the windowing + span painting, not viewport allocation.
fn paint_frame(scroll: &Scroll, buffer: &mut Buffer, area: Rect, ctx: &RenderCtx) {
    let mut surface = Surface::new(buffer, area);
    scroll.render(black_box(area), &mut surface, ctx);
}

fn render(c: &mut Criterion) {
    let theme = Theme::default();
    let ctx = RenderCtx::new(&theme);
    let area = Rect::new(0, 0, WIDTH, HEIGHT);
    let mut group = c.benchmark_group("scroll/render");
    for &(name, rows) in SIZES {
        let scroll = scroll_at(rows, false);
        let mut buffer = Buffer::empty(area);
        group.bench_function(name, |b| {
            b.iter(|| paint_frame(&scroll, &mut buffer, area, &ctx));
        });
    }
    group.finish();
}

fn render_offset(c: &mut Criterion) {
    let theme = Theme::default();
    let ctx = RenderCtx::new(&theme);
    let area = Rect::new(0, 0, WIDTH, HEIGHT);
    let rows = 100_000;
    let mut group = c.benchmark_group("scroll/render_offset");
    for &(name, top) in &[("top", true), ("bottom", false)] {
        let scroll = scroll_at(rows, top);
        let mut buffer = Buffer::empty(area);
        group.bench_function(name, |b| {
            b.iter(|| paint_frame(&scroll, &mut buffer, area, &ctx));
        });
    }
    group.finish();
}

fn paging(c: &mut Criterion) {
    let mut group = c.benchmark_group("scroll/paging");
    let page_down = Event::Key(Key::new(KeyCode::PageDown));
    for &(name, rows) in SIZES {
        group.bench_with_input(BenchmarkId::from_parameter(name), &rows, |b, &rows| {
            b.iter(|| {
                let mut state = ScrollState::new();
                state.jump_to_top();
                // Page to the bottom; `handle` re-arms stick-to-bottom at the end.
                while !state.is_stuck_to_bottom() {
                    state.handle(black_box(&page_down), rows, HEIGHT as usize);
                }
                black_box(state.offset())
            });
        });
    }
    group.finish();
}

fn frame(c: &mut Criterion) {
    let theme = Theme::default();
    let ctx = RenderCtx::new(&theme);
    let area = Rect::new(0, 0, WIDTH, HEIGHT);
    let h = HEIGHT as usize;
    let mut group = c.benchmark_group("scroll/frame");
    for &(name, rows) in SIZES {
        let content = transcript(rows);
        let mut state = ScrollState::new();
        state.clamp(rows, h); // bottom-stuck
        let offset = state.offset();
        let mut buffer = Buffer::empty(area);

        // Pre-windowing: clone the whole transcript into a fresh Scroll, paint,
        // drop it — every frame. O(transcript).
        group.bench_with_input(BenchmarkId::new("full", name), &content, |b, content| {
            b.iter(|| {
                let scroll = Scroll::new(content.clone(), &state);
                paint_frame(&scroll, &mut buffer, area, &ctx);
            });
        });

        // Windowed: clone only the visible slice. O(viewport), flat across sizes.
        group.bench_with_input(
            BenchmarkId::new("windowed", name),
            &content,
            |b, content| {
                b.iter(|| {
                    let end = (offset + h).min(content.len());
                    let window = content[offset..end].to_vec();
                    let scroll = Scroll::windowed(window, content.len(), &state);
                    paint_frame(&scroll, &mut buffer, area, &ctx);
                });
            },
        );
    }
    group.finish();
}

criterion_group!(benches, render, render_offset, paging, frame);
criterion_main!(benches);

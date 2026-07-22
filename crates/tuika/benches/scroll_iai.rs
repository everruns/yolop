//! Instruction-count benchmarks for the windowed [`Scroll`] viewport
//! (iai-callgrind).
//!
//! Unlike the wall-clock `scroll` bench, these count CPU instructions under
//! Valgrind/callgrind, which is *deterministic and machine-independent* — so the
//! numbers can be committed as a baseline and a regression can fail CI. See
//! `crates/tuika/benches/check_iai.py` and the committed `iai-baseline.json` next
//! to this file.
//!
//! Requires Valgrind and a version-matched `iai-callgrind-runner` on PATH
//! (`cargo install iai-callgrind-runner`). Run with
//! `cargo bench -p tuika --bench scroll_iai`.
//!
//! `Scroll` paints only the visible window, so a frame is O(viewport) no matter
//! how tall the transcript is. `render.small` (100 rows) and `render.large`
//! (100k rows) render the *same* viewport: their instruction counts should be
//! nearly equal, and the baseline gate turns "render started scaling with the
//! transcript" into a CI failure. `paging.large` guards the O(1)-per-event
//! `ScrollState` path across a full top-to-bottom traversal.

use std::hint::black_box;

use iai_callgrind::{library_benchmark, library_benchmark_group, main};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use tuika::{Event, Key, KeyCode, RenderCtx, Scroll, ScrollState, Surface, Theme, View};

const WIDTH: u16 = 80;
const HEIGHT: u16 = 24;

/// A `rows`-row transcript of styled, pre-wrapped lines — a gutter span plus a
/// body span each — matching the wall-clock `scroll` bench's corpus.
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

/// A `Scroll` over a fresh `rows`-row transcript pinned to the bottom (the
/// live-transcript default). Built in iai *setup*, so the corpus construction is
/// untimed and only the render is measured.
fn scroll_at_bottom(rows: usize) -> Scroll {
    let mut state = ScrollState::new();
    state.clamp(rows, HEIGHT as usize);
    Scroll::new(transcript(rows), &state)
}

#[library_benchmark]
#[bench::small(scroll_at_bottom(100))]
#[bench::large(scroll_at_bottom(100_000))]
fn render(scroll: Scroll) -> usize {
    let theme = Theme::default();
    let ctx = RenderCtx::new(&theme);
    let area = Rect::new(0, 0, WIDTH, HEIGHT);
    let mut buffer = Buffer::empty(area);
    {
        let mut surface = Surface::new(&mut buffer, area);
        scroll.render(black_box(area), &mut surface, &ctx);
    }
    // Fold the painted cells so the render can't be optimized away.
    let mut painted = 0usize;
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            if buffer[(x, y)].symbol() != " " {
                painted += 1;
            }
        }
    }
    // Isolate the O(viewport) paint: dropping the `large` transcript here would
    // be an O(rows) free that swamps the render we're measuring (and would make
    // `render.large` scale with the transcript, defeating the whole point of the
    // case). Leak it — the bench process exits the moment this returns. The
    // wall-clock `scroll` bench avoids this by building the `Scroll` once,
    // outside the measured closure.
    std::mem::forget(scroll);
    black_box(painted)
}

#[library_benchmark]
#[bench::large(100_000)]
fn paging(rows: usize) -> usize {
    let mut state = ScrollState::new();
    state.jump_to_top();
    let page_down = Event::Key(Key::new(KeyCode::PageDown));
    // Page from top to bottom; `handle` re-arms stick-to-bottom at the end.
    while !state.is_stuck_to_bottom() {
        state.handle(black_box(&page_down), rows, HEIGHT as usize);
    }
    black_box(state.offset())
}

library_benchmark_group!(
    name = scroll_iai;
    benchmarks = render, paging
);
main!(library_benchmark_groups = scroll_iai);

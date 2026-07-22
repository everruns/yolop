//! Rendering benchmarks for the streaming `Markdown` renderer.
//!
//! Run with `cargo bench -p tuika --bench markdown`. Save a baseline to compare
//! against later with `cargo bench -p tuika --bench markdown -- --save-baseline
//! <name>`, then `--baseline <name>` on a later run to see the delta. Criterion
//! writes machine-readable results under `target/criterion/**/estimates.json`.
//!
//! Four groups, each over a matrix of document *shapes* and *sizes*:
//!
//! - `one_shot` — [`markdown_to_lines`] over a whole document, the cost of a
//!   static render.
//! - `streaming` — feed the document in stream-sized deltas through
//!   [`MarkdownState`], rendering every delta. This is the realistic live-
//!   transcript cost.
//! - `streaming_model` — the same streamed input rendered two ways: the cached
//!   [`MarkdownState`] versus a naive renderer that re-parses the whole buffer
//!   on every delta. The gap is what the settled-prefix cache buys, and the
//!   guard against a refactor quietly turning the cache back into full re-parse.
//! - `reflow` — re-render a *settled* document across a sweep of widths (a
//!   terminal resize). The parse cache is width-independent, so this exercises
//!   re-flatten only and guards that a resize never silently re-parses. A theme
//!   change is the opposite (it invalidates the cache and re-parses); its cost
//!   is approximately `one_shot`, so it isn't benched separately.
//!
//! Highlighting is deliberately [`CodeHighlighter::Plain`] here: the tree-sitter
//! cost lives in `tuika-codeformatters`' own `highlight` bench, and pulling that
//! crate in would be a dependency cycle. These benches isolate parse + layout.

use std::hint::black_box;

use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use tuika::{CodeHighlighter, MarkdownState, Theme, markdown_to_lines};

use corpus::Shape;

/// Fixed render width — wide enough to exercise prose wrapping without dwarfing
/// the parse cost.
const WIDTH: u16 = 80;

/// Document sizes for the one-shot bench, as the repetition `scale` handed to
/// [`corpus::doc`]. The byte size each produces is reported per-case via
/// [`Throughput::Bytes`].
const SIZES: &[(&str, usize)] = &[("small", 1), ("medium", 8), ("large", 40)];

/// Smaller sizes for the streaming benches. A streamed render re-flattens the
/// settled prefix on every delta, so its cost grows quadratically with document
/// length; the `large` scale here is capped so a run finishes quickly. (That
/// quadratic is a known optimization target, not a property to bench to death.)
const STREAM_SIZES: &[(&str, usize)] = &[("small", 1), ("medium", 4), ("large", 12)];

/// Scale for the cached-vs-naive comparison — large enough that the naive
/// full-reparse is clearly worse, small enough to stay fast.
const MODEL_SCALE: usize = 12;

/// Reduced sample count for the streaming groups: their slowest cases run in the
/// tens of milliseconds, so the Criterion default of 100 would be needlessly
/// slow. Fewer samples still give a usable estimate and CI for a trend.
const STREAM_SAMPLES: usize = 30;

/// Document shapes, each stressing a different part of the renderer.
const SHAPES: &[(&str, Shape)] = &[
    ("prose", Shape::Prose),
    ("list", Shape::List),
    ("code", Shape::Code),
    ("mixed", Shape::Mixed),
];

/// Stream delta size in bytes — roughly a token's worth, so a document streams
/// in many small pushes the way a provider emits it.
const DELTA: usize = 16;

/// Render widths swept during the `reflow` (resize) bench, narrow to wide.
const REFLOW_WIDTHS: &[u16] = &[40, 60, 80, 100, 120];

fn one_shot(c: &mut Criterion) {
    let theme = Theme::default();
    let mut group = c.benchmark_group("markdown/one_shot");
    for &(shape_name, shape) in SHAPES {
        for &(size_name, scale) in SIZES {
            let src = corpus::doc(shape, scale);
            group.throughput(Throughput::Bytes(src.len() as u64));
            group.bench_with_input(BenchmarkId::new(shape_name, size_name), &src, |b, src| {
                b.iter(|| {
                    black_box(markdown_to_lines(
                        black_box(src),
                        WIDTH,
                        &theme,
                        CodeHighlighter::Plain,
                    ));
                });
            });
        }
    }
    group.finish();
}

fn streaming(c: &mut Criterion) {
    let theme = Theme::default();
    let mut group = c.benchmark_group("markdown/streaming");
    group.sample_size(STREAM_SAMPLES);
    for &(shape_name, shape) in SHAPES {
        for &(size_name, scale) in STREAM_SIZES {
            let src = corpus::doc(shape, scale);
            let deltas = corpus::deltas(&src, DELTA);
            group.throughput(Throughput::Bytes(src.len() as u64));
            group.bench_with_input(
                BenchmarkId::new(shape_name, size_name),
                &deltas,
                |b, deltas| {
                    b.iter(|| {
                        let mut md = MarkdownState::new();
                        for delta in deltas {
                            md.push_str(delta);
                            black_box(md.lines(WIDTH, &theme, CodeHighlighter::Plain));
                        }
                    });
                },
            );
        }
    }
    group.finish();
}

fn streaming_model(c: &mut Criterion) {
    let theme = Theme::default();
    let mut group = c.benchmark_group("markdown/streaming_model");
    group.sample_size(STREAM_SAMPLES);
    // The cache pays off most on a large transcript, where the naive renderer
    // re-parses the whole settled prefix on every delta.
    for &(shape_name, shape) in &[("mixed", Shape::Mixed), ("code", Shape::Code)] {
        let src = corpus::doc(shape, MODEL_SCALE);
        let deltas = corpus::deltas(&src, DELTA);
        group.throughput(Throughput::Bytes(src.len() as u64));

        group.bench_with_input(
            BenchmarkId::new("cached", shape_name),
            &deltas,
            |b, deltas| {
                b.iter(|| {
                    let mut md = MarkdownState::new();
                    for delta in deltas {
                        md.push_str(delta);
                        black_box(md.lines(WIDTH, &theme, CodeHighlighter::Plain));
                    }
                });
            },
        );

        group.bench_with_input(
            BenchmarkId::new("naive_full_reparse", shape_name),
            &deltas,
            |b, deltas| {
                b.iter(|| {
                    let mut acc = String::new();
                    for delta in deltas {
                        acc.push_str(delta);
                        black_box(markdown_to_lines(
                            &acc,
                            WIDTH,
                            &theme,
                            CodeHighlighter::Plain,
                        ));
                    }
                });
            },
        );
    }
    group.finish();
}

fn reflow(c: &mut Criterion) {
    let theme = Theme::default();
    let mut group = c.benchmark_group("markdown/reflow");
    group.sample_size(STREAM_SAMPLES);
    for &(shape_name, shape) in SHAPES {
        for &(size_name, scale) in SIZES {
            let src = corpus::doc(shape, scale);
            group.throughput(Throughput::Bytes(src.len() as u64));
            group.bench_with_input(BenchmarkId::new(shape_name, size_name), &src, |b, src| {
                b.iter_batched(
                    || {
                        // Prime the settled-prefix cache once, outside the measured
                        // closure, so the sweep measures re-flatten and not the
                        // initial parse. Every corpus doc ends on a blank line, so
                        // the whole document settles and the tail is empty.
                        let mut md = MarkdownState::new();
                        md.set(src.clone());
                        md.lines(REFLOW_WIDTHS[0], &theme, CodeHighlighter::Plain);
                        md
                    },
                    |mut md| {
                        // A drag-resize: the same settled document re-rendered at a
                        // sweep of widths. The parse cache is width-independent, so
                        // this must never re-parse — only re-flatten.
                        for &width in REFLOW_WIDTHS {
                            black_box(md.lines(width, &theme, CodeHighlighter::Plain));
                        }
                    },
                    BatchSize::SmallInput,
                );
            });
        }
    }
    group.finish();
}

criterion_group!(benches, one_shot, streaming, streaming_model, reflow);
criterion_main!(benches);

/// Synthetic markdown documents for the benchmarks. Kept deterministic (no
/// randomness) so runs are comparable, and parameterized by a repetition
/// `scale` so the same shape can be benched small, medium, and large.
mod corpus {
    /// The document shapes we bench. Each stresses a different renderer path:
    /// [`Prose`](Shape::Prose) the word-wrapper and inline-emphasis splitter,
    /// [`List`](Shape::List) nested block layout, [`Code`](Shape::Code) fenced-
    /// block handling (the tail a highlighter would run over), and
    /// [`Mixed`](Shape::Mixed) a realistic assistant answer combining all three.
    #[derive(Clone, Copy)]
    pub enum Shape {
        Prose,
        List,
        Code,
        Mixed,
    }

    const PROSE: &str = "Yolop streams **markdown** with _inline emphasis_, `inline code`, and the \
odd [link](https://example.com/docs). Sentences wrap to the render width, so a \
paragraph of several lines exercises the word-wrapper and the emphasis span-\
splitter together rather than in isolation.\n\n";

    const LIST: &str = "- top-level item with some **bold** text\n  - nested item with `code`\n    \
- twice-nested item that runs long enough to wrap across the render width once\n  \
- back to one level of nesting\n- another top-level item\n\n";

    const CODE: &str = "```rust\nfn compute(n: usize) -> usize {\n    (0..n)\n        .map(|i| i * i)\n\
        .filter(|x| x % 2 == 0)\n        .sum()\n}\n```\n\n";

    /// One "unit" of the mixed shape: a heading, a paragraph, a short list, and
    /// a code block — the texture of a real assistant answer.
    fn mixed_unit(n: usize) -> String {
        format!("## Section {n}\n\n{PROSE}{LIST}{CODE}")
    }

    /// Build a document of the given `shape`, repeated `scale` times.
    pub fn doc(shape: Shape, scale: usize) -> String {
        let scale = scale.max(1);
        match shape {
            Shape::Prose => PROSE.repeat(scale),
            Shape::List => LIST.repeat(scale),
            Shape::Code => CODE.repeat(scale),
            Shape::Mixed => (0..scale).map(mixed_unit).collect(),
        }
    }

    /// Split `doc` into stream-sized deltas of about `chunk` bytes each, always
    /// on a UTF-8 char boundary — mimicking how a provider streams tokens into
    /// [`MarkdownState`](tuika::MarkdownState).
    pub fn deltas(doc: &str, chunk: usize) -> Vec<&str> {
        let chunk = chunk.max(1);
        let mut out = Vec::new();
        let mut start = 0;
        while start < doc.len() {
            let mut end = (start + chunk).min(doc.len());
            while end < doc.len() && !doc.is_char_boundary(end) {
                end += 1;
            }
            out.push(&doc[start..end]);
            start = end;
        }
        out
    }
}

//! Instruction-count benchmarks for the `Markdown` renderer (iai-callgrind).
//!
//! Unlike the wall-clock `markdown` bench, these count CPU instructions under
//! Valgrind/callgrind, which is *deterministic and machine-independent* — so the
//! numbers can be committed as a baseline and a regression can fail CI. See
//! `crates/tuika/benches/check_iai.py` and the committed `iai-baseline.json` next to this
//! file.
//!
//! Requires Valgrind and a version-matched `iai-callgrind-runner` on PATH
//! (`cargo install iai-callgrind-runner`). Run with
//! `cargo bench -p tuika --bench markdown_iai`.
//!
//! A focused set of the highest-signal cases, not the full wall-clock matrix:
//! instruction-count runs are slower (under Valgrind) and exist to gate
//! regressions, not to explore the size curve.

use std::hint::black_box;

use iai_callgrind::{library_benchmark, library_benchmark_group, main};
use tuika::{CodeHighlighter, MarkdownState, Theme, markdown_to_lines};

const WIDTH: u16 = 80;

/// One unit of a realistic mixed document — heading, prose with inline emphasis,
/// a nested list, and a fenced code block.
const UNIT: &str = "## Section\n\nYolop streams **markdown** with _emphasis_, `code`, and a \
[link](https://example.com). This paragraph is long enough to wrap at the render \
width, exercising the word-wrapper.\n\n- item with **bold**\n  - nested `code`\n\n\
```rust\nfn compute(n: usize) -> usize {\n    (0..n).map(|i| i * i).sum()\n}\n```\n\n";

fn doc(scale: usize) -> String {
    UNIT.repeat(scale)
}

/// Split into ~16-byte, char-boundary-safe deltas — a provider's token stream.
/// Returns owned `String`s so the value crosses the iai setup boundary cleanly.
fn deltas(source: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut start = 0;
    while start < source.len() {
        let mut end = (start + 16).min(source.len());
        while end < source.len() && !source.is_char_boundary(end) {
            end += 1;
        }
        out.push(source[start..end].to_string());
        start = end;
    }
    out
}

#[library_benchmark]
#[bench::small(doc(1))]
#[bench::large(doc(24))]
fn one_shot(source: String) -> usize {
    let theme = Theme::default();
    black_box(markdown_to_lines(
        black_box(&source),
        WIDTH,
        &theme,
        CodeHighlighter::Plain,
    ))
    .len()
}

#[library_benchmark]
#[bench::mixed(deltas(&doc(8)))]
fn streaming(deltas: Vec<String>) -> usize {
    let theme = Theme::default();
    let mut md = MarkdownState::new();
    let mut last = 0;
    for delta in &deltas {
        md.push_str(delta);
        last = black_box(md.lines(WIDTH, &theme, CodeHighlighter::Plain)).len();
    }
    last
}

#[library_benchmark]
#[bench::mixed(doc(12))]
fn reflow(source: String) -> usize {
    let theme = Theme::default();
    let mut md = MarkdownState::new();
    md.set(source);
    let mut last = 0;
    // A resize: re-render the settled document across a width sweep. The parse
    // cache is width-independent, so this should re-flatten only.
    for width in [40u16, 60, 80, 100, 120] {
        last = black_box(md.lines(width, &theme, CodeHighlighter::Plain)).len();
    }
    last
}

library_benchmark_group!(
    name = markdown_iai;
    benchmarks = one_shot, streaming, reflow
);
main!(library_benchmark_groups = markdown_iai);

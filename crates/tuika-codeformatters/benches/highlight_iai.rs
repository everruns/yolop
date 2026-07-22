//! Instruction-count benchmarks for the tree-sitter highlighter (iai-callgrind).
//!
//! Deterministic, machine-independent instruction counts for the committed
//! baseline / CI regression gate — the counterpart to the wall-clock `highlight`
//! bench. See `crates/tuika/benches/check_iai.py` and the committed `iai-baseline.json` next
//! to this file. Requires Valgrind + a version-matched `iai-callgrind-runner`.
//! Run with `cargo bench -p tuika-codeformatters --bench highlight_iai`.
//!
//! Each case highlights a sizeable snippet so the one-time grammar-config build
//! (thread-local, paid once per process) is a small fraction of the measured
//! instructions and the highlight loop dominates. Two representative grammars
//! keep the gate fast; add languages here if a regression slips through.

use std::hint::black_box;

use iai_callgrind::{library_benchmark, library_benchmark_group, main};
use tuika::{Highlighter, Theme};
use tuika_codeformatters::TreeSitterHighlighter;

const RUST: &str = "use std::collections::HashMap;\n\n\
fn word_counts(text: &str) -> HashMap<&str, usize> {\n\
    let mut counts = HashMap::new();\n\
    for word in text.split_whitespace() {\n\
        *counts.entry(word).or_insert(0) += 1;\n\
    }\n\
    counts\n\
}\n";

const PYTHON: &str = "from collections import Counter\n\n\
def word_counts(text: str) -> Counter:\n\
    \"\"\"Count words, case-insensitively.\"\"\"\n\
    return Counter(w.lower() for w in text.split())\n";

fn repeat(snippet: &str, reps: usize) -> String {
    snippet.repeat(reps)
}

#[library_benchmark]
#[bench::rust("rust", repeat(RUST, 40))]
#[bench::python("python", repeat(PYTHON, 40))]
fn highlight(lang: &str, source: String) -> usize {
    let highlighter = TreeSitterHighlighter::new();
    let theme = Theme::default();
    let lines: Vec<&str> = source.lines().collect();
    black_box(highlighter.highlight(lang, black_box(&lines), &theme))
        .map(|v| v.len())
        .unwrap_or(0)
}

library_benchmark_group!(name = highlight_iai; benchmarks = highlight);
main!(library_benchmark_groups = highlight_iai);

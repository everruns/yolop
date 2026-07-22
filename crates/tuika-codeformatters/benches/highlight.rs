//! Highlighting benchmarks for the tree-sitter [`TreeSitterHighlighter`].
//!
//! Run with `cargo bench -p tuika-codeformatters --bench highlight`. Use
//! `--save-baseline <name>` / `--baseline <name>` to track a change; Criterion
//! writes machine-readable results under `target/criterion/**/estimates.json`.
//!
//! One group, `highlight`, over a matrix of language × size. Each case is one
//! `highlight` call — grammar config lookup, parse, and event walk. The parser
//! configurations are cached thread-locally on first use, and Criterion's warm-
//! up primes that cache, so the reported number is the *steady-state* per-call
//! cost — the cost paid for the in-flight code fence on every streamed delta,
//! which is the number that matters for a live transcript.

use std::hint::black_box;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use tuika::{Highlighter, Theme};
use tuika_codeformatters::TreeSitterHighlighter;

/// Code sizes, as the repetition count applied to each snippet. The byte size
/// each produces is reported per-case via [`Throughput::Bytes`].
const SIZES: &[(&str, usize)] = &[("small", 1), ("medium", 10), ("large", 50)];

fn highlight(c: &mut Criterion) {
    let highlighter = TreeSitterHighlighter::new();
    let theme = Theme::default();
    let mut group = c.benchmark_group("highlight");
    for &(lang, snippet) in corpus::SNIPPETS {
        for &(size_name, reps) in SIZES {
            let src = corpus::repeat(snippet, reps);
            let lines: Vec<&str> = src.lines().collect();
            group.throughput(Throughput::Bytes(src.len() as u64));
            group.bench_with_input(BenchmarkId::new(lang, size_name), &lines, |b, lines| {
                b.iter(|| {
                    black_box(highlighter.highlight(lang, black_box(lines), &theme));
                });
            });
        }
    }
    group.finish();
}

criterion_group!(benches, highlight);
criterion_main!(benches);

/// Representative snippets per language. Kept deterministic and syntactically
/// valid so every case highlights (rather than falling back), and repeated to
/// reach the larger sizes.
mod corpus {
    /// `(language tag, snippet)` pairs spanning the grammars most common in
    /// assistant answers.
    pub const SNIPPETS: &[(&str, &str)] = &[
        (
            "rust",
            "use std::collections::HashMap;\n\n\
fn word_counts(text: &str) -> HashMap<&str, usize> {\n\
    let mut counts = HashMap::new();\n\
    for word in text.split_whitespace() {\n\
        *counts.entry(word).or_insert(0) += 1;\n\
    }\n\
    counts\n\
}\n",
        ),
        (
            "python",
            "from collections import Counter\n\n\
def word_counts(text: str) -> Counter:\n\
    \"\"\"Count words, case-insensitively.\"\"\"\n\
    return Counter(w.lower() for w in text.split())\n\n\
if __name__ == \"__main__\":\n\
    print(word_counts(\"a b a c\"))\n",
        ),
        (
            "typescript",
            "interface User {\n\
  id: number;\n\
  name: string;\n\
}\n\n\
export function greet(users: User[]): string[] {\n\
  return users.map((u) => `Hello, ${u.name}!`);\n\
}\n",
        ),
        (
            "go",
            "package main\n\n\
import \"fmt\"\n\n\
func sum(nums []int) int {\n\
\ttotal := 0\n\
\tfor _, n := range nums {\n\
\t\ttotal += n\n\
\t}\n\
\treturn total\n\
}\n",
        ),
        (
            "ruby",
            "class Greeter\n\
  def initialize(name)\n\
    @name = name\n\
  end\n\n\
  def greet\n\
    puts \"Hello, #{@name}!\"\n\
  end\n\
end\n",
        ),
        (
            "sql",
            "SELECT u.name, COUNT(o.id) AS orders\n\
FROM users u\n\
LEFT JOIN orders o ON o.user_id = u.id\n\
WHERE u.active = TRUE\n\
GROUP BY u.name\n\
ORDER BY orders DESC;\n",
        ),
    ];

    /// Repeat a snippet `reps` times to reach a larger size.
    pub fn repeat(snippet: &str, reps: usize) -> String {
        snippet.repeat(reps.max(1))
    }
}

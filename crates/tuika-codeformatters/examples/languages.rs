//! Syntax-highlighted code across languages, via [`TreeSitterHighlighter`] +
//! tuika's [`CodeBlock`](tuika::CodeBlock). Run with
//! `cargo run -p tuika-codeformatters --example languages`
//! (←/→ or Tab switch language · q or esc quit).
//!
//! Each tab feeds a snippet through the highlighter and renders it in a themed
//! code block; the same highlighter powers fenced blocks in tuika's `Markdown`.

use std::io;
use std::time::Duration;

use crossterm::event;
use ratatui::backend::CrosstermBackend;
use ratatui::text::{Line, Span};
use ratatui::{Terminal, TerminalOptions, Viewport};

use tuika::{
    CodeBlock, Event, KeyCode, Padding, Tabs, TabsState, TerminalSession, Text, Theme, paint,
    translate_event, view,
};
use tuika_codeformatters::TreeSitterHighlighter;

/// A `'static` highlighter so the borrowed `CodeBlock` can enter the `view!`
/// DSL (which boxes children into `'static` elements).
static HL: TreeSitterHighlighter = TreeSitterHighlighter;

/// (tab label, fence language tag, source) per language.
const SNIPPETS: &[(&str, &str, &str)] = &[
    (
        "Rust",
        "rust",
        r#"
use std::collections::HashMap;

// Memoized Fibonacci.
fn fib(n: u64, cache: &mut HashMap<u64, u64>) -> u64 {
    if n < 2 {
        return n;
    }
    if let Some(&v) = cache.get(&n) {
        return v;
    }
    let v = fib(n - 1, cache) + fib(n - 2, cache);
    cache.insert(n, v);
    v
}
"#,
    ),
    (
        "Python",
        "python",
        r#"
from functools import lru_cache

@lru_cache(maxsize=None)
def fib(n: int) -> int:
    """Return the nth Fibonacci number."""
    if n < 2:
        return n
    return fib(n - 1) + fib(n - 2)

print([fib(i) for i in range(10)])
"#,
    ),
    (
        "TypeScript",
        "ts",
        r#"
interface User {
  id: number;
  name: string;
}

// Greet a user based on the time of day.
async function greet(user: User): Promise<string> {
  const hour = new Date().getHours();
  const part = hour < 12 ? "morning" : "afternoon";
  return `Good ${part}, ${user.name}!`;
}
"#,
    ),
    (
        "Go",
        "go",
        r#"
package main

import "fmt"

// fib returns the nth Fibonacci number.
func fib(n int) int {
    if n < 2 {
        return n
    }
    return fib(n-1) + fib(n-2)
}

func main() {
    fmt.Println(fib(10))
}
"#,
    ),
    (
        "Ruby",
        "ruby",
        r#"
# A tiny memoized Fibonacci.
def fib(n, cache = {})
  return n if n < 2
  cache[n] ||= fib(n - 1, cache) + fib(n - 2, cache)
end

puts (0...10).map { |i| fib(i) }.inspect
"#,
    ),
    (
        "SQL",
        "sql",
        r#"
-- Top 5 customers by paid revenue.
SELECT c.name, SUM(o.total) AS revenue
FROM customers AS c
JOIN orders AS o ON o.customer_id = c.id
WHERE o.status = 'paid'
GROUP BY c.name
ORDER BY revenue DESC
LIMIT 5;
"#,
    ),
];

fn main() -> io::Result<()> {
    let mut tabs = TabsState::default();

    let _session = TerminalSession::enter()?;
    let mut terminal = Terminal::with_options(
        CrosstermBackend::new(io::stdout()),
        TerminalOptions {
            viewport: Viewport::Fullscreen,
        },
    )?;
    let theme = Theme::default();

    loop {
        terminal.draw(|f| {
            let area = f.area();
            let (_, lang, src) = SNIPPETS[tabs.selected()];
            let labels: Vec<Line> = SNIPPETS
                .iter()
                .map(|(label, _, _)| Line::from(Span::styled((*label).to_string(), theme.text_style())))
                .collect();
            // `CodeBlock` renders its own language label, so hide the box title's.
            let code = CodeBlock::new(lang, src.trim()).highlighter(&HL);
            let root = view! {
                col(padding = Padding::all(1), gap = 1) {
                    fixed(1) { node(Tabs::new(labels, &tabs)) }
                    grow(1) {
                        boxed(title = Line::from(Span::styled(" highlighted ", theme.accent_style()))) {
                            node(code)
                        }
                    }
                    fixed(1) {
                        node(Text::new(vec![Line::from(Span::styled(
                            "←/→ or Tab switch language · q quit",
                            theme.muted_style(),
                        ))]))
                    }
                }
            };
            paint(f.buffer_mut(), area, &theme, root.as_ref(), &[]);
        })?;

        if !event::poll(Duration::from_millis(150))? {
            continue;
        }
        let Some(ev) = translate_event(event::read()?) else {
            continue;
        };
        if let Event::Key(k) = &ev
            && k.plain()
            && matches!(k.code, KeyCode::Char('q') | KeyCode::Esc)
        {
            break;
        }
        tabs.handle(&ev, SNIPPETS.len());
    }

    let _ = terminal.clear();
    drop(terminal);
    Ok(())
}

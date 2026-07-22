//! Streaming Markdown renderer. Run with `cargo run -p tuika --example markdown`
//! (space pause/resume · r restart · q or esc quit).
//!
//! Demonstrates [`MarkdownState`]: a rich CommonMark document is fed in a few
//! glyphs per frame — exactly how a host feeds an assistant message as it
//! streams — while only the in-flight tail re-parses each frame. Fenced code is
//! syntax-highlighted through a tiny self-contained [`Highlighter`](tuika::Highlighter)
//! (see `DemoHighlighter` below); for production highlighting across many
//! languages, use the `tuika-codeformatters` crate.

use std::io;
use std::time::Duration;

use crossterm::event::{self};
use ratatui::backend::CrosstermBackend;
use ratatui::style::{Modifier, Style};
use ratatui::text::Span;
use ratatui::{Terminal, TerminalOptions, Viewport};

use tuika::{
    CodeHighlighter, CodeTheme, Event, Highlighter, KeyCode, MarkdownState, Padding, StatusBar,
    TerminalSession, Text, Theme, paint, translate_event, view,
};

/// A tiny self-contained Rust highlighter — enough to satisfy the
/// [`Highlighter`] seam without a grammar dependency. Real hosts use the
/// `tuika-codeformatters` crate (tree-sitter, many languages).
struct DemoHighlighter;

impl Highlighter for DemoHighlighter {
    fn highlight(
        &self,
        lang: &str,
        lines: &[&str],
        theme: &Theme,
    ) -> Option<Vec<Vec<Span<'static>>>> {
        if lang != "rust" {
            return None;
        }
        Some(
            lines
                .iter()
                .map(|l| highlight_rust(l, &theme.code))
                .collect(),
        )
    }
}

/// Split one line of Rust into styled spans, reconstructing it exactly.
fn highlight_rust(line: &str, code: &CodeTheme) -> Vec<Span<'static>> {
    const KW: &[&str] = &[
        "fn", "let", "mut", "pub", "use", "struct", "enum", "impl", "match", "return", "for", "in",
        "if", "else", "while", "loop", "const", "as", "mod", "trait", "self", "true", "false",
    ];
    let chars: Vec<char> = line.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '/' && chars.get(i + 1) == Some(&'/') {
            let rest: String = chars[i..].iter().collect();
            out.push(Span::styled(
                rest,
                Style::default()
                    .fg(code.comment)
                    .add_modifier(Modifier::ITALIC),
            ));
            break;
        }
        if c == '"' {
            let start = i;
            i += 1;
            while i < chars.len() && chars[i] != '"' {
                i += if chars[i] == '\\' { 2 } else { 1 };
            }
            i = (i + 1).min(chars.len());
            let s: String = chars[start..i].iter().collect();
            out.push(Span::styled(s, Style::default().fg(code.string)));
            continue;
        }
        if c.is_alphanumeric() || c == '_' {
            let start = i;
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();
            let style = if KW.contains(&word.as_str()) {
                Style::default()
                    .fg(code.keyword)
                    .add_modifier(Modifier::BOLD)
            } else if word.chars().all(|d| d.is_ascii_digit()) {
                Style::default().fg(code.constant)
            } else {
                Style::default().fg(code.text)
            };
            out.push(Span::styled(word, style));
            continue;
        }
        let start = i;
        i += 1;
        while i < chars.len() {
            let d = chars[i];
            if d.is_alphanumeric()
                || d == '_'
                || d == '"'
                || (d == '/' && chars.get(i + 1) == Some(&'/'))
            {
                break;
            }
            i += 1;
        }
        let s: String = chars[start..i].iter().collect();
        let style = if s.trim().is_empty() {
            Style::default().fg(code.text)
        } else {
            Style::default().fg(code.punctuation)
        };
        out.push(Span::styled(s, style));
    }
    out
}

const SOURCE: &str = "\
# Release checklist

Ship **only** when CI is green. The renderer streams *this very document*
in as it is typed — see <https://docs.rs/tuika>.

## Steps

1. `cargo test --all-features`
2. Rebase onto `origin/main`
3. Squash & merge

| Stage  | Owner | Gate        |
| ------ | ----- | ----------- |
| build  | ci    | clippy      |
| verify | you   | tests green |

```rust
fn main() {
    let ready = check_ci() && tests_pass();
    assert!(ready, \"do not ship red CI\");
}
```

> Never merge red CI.
";

fn main() -> io::Result<()> {
    let highlighter = DemoHighlighter;
    let doc: Vec<char> = SOURCE.chars().collect();

    let mut state = MarkdownState::new();
    let mut cursor = 0usize;
    let mut paused = false;

    let _session = TerminalSession::enter()?;
    let mut terminal = Terminal::with_options(
        CrosstermBackend::new(io::stdout()),
        TerminalOptions {
            viewport: Viewport::Fullscreen,
        },
    )?;
    let theme = Theme::default();

    loop {
        // Advance the stream: a few glyphs per frame until the document is fully
        // in, then hold. `push_str` is the delta a host would forward verbatim.
        if !paused && cursor < doc.len() {
            let end = (cursor + 3).min(doc.len());
            let chunk: String = doc[cursor..end].iter().collect();
            state.push_str(&chunk);
            cursor = end;
        }

        terminal.draw(|f| {
            let area = f.area();
            let width = area.width.saturating_sub(4);
            let lines = state
                .lines(width, &theme, CodeHighlighter::With(&highlighter))
                .to_vec();
            let status = if cursor < doc.len() {
                format!("streaming… {}/{} glyphs", cursor, doc.len())
            } else {
                "done".to_string()
            };
            let bar = StatusBar::new()
                .left(vec![Span::styled(
                    format!(" {status} "),
                    theme.selection_style(),
                )])
                .right(vec![Span::styled(
                    "space pause · r restart · q quit ",
                    theme.muted_style(),
                )])
                .background(Style::default().bg(theme.surface));
            let root = view! {
                col(padding = Padding::all(1), gap = 1) {
                    grow(1) { node(Text::new(lines)) }
                    fixed(1) { node(bar) }
                }
            };
            paint(f.buffer_mut(), area, &theme, root.as_ref(), &[]);
        })?;

        if !event::poll(Duration::from_millis(70))? {
            continue;
        }
        let Some(Event::Key(k)) = translate_event(event::read()?) else {
            continue;
        };
        if !k.plain() {
            continue;
        }
        match k.code {
            KeyCode::Char('q') | KeyCode::Esc => break,
            KeyCode::Char(' ') => paused = !paused,
            KeyCode::Char('r') => {
                state = MarkdownState::new();
                cursor = 0;
                paused = false;
            }
            _ => {}
        }
    }

    let _ = terminal.clear();
    drop(terminal);
    Ok(())
}

//! Single-component demo harness.
//!
//! It renders one component per scene inside a small labeled frame. It powers
//! two things:
//!
//! - the animated GIFs under `crates/tuika/docs/demos/`, recorded with
//!   [VHS](https://github.com/charmbracelet/vhs) from the tapes in
//!   `crates/tuika/docs/tapes/` (see `crates/tuika/docs/generate.sh`);
//! - a headless text dump used to eyeball a scene without a real terminal.
//!
//! Usage:
//!
//! ```text
//! cargo run -p tuika --example demo -- spinner          # interactive, records a GIF
//! cargo run -p tuika --example demo -- spinner --dump    # print one frame as text
//! cargo run -p tuika --example demo -- list              # list scene names
//! ```
//!
//! Interactive mode enters the alternate screen and animates from a frame
//! counter; press `q` or `Esc` to quit.

use std::io;
use std::time::Duration;

use crossterm::event::{self, Event as CtEvent, KeyCode as CtKeyCode, KeyEventKind};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::{Terminal, TerminalOptions, Viewport};

use tuika::{
    BorderStyle, Element, Event, Key, KeyCode, Loader, Mouse, MouseKind, Padding, ProgressBar,
    Rule, Scroll, ScrollState, SelectList, SelectState, Spinner, SpinnerStyle, StatusBar, Tabs,
    TabsState, Text, TextInput, TextInputState, Theme, element, paint, view,
};

/// Every scene: a name, a one-line blurb, and a pure builder from the frame
/// counter. The registry is the single source of truth for the CLI, the
/// generator, and the docs.
type Build = fn(u64, &Theme) -> Element;

const DEMOS: &[(&str, &str, Build)] = &[
    ("spinner", "frame-cycled activity glyphs", scene_spinner),
    (
        "progress_bar",
        "determinate & indeterminate bars",
        scene_progress,
    ),
    ("loader", "spinner + message + hint row", scene_loader),
    ("text", "styled lines and word-wrapped prose", scene_text),
    ("rule", "titled horizontal separators", scene_rule),
    ("boxed", "borders, titles, and padding", scene_boxed),
    ("flex", "flexbox grow / fixed distribution", scene_flex),
    (
        "scroll",
        "viewport + scrollbar over long content",
        scene_scroll,
    ),
    ("select", "keyboard-navigable selection list", scene_select),
    ("tabs", "host-state tab strip", scene_tabs),
    (
        "status_bar",
        "left / right status segments",
        scene_status_bar,
    ),
    ("textinput", "multi-line edit model", scene_textinput),
];

fn main() -> io::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let name = args.first().map(String::as_str).unwrap_or("list");

    if name == "list" || name == "--list" || name == "-h" || name == "--help" {
        println!("scenes:");
        for (n, blurb, _) in DEMOS {
            println!("  {n:<14} {blurb}");
        }
        return Ok(());
    }

    let Some(&(_, blurb, build)) = DEMOS.iter().find(|(n, _, _)| *n == name) else {
        eprintln!("unknown scene {name:?}; run `list` to see the options");
        std::process::exit(2);
    };

    if args.iter().any(|a| a == "--dump") {
        return dump(name, blurb, build);
    }
    run(name, blurb, build)
}

/// Common chrome: a title/blurb header, a rule, then the scene body.
fn framed(name: &str, blurb: &str, body: Element, theme: &Theme) -> Element {
    let bg = Style::default().bg(theme.background);
    let header = Text::new(vec![Line::from(vec![
        Span::styled(
            name.to_string(),
            theme.accent_style().add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("  {blurb}"), theme.muted_style()),
    ])]);
    view! {
        col(padding = Padding::all(1), gap = 0, background = bg) {
            fixed(1) { node(header) }
            fixed(1) { node(Rule::new().style(theme.muted_style())) }
            fixed(1) { spacer() }
            grow(1) { node(body) }
        }
    }
}

/// Render one frame into an in-memory buffer and print it — no terminal needed.
fn dump(name: &str, blurb: &str, build: Build) -> io::Result<()> {
    let theme = Theme::default();
    let (w, h) = (76u16, 20u16);
    let root = framed(name, blurb, build(24, &theme), &theme);
    let buffer = tuika::testing::render(root.as_ref(), w, h, &theme);
    println!("{}", tuika::testing::grid(&buffer));
    Ok(())
}

/// Interactive loop: animate from a frame counter until `q`/`Esc`.
fn run(name: &str, blurb: &str, build: Build) -> io::Result<()> {
    let _session = tuika::TerminalSession::enter()?;
    let mut terminal = Terminal::with_options(
        ratatui::backend::CrosstermBackend::new(io::stdout()),
        TerminalOptions {
            viewport: Viewport::Fullscreen,
        },
    )?;
    let theme = Theme::default();
    let mut frame = 0u64;
    loop {
        terminal.draw(|f| {
            let area = f.area();
            let root = framed(name, blurb, build(frame, &theme), &theme);
            paint(f.buffer_mut(), area, &theme, root.as_ref(), &[]);
        })?;
        if event::poll(Duration::from_millis(80))?
            && let CtEvent::Key(key) = event::read()?
            && key.kind != KeyEventKind::Release
            && matches!(key.code, CtKeyCode::Char('q') | CtKeyCode::Esc)
        {
            break;
        }
        frame = frame.wrapping_add(1);
    }
    let _ = terminal.clear();
    drop(terminal);
    Ok(())
}

// ---------------------------------------------------------------------------
// Scenes. Each is a pure function of the frame counter and theme.
// ---------------------------------------------------------------------------

fn labeled_row(glyph: Element, label: &str, theme: &Theme) -> Element {
    let text = Text::new(vec![Line::from(Span::styled(
        label.to_string(),
        theme.text_style(),
    ))]);
    view! {
        row(gap = 1) {
            fixed(2) { node(glyph) }
            grow(1) { node(text) }
        }
    }
}

fn scene_spinner(frame: u64, theme: &Theme) -> Element {
    view! {
        col(gap = 1) {
            fixed(1) { node(labeled_row(element(Spinner::new(frame).style(SpinnerStyle::Braille)), "Braille — the smooth default", theme)) }
            fixed(1) { node(labeled_row(element(Spinner::new(frame).style(SpinnerStyle::Line)), "Line — ASCII fallback", theme)) }
            fixed(1) { node(labeled_row(element(Spinner::new(frame).style(SpinnerStyle::Dots)), "Dots — bouncing", theme)) }
        }
    }
}

fn scene_progress(frame: u64, theme: &Theme) -> Element {
    let animated = tuika::anim::ping_pong(frame, 140);
    let _ = theme;
    view! {
        col(gap = 1) {
            fixed(1) { node(ProgressBar::determinate(0.25).percent(true)) }
            fixed(1) { node(ProgressBar::determinate(0.60).percent(true)) }
            fixed(1) { node(ProgressBar::determinate(animated).percent(true)) }
            fixed(1) { node(ProgressBar::indeterminate(frame)) }
        }
    }
}

fn scene_loader(frame: u64, theme: &Theme) -> Element {
    let _ = theme;
    view! {
        col(gap = 1) {
            fixed(1) { node(Loader::new(frame, "compiling crate…").hint("esc to cancel")) }
            fixed(1) { node(Loader::new(frame, "fetching dependencies…").spinner_style(SpinnerStyle::Line)) }
        }
    }
}

fn scene_text(frame: u64, theme: &Theme) -> Element {
    let _ = frame;
    let styled = Text::new(vec![
        Line::from(vec![
            Span::styled(
                "error",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
            Span::styled(": unresolved import ", theme.text_style()),
            Span::styled("`tuika::Widget`", theme.accent_style()),
        ]),
        Line::from(Span::styled(
            "  perhaps you meant `tuika::View`?",
            theme.muted_style(),
        )),
    ]);
    let prose = tuika::Paragraph::new(
        "Paragraph word-wraps plain text to the render width in a single style, \
         re-flowing every frame so a resize just re-wraps.",
        theme.text_style(),
    );
    view! {
        col(gap = 1) {
            fixed(2) { node(styled) }
            grow(1) { node(prose) }
        }
    }
}

fn scene_rule(frame: u64, theme: &Theme) -> Element {
    let _ = frame;
    view! {
        col(gap = 1) {
            fixed(1) { node(Rule::new().style(theme.muted_style())) }
            fixed(1) { node(Rule::new().title(Line::from(Span::styled(" Section ", theme.accent_style()))).style(theme.muted_style())) }
            fixed(1) { node(Rule::new().glyph('┈').style(theme.muted_style())) }
            fixed(1) { node(Rule::new().title(Line::from(Span::styled(" dotted ", theme.accent_style()))).glyph('·').style(theme.muted_style())) }
        }
    }
}

fn scene_boxed(frame: u64, theme: &Theme) -> Element {
    let _ = frame;
    let inner = Text::new(vec![Line::from(Span::styled(
        "border + padding + title",
        theme.text_style(),
    ))]);
    let plain = Text::new(vec![Line::from(Span::styled(
        "rounded border",
        theme.text_style(),
    ))]);
    view! {
        col(gap = 1) {
            fixed(3) {
                boxed(title = Line::from(Span::styled(" thick ", theme.accent_style())), border = BorderStyle::Thick, padding = Padding::symmetric(1, 0)) {
                    node(inner)
                }
            }
            fixed(3) {
                boxed(title = Line::from(Span::styled(" rounded ", theme.accent_style())), border = BorderStyle::Rounded) {
                    node(plain)
                }
            }
        }
    }
}

fn scene_flex(frame: u64, theme: &Theme) -> Element {
    let _ = frame;
    let cell = |label: &str, color: Color| -> Element {
        let text = Text::new(vec![Line::from(Span::styled(
            label.to_string(),
            Style::default()
                .fg(theme.background)
                .bg(color)
                .add_modifier(Modifier::BOLD),
        ))]);
        element(
            tuika::Boxed::new(element(text))
                .border(BorderStyle::Plain)
                .background(Style::default().bg(color)),
        )
    };
    view! {
        col(gap = 1) {
            fixed(3) {
                row(gap = 1) {
                    grow(1) { node(cell("grow 1", theme.accent)) }
                    grow(2) { node(cell("grow 2", theme.accent_alt)) }
                    fixed(12) { node(cell("fixed 12", theme.muted)) }
                }
            }
            fixed(1) { node(Text::new(vec![Line::from(Span::styled("row · gap 1 · grow shares leftover width", theme.muted_style()))])) }
        }
    }
}

fn scene_scroll(frame: u64, theme: &Theme) -> Element {
    let lines: Vec<Line<'static>> = (1..=24)
        .map(|i| {
            Line::from(Span::styled(
                format!("  line {i:>2} — content that overflows the viewport"),
                theme.text_style(),
            ))
        })
        .collect();
    let viewport_h = 8u16;
    let content_h = lines.len() as u16;
    let mut state = ScrollState::new();
    let reach = tuika::anim::ping_pong(frame, 200);
    let steps = (reach * (content_h.saturating_sub(viewport_h) as f32 / 3.0 + 1.0)) as u32;
    let down = Event::Mouse(Mouse::at(MouseKind::ScrollDown, 0, 0));
    for _ in 0..steps {
        state.handle(&down, content_h, viewport_h);
    }
    view! {
        col {
            fixed(8) { node(Scroll::new(lines, &state)) }
        }
    }
}

fn scene_select(frame: u64, theme: &Theme) -> Element {
    let items: Vec<Line<'static>> = ["Open file…", "Save", "Save As…", "Toggle theme", "Quit"]
        .iter()
        .map(|s| Line::from(Span::styled((*s).to_string(), theme.text_style())))
        .collect();
    let mut state = SelectState::new();
    let target = (frame / 10) % items.len() as u64;
    let down = Event::Key(Key::new(KeyCode::Down));
    for _ in 0..target {
        state.handle(&down, items.len());
    }
    view! {
        col {
            grow(1) { node(SelectList::new(items, &state)) }
        }
    }
}

fn scene_tabs(frame: u64, theme: &Theme) -> Element {
    let labels: Vec<Line<'static>> = ["Chat", "Diff", "Logs", "Files"]
        .iter()
        .map(|s| Line::from(Span::styled((*s).to_string(), theme.text_style())))
        .collect();
    let mut state = TabsState::default();
    let target = (frame / 16) % labels.len() as u64;
    let right = Event::Key(Key::new(KeyCode::Right));
    for _ in 0..target {
        state.handle(&right, labels.len());
    }
    view! {
        col(gap = 1) {
            fixed(1) { node(Tabs::new(labels, &state)) }
            fixed(1) { node(Text::new(vec![Line::from(Span::styled("←/→ or Tab to switch", theme.muted_style()))])) }
        }
    }
}

fn scene_status_bar(frame: u64, theme: &Theme) -> Element {
    let _ = frame;
    let bar = StatusBar::new()
        .left(vec![
            Span::styled(" NORMAL ", theme.selection_style()),
            Span::styled("  main.rs", theme.text_style()),
        ])
        .right(vec![
            Span::styled("utf-8  ", theme.muted_style()),
            Span::styled("Ln 42, Col 7 ", theme.text_style()),
        ])
        .background(Style::default().bg(theme.surface));
    view! {
        col {
            fixed(1) { node(bar) }
        }
    }
}

fn scene_textinput(frame: u64, theme: &Theme) -> Element {
    let _ = theme;
    let full = "fix(parser): handle trailing commas";
    let n = ((frame / 3) as usize % (full.chars().count() + 12)).min(full.chars().count());
    let typed: String = full.chars().take(n).collect();
    let state = TextInputState::from_text(&typed);
    view! {
        col {
            fixed(3) {
                boxed(title = Line::from(Span::styled(" commit message ", theme.accent_style()))) {
                    node(TextInput::new(&state))
                }
            }
        }
    }
}

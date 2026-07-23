//! Styling demo — one component tree, several stylesheets.
//!
//! `tuika` splits styling into two layers: a [`Theme`] of named *colors*, and a
//! [`StyleSheet`] mapping semantic [`Role`](tuika::Role)s onto those colors
//! (heading, link, inline code, list marker, the panel border/fill, …). This
//! scene renders the *same* markdown block and panels under different
//! stylesheets, so a single central swap visibly restyles every heading, link,
//! bare URL, code span, and panel at once — no per-call-site styling.
//!
//! ```text
//! cargo run -p tuika --example styling -- run          # cycle sheets, full-screen
//! cargo run -p tuika --example styling -- run vivid    # hold one sheet
//! cargo run -p tuika --example styling -- --dump vivid # print one frame as text
//! cargo run -p tuika --example styling -- bg           # print the background hex
//! ```
//!
//! The `run` subcommand animates the scene in the alternate screen so a terminal
//! recorder (VHS) can capture it as a GIF; that is how the demos in
//! `docs/styling.md` are produced (see `scripts/gen-tuika-styling-demos.sh`).

use std::io;
use std::time::Duration;

use crossterm::event::{self, Event as CtEvent, KeyCode as CtKeyCode, KeyEventKind};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier};
use ratatui::text::{Line, Span};
use ratatui::{Terminal, TerminalOptions, Viewport};

use tuika::{
    BorderStyle, Element, Markdown, Padding, ProgressBar, Rule, Spinner, SpinnerStyle, StyleBundle,
    StyleSheet, Text, Theme, element, paint_with_sheet, view,
};

/// Grid size (character cells).
const COLS: u16 = 82;
const ROWS: u16 = 26;

/// Frames a single stylesheet is held before the scene cycles to the next one.
const HOLD: u64 = 26;

/// The markdown block every sheet restyles. It deliberately exercises one of
/// each stylable part: a heading, strong/emphasis, an inline-code span, a
/// bracketed link, a bare URL, and a bulleted list.
const DOC: &str = "\
# Release notes

Ship **bold** ideas and *careful* fixes. Read the [handbook](https://tuika.dev) \
or visit https://everruns.dev for the rest. Run `cargo test` before you push.

- Centralized styling — one sheet, every component
- Themes stay the color tokens; the sheet is the rules";

/// The named stylesheets this demo cycles through. Each starts from
/// `StyleSheet::from_theme(theme)` (the look every component had before the
/// stylesheet existed) and overrides a handful of roles, so the diff between
/// them is exactly the centralized restyle.
fn variants(theme: &Theme) -> Vec<(&'static str, StyleSheet)> {
    let base = StyleSheet::from_theme(theme);
    vec![
        // The toolkit default: warm heading, red-ish links, no panel fill.
        ("default", base),
        // A vivid remap: green bold links (and bare URLs), a magenta heading,
        // cyan list bullets, and panels that take an accent border + surface fill.
        (
            "vivid",
            StyleSheet {
                heading: StyleBundle::new().fg(Color::Rgb(210, 120, 200)).bold(),
                link: StyleBundle::new()
                    .fg(Color::Rgb(120, 200, 120))
                    .bold()
                    .underlined(),
                inline_code: StyleBundle::new()
                    .fg(Color::Rgb(240, 220, 150))
                    .bg(Color::Rgb(48, 40, 30)),
                strong: StyleBundle::new().fg(Color::Rgb(230, 150, 100)).bold(),
                list_marker: StyleBundle::new().fg(Color::Rgb(120, 200, 210)),
                panel: StyleBundle::new().fg(theme.accent_alt).bg(theme.surface),
                ..base
            },
        ),
        // A restrained monochrome: everything sits on the text/muted colors,
        // links keep only their underline, panels take a dim border.
        (
            "mono",
            StyleSheet {
                heading: StyleBundle::new().fg(theme.text).bold(),
                link: StyleBundle::new().fg(theme.text).underlined(),
                inline_code: StyleBundle::new().fg(theme.text).bg(theme.surface),
                strong: StyleBundle::new().bold(),
                emphasis: StyleBundle::new().italic(),
                list_marker: StyleBundle::new().fg(theme.muted),
                panel: StyleBundle::new().fg(theme.dim),
                ..base
            },
        ),
    ]
}

fn main() -> io::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let theme = Theme::default();

    if args.first().map(String::as_str) == Some("bg") {
        // The theme background as `#rrggbb`, so a GIF recorder can blend its
        // window padding into the app (mirrors the screenshot example).
        println!("{}", hex(theme.background));
        return Ok(());
    }

    if let Some(pos) = args.iter().position(|a| a == "--dump") {
        // One representative frame, as text, for a quick hermetic sanity check.
        let name = args.get(pos + 1).map(String::as_str).unwrap_or("vivid");
        let (label, sheet) = pick(&theme, name);
        let buffer = render_frame(6, &theme, sheet, label);
        println!("{}", tuika::testing::grid(&buffer));
        return Ok(());
    }

    if args.first().map(String::as_str) == Some("run") {
        // A fixed variant name holds one sheet; otherwise the scene cycles.
        let held = args.get(1).cloned();
        return run_interactive(&theme, held.as_deref());
    }

    eprintln!(
        "usage: styling (run [variant] | --dump [variant] | bg)\nvariants: default, vivid, mono"
    );
    Ok(())
}

/// The sheet named `name` (falling back to `default`), with its label.
fn pick(theme: &Theme, name: &str) -> (&'static str, StyleSheet) {
    let vars = variants(theme);
    vars.iter()
        .find(|(label, _)| *label == name)
        .copied()
        .unwrap_or(vars[0])
}

fn render_frame(frame: u64, theme: &Theme, sheet: StyleSheet, label: &str) -> Buffer {
    let area = Rect::new(0, 0, COLS, ROWS);
    let mut buffer = Buffer::empty(area);
    let root = scene(frame, theme, label);
    paint_with_sheet(&mut buffer, area, theme, sheet, root.as_ref(), &[]);
    buffer
}

/// Animate the scene full-screen until `q`/`Esc`, so a recorder can capture it.
/// With `held` set, one sheet stays on screen; otherwise the sheet cycles.
fn run_interactive(theme: &Theme, held: Option<&str>) -> io::Result<()> {
    let _session = tuika::TerminalSession::enter()?;
    let mut terminal = Terminal::with_options(
        ratatui::backend::CrosstermBackend::new(io::stdout()),
        TerminalOptions {
            viewport: Viewport::Fullscreen,
        },
    )?;
    let vars = variants(theme);
    let mut frame = 0u64;
    loop {
        let (label, sheet) = match held {
            Some(name) => pick(theme, name),
            None => vars[(frame / HOLD) as usize % vars.len()],
        };
        terminal.draw(|f| {
            let area = f.area();
            let root = scene(frame, theme, label);
            paint_with_sheet(f.buffer_mut(), area, theme, sheet, root.as_ref(), &[]);
        })?;
        if event::poll(Duration::from_millis(90))?
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

/// The composite scene: a header naming the active sheet, a panel holding the
/// markdown block, and a side panel with live motion (so the capture breathes).
fn scene(frame: u64, theme: &Theme, label: &str) -> Element {
    let bg = ratatui::style::Style::default().bg(theme.background);
    view! {
        col(padding = Padding::all(1), gap = 0, background = bg) {
            fixed(1) { node(header(theme, label)) }
            fixed(1) { node(Rule::new().style(theme.muted_style())) }
            fixed(1) { node(spacer()) }
            grow(1) {
                row(gap = 2) {
                    grow(3) { node(doc_panel()) }
                    grow(2) { node(activity_panel(frame, theme)) }
                }
            }
        }
    }
}

fn header(theme: &Theme, label: &str) -> Element {
    element(Text::new(vec![Line::from(vec![
        Span::styled(
            "▲ tuika styling",
            theme.accent_style().add_modifier(Modifier::BOLD),
        ),
        Span::styled("   stylesheet = ", theme.muted_style()),
        Span::styled(label.to_string(), theme.accent_style()),
    ])]))
}

/// The markdown block inside a panel — the panel border/fill and every markdown
/// part restyle from whichever sheet the frame is painted with.
fn doc_panel() -> Element {
    view! {
        boxed(border = BorderStyle::Rounded, padding = Padding::symmetric(1, 0)) {
            node(element(Markdown::new(DOC)))
        }
    }
}

fn activity_panel(frame: u64, theme: &Theme) -> Element {
    let spin = |style: SpinnerStyle, label: &str| -> Element {
        view! {
            row(gap = 1) {
                fixed(2) { node(Spinner::new(frame).style(style)) }
                grow(1) { node(Text::new(vec![Line::from(Span::styled(label.to_string(), theme.text_style()))])) }
            }
        }
    };
    let animated = tuika::anim::ping_pong(frame.wrapping_mul(2), 48);
    view! {
        boxed(title = Line::from(Span::styled(" activity ", theme.accent_style())),
              border = BorderStyle::Rounded, padding = Padding::symmetric(1, 0)) {
            col(gap = 1) {
                fixed(1) { node(spin(SpinnerStyle::Braille, "restyling tree…")) }
                fixed(1) { node(Rule::new().style(theme.muted_style())) }
                fixed(1) { node(ProgressBar::determinate(animated).percent(true)) }
                fixed(1) { node(ProgressBar::indeterminate(frame)) }
            }
        }
    }
}

fn spacer() -> Element {
    element(Text::raw(""))
}

/// `#rrggbb` for an RGB theme color (used by the recorder to match padding).
fn hex(color: Color) -> String {
    match color {
        Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
        _ => "#000000".to_string(),
    }
}

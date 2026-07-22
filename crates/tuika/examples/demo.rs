//! Single-component demo harness.
//!
//! It renders one component per scene inside a small labeled frame, and is the
//! source of truth for the gallery: the scene registry drives the CLI, the tape
//! generator, and the integrity check.
//!
//! ```text
//! cargo run -p tuika --example demo -- spinner          # interactive, records a GIF
//! cargo run -p tuika --example demo -- spinner --dump    # print one frame as text
//! cargo run -p tuika --example demo -- list              # list scene names
//! cargo run -p tuika --example demo -- check             # verify the docs assets
//! cargo run -p tuika --example demo -- tapes <dir>       # emit VHS tapes (used by the generator)
//! ```
//!
//! The GIFs under `docs/demos/` are recorded by `scripts/gen-tuika-demos.sh`,
//! which asks this example to emit the VHS tapes and records each — the tapes
//! are generated, not committed. See `crates/tuika/AGENTS.md`.

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
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

type Build = fn(u64, &Theme) -> Element;

/// A single gallery scene. The registry below is the one place a component's
/// demo is declared — name, blurb, recording size, and builder.
struct Demo {
    name: &'static str,
    blurb: &'static str,
    /// Content height in rows; sets the recorded frame's height.
    rows: u16,
    /// Motion scenes hold longer and record at a higher frame rate.
    animated: bool,
    build: Build,
}

const fn demo(
    name: &'static str,
    blurb: &'static str,
    rows: u16,
    animated: bool,
    build: Build,
) -> Demo {
    Demo {
        name,
        blurb,
        rows,
        animated,
        build,
    }
}

const DEMOS: &[Demo] = &[
    demo(
        "spinner",
        "frame-cycled activity glyphs",
        10,
        true,
        scene_spinner,
    ),
    demo(
        "progress_bar",
        "determinate & indeterminate bars",
        12,
        true,
        scene_progress,
    ),
    demo(
        "loader",
        "spinner + message + hint row",
        9,
        true,
        scene_loader,
    ),
    demo(
        "text",
        "styled lines and word-wrapped prose",
        11,
        false,
        scene_text,
    ),
    demo(
        "rule",
        "titled horizontal separators",
        12,
        false,
        scene_rule,
    ),
    demo(
        "boxed",
        "borders, titles, and padding",
        12,
        false,
        scene_boxed,
    ),
    demo(
        "flex",
        "flexbox grow / fixed distribution",
        10,
        false,
        scene_flex,
    ),
    demo(
        "scroll",
        "viewport + scrollbar over long content",
        13,
        true,
        scene_scroll,
    ),
    demo(
        "select",
        "keyboard-navigable selection list",
        11,
        true,
        scene_select,
    ),
    demo("tabs", "host-state tab strip", 9, true, scene_tabs),
    demo(
        "status_bar",
        "left / right status segments",
        7,
        false,
        scene_status_bar,
    ),
    demo(
        "textinput",
        "multi-line edit model",
        9,
        true,
        scene_textinput,
    ),
    demo(
        "hyperlinks",
        "OSC 8 clickable http(s) URLs",
        10,
        false,
        scene_hyperlinks,
    ),
    demo(
        "overlay",
        "anchored dialog over the base tree",
        16,
        false,
        scene_overlay,
    ),
];

fn main() -> io::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str).unwrap_or("list");

    match cmd {
        "list" | "--list" | "-h" | "--help" => {
            println!("scenes:");
            for d in DEMOS {
                println!("  {:<14} {}", d.name, d.blurb);
            }
            Ok(())
        }
        "check" => check(),
        "tapes" => {
            let Some(dir) = args.get(1) else {
                eprintln!("usage: demo tapes <output-dir>");
                std::process::exit(2);
            };
            emit_tapes(Path::new(dir))
        }
        name => {
            let Some(d) = DEMOS.iter().find(|d| d.name == name) else {
                eprintln!("unknown scene {name:?}; run `list` to see the options");
                std::process::exit(2);
            };
            if args.iter().any(|a| a == "--dump") {
                dump(d.name, d.blurb, d.build)
            } else {
                run(d.name, d.blurb, d.build)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// `tapes` — emit a VHS tape per scene from the registry, so the tapes are
// generated rather than committed. Recorded at 2× pixel density and displayed
// at half width, so the GIFs stay crisp on HiDPI screens. `Output` is relative
// to the tuika crate dir (the generator cds there); the command is an absolute
// path to this very binary.
// ---------------------------------------------------------------------------

fn emit_tapes(dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    let bin = std::env::current_exe()?;
    for d in DEMOS {
        let height = u32::from(d.rows) * 52 + 96;
        let (hold, fps) = if d.animated {
            ("4s", 24)
        } else {
            ("1600ms", 12)
        };
        let tape = format!(
            "# Generated by `demo -- tapes`; recorded by scripts/gen-tuika-demos.sh.\n\
             # Do not edit or commit — regenerate from the scene registry instead.\n\
             Output docs/demos/{name}.gif\n\
             \n\
             Set Shell bash\n\
             Set FontSize 40\n\
             Set Width 1760\n\
             Set Height {height}\n\
             Set Padding 40\n\
             Set Framerate {fps}\n\
             \n\
             Hide\n\
             Type \"{bin} {name}\"\n\
             Enter\n\
             Sleep 900ms\n\
             Show\n\
             Sleep {hold}\n",
            name = d.name,
            bin = bin.display(),
        );
        fs::write(dir.join(format!("{}.tape", d.name)), tape)?;
    }
    println!("wrote {} tapes to {}", DEMOS.len(), dir.display());
    Ok(())
}

// ---------------------------------------------------------------------------
// `check` — the gallery integrity guard. The scene registry is the source of
// truth: every scene needs a non-empty recording, no orphan GIF may linger, and
// every `demos/<name>.gif` referenced by a component/feature doc must
// map to a real scene. Runs in CI (tuika-msrv) and at the end of the generator,
// so drift fails loudly instead of shipping a broken image to docs.rs.
// ---------------------------------------------------------------------------

fn check() -> io::Result<()> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let demos = dir.join("docs/demos");
    let names: BTreeSet<&str> = DEMOS.iter().map(|d| d.name).collect();
    let mut errors: Vec<String> = Vec::new();

    // Every scene has a non-empty recording.
    for name in &names {
        let gif = demos.join(format!("{name}.gif"));
        match fs::metadata(&gif) {
            Ok(m) if m.len() > 0 => {}
            Ok(_) => errors.push(format!("recording {} is empty", gif.display())),
            Err(_) => errors.push(format!(
                "scene `{name}` has no recording at {} (run scripts/gen-tuika-demos.sh)",
                gif.display()
            )),
        }
    }

    // No orphan recording without a scene.
    for stem in stems(&demos, "gif") {
        if !names.contains(stem.as_str()) {
            errors.push(format!(
                "docs/demos/{stem}.gif has no matching scene in DEMOS"
            ));
        }
    }

    // Every demo GIF referenced by a component/feature doc maps to a scene.
    let mut sources: Vec<PathBuf> = vec![
        dir.join("docs/components.md"),
        dir.join("docs/features.md"),
    ];
    for entry in fs::read_dir(dir.join("src/components"))?.filter_map(Result::ok) {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) == Some("rs") {
            sources.push(path);
        }
    }
    // Module-level docs outside `components/` (e.g. hyperlink, overlay) may embed demos too.
    for module in ["hyperlink.rs", "overlay.rs", "native.rs", "mouse.rs"] {
        let path = dir.join("src").join(module);
        if path.exists() {
            sources.push(path);
        }
    }
    for path in sources {
        let text = fs::read_to_string(&path)?;
        for referenced in referenced_gifs(&text) {
            if !names.contains(referenced.as_str()) {
                errors.push(format!(
                    "{} references demos/{referenced}.gif but there is no such scene",
                    path.display()
                ));
            }
        }
    }

    if errors.is_empty() {
        println!(
            "ok: {} scenes, recordings, and references in sync",
            names.len()
        );
        Ok(())
    } else {
        for e in &errors {
            eprintln!("error: {e}");
        }
        std::process::exit(1);
    }
}

/// File stems (without extension) of every `*.ext` entry in `dir`.
fn stems(dir: &Path, ext: &str) -> BTreeSet<String> {
    let Ok(read) = fs::read_dir(dir) else {
        return BTreeSet::new();
    };
    read.filter_map(Result::ok)
        .filter_map(|e| {
            let path = e.path();
            (path.extension().and_then(|s| s.to_str()) == Some(ext))
                .then(|| path.file_stem().and_then(|s| s.to_str()).map(str::to_owned))
                .flatten()
        })
        .collect()
}

/// Every `demos/<name>.gif` reference in `text` — matches both the relative
/// `demos/x.gif` in gallery markdown and the absolute `.../docs/demos/x.gif`
/// URLs embedded in rustdoc.
fn referenced_gifs(text: &str) -> BTreeSet<String> {
    const MARKER: &str = "demos/";
    let mut found = BTreeSet::new();
    for (idx, _) in text.match_indices(MARKER) {
        let rest = &text[idx + MARKER.len()..];
        let Some(end) = rest.find(".gif") else {
            continue;
        };
        let stem = &rest[..end];
        if !stem.is_empty() && stem.chars().all(|c| c.is_ascii_lowercase() || c == '_') {
            found.insert(stem.to_owned());
        }
    }
    found
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

fn scene_hyperlinks(frame: u64, theme: &Theme) -> Element {
    let _ = frame;
    let link = |url: &str| {
        Span::styled(
            url.to_string(),
            theme.accent_style().add_modifier(Modifier::UNDERLINED),
        )
    };
    let body = Text::new(vec![
        Line::from(vec![
            Span::styled("See the docs at ", theme.text_style()),
            link("https://docs.rs/tuika"),
        ]),
        Line::from(vec![
            Span::styled("Source: ", theme.muted_style()),
            link("https://github.com/everruns/yolop"),
        ]),
        Line::from(Span::styled(
            "HyperlinkBackend wraps URL runs in OSC 8 so supporting",
            theme.muted_style(),
        )),
        Line::from(Span::styled(
            "terminals make them clickable (others render plain text).",
            theme.muted_style(),
        )),
    ]);
    view! {
        col(gap = 1) {
            grow(1) { node(body) }
        }
    }
}

fn scene_overlay(frame: u64, theme: &Theme) -> Element {
    let _ = frame;
    // Visual stand-in for OverlaySpec::centered: base content with a dialog
    // panel stacked on top. The live `overlay` example drives real OverlaySpec
    // compositing; this scene shows the resulting look for the docs gallery.
    let base = Text::new(vec![
        Line::from(Span::styled(
            "base layer stays visible around the panel",
            theme.text_style(),
        )),
        Line::from(Span::styled(
            "confirmed 2 time(s)",
            theme.muted_style(),
        )),
    ]);
    let dialog = view! {
        boxed(
            title = Line::from(Span::styled(" confirm ", theme.accent_style())),
            border = BorderStyle::Rounded,
            padding = Padding::all(1)
        ) {
            col(gap = 1) {
                node(Text::raw("Proceed with the action?"))
                node(Text::new(vec![Line::from(Span::styled(
                    "enter = yes · esc = no",
                    theme.muted_style(),
                ))]))
            }
        }
    };
    view! {
        col(gap = 1) {
            fixed(2) { node(base) }
            fixed(7) { node(dialog) }
            fixed(1) {
                node(Text::new(vec![Line::from(Span::styled(
                    "OverlaySpec::centered(50, 40).min_size(34, 7)",
                    theme.muted_style(),
                ))]))
            }
        }
    }
}

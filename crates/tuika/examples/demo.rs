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
    AsciiFont, BorderStyle, CodeBlock, CodeTheme, Console, ConsoleLog, Diff, Element, Event,
    FrameBuffer, FrameBufferView, Highlighter, Key, KeyCode, Loader, MarkdownState, Mouse,
    MouseKind, Padding, ProgressBar, QrCode, QrEcc, Repeat, Rule, Scroll, ScrollState, SelectList,
    SelectState, Slider, SliderState, Spinner, SpinnerStyle, StatusBar, TabSelect, TabSelectState,
    Tabs, TabsState, Text, TextInput, TextInputState, Theme, Timeline, ToastLevel, ToastList,
    Toasts, element, paint, view,
};

/// A tiny self-contained Rust highlighter for the `markdown`/`code_block` scenes.
///
/// The examples deliberately avoid a grammar dependency (that would drag
/// tree-sitter into tuika's dev/MSRV builds); this shows how little it takes to
/// satisfy the [`Highlighter`] seam. For production-grade highlighting across
/// many languages, use the `tuika-codeformatters` crate.
struct DemoHighlighter;

/// A `'static` instance so `Markdown`/`CodeBlock` views (which borrow one) can be
/// boxed into `Element` in the scene builders.
static HL: DemoHighlighter = DemoHighlighter;

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
        "markdown",
        "CommonMark streamed in, only the tail re-parses",
        18,
        true,
        scene_markdown,
    ),
    demo(
        "code_block",
        "themed, syntax-highlighted fenced code with a line-number gutter",
        12,
        false,
        scene_code_block,
    ),
    demo(
        "diff",
        "unified line diff with +/- gutters and line numbers",
        11,
        false,
        scene_diff,
    ),
    demo(
        "ascii_font",
        "large figlet-style block-letter banners",
        9,
        false,
        scene_ascii_font,
    ),
    demo(
        "qr",
        "QR code encoded and drawn with half-blocks",
        18,
        false,
        scene_qr,
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
        "tab_select",
        "value-selecting segmented control",
        8,
        true,
        scene_tab_select,
    ),
    demo(
        "slider",
        "value picker over a numeric range",
        9,
        true,
        scene_slider,
    ),
    demo(
        "timeline",
        "keyframed easing tracks sampled from the frame counter",
        11,
        true,
        scene_timeline,
    ),
    demo(
        "toast",
        "level-colored notification stack",
        9,
        false,
        scene_toast,
    ),
    demo(
        "console",
        "captured stdout/log tail overlay",
        11,
        false,
        scene_console,
    ),
    demo(
        "framebuffer",
        "RGBA canvas + sprite drawn with half-blocks",
        14,
        true,
        scene_framebuffer,
    ),
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
        "hyperlink",
        "OSC 8 links — clickable URLs in the transcript",
        11,
        false,
        scene_hyperlink,
    ),
    demo(
        "mouse",
        "drag-to-select, highlight, and clickable regions",
        11,
        true,
        scene_mouse,
    ),
    demo(
        "overlay",
        "anchored dialog composited over the base tree",
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
// every `demos/<name>.gif` referenced by a component doc or `components.md` must
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

    // Every demo GIF referenced by a component doc, components.md, or features.md
    // maps to a scene.
    let mut sources: Vec<PathBuf> =
        vec![dir.join("docs/components.md"), dir.join("docs/features.md")];
    for entry in fs::read_dir(dir.join("src/components"))?.filter_map(Result::ok) {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) == Some("rs") {
            sources.push(path);
        }
    }
    // Module-level docs outside `components/` may embed a demo too (e.g. the
    // `overlay` GIF on `OverlaySpec`).
    sources.push(dir.join("src/overlay.rs"));
    for path in sources {
        // A doc source may not exist yet (e.g. features.md before it lands);
        // skip a missing one rather than failing the whole check.
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
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
/// `demos/x.gif` in `components.md` and the absolute `.../docs/demos/x.gif`
/// URLs embedded in component docs.
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

/// The document the `markdown` scene streams in, one glyph at a time.
const MARKDOWN_DOC: &str = "\
# Streaming Markdown

Renders **CommonMark** as it *streams* — only the in-flight tail
re-parses, so settled blocks and `code` never re-tokenize.

- headings, **bold**, *italic*, `inline code`
- nested lists and tables

```rust
fn greet(name: &str) {
    println!(\"hello, {name}!\");
}
```
";

/// Animated: reveal `MARKDOWN_DOC` progressively through a `MarkdownState`, the
/// same way a host feeds an assistant message as it streams. Holds on the full
/// document, then restarts.
fn scene_markdown(frame: u64, theme: &Theme) -> Element {
    let _ = theme;
    let total = MARKDOWN_DOC.chars().count();
    // Reveal briskly so the whole document — including the highlighted code
    // block — lands within a short recording, then hold before the cycle repeats.
    let pos = (frame as usize * 6) % (total + 120);
    let revealed: String = MARKDOWN_DOC.chars().take(pos.min(total)).collect();

    let mut state = MarkdownState::new();
    state.set(revealed);
    // Rendered at a fixed width for the demo frame; a real host passes the live
    // viewport width so prose re-wraps on resize.
    let sheet = tuika::StyleSheet::from_theme(theme);
    let lines = state
        .lines(64, theme, &sheet, tuika::CodeHighlighter::With(&HL))
        .to_vec();
    element(Text::new(lines))
}

/// A single themed, syntax-highlighted fenced block via `CodeBlock`.
fn scene_code_block(frame: u64, theme: &Theme) -> Element {
    let _ = (frame, theme);
    let source = "pub fn fib(n: u64) -> u64 {\n    match n {\n        0 | 1 => n,\n        _ => fib(n - 1) + fib(n - 2),\n    }\n}";
    element(
        CodeBlock::new("rust", source)
            .highlighter(&HL)
            .line_numbers(true),
    )
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
    let viewport_h = 8usize;
    let content_h = lines.len();
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

/// Bare `http(s)` URLs the host wraps in OSC 8, and a markdown link whose label
/// carries the target. Rendered with the theme's link color + underline — the
/// look a supporting terminal makes clickable; others show the text unchanged.
/// The normal paint path draws styled cells; real OSC 8 emission is the job of
/// `HyperlinkBackend` / `write_line`, so this scene shows the *appearance*.
fn scene_hyperlink(frame: u64, theme: &Theme) -> Element {
    let _ = frame;
    let link = Style::default()
        .fg(theme.code.link)
        .add_modifier(Modifier::UNDERLINED);
    let body = Text::new(vec![
        Line::from(Span::styled(
            "A bare URL is wrapped in place — clickable, text unchanged:",
            theme.muted_style(),
        )),
        Line::from(vec![
            Span::styled("  see ", theme.text_style()),
            Span::styled("https://docs.rs/tuika", link),
            Span::styled(" for the API.", theme.text_style()),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            "A markdown link shows its label, hiding the target:",
            theme.muted_style(),
        )),
        Line::from(vec![
            Span::styled("  the ", theme.text_style()),
            Span::styled("tuika component gallery", link),
            Span::styled(" demos every widget.", theme.text_style()),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            "Only http(s) links are emitted; anything else stays plain text.",
            theme.muted_style(),
        )),
    ]);
    element(body)
}

/// A left-drag selection growing over a phrase (real, copyable text), plus a
/// row of clickable regions a `HitMap` would resolve to actions.
fn scene_mouse(frame: u64, theme: &Theme) -> Element {
    let phrase = "the quick brown fox jumps over the lazy dog";
    let count = phrase.chars().count();
    let reach = tuika::anim::ping_pong(frame, 200);
    let selected = (reach * count as f32).round() as usize;
    let sel: String = phrase.chars().take(selected).collect();
    let rest: String = phrase.chars().skip(selected).collect();

    let button = |label: &str, active: bool| -> Span<'static> {
        if active {
            Span::styled(
                format!(" {label} "),
                Style::default()
                    .fg(theme.background)
                    .bg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            Span::styled(format!(" {label} "), theme.muted_style())
        }
    };
    let hot = (frame / 24) % 3;

    let body = Text::new(vec![
        Line::from(Span::styled(
            "Left-drag selects real text — copy it over SSH via OSC 52:",
            theme.muted_style(),
        )),
        Line::from(vec![
            Span::styled("  ", theme.text_style()),
            Span::styled(sel, theme.selection_style()),
            Span::styled(rest, theme.text_style()),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            "A HitMap maps screen regions to values — clicks become actions:",
            theme.muted_style(),
        )),
        Line::from(vec![
            Span::styled("  ", theme.text_style()),
            button("Run", hot == 0),
            Span::styled("  ", theme.text_style()),
            button("Diff", hot == 1),
            Span::styled("  ", theme.text_style()),
            button("Cancel", hot == 2),
        ]),
    ]);
    element(body)
}

fn scene_overlay(frame: u64, theme: &Theme) -> Element {
    let _ = frame;
    // Overlays composite a view over the base tree at an anchored, sized rect
    // (see `OverlaySpec`). The demo harness paints a single tree, so this scene
    // arranges the same look — a centered dialog over the base content — that
    // `OverlaySpec::centered(..).resolve(area)` plus `paint`'s overlay slice
    // produce at runtime; the `overlay` example drives the real compositing.
    let base = |s: &str| {
        Text::new(vec![Line::from(Span::styled(
            s.to_string(),
            theme.muted_style(),
        ))])
    };
    let dialog = view! {
        boxed(
            title = Line::from(Span::styled(" confirm ", theme.accent_style())),
            border = BorderStyle::Rounded,
            padding = Padding::all(1)
        ) {
            col(gap = 1) {
                node(Text::new(vec![Line::from(Span::styled(
                    "Proceed with the action?",
                    theme.text_style(),
                ))]))
                node(Text::new(vec![Line::from(Span::styled(
                    "enter = yes · esc = no",
                    theme.muted_style(),
                ))]))
            }
        }
    };
    view! {
        col(gap = 0) {
            fixed(1) { node(base("base layer stays visible around the panel")) }
            fixed(1) { node(base("… app content continues behind …")) }
            fixed(1) { spacer() }
            fixed(7) {
                row(gap = 0) {
                    grow(1) { spacer() }
                    fixed(46) { node(dialog) }
                    grow(1) { spacer() }
                }
            }
            grow(1) { spacer() }
            fixed(1) {
                node(Text::new(vec![Line::from(Span::styled(
                    "OverlaySpec::centered(50, 40).min_size(34, 7)",
                    theme.muted_style(),
                ))]))
            }
        }
    }
}

/// A muted caption line, reused by several scenes below.
fn caption(text: &str, theme: &Theme) -> Element {
    element(Text::new(vec![Line::from(Span::styled(
        text.to_string(),
        theme.muted_style(),
    ))]))
}

fn scene_diff(frame: u64, theme: &Theme) -> Element {
    let _ = frame;
    let old = "fn add(a: i32, b: i32) -> i32 {\n    a - b\n}\n\nlet total = add(2, 3);";
    let new = "fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n\nlet total = add(2, 3);\nprintln!(\"{total}\");";
    view! {
        col(gap = 1) {
            fixed(1) { node(caption("unified · red = removed, green = added", theme)) }
            grow(1) { node(Diff::new(old, new).line_numbers(true)) }
        }
    }
}

fn scene_ascii_font(frame: u64, theme: &Theme) -> Element {
    let _ = frame;
    view! {
        col(gap = 1) {
            fixed(5) { node(AsciiFont::new("TUIKA")) }
            fixed(1) { node(caption("embedded 5-row block font · A-Z 0-9 punctuation", theme)) }
        }
    }
}

fn scene_qr(frame: u64, theme: &Theme) -> Element {
    let _ = frame;
    // Byte-mode v1-4 encoder; a short URL fits comfortably in version 1-2.
    let qr = QrCode::encode("https://opentui.com", QrEcc::Medium)
        .map(element)
        .unwrap_or_else(|| caption("(payload too large)", theme));
    view! {
        col(gap = 1) {
            grow(1) { node(qr) }
            fixed(1) { node(caption("QrCode::encode(\"https://opentui.com\", QrEcc::Medium)", theme)) }
        }
    }
}

fn scene_tab_select(frame: u64, theme: &Theme) -> Element {
    let labels: Vec<Line<'static>> = ["Low", "Medium", "High", "Ultra"]
        .iter()
        .map(|s| Line::from(Span::styled((*s).to_string(), theme.text_style())))
        .collect();
    let mut state = TabSelectState::default();
    let target = (frame / 18) % labels.len() as u64;
    let right = Event::Key(Key::new(KeyCode::Right));
    for _ in 0..target {
        state.handle(&right, labels.len());
    }
    view! {
        col(gap = 1) {
            fixed(1) { node(TabSelect::new(labels, &state)) }
            fixed(1) { node(caption("← → move the selection · enter/space activates", theme)) }
        }
    }
}

fn scene_slider(frame: u64, theme: &Theme) -> Element {
    let swept = tuika::anim::ping_pong(frame, 160);
    let mut volume = SliderState::new(0.0, 100.0, 0.0);
    volume.set_ratio(swept);
    let contrast = SliderState::new(0.0, 10.0, 6.0);
    let mut balance = SliderState::new(0.0, 1.0, 0.0);
    balance.set_ratio(1.0 - swept);

    let row = |label: &str, s: &SliderState, theme: &Theme| -> Element {
        let lbl = Text::new(vec![Line::from(Span::styled(
            label.to_string(),
            theme.muted_style(),
        ))]);
        let slider = Slider::new(s).label(s);
        view! {
            row(gap = 1) {
                fixed(9) { node(lbl) }
                grow(1) { node(slider) }
            }
        }
    };
    view! {
        col(gap = 1) {
            fixed(1) { node(row("volume", &volume, theme)) }
            fixed(1) { node(row("contrast", &contrast, theme)) }
            fixed(1) { node(row("balance", &balance, theme)) }
        }
    }
}

fn scene_timeline(frame: u64, theme: &Theme) -> Element {
    // Three tracks over a shared 120-frame loop: a linear ramp, an eased ramp,
    // and a 0→1→0 pulse — each a pure function of the frame counter.
    let linear = Timeline::new()
        .keyframe(0, 0.0)
        .keyframe(120, 1.0)
        .repeat(Repeat::Loop);
    let eased = Timeline::new()
        .keyframe(0, 0.0)
        .ease(120, 1.0, tuika::anim::ease_in_out)
        .repeat(Repeat::Loop);
    let pulse = Timeline::new()
        .keyframe(0, 0.0)
        .keyframe(60, 1.0)
        .keyframe(120, 0.0)
        .repeat(Repeat::Loop);

    let track = |label: &str, value: f32, theme: &Theme| -> Element {
        let lbl = Text::new(vec![Line::from(Span::styled(
            label.to_string(),
            theme.muted_style(),
        ))]);
        view! {
            row(gap = 1) {
                fixed(11) { node(lbl) }
                grow(1) { node(ProgressBar::determinate(value).percent(true)) }
            }
        }
    };
    view! {
        col(gap = 1) {
            fixed(1) { node(track("linear", linear.sample(frame), theme)) }
            fixed(1) { node(track("ease_in_out", eased.sample(frame), theme)) }
            fixed(1) { node(track("pulse", pulse.sample(frame), theme)) }
            fixed(1) { node(caption("Timeline::new().keyframe(..).ease(..).repeat(Loop)", theme)) }
        }
    }
}

fn scene_toast(frame: u64, theme: &Theme) -> Element {
    let _ = (frame, theme);
    // A representative stack, newest on top. The host ticks these down; here we
    // show the steady-state look of all four levels at once.
    let mut toasts = Toasts::new(4);
    toasts.push(ToastLevel::Info, "Build started");
    toasts.push(ToastLevel::Success, "42 tests passed");
    toasts.push(ToastLevel::Warning, "2 unused imports");
    toasts.push(ToastLevel::Error, "Deploy to prod failed");
    let toasts: &'static Toasts = Box::leak(Box::new(toasts));
    view! {
        col {
            grow(1) { node(ToastList::new(toasts)) }
        }
    }
}

fn scene_console(frame: u64, theme: &Theme) -> Element {
    let _ = (frame, theme);
    let log = ConsoleLog::new(50);
    for line in [
        "INFO  server listening on 127.0.0.1:8080",
        "DEBUG loaded 12 routes in 3ms",
        "INFO  GET /  200  1.2ms",
        "WARN  slow query: users.by_email  214ms",
        "INFO  GET /health  200  0.3ms",
        "ERROR upstream timeout after 5s",
        "INFO  reconnecting to upstream…",
    ] {
        log.line(line);
    }
    let log: &'static ConsoleLog = Box::leak(Box::new(log));
    view! {
        col {
            grow(1) { node(Console::new(log).title(" console ")) }
        }
    }
}

fn scene_framebuffer(frame: u64, theme: &Theme) -> Element {
    let _ = theme;
    let (w, h) = (56u32, 22u32);
    let mut fb = FrameBuffer::new(w, h);
    // A diagonal gradient background.
    fb.shade(|x, y, _| {
        let r = 30 + (x * 120 / w) as u8;
        let g = 20;
        let b = 60 + (y * 120 / h) as u8;
        [r, g, b, 255]
    });
    // A "ball" bouncing over the canvas, drawn as an opaque square sprite.
    let bx = (tuika::anim::ping_pong(frame, 120) * (w - 8) as f32) as u32;
    let by = (tuika::anim::ping_pong(frame.wrapping_add(37), 84) * (h - 8) as f32) as u32;
    fb.fill_rect(bx, by, 8, 8, [240, 210, 90, 255]);
    // A scanline shader post-pass darkens every other row.
    fb.shade(|_x, y, [r, g, b, a]| {
        if y % 2 == 0 {
            [r, g, b, a]
        } else {
            [r / 2, g / 2, b / 2, a]
        }
    });
    let fb: &'static FrameBuffer = Box::leak(Box::new(fb));
    view! {
        col(gap = 1) {
            grow(1) { node(FrameBufferView::new(fb, w as u16, (h / 2) as u16)) }
            fixed(1) { node(caption("FrameBuffer → half-block cells · sprite + scanline shader", theme)) }
        }
    }
}

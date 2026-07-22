//! Hero screenshot generator for the tuika README.
//!
//! Rather than record a GIF with an external tool, this renders the *real*
//! component tree into an in-memory `ratatui` buffer (the same hermetic path the
//! tests use) and serializes it to a crisp, self-contained **animated SVG**. The
//! image is therefore generated from the live gallery — regenerate it whenever a
//! component's look changes:
//!
//! ```text
//! cargo run -p tuika --example screenshot            # writes docs/hero.svg
//! cargo run -p tuika --example screenshot -- out.svg # custom path
//! cargo run -p tuika --example screenshot -- --dump  # print one frame as text
//! cargo run -p tuika --example screenshot -- run     # animate it in a real terminal
//! cargo run -p tuika --example screenshot -- run gruvbox-dark  # …in a bundled theme
//! cargo run -p tuika --example screenshot -- bg gruvbox-dark   # print its background hex
//! ```
//!
//! Vector text stays sharp at any zoom, and — because an externally referenced
//! `<img src="…svg">` is proxied byte-for-byte by GitHub's camo cache — the CSS
//! keyframe animation plays on both GitHub and docs.rs. A handful of real frames
//! are captured (spinners cycling, progress filling, the palette and tabs moving,
//! a commit message typing); cells that never change are emitted once in a static
//! base layer, so only the moving region is duplicated per frame.
//!
//! The `run` subcommand animates the same `scene()` in the alternate screen so a
//! terminal recorder (VHS) can capture it as a GIF — that is how `docs/hero.gif`
//! is produced (see `scripts/gen-tuika-hero.sh`). SVG and GIF therefore share one
//! source of truth. `run <theme>` animates a bundled palette instead of the
//! default, which is how the per-theme demos in `docs/themes.md` are recorded
//! (see `scripts/gen-tuika-theme-demos.sh`).

use std::io;
use std::path::PathBuf;
use std::time::Duration;

use crossterm::event::{self, Event as CtEvent, KeyCode as CtKeyCode, KeyEventKind};
use ratatui::buffer::Buffer;
use ratatui::style::{Color, Modifier};
use ratatui::text::{Line, Span};
use ratatui::{Terminal, TerminalOptions, Viewport};

use tuika::{
    BorderStyle, Element, Event, Key, KeyCode, Loader, Padding, ProgressBar, Rule, SelectList,
    SelectState, Spinner, SpinnerStyle, StatusBar, Tabs, TabsState, Text, TextInput,
    TextInputState, Theme, element, paint, view,
};

/// Grid size (character cells). Chosen to give the gallery room to breathe while
/// staying a comfortable README width.
const COLS: u16 = 90;
const ROWS: u16 = 28;

/// Number of captured animation frames. The braille spinner has a 10-step cycle,
/// so a value a little above that shows more than one full turn.
const FRAMES: usize = 16;

fn main() -> io::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let theme = Theme::default();

    if args.iter().any(|a| a == "--dump") {
        // One representative frame, as text, for a quick sanity check.
        let buffer = render_frame(6, &theme);
        println!("{}", tuika::testing::grid(&buffer));
        return Ok(());
    }

    if args.first().map(String::as_str) == Some("run") {
        // Animate the scene in a real terminal so a recorder can capture it.
        // An optional theme name (`run gruvbox-dark`) animates that palette; the
        // per-theme GIFs are recorded this way.
        let scene_theme = args
            .get(1)
            .and_then(|name| tuika::themes::by_name(name))
            .unwrap_or(theme);
        return run_interactive(&scene_theme);
    }

    if args.first().map(String::as_str) == Some("bg") {
        // Print a theme's background as `#rrggbb` so the GIF recorder can blend
        // VHS's window padding into the app background. Defaults to the toolkit
        // theme when the name is unknown.
        let t = args
            .get(1)
            .and_then(|n| tuika::themes::by_name(n))
            .unwrap_or(theme);
        println!("{}", hex(t.background));
        return Ok(());
    }

    let out = args
        .iter()
        .find(|a| !a.starts_with("--"))
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("docs")
                .join("hero.svg")
        });

    let frames: Vec<Buffer> = (0..FRAMES)
        .map(|i| render_frame(i as u64, &theme))
        .collect();
    let svg = render_svg(&frames, &theme);
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&out, svg.as_bytes())?;
    println!(
        "wrote {} ({} frames, {}×{} cells, {} bytes)",
        out.display(),
        FRAMES,
        COLS,
        ROWS,
        svg.len()
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// The composite "app" scene — a single screen that exercises layout, borders,
// the motion components, an interactive list and tab strip, a text input, and a
// status bar. It is a pure function of the frame counter so every frame is
// reproducible.
// ---------------------------------------------------------------------------

fn render_frame(frame: u64, theme: &Theme) -> Buffer {
    let root = scene(frame, theme);
    tuika::testing::render(root.as_ref(), COLS, ROWS, theme)
}

/// Animate `scene()` full-screen until `q`/`Esc`. Used only for recording; the
/// same builder feeds the SVG, so a GIF capture matches the vector image.
fn run_interactive(theme: &Theme) -> io::Result<()> {
    let _session = tuika::TerminalSession::enter()?;
    let mut terminal = Terminal::with_options(
        ratatui::backend::CrosstermBackend::new(io::stdout()),
        TerminalOptions {
            viewport: Viewport::Fullscreen,
        },
    )?;
    let mut frame = 0u64;
    loop {
        terminal.draw(|f| {
            let area = f.area();
            let root = scene(frame, theme);
            paint(f.buffer_mut(), area, theme, root.as_ref(), &[]);
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

fn scene(frame: u64, theme: &Theme) -> Element {
    let bg = ratatui::style::Style::default().bg(theme.background);

    view! {
        col(padding = Padding::all(1), gap = 0, background = bg) {
            fixed(1) { node(header(theme)) }
            fixed(1) { node(spacer()) }
            fixed(1) { node(tab_strip(frame, theme)) }
            fixed(1) { node(Rule::new().style(theme.muted_style())) }
            grow(1) {
                row(gap = 2) {
                    grow(3) { node(activity_panel(frame, theme)) }
                    grow(2) { node(palette_panel(frame, theme)) }
                }
            }
            fixed(3) { node(commit_panel(frame, theme)) }
            fixed(1) { node(status(theme)) }
        }
    }
}

fn header(theme: &Theme) -> Element {
    element(Text::new(vec![Line::from(vec![
        Span::styled("▲ tuika", theme.accent_style().add_modifier(Modifier::BOLD)),
        Span::styled("  a composable terminal UI toolkit", theme.muted_style()),
    ])]))
}

fn tab_strip(frame: u64, theme: &Theme) -> Element {
    let labels: Vec<Line<'static>> = ["Chat", "Diff", "Logs", "Files"]
        .iter()
        .map(|s| Line::from(Span::styled((*s).to_string(), theme.text_style())))
        .collect();
    let mut state = TabsState::default();
    let target = (frame / 3) % labels.len() as u64;
    let right = Event::Key(Key::new(KeyCode::Right));
    for _ in 0..target {
        state.handle(&right, labels.len());
    }
    element(Tabs::new(labels, &state))
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
                fixed(1) { node(spin(SpinnerStyle::Braille, "compiling workspace…")) }
                fixed(1) { node(spin(SpinnerStyle::Line, "resolving dependencies")) }
                fixed(1) { node(spin(SpinnerStyle::Dots, "warming the cache")) }
                fixed(1) { node(Rule::new().style(theme.muted_style())) }
                fixed(1) { node(ProgressBar::determinate(0.42).percent(true)) }
                fixed(1) { node(ProgressBar::determinate(animated).percent(true)) }
                fixed(1) { node(ProgressBar::indeterminate(frame)) }
                fixed(1) { node(Loader::new(frame, "linking binary…").hint("q to quit")) }
            }
        }
    }
}

fn palette_panel(frame: u64, theme: &Theme) -> Element {
    let items: Vec<Line<'static>> = [
        "Open file…",
        "Go to symbol",
        "Toggle theme",
        "Format buffer",
        "Split right",
        "Quit",
    ]
    .iter()
    .map(|s| Line::from(Span::styled((*s).to_string(), theme.text_style())))
    .collect();
    let mut state = SelectState::new();
    let target = (frame / 2) % items.len() as u64;
    let down = Event::Key(Key::new(KeyCode::Down));
    for _ in 0..target {
        state.handle(&down, items.len());
    }
    view! {
        boxed(title = Line::from(Span::styled(" command palette ", theme.accent_style())),
              border = BorderStyle::Rounded, padding = Padding::symmetric(1, 0)) {
            node(SelectList::new(items, &state))
        }
    }
}

fn commit_panel(frame: u64, theme: &Theme) -> Element {
    let full = "feat: ship the tuika gallery";
    let n = (frame as usize * 2).min(full.chars().count());
    let typed: String = full.chars().take(n).collect();
    let state = TextInputState::from_text(&typed);
    view! {
        boxed(title = Line::from(Span::styled(" commit message ", theme.accent_style())),
              border = BorderStyle::Rounded) {
            node(TextInput::new(&state))
        }
    }
}

fn status(theme: &Theme) -> Element {
    element(
        StatusBar::new()
            .left(vec![
                Span::styled(" READY ", theme.selection_style()),
                Span::styled("  main", theme.text_style()),
            ])
            .right(vec![
                Span::styled("tuika 0.2  ", theme.muted_style()),
                Span::styled("⏎ run ", theme.text_style()),
            ])
            .background(ratatui::style::Style::default().bg(theme.surface)),
    )
}

fn spacer() -> Element {
    element(Text::raw(""))
}

// ---------------------------------------------------------------------------
// SVG serialization. Each buffer cell becomes an absolutely positioned glyph, so
// alignment never depends on the viewer's font metrics. Cells identical across
// every frame are written once in a static base layer; the rest go into one
// group per frame, and a CSS keyframe animation reveals exactly one frame group
// at a time in a loop.
// ---------------------------------------------------------------------------

/// Cell geometry, in SVG user units.
const CELL_W: f32 = 9.6;
const CELL_H: f32 = 19.0;
const FONT_SIZE: f32 = 15.5;
const BASELINE: f32 = 14.2;

/// Window chrome.
const SCREEN_MARGIN: f32 = 14.0; // gap between window edge and the "screen"
const GRID_PAD: f32 = 12.0; // gap between the screen edge and the first cell
const TITLE_BAR: f32 = 40.0;

/// The fully resolved appearance of one cell in one frame.
#[derive(Clone, PartialEq)]
struct Paint {
    sym: String,
    fg: String,
    bg: Option<String>,
    bold: bool,
    dim: bool,
    italic: bool,
    underline: bool,
    hidden: bool,
}

fn paint_of(buffer: &Buffer, x: u16, y: u16, theme: &Theme) -> Paint {
    let cell = &buffer[(x, y)];
    let m = cell.modifier;
    let reversed = m.contains(Modifier::REVERSED);
    let raw_fg = color_hex(cell.fg, theme, false);
    let raw_bg = color_hex(cell.bg, theme, true);
    let (fg, bg) = if reversed {
        // Swap, defaulting the "under" side to the screen's own colors.
        (
            raw_bg.clone().unwrap_or_else(|| hex(theme.background)),
            Some(raw_fg.clone().unwrap_or_else(|| hex(theme.text))),
        )
    } else {
        (raw_fg.unwrap_or_else(|| hex(theme.text)), raw_bg)
    };
    Paint {
        sym: cell.symbol().to_string(),
        fg,
        bg,
        bold: m.contains(Modifier::BOLD),
        dim: m.contains(Modifier::DIM),
        italic: m.contains(Modifier::ITALIC),
        underline: m.contains(Modifier::UNDERLINED),
        hidden: m.contains(Modifier::HIDDEN),
    }
}

fn render_svg(frames: &[Buffer], theme: &Theme) -> String {
    let cols = COLS as usize;
    let rows = ROWS as usize;

    let grid_w = COLS as f32 * CELL_W;
    let grid_h = ROWS as f32 * CELL_H;
    let screen_w = grid_w + 2.0 * GRID_PAD;
    let screen_h = grid_h + 2.0 * GRID_PAD;
    let width = screen_w + 2.0 * SCREEN_MARGIN;
    let height = screen_h + TITLE_BAR + SCREEN_MARGIN;

    let gx = SCREEN_MARGIN + GRID_PAD;
    let gy = TITLE_BAR + GRID_PAD;

    // Per-cell paints for every frame.
    let paints: Vec<Vec<Paint>> = frames
        .iter()
        .map(|buf| {
            let mut v = Vec::with_capacity(cols * rows);
            for y in 0..rows {
                for x in 0..cols {
                    v.push(paint_of(buf, x as u16, y as u16, theme));
                }
            }
            v
        })
        .collect();

    let mut base = String::new(); // rects + glyphs identical in every frame
    let mut anim: Vec<String> = vec![String::new(); frames.len()]; // per-frame, only moving cells

    for idx in 0..(cols * rows) {
        let x = (idx % cols) as f32;
        let y = (idx / cols) as f32;
        let cx = gx + x * CELL_W;
        let cy = gy + y * CELL_H;

        let first = &paints[0][idx];
        let is_static = paints.iter().all(|p| p[idx] == *first);
        if is_static {
            emit_cell(&mut base, first, cx, cy);
        } else {
            for (f, p) in paints.iter().enumerate() {
                emit_cell(&mut anim[f], &p[idx], cx, cy);
            }
        }
    }

    // CSS: reveal one frame group at a time. A boxcar keyframe (opacity 1 across
    // its slot, then an ~instant drop) staggered by a negative animation-delay
    // gives every group a disjoint window over one shared timeline.
    let per_frame = 0.5_f32; // seconds each frame is shown
    let dur = per_frame * frames.len() as f32;
    let slot = 100.0 / frames.len() as f32;
    let slot_end = slot + 0.02;

    let mut s = String::new();
    s.push_str(&format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{width:.0}\" height=\"{height:.0}\" \
viewBox=\"0 0 {width:.0} {height:.0}\" font-family=\"'JetBrains Mono','SFMono-Regular',\
'Cascadia Code',Menlo,Consolas,'Liberation Mono',monospace\" \
role=\"img\" aria-label=\"tuika terminal UI toolkit — animated component gallery\">\n"
    ));

    // Styles + keyframes.
    s.push_str("<style>\n");
    s.push_str(&format!(
        ".t{{font-size:{FONT_SIZE}px;dominant-baseline:alphabetic}}\n"
    ));
    s.push_str(".fr{opacity:0;animation:cyc ");
    s.push_str(&format!("{dur:.2}s linear infinite}}\n"));
    s.push_str(&format!(
        "@keyframes cyc{{0%,{slot:.3}%{{opacity:1}}{slot_end:.3}%,100%{{opacity:0}}}}\n"
    ));
    s.push_str("</style>\n");

    // Window bezel + soft shadow.
    s.push_str(
        "<defs><filter id=\"sh\" x=\"-20%\" y=\"-20%\" width=\"140%\" height=\"140%\">\
<feDropShadow dx=\"0\" dy=\"10\" stdDeviation=\"16\" flood-color=\"#000\" flood-opacity=\"0.45\"/>\
</filter></defs>\n",
    );
    s.push_str(&format!(
        "<rect x=\"{m:.1}\" y=\"{m:.1}\" width=\"{w:.1}\" height=\"{h:.1}\" rx=\"12\" \
fill=\"{chrome}\" stroke=\"{stroke}\" stroke-width=\"1\" filter=\"url(#sh)\"/>\n",
        m = SCREEN_MARGIN / 2.0,
        w = width - SCREEN_MARGIN,
        h = height - SCREEN_MARGIN,
        chrome = hex(theme.surface),
        stroke = hex(theme.border),
    ));

    // Traffic lights + title.
    let ty = SCREEN_MARGIN / 2.0 + TITLE_BAR / 2.0;
    for (i, color) in ["#ff5f56", "#ffbd2e", "#27c93f"].iter().enumerate() {
        let dot_x = SCREEN_MARGIN + 6.0 + i as f32 * 20.0;
        s.push_str(&format!(
            "<circle cx=\"{dot_x:.1}\" cy=\"{ty:.1}\" r=\"6.5\" fill=\"{color}\"/>\n"
        ));
    }
    s.push_str(&format!(
        "<text x=\"{cx:.1}\" y=\"{cy:.1}\" text-anchor=\"middle\" font-size=\"13\" \
fill=\"{muted}\">tuika · component gallery</text>\n",
        cx = width / 2.0,
        cy = ty + 4.5,
        muted = hex(theme.muted),
    ));

    // The terminal "screen".
    s.push_str(&format!(
        "<rect x=\"{x:.1}\" y=\"{y:.1}\" width=\"{w:.1}\" height=\"{h:.1}\" rx=\"6\" fill=\"{bg}\"/>\n",
        x = SCREEN_MARGIN,
        y = TITLE_BAR,
        w = screen_w,
        h = screen_h,
        bg = hex(theme.background),
    ));

    // Clip cells to the screen so nothing bleeds over the rounded corners.
    s.push_str(&format!(
        "<clipPath id=\"scr\"><rect x=\"{x:.1}\" y=\"{y:.1}\" width=\"{w:.1}\" height=\"{h:.1}\" rx=\"6\"/></clipPath>\n",
        x = SCREEN_MARGIN,
        y = TITLE_BAR,
        w = screen_w,
        h = screen_h,
    ));
    s.push_str("<g clip-path=\"url(#scr)\">\n");

    // Static base layer, then one animated group per frame.
    s.push_str("<g>");
    s.push_str(&base);
    s.push_str("</g>\n");
    for (f, group) in anim.iter().enumerate() {
        let delay = -(f as f32) * per_frame;
        s.push_str(&format!(
            "<g class=\"fr\" style=\"animation-delay:{delay:.3}s\">"
        ));
        s.push_str(group);
        s.push_str("</g>\n");
    }

    s.push_str("</g>\n</svg>\n");
    s
}

/// Terminal block-drawing glyphs describe a filled fraction of the cell. Painting
/// them as rects (rather than font glyphs, whose advance is narrower than the
/// cell) keeps progress bars and shaded tracks seamless and crisp.
enum Block {
    Full,
    /// Left-anchored horizontal fraction (eighth blocks ▏…▉).
    Left(f32),
    /// Bottom-anchored vertical fraction (▁…▇).
    Bottom(f32),
    /// Full-cell wash at the given opacity (shades ░▒▓).
    Shade(f32),
}

fn block_of(sym: &str) -> Option<Block> {
    let mut chars = sym.chars();
    let c = chars.next()?;
    if chars.next().is_some() {
        return None;
    }
    Some(match c {
        '█' => Block::Full,
        '▉' => Block::Left(7.0 / 8.0),
        '▊' => Block::Left(6.0 / 8.0),
        '▋' => Block::Left(5.0 / 8.0),
        '▌' => Block::Left(4.0 / 8.0),
        '▍' => Block::Left(3.0 / 8.0),
        '▎' => Block::Left(2.0 / 8.0),
        '▏' => Block::Left(1.0 / 8.0),
        '▇' => Block::Bottom(7.0 / 8.0),
        '▆' => Block::Bottom(6.0 / 8.0),
        '▅' => Block::Bottom(5.0 / 8.0),
        '▄' => Block::Bottom(4.0 / 8.0),
        '▃' => Block::Bottom(3.0 / 8.0),
        '▂' => Block::Bottom(2.0 / 8.0),
        '▁' => Block::Bottom(1.0 / 8.0),
        '░' => Block::Shade(0.25),
        '▒' => Block::Shade(0.50),
        '▓' => Block::Shade(0.75),
        _ => return None,
    })
}

/// Emit one cell's background rect (if any) and glyph (if visible) into `out`.
fn emit_cell(out: &mut String, p: &Paint, cx: f32, cy: f32) {
    if let Some(bg) = &p.bg {
        // Slight overdraw removes hairline seams between adjacent filled cells.
        out.push_str(&format!(
            "<rect x=\"{cx:.2}\" y=\"{cy:.2}\" width=\"{w:.2}\" height=\"{h:.2}\" fill=\"{bg}\" shape-rendering=\"crispEdges\"/>",
            w = CELL_W + 0.6,
            h = CELL_H + 0.6,
        ));
    }
    let sym = p.sym.trim_end();
    if p.hidden || sym.is_empty() || sym == " " {
        return;
    }
    // Block-drawing glyphs paint as rects in the foreground color.
    if let Some(block) = block_of(sym) {
        let op = if p.dim { 0.55 } else { 1.0 };
        let (rx, ry, rw, rh, o) = match block {
            Block::Full => (cx, cy, CELL_W + 0.6, CELL_H + 0.6, op),
            Block::Left(f) => (cx, cy, CELL_W * f, CELL_H + 0.6, op),
            Block::Bottom(f) => (
                cx,
                cy + CELL_H * (1.0 - f),
                CELL_W + 0.6,
                CELL_H * f + 0.6,
                op,
            ),
            Block::Shade(a) => (cx, cy, CELL_W + 0.6, CELL_H + 0.6, op * a),
        };
        out.push_str(&format!(
            "<rect x=\"{rx:.2}\" y=\"{ry:.2}\" width=\"{rw:.2}\" height=\"{rh:.2}\" fill=\"{}\"",
            p.fg
        ));
        if o < 1.0 {
            out.push_str(&format!(" opacity=\"{o:.2}\""));
        }
        out.push_str(" shape-rendering=\"crispEdges\"/>");
        return;
    }
    let tx = cx;
    let ty = cy + BASELINE;
    out.push_str(&format!(
        "<text class=\"t\" x=\"{tx:.2}\" y=\"{ty:.2}\" fill=\"{}\"",
        p.fg
    ));
    if p.bold {
        out.push_str(" font-weight=\"700\"");
    }
    if p.italic {
        out.push_str(" font-style=\"italic\"");
    }
    if p.dim {
        out.push_str(" opacity=\"0.55\"");
    }
    if p.underline {
        out.push_str(" text-decoration=\"underline\"");
    }
    out.push('>');
    out.push_str(&xml_escape(sym));
    out.push_str("</text>");
}

/// Resolve a ratatui `Color` to a `#rrggbb` string. Background `Reset`/default
/// returns `None` (transparent — the screen fill shows through); a foreground
/// `Reset`/default returns `None` too and the caller substitutes the theme text.
fn color_hex(color: Color, theme: &Theme, is_bg: bool) -> Option<String> {
    // Cells painted with the screen's own background are treated as transparent
    // (the screen fill shows through), so we never emit a rect for an empty cell.
    // This check must precede the concrete-color arms below.
    if is_bg && color == theme.background {
        return None;
    }
    match color {
        Color::Reset => None,
        Color::Rgb(r, g, b) => Some(rgb_hex(r, g, b)),
        Color::Indexed(i) => Some(indexed_hex(i)),
        named => Some(named_hex(named)),
    }
}

fn hex(color: Color) -> String {
    match color {
        Color::Rgb(r, g, b) => rgb_hex(r, g, b),
        Color::Indexed(i) => indexed_hex(i),
        other => named_hex(other),
    }
}

fn rgb_hex(r: u8, g: u8, b: u8) -> String {
    format!("#{r:02x}{g:02x}{b:02x}")
}

/// The 16 ANSI colors, in a tasteful dark-terminal palette. tuika's own theme is
/// all `Rgb`, so this only matters for the odd component that reaches for a named
/// color (e.g. a hard `Color::Red`).
fn named_hex(color: Color) -> String {
    let (r, g, b) = match color {
        Color::Black => (0x1e, 0x1e, 0x1e),
        Color::Red => (0xe0, 0x50, 0x5a),
        Color::Green => (0x5a, 0xc0, 0x78),
        Color::Yellow => (0xe0, 0xb0, 0x50),
        Color::Blue => (0x60, 0x90, 0xd0),
        Color::Magenta => (0xb0, 0x70, 0xd0),
        Color::Cyan => (0x50, 0xb0, 0xc0),
        Color::Gray => (0xb0, 0xb0, 0xb0),
        Color::DarkGray => (0x70, 0x70, 0x70),
        Color::LightRed => (0xf0, 0x80, 0x88),
        Color::LightGreen => (0x88, 0xe0, 0xa0),
        Color::LightYellow => (0xf0, 0xd0, 0x88),
        Color::LightBlue => (0x90, 0xc0, 0xf0),
        Color::LightMagenta => (0xd0, 0xa0, 0xf0),
        Color::LightCyan => (0x90, 0xe0, 0xf0),
        Color::White => (0xf0, 0xf0, 0xf0),
        _ => (0xeb, 0xe6, 0xe6), // Reset/other — theme-neutral light
    };
    rgb_hex(r, g, b)
}

/// The xterm 256-color cube + grayscale ramp.
fn indexed_hex(i: u8) -> String {
    match i {
        0..=15 => {
            const BASE: [Color; 16] = [
                Color::Black,
                Color::Red,
                Color::Green,
                Color::Yellow,
                Color::Blue,
                Color::Magenta,
                Color::Cyan,
                Color::Gray,
                Color::DarkGray,
                Color::LightRed,
                Color::LightGreen,
                Color::LightYellow,
                Color::LightBlue,
                Color::LightMagenta,
                Color::LightCyan,
                Color::White,
            ];
            named_hex(BASE[i as usize])
        }
        16..=231 => {
            let n = i - 16;
            let steps = [0u8, 95, 135, 175, 215, 255];
            let r = steps[(n / 36) as usize];
            let g = steps[((n / 6) % 6) as usize];
            let b = steps[(n % 6) as usize];
            rgb_hex(r, g, b)
        }
        232..=255 => {
            let v = 8 + (i - 232) * 10;
            rgb_hex(v, v, v)
        }
    }
}

fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

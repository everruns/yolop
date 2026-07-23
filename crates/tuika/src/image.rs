//! Terminal image rendering via the Kitty graphics protocol.
//!
//! A cell in ratatui's buffer carries one grapheme plus a style and nothing
//! else, so a picture has no home there — the same wall [`crate::hyperlink`]
//! (OSC 8), [`crate::clipboard`] (OSC 52), and [`crate::native`] (OSC 9;4) hit.
//! Like them, images are emitted out-of-band as an escape sequence, past the
//! cell buffer. Unlike them, the graphics escape is **not** cursor-neutral — it
//! paints at the cursor — so emission is split from layout:
//!
//! 1. An [`Image`] view reserves a `cols × rows` cell footprint via
//!    [`View::measure`], so the flex solver lays out around it like any leaf.
//! 2. On [`View::render`] it records its absolute painted [`Rect`] plus a
//!    handle to its pixels into a shared [`ImageLayer`] (cheap to clone, cleared
//!    each frame — the ownership shape of [`RectProbe`](crate::probe::RectProbe)),
//!    and paints the reserved cells blank (or the alt-text placeholder when the
//!    terminal can't show the image) so ratatui's diff has stable content there.
//! 3. **After** the frame is painted, the host calls [`ImageLayer::emit`], which
//!    moves the cursor to each image's cell origin and writes its Kitty escape,
//!    wrapped in a cursor save/restore so ratatui's cursor model is undisturbed.
//!
//! Decoding (PNG/JPEG → RGBA) is intentionally the host's job — it is a heavy
//! dependency, kept out of tuika exactly like syntax highlighting is (see
//! [`mod@crate::highlight`]). tuika owns presentation only: protocol encoding, cell
//! reservation, and the fallback. The host hands in raw RGBA via [`ImageData`].
//!
//! Graphics protocols do not degrade as harmlessly as an unknown OSC — an
//! unsupported terminal may paint the payload as garbage — so this is the first
//! tuika feature to gate on real capability detection ([`ImageSupport::detect`]).
//! See `specs/tuika-images.md` for the full design and phased plan.

use std::cell::RefCell;
use std::io::{self, Write};
use std::rc::Rc;
use std::sync::Arc;

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};

use crate::geometry::Size;
use crate::surface::Surface;
use crate::view::{RenderCtx, View};

/// String terminator for an APC sequence: `ESC \`.
const ST: &str = "\x1b\\";

/// Max base64 payload bytes per Kitty transmission chunk, per the protocol.
const CHUNK: usize = 4096;

/// Raw RGBA pixels plus their dimensions — the decoded image a host hands to an
/// [`Image`]. Cheap to clone (the pixel buffer is shared via [`Arc`]), so the
/// same image can back several views or be re-recorded every frame for free.
#[derive(Clone, Debug)]
pub struct ImageData {
    rgba: Arc<[u8]>,
    pixel_width: u32,
    pixel_height: u32,
}

impl ImageData {
    /// Build image data from a `pixel_width × pixel_height` RGBA buffer (4 bytes
    /// per pixel, row-major, no stride padding).
    ///
    /// Returns `None` if the dimensions are zero or `rgba.len()` is not exactly
    /// `pixel_width * pixel_height * 4`, so a malformed buffer can never reach
    /// the encoder.
    pub fn from_rgba(
        pixel_width: u32,
        pixel_height: u32,
        rgba: impl Into<Arc<[u8]>>,
    ) -> Option<Self> {
        let rgba = rgba.into();
        let expected = (pixel_width as usize)
            .checked_mul(pixel_height as usize)?
            .checked_mul(4)?;
        if pixel_width == 0 || pixel_height == 0 || rgba.len() != expected {
            return None;
        }
        Some(Self {
            rgba,
            pixel_width,
            pixel_height,
        })
    }

    /// The image width in pixels.
    pub fn pixel_width(&self) -> u32 {
        self.pixel_width
    }

    /// The image height in pixels.
    pub fn pixel_height(&self) -> u32 {
        self.pixel_height
    }
}

/// Which graphics protocol a terminal is believed to speak.
///
/// Phase 1 recognizes only the Kitty graphics protocol; iTerm2 and Sixel are
/// future variants selected by the same detection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageSupport {
    /// No known graphics protocol — [`Image`] renders its text fallback.
    None,
    /// The Kitty graphics protocol (Kitty, Ghostty, WezTerm, Konsole).
    Kitty,
}

impl ImageSupport {
    /// Probe the environment for graphics support.
    ///
    /// Conservative by design: it reads `TERM`, `TERM_PROGRAM`,
    /// `KITTY_WINDOW_ID`, and the Ghostty resources marker, and returns
    /// [`ImageSupport::None`] when nothing matches, so a terminal that would
    /// paint the payload as garbage gets the text fallback instead. A host that
    /// knows better can override with a literal variant.
    pub fn detect() -> Self {
        Self::detect_from(
            std::env::var("TERM").ok().as_deref(),
            std::env::var("TERM_PROGRAM").ok().as_deref(),
            std::env::var("KITTY_WINDOW_ID").ok().as_deref(),
            std::env::var("GHOSTTY_RESOURCES_DIR").ok().as_deref(),
        )
    }

    /// The pure core of [`detect`](Self::detect), taking the environment
    /// explicitly so the decision is unit-testable without touching the process
    /// environment.
    pub fn detect_from(
        term: Option<&str>,
        term_program: Option<&str>,
        kitty_window_id: Option<&str>,
        ghostty_resources_dir: Option<&str>,
    ) -> Self {
        // Any of these is a positive signal for the Kitty graphics protocol.
        let kitty = kitty_window_id.is_some_and(|s| !s.is_empty())
            || ghostty_resources_dir.is_some_and(|s| !s.is_empty())
            || term.is_some_and(|t| t.contains("kitty"))
            || term_program.is_some_and(|p| {
                let p = p.to_ascii_lowercase();
                p == "ghostty" || p == "wezterm"
            });
        if kitty {
            ImageSupport::Kitty
        } else {
            ImageSupport::None
        }
    }
}

/// One image queued for emission this frame: where it landed, in cells, and the
/// pixels to paint there.
#[derive(Clone, Debug)]
struct Placement {
    rect: Rect,
    data: ImageData,
}

/// A shared collector of the images placed during a frame, and the emitter that
/// paints them after the frame is drawn.
///
/// Cheap to clone (a shared handle), the way [`RectProbe`](crate::probe::RectProbe)
/// is: a host holds one, hands clones to its [`Image`] views via
/// [`Image::in_layer`], then after `terminal.draw()` calls [`emit`](Self::emit)
/// and [`clear`](Self::clear) for the next frame.
#[derive(Clone, Debug, Default)]
pub struct ImageLayer(Rc<RefCell<Vec<Placement>>>);

impl ImageLayer {
    /// Create an empty layer.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record an image at its painted cell rect. Zero-area rects are dropped —
    /// a clipped-away image has nothing to paint.
    fn record(&self, rect: Rect, data: ImageData) {
        if rect.width == 0 || rect.height == 0 {
            return;
        }
        self.0.borrow_mut().push(Placement { rect, data });
    }

    /// Drop all recorded placements. Call once per frame, after [`emit`](Self::emit),
    /// so the next frame starts clean.
    pub fn clear(&self) {
        self.0.borrow_mut().clear();
    }

    /// Whether any image was placed this frame.
    pub fn is_empty(&self) -> bool {
        self.0.borrow().is_empty()
    }

    /// Number of images placed this frame.
    pub fn len(&self) -> usize {
        self.0.borrow().len()
    }

    /// Write every recorded image to `out` as a Kitty graphics escape at its
    /// cell origin.
    ///
    /// Call this after `terminal.draw()` has flushed the frame, against the same
    /// sink as the terminal backend. The whole batch is wrapped in a cursor
    /// save/restore (`ESC 7` / `ESC 8`) and each image is positioned with a CUP
    /// (`ESC [ row ; col H`, 1-based) before its escape, so ratatui's cursor
    /// model is left exactly as it was.
    pub fn emit(&self, out: &mut impl Write) -> io::Result<()> {
        let placements = self.0.borrow();
        if placements.is_empty() {
            return Ok(());
        }
        // Save the cursor once, restore once, so nothing between shifts it.
        out.write_all(b"\x1b7")?;
        for Placement { rect, data } in placements.iter() {
            // CUP is 1-based; the reserved rect is 0-based screen coordinates.
            let (row, col) = (rect.y + 1, rect.x + 1);
            write!(out, "\x1b[{row};{col}H")?;
            out.write_all(encode_kitty(data, rect.width, rect.height).as_bytes())?;
        }
        out.write_all(b"\x1b8")?;
        out.flush()
    }
}

/// A view that displays a decoded image over the cells it reserves.
///
/// It always reserves `cols × rows` cells (bounded by the area it's given) and
/// always paints those cells — blank when the image will cover them, or the alt
/// text when the terminal can't. When [`ImageSupport::Kitty`] and a layer are
/// both set, it additionally records itself into the layer so the host's
/// [`ImageLayer::emit`] paints the picture over the reserved cells.
pub struct Image {
    data: ImageData,
    cols: u16,
    rows: u16,
    alt: String,
    support: ImageSupport,
    layer: Option<ImageLayer>,
}

impl Image {
    /// An image occupying `cols × rows` cells. It renders as a text placeholder
    /// until a graphics [`ImageSupport`] and an [`ImageLayer`] are attached with
    /// [`support`](Self::support) and [`in_layer`](Self::in_layer).
    pub fn new(data: ImageData, cols: u16, rows: u16) -> Self {
        Self {
            data,
            cols,
            rows,
            alt: String::new(),
            support: ImageSupport::None,
            layer: None,
        }
    }

    /// Set the graphics protocol to use (usually [`ImageSupport::detect`]).
    pub fn support(mut self, support: ImageSupport) -> Self {
        self.support = support;
        self
    }

    /// Register this image with a layer so it is emitted after the frame. Without
    /// a layer the view only ever paints its fallback.
    pub fn in_layer(mut self, layer: &ImageLayer) -> Self {
        self.layer = Some(layer.clone());
        self
    }

    /// Text shown (dimmed) when the image can't be painted — a supportless
    /// terminal, or before a layer is attached.
    pub fn alt(mut self, alt: impl Into<String>) -> Self {
        self.alt = alt.into();
        self
    }

    /// Whether this image will be emitted through a graphics protocol (as
    /// opposed to falling back to text).
    fn will_paint(&self) -> bool {
        self.support == ImageSupport::Kitty && self.layer.is_some()
    }
}

impl View for Image {
    fn measure(&self, available: Size) -> Size {
        Size::new(self.cols, self.rows).clamp_to(available)
    }

    fn render(&self, area: Rect, surface: &mut Surface, ctx: &RenderCtx) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        if self.will_paint() {
            // The graphics escape will cover these cells after the frame; keep
            // them blank so nothing shows through at the image's edges and the
            // ratatui diff over the region is stable.
            surface.fill(Style::default().bg(ctx.theme.background));
            if let Some(layer) = &self.layer {
                layer.record(area, self.data.clone());
            }
        } else {
            self.render_fallback(area, surface, ctx);
        }
    }
}

impl Image {
    /// Paint the alt-text placeholder centered in `area` — what a terminal
    /// without graphics support shows.
    fn render_fallback(&self, area: Rect, surface: &mut Surface, ctx: &RenderCtx) {
        surface.fill(Style::default().bg(ctx.theme.background));
        let label = if self.alt.is_empty() {
            "[image]".to_string()
        } else {
            format!("[image: {}]", self.alt)
        };
        let style = Style::default()
            .fg(ctx.theme.muted)
            .add_modifier(Modifier::ITALIC);
        // Center within the reserved box; truncation is handled by set_string's
        // clip against the surface.
        let width = crate::width::str_cols(&label);
        let x = area.x + area.width.saturating_sub(width) / 2;
        let y = area.y + area.height / 2;
        surface.set_string(x, y, &label, style);
    }
}

/// Encode `data` as a Kitty graphics protocol command that transmits and
/// displays the image scaled into `cols × rows` cells.
///
/// Pure and allocation-only — no I/O — so the wire format is unit-testable, the
/// same as [`crate::hyperlink::osc8`] and [`crate::native::encode`]. The payload
/// is raw RGBA (`f=32`) with its source pixel dimensions (`s`/`v`), displayed at
/// the cursor (`a=T`) across `c`/`r` cells, base64-encoded and split into
/// [`CHUNK`]-sized pieces with the `m` continuation key. `q=2` suppresses the
/// terminal's acknowledgement replies so they can't be read as input.
fn encode_kitty(data: &ImageData, cols: u16, rows: u16) -> String {
    let payload = base64_encode(&data.rgba);
    let chunks: Vec<&str> = split_chunks(&payload, CHUNK);
    let mut out = String::new();
    let last = chunks.len().saturating_sub(1);
    for (i, chunk) in chunks.iter().enumerate() {
        // `m=1` on every chunk but the last signals "more data follows".
        let more = u8::from(i != last);
        out.push_str("\x1b_G");
        if i == 0 {
            // Control keys ride only on the first chunk; continuations carry `m`.
            out.push_str(&format!(
                "f=32,s={},v={},a=T,c={},r={},q=2,m={}",
                data.pixel_width, data.pixel_height, cols, rows, more
            ));
        } else {
            out.push_str(&format!("m={more}"));
        }
        out.push(';');
        out.push_str(chunk);
        out.push_str(ST);
    }
    out
}

/// Split `s` into `size`-byte pieces (the last may be shorter). `s` is base64,
/// which is ASCII, so byte slicing never lands inside a character.
fn split_chunks(s: &str, size: usize) -> Vec<&str> {
    if s.is_empty() {
        return vec![""];
    }
    let mut chunks = Vec::with_capacity(s.len().div_ceil(size));
    let mut i = 0;
    while i < s.len() {
        let end = (i + size).min(s.len());
        chunks.push(&s[i..end]);
        i = end;
    }
    chunks
}

/// Standard base64 (RFC 4648, with `=` padding) — the encoding Kitty expects for
/// the graphics payload. Small and self-contained so tuika keeps its minimal
/// dependency set.
fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for group in bytes.chunks(3) {
        let b0 = group[0] as u32;
        let b1 = *group.get(1).unwrap_or(&0) as u32;
        let b2 = *group.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[(n >> 18) as usize & 0x3f] as char);
        out.push(ALPHABET[(n >> 12) as usize & 0x3f] as char);
        out.push(if group.len() > 1 {
            ALPHABET[(n >> 6) as usize & 0x3f] as char
        } else {
            '='
        });
        out.push(if group.len() > 2 {
            ALPHABET[n as usize & 0x3f] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::style::Theme;
    use crate::testing::render;
    use crate::view::element;

    /// A tiny solid RGBA image: `w × h` pixels, all the same color.
    fn solid(w: u32, h: u32, rgba: [u8; 4]) -> ImageData {
        let buf: Vec<u8> = std::iter::repeat_n(rgba, (w * h) as usize)
            .flatten()
            .collect();
        ImageData::from_rgba(w, h, buf).expect("valid rgba")
    }

    #[test]
    fn from_rgba_validates_length() {
        assert!(ImageData::from_rgba(2, 2, vec![0u8; 16]).is_some());
        // Wrong length, zero dimensions.
        assert!(ImageData::from_rgba(2, 2, vec![0u8; 15]).is_none());
        assert!(ImageData::from_rgba(0, 2, vec![0u8; 0]).is_none());
    }

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn encode_kitty_single_chunk_shape() {
        let img = solid(1, 1, [1, 2, 3, 4]);
        let out = encode_kitty(&img, 4, 2);
        // APC open + control keys with pixel + cell dims, response suppression,
        // single-chunk (m=0), then payload and ST.
        assert!(out.starts_with("\x1b_Gf=32,s=1,v=1,a=T,c=4,r=2,q=2,m=0;"));
        assert!(out.ends_with("\x1b\\"));
        // One RGBA pixel [1,2,3,4] base64-encodes to "AQIDBA==".
        assert!(out.contains(";AQIDBA==\x1b\\"));
        // Exactly one APC command.
        assert_eq!(out.matches("\x1b_G").count(), 1);
    }

    #[test]
    fn encode_kitty_chunks_large_payloads() {
        // Enough pixels that the base64 payload exceeds one CHUNK and must split.
        let img = solid(64, 64, [9, 9, 9, 9]);
        let out = encode_kitty(&img, 10, 5);
        let commands = out.matches("\x1b_G").count();
        assert!(commands > 1, "large image should span multiple chunks");
        // First chunk carries the control keys and m=1 (more follows).
        assert!(out.starts_with("\x1b_Gf=32,s=64,v=64,a=T,c=10,r=5,q=2,m=1;"));
        // Continuation chunks carry only the m key.
        assert!(out.contains("\x1b_Gm=1;"));
        // The final chunk closes with m=0.
        assert!(out.contains("\x1b_Gm=0;"));
        // Every command is ST-terminated.
        assert_eq!(out.matches("\x1b\\").count(), commands);
    }

    #[test]
    fn detect_recognizes_kitty_signals() {
        let kitty = ImageSupport::Kitty;
        let none = ImageSupport::None;
        // Positive signals.
        assert_eq!(
            ImageSupport::detect_from(Some("xterm-kitty"), None, None, None),
            kitty
        );
        assert_eq!(
            ImageSupport::detect_from(Some("xterm-256color"), None, Some("1"), None),
            kitty
        );
        assert_eq!(
            ImageSupport::detect_from(None, Some("ghostty"), None, None),
            kitty
        );
        assert_eq!(
            ImageSupport::detect_from(None, Some("WezTerm"), None, None),
            kitty
        );
        assert_eq!(
            ImageSupport::detect_from(None, None, None, Some("/x")),
            kitty
        );
        // Nothing matches, and empty markers don't count.
        assert_eq!(
            ImageSupport::detect_from(Some("xterm-256color"), Some("Apple_Terminal"), None, None),
            none
        );
        assert_eq!(
            ImageSupport::detect_from(None, None, Some(""), Some("")),
            none
        );
    }

    #[test]
    fn image_reserves_its_cell_footprint() {
        let img = Image::new(solid(2, 2, [0, 0, 0, 255]), 6, 3);
        assert_eq!(img.measure(Size::new(80, 24)), Size::new(6, 3));
        // Clamped to the available area.
        assert_eq!(img.measure(Size::new(4, 2)), Size::new(4, 2));
    }

    #[test]
    fn supported_image_records_a_placement_and_leaves_cells_blank() {
        let theme = Theme::default();
        let layer = ImageLayer::new();
        let view = element(
            Image::new(solid(2, 2, [0, 0, 0, 255]), 4, 2)
                .support(ImageSupport::Kitty)
                .in_layer(&layer)
                .alt("cat"),
        );
        let buf = render(&view, 4, 2, &theme);
        // The reserved cells are blank (the image will cover them post-frame),
        // so no alt text leaks into the buffer.
        let text: String = (0..buf.area.width).map(|x| buf[(x, 0)].symbol()).collect();
        assert!(
            !text.contains("cat"),
            "supported image must not paint alt text"
        );
        assert_eq!(layer.len(), 1, "a placement was recorded");
        assert!(!layer.is_empty());
    }

    #[test]
    fn unsupported_image_paints_alt_text_and_records_nothing() {
        let theme = Theme::default();
        let layer = ImageLayer::new();
        // Support::None → fallback, even with a layer attached.
        let view = element(
            Image::new(solid(2, 2, [0, 0, 0, 255]), 20, 3)
                .in_layer(&layer)
                .alt("cat"),
        );
        let buf = render(&view, 20, 3, &theme);
        let mut whole = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                whole.push_str(buf[(x, y)].symbol());
            }
        }
        assert!(
            whole.contains("[image: cat]"),
            "fallback should show alt text"
        );
        assert!(layer.is_empty(), "fallback records no placement");
    }

    #[test]
    fn emit_positions_each_image_and_brackets_with_cursor_save_restore() {
        let layer = ImageLayer::new();
        layer.record(Rect::new(2, 1, 4, 2), solid(1, 1, [1, 2, 3, 4]));
        layer.record(Rect::new(0, 5, 3, 3), solid(1, 1, [5, 6, 7, 8]));
        let mut out: Vec<u8> = Vec::new();
        layer.emit(&mut out).expect("emit");
        let s = String::from_utf8(out).expect("utf8");
        // Bracketed by a single save / restore.
        assert!(s.starts_with("\x1b7"));
        assert!(s.ends_with("\x1b8"));
        // CUP is 1-based: rect (2,1) → row 2, col 3; rect (0,5) → row 6, col 1.
        assert!(s.contains("\x1b[2;3H\x1b_G"));
        assert!(s.contains("\x1b[6;1H\x1b_G"));
        assert_eq!(s.matches("\x1b_G").count(), 2, "one command per image");
    }

    #[test]
    fn emit_is_a_noop_when_empty() {
        let layer = ImageLayer::new();
        let mut out: Vec<u8> = Vec::new();
        layer.emit(&mut out).expect("emit");
        assert!(
            out.is_empty(),
            "no images → no bytes, no stray cursor moves"
        );
    }

    #[test]
    fn clear_resets_between_frames() {
        let layer = ImageLayer::new();
        layer.record(Rect::new(0, 0, 2, 2), solid(1, 1, [0, 0, 0, 0]));
        assert_eq!(layer.len(), 1);
        layer.clear();
        assert!(layer.is_empty());
    }

    #[test]
    fn zero_area_placements_are_dropped() {
        let layer = ImageLayer::new();
        layer.record(Rect::new(0, 0, 0, 3), solid(1, 1, [0, 0, 0, 0]));
        layer.record(Rect::new(0, 0, 3, 0), solid(1, 1, [0, 0, 0, 0]));
        assert!(
            layer.is_empty(),
            "clipped-away images have nothing to paint"
        );
    }
}

//! [`FrameBuffer`] — a mutable RGBA canvas with sprite blitting and per-pixel
//! shaders, rendered to cells with half-blocks.
//!
//! This is the drawing-surface analog of OpenTUI's framebuffer/sprite stack,
//! built on the same RGBA pixel path as [`Image`](crate::Image) (so a buffer can
//! also be handed to the crisp Kitty/iTerm2/Sixel graphics protocols via
//! [`to_image_data`](FrameBuffer::to_image_data)). The always-available
//! [`FrameBufferView`] needs no graphics protocol: it packs two vertical pixels
//! into each terminal cell with the `▀` half-block, so a game/visualization
//! renders in any terminal.
//!
//! The pieces:
//! - [`FrameBuffer`] — clear, `set`/`blend` pixels, `fill_rect`, `blit` another
//!   buffer (alpha-composited), and `shade` a per-pixel post-pass.
//! - [`Sprite`] — a spritesheet sliced into equal frames; [`Sprite::draw`] blits
//!   one frame onto a target buffer for animation.
//! - [`FrameBufferView`] — a [`View`](crate::View) that paints a buffer into the
//!   cells it reserves.
//!
//! tuika owns none of the *content* — the host draws into the buffer each frame,
//! exactly as it owns image decoding and syntax highlighting.

use ratatui_core::layout::Rect;
use ratatui_core::style::{Color, Style};

use crate::geometry::Size;
use crate::image::ImageData;
use crate::surface::Surface;
use crate::view::{RenderCtx, View};

/// An 8-bit RGBA pixel: `[r, g, b, a]`, `a` = 255 opaque, 0 transparent.
pub type Rgba = [u8; 4];

/// A fully transparent pixel.
pub const TRANSPARENT: Rgba = [0, 0, 0, 0];

/// Source-over alpha composite of `src` onto `dst`.
fn over(src: Rgba, dst: Rgba) -> Rgba {
    let sa = src[3] as u32;
    if sa == 255 {
        return src;
    }
    if sa == 0 {
        return dst;
    }
    let da = dst[3] as u32;
    let da_out = da * (255 - sa) / 255;
    let out_a = sa + da_out;
    if out_a == 0 {
        return TRANSPARENT;
    }
    let ch = |s: u8, d: u8| ((s as u32 * sa + d as u32 * da_out) / out_a) as u8;
    [
        ch(src[0], dst[0]),
        ch(src[1], dst[1]),
        ch(src[2], dst[2]),
        out_a as u8,
    ]
}

/// A mutable RGBA pixel canvas.
#[derive(Clone, Debug)]
pub struct FrameBuffer {
    width: u32,
    height: u32,
    /// Row-major RGBA, `width * height * 4` bytes.
    pixels: Vec<u8>,
}

impl FrameBuffer {
    /// A `width × height` buffer, initially fully transparent.
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            pixels: vec![0; (width as usize) * (height as usize) * 4],
        }
    }

    /// Buffer width in pixels.
    pub fn width(&self) -> u32 {
        self.width
    }

    /// Buffer height in pixels.
    pub fn height(&self) -> u32 {
        self.height
    }

    /// The raw RGBA bytes (row-major), for the graphics protocols.
    pub fn as_rgba(&self) -> &[u8] {
        &self.pixels
    }

    fn index(&self, x: u32, y: u32) -> Option<usize> {
        (x < self.width && y < self.height).then(|| ((y * self.width + x) * 4) as usize)
    }

    /// The pixel at `(x, y)`, or [`TRANSPARENT`] if out of bounds.
    pub fn pixel(&self, x: u32, y: u32) -> Rgba {
        match self.index(x, y) {
            Some(i) => [
                self.pixels[i],
                self.pixels[i + 1],
                self.pixels[i + 2],
                self.pixels[i + 3],
            ],
            None => TRANSPARENT,
        }
    }

    /// Overwrite the pixel at `(x, y)` (no blending); out-of-bounds is ignored.
    pub fn set(&mut self, x: u32, y: u32, color: Rgba) {
        if let Some(i) = self.index(x, y) {
            self.pixels[i..i + 4].copy_from_slice(&color);
        }
    }

    /// Alpha-composite `color` over the pixel at `(x, y)`.
    pub fn blend(&mut self, x: u32, y: u32, color: Rgba) {
        if let Some(i) = self.index(x, y) {
            let dst = [
                self.pixels[i],
                self.pixels[i + 1],
                self.pixels[i + 2],
                self.pixels[i + 3],
            ];
            self.pixels[i..i + 4].copy_from_slice(&over(color, dst));
        }
    }

    /// Set every pixel to `color` (overwrite).
    pub fn clear(&mut self, color: Rgba) {
        for px in self.pixels.chunks_exact_mut(4) {
            px.copy_from_slice(&color);
        }
    }

    /// Alpha-composite `color` over a rectangle (clipped to the buffer).
    pub fn fill_rect(&mut self, x: u32, y: u32, w: u32, h: u32, color: Rgba) {
        for py in y..y.saturating_add(h).min(self.height) {
            for px in x..x.saturating_add(w).min(self.width) {
                self.blend(px, py, color);
            }
        }
    }

    /// Alpha-composite the whole of `src` onto this buffer at `(dx, dy)`.
    pub fn blit(&mut self, src: &FrameBuffer, dx: i32, dy: i32) {
        self.blit_region(src, 0, 0, src.width, src.height, dx, dy);
    }

    /// Alpha-composite a `sw × sh` region of `src` (from `(sx, sy)`) onto this
    /// buffer at `(dx, dy)`. Everything is clipped to both buffers.
    #[allow(clippy::too_many_arguments)]
    pub fn blit_region(
        &mut self,
        src: &FrameBuffer,
        sx: u32,
        sy: u32,
        sw: u32,
        sh: u32,
        dx: i32,
        dy: i32,
    ) {
        for row in 0..sh {
            let ty = dy + row as i32;
            if ty < 0 || ty as u32 >= self.height {
                continue;
            }
            for col in 0..sw {
                let tx = dx + col as i32;
                if tx < 0 || tx as u32 >= self.width {
                    continue;
                }
                let color = src.pixel(sx + col, sy + row);
                if color[3] != 0 {
                    self.blend(tx as u32, ty as u32, color);
                }
            }
        }
    }

    /// Apply a per-pixel shader post-pass: `f(x, y, current)` returns the new
    /// pixel. This is the terminal analog of a fragment shader — invert, tint,
    /// fade, scanlines, etc.
    pub fn shade(&mut self, f: impl Fn(u32, u32, Rgba) -> Rgba) {
        for y in 0..self.height {
            for x in 0..self.width {
                let i = ((y * self.width + x) * 4) as usize;
                let cur = [
                    self.pixels[i],
                    self.pixels[i + 1],
                    self.pixels[i + 2],
                    self.pixels[i + 3],
                ];
                let out = f(x, y, cur);
                self.pixels[i..i + 4].copy_from_slice(&out);
            }
        }
    }

    /// Snapshot as [`ImageData`] for the Kitty/iTerm2/Sixel graphics path, or
    /// `None` for a zero-sized buffer.
    pub fn to_image_data(&self) -> Option<ImageData> {
        ImageData::from_rgba(self.width, self.height, self.pixels.clone())
    }
}

/// A spritesheet sliced into equal `frame_w × frame_h` frames, left-to-right then
/// top-to-bottom.
#[derive(Clone, Debug)]
pub struct Sprite {
    sheet: FrameBuffer,
    frame_w: u32,
    frame_h: u32,
    cols: u32,
    rows: u32,
}

impl Sprite {
    /// Slice `sheet` into `frame_w × frame_h` frames. Partial trailing frames
    /// (if the sheet isn't an exact multiple) are ignored.
    pub fn new(sheet: FrameBuffer, frame_w: u32, frame_h: u32) -> Self {
        let cols = if frame_w == 0 {
            0
        } else {
            sheet.width / frame_w
        };
        let rows = if frame_h == 0 {
            0
        } else {
            sheet.height / frame_h
        };
        Self {
            sheet,
            frame_w,
            frame_h,
            cols,
            rows,
        }
    }

    /// Number of whole frames in the sheet.
    pub fn frame_count(&self) -> u32 {
        self.cols * self.rows
    }

    /// Pixel dimensions of a single frame.
    pub fn frame_size(&self) -> (u32, u32) {
        (self.frame_w, self.frame_h)
    }

    /// Alpha-composite frame `index` (wrapping — handy to drive from a host frame
    /// counter) onto `target` at `(dx, dy)`.
    pub fn draw(&self, target: &mut FrameBuffer, dx: i32, dy: i32, index: u32) {
        let count = self.frame_count();
        if count == 0 {
            return;
        }
        let index = index % count;
        let (cx, cy) = (index % self.cols, index / self.cols);
        target.blit_region(
            &self.sheet,
            cx * self.frame_w,
            cy * self.frame_h,
            self.frame_w,
            self.frame_h,
            dx,
            dy,
        );
    }
}

/// A [`View`] that paints a [`FrameBuffer`] into the `cols × rows` cells it
/// reserves, packing two vertical pixels per cell with the `▀` half-block.
///
/// The buffer is nearest-neighbor sampled to `cols × 2·rows` pixels, and each
/// pixel is alpha-composited over the theme background so transparency reads as
/// the surrounding UI.
pub struct FrameBufferView<'a> {
    fb: &'a FrameBuffer,
    cols: u16,
    rows: u16,
}

impl<'a> FrameBufferView<'a> {
    /// A view of `fb` reserving `cols × rows` cells.
    pub fn new(fb: &'a FrameBuffer, cols: u16, rows: u16) -> Self {
        Self { fb, cols, rows }
    }
}

/// Composite `px` over an opaque `bg`, returning a ratatui color.
fn composite(px: Rgba, bg: Color) -> Color {
    let (br, bg_, bb) = match bg {
        Color::Rgb(r, g, b) => (r, g, b),
        _ => (0, 0, 0),
    };
    let out = over(px, [br, bg_, bb, 255]);
    Color::Rgb(out[0], out[1], out[2])
}

impl View for FrameBufferView<'_> {
    fn measure(&self, available: Size) -> Size {
        Size::new(self.cols, self.rows).clamp_to(available)
    }

    fn render(&self, area: Rect, surface: &mut Surface, ctx: &RenderCtx) {
        if area.is_empty() || self.fb.width == 0 || self.fb.height == 0 {
            return;
        }
        let cols = self.cols.min(area.width);
        let rows = self.rows.min(area.height);
        let bg = ctx.theme.background;
        // Sample the buffer into a cols × (2*rows) pixel grid (nearest neighbor).
        let sample = |cx: u16, py: u32| -> Rgba {
            let src_x = (cx as u32 * self.fb.width) / cols as u32;
            let src_y = (py * self.fb.height) / (rows as u32 * 2);
            self.fb
                .pixel(src_x.min(self.fb.width - 1), src_y.min(self.fb.height - 1))
        };
        for row in 0..rows {
            let y = area.y + row;
            for cx in 0..cols {
                let top = sample(cx, row as u32 * 2);
                let bottom = sample(cx, row as u32 * 2 + 1);
                let style = Style::default()
                    .fg(composite(top, bg))
                    .bg(composite(bottom, bg));
                surface.set(area.x + cx, y, '▀', style);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::style::Theme;

    const RED: Rgba = [255, 0, 0, 255];
    const BLUE: Rgba = [0, 0, 255, 255];
    const HALF_RED: Rgba = [255, 0, 0, 128];

    #[test]
    fn set_get_and_clear() {
        let mut fb = FrameBuffer::new(4, 3);
        assert_eq!(fb.pixel(0, 0), TRANSPARENT);
        fb.set(1, 2, RED);
        assert_eq!(fb.pixel(1, 2), RED);
        // Out-of-bounds reads are transparent, writes are ignored (no panic).
        assert_eq!(fb.pixel(99, 99), TRANSPARENT);
        fb.set(99, 99, BLUE);
        fb.clear(BLUE);
        assert_eq!(fb.pixel(1, 2), BLUE);
    }

    #[test]
    fn blend_composites_alpha() {
        let mut fb = FrameBuffer::new(1, 1);
        fb.set(0, 0, BLUE);
        fb.blend(0, 0, HALF_RED);
        let p = fb.pixel(0, 0);
        // Half-opaque red over blue → a purple, fully opaque.
        assert!(p[0] > 100 && p[2] > 100, "purple-ish: {p:?}");
        assert_eq!(p[3], 255, "opaque result");
    }

    #[test]
    fn fill_rect_clips_to_bounds() {
        let mut fb = FrameBuffer::new(4, 4);
        fb.fill_rect(2, 2, 10, 10, RED); // overflows; must clip
        assert_eq!(fb.pixel(3, 3), RED);
        assert_eq!(fb.pixel(0, 0), TRANSPARENT);
    }

    #[test]
    fn blit_composites_and_clips_offscreen() {
        let mut dst = FrameBuffer::new(4, 4);
        let mut src = FrameBuffer::new(2, 2);
        src.clear(RED);
        dst.blit(&src, 3, 3); // only the top-left source pixel lands in-bounds
        assert_eq!(dst.pixel(3, 3), RED);
        assert_eq!(dst.pixel(0, 0), TRANSPARENT);
        // Negative offset clips the top-left of the source.
        dst.blit(&src, -1, -1);
        assert_eq!(dst.pixel(0, 0), RED);
    }

    #[test]
    fn shade_runs_over_every_pixel() {
        let mut fb = FrameBuffer::new(2, 2);
        fb.clear(RED);
        // Invert RGB.
        fb.shade(|_x, _y, [r, g, b, a]| [255 - r, 255 - g, 255 - b, a]);
        assert_eq!(fb.pixel(0, 0), [0, 255, 255, 255]); // inverted red = cyan
    }

    #[test]
    fn sprite_slices_and_draws_frames() {
        // A 4x2 sheet of two 2x2 frames: frame 0 red, frame 1 blue.
        let mut sheet = FrameBuffer::new(4, 2);
        sheet.fill_rect(0, 0, 2, 2, RED);
        sheet.fill_rect(2, 0, 2, 2, BLUE);
        let sprite = Sprite::new(sheet, 2, 2);
        assert_eq!(sprite.frame_count(), 2);
        assert_eq!(sprite.frame_size(), (2, 2));

        let mut target = FrameBuffer::new(2, 2);
        sprite.draw(&mut target, 0, 0, 1); // frame 1 = blue
        assert_eq!(target.pixel(0, 0), BLUE);
        // Index wraps modulo the frame count.
        sprite.draw(&mut target, 0, 0, 2); // 2 % 2 == 0 = red
        assert_eq!(target.pixel(0, 0), RED);
    }

    #[test]
    fn to_image_data_matches_dimensions() {
        let mut fb = FrameBuffer::new(3, 2);
        fb.clear(RED);
        let img = fb.to_image_data().expect("non-empty");
        assert_eq!((img.pixel_width(), img.pixel_height()), (3, 2));
        assert!(FrameBuffer::new(0, 0).to_image_data().is_none());
    }

    #[test]
    fn view_measures_and_renders_half_blocks() {
        let theme = Theme::default();
        let mut fb = FrameBuffer::new(4, 4);
        fb.clear(RED);
        let view = FrameBufferView::new(&fb, 4, 2);
        assert_eq!(view.measure(Size::new(80, 24)), Size::new(4, 2));
        let buf = crate::testing::render(&view, 4, 2, &theme);
        // Every cell is the half-block glyph; a fully-red buffer paints red fg/bg.
        assert_eq!(buf[(0, 0)].symbol(), "▀");
        assert_eq!(buf[(0, 0)].fg, Color::Rgb(255, 0, 0));
        assert_eq!(buf[(0, 0)].bg, Color::Rgb(255, 0, 0));
    }

    #[test]
    fn view_composites_transparency_over_theme_background() {
        let theme = Theme::default();
        // The default theme background is an Rgb color; compositing fully
        // transparent pixels over it yields that exact color back.
        let bg = theme.background;
        assert!(matches!(bg, Color::Rgb(..)));
        // A transparent buffer should read entirely as the theme background.
        let fb = FrameBuffer::new(2, 2);
        let buf = crate::testing::render(&FrameBufferView::new(&fb, 2, 1), 2, 1, &theme);
        assert_eq!(buf[(0, 0)].fg, bg);
        assert_eq!(buf[(0, 0)].bg, bg);
    }

    #[test]
    fn degenerate_sizes_do_not_panic() {
        let theme = Theme::default();
        let mut fb = FrameBuffer::new(8, 8);
        fb.clear(BLUE);
        for (w, h) in [(0u16, 0u16), (1, 1), (3, 2)] {
            let _ = crate::testing::render(&FrameBufferView::new(&fb, 8, 4), w, h, &theme);
        }
        // A zero-sized buffer renders nothing but must not panic.
        let empty = FrameBuffer::new(0, 0);
        let _ = crate::testing::render(&FrameBufferView::new(&empty, 4, 2), 4, 2, &theme);
    }
}

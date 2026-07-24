//! [`QrCode`] — encode a short byte payload to a QR matrix and render it with
//! half-block cells.
//!
//! The bundled encoder is byte-mode, QR versions 1–4 (up to 78 bytes at ECC
//! level Low — enough for URLs, Wi-Fi credentials, or tokens), with Reed-Solomon
//! error correction, block interleaving, and penalty-scored data masking. A
//! payload too large for version 4 returns `None`; a host that needs bigger codes
//! can encode with its own library and hand the module matrix to
//! [`QrCode::from_matrix`] — the same present-a-matrix seam as
//! [`Image`](crate::Image).
//!
//! Rendering packs two module rows into each terminal row with the `▀` half-block
//! (dark modules black, light modules white by default, for scanner contrast),
//! surrounded by the mandatory 4-module quiet zone.

use ratatui_core::layout::Rect;
use ratatui_core::style::{Color, Style};

use crate::geometry::Size;
use crate::surface::Surface;
use crate::view::{RenderCtx, View};

/// QR error-correction level: higher levels tolerate more damage but hold less
/// data at a given version.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum QrEcc {
    /// ~7% recovery — the most data.
    Low,
    /// ~15% recovery (the default).
    #[default]
    Medium,
    /// ~25% recovery.
    Quartile,
    /// ~30% recovery — the least data.
    High,
}

impl QrEcc {
    /// The two-bit format code for this level (per the QR spec's format info).
    fn format_bits(self) -> u32 {
        match self {
            QrEcc::Low => 1,
            QrEcc::Medium => 0,
            QrEcc::Quartile => 3,
            QrEcc::High => 2,
        }
    }

    /// Index into the per-version block tables below.
    fn index(self) -> usize {
        match self {
            QrEcc::Low => 0,
            QrEcc::Medium => 1,
            QrEcc::Quartile => 2,
            QrEcc::High => 3,
        }
    }
}

/// For versions 1–4 and each ECC level: `(ec_codewords_per_block,
/// &[data_codewords_per_block; one entry per block])`. Indexed `[version-1][ecc]`.
#[rustfmt::skip]
const BLOCKS: [[(usize, &[usize]); 4]; 4] = [
    // Version 1 (26 total codewords)
    [(7, &[19]), (10, &[16]), (13, &[13]), (17, &[9])],
    // Version 2 (44)
    [(10, &[34]), (16, &[28]), (22, &[22]), (28, &[16])],
    // Version 3 (70)
    [(15, &[55]), (26, &[44]), (18, &[17, 17]), (22, &[13, 13])],
    // Version 4 (100)
    [(20, &[80]), (18, &[32, 32]), (26, &[24, 24]), (16, &[9, 9, 9, 9])],
];

// ---- GF(256) arithmetic (primitive polynomial 0x11d) --------------------------

struct Gf {
    exp: [u8; 256],
    log: [u8; 256],
}

impl Gf {
    fn new() -> Self {
        let mut exp = [0u8; 256];
        let mut log = [0u8; 256];
        let mut x = 1u16;
        #[allow(clippy::needless_range_loop)] // `x` and the index advance together
        for i in 0..255 {
            exp[i] = x as u8;
            log[x as usize] = i as u8;
            x <<= 1;
            if x & 0x100 != 0 {
                x ^= 0x11d;
            }
        }
        exp[255] = exp[0]; // convenience wrap
        Gf { exp, log }
    }

    fn mul(&self, a: u8, b: u8) -> u8 {
        if a == 0 || b == 0 {
            0
        } else {
            let l = self.log[a as usize] as usize + self.log[b as usize] as usize;
            self.exp[l % 255]
        }
    }

    /// The monic generator polynomial of degree `degree`, coefficients high→low
    /// with a leading `1` — the product `∏(x - α^i)` for `i in 0..degree`.
    fn rs_generator(&self, degree: usize) -> Vec<u8> {
        // Start from the constant polynomial 1 and multiply in one `(x + α^i)`
        // factor at a time (subtraction is XOR in GF(2^8)).
        let mut g = vec![1u8];
        for i in 0..degree {
            let factor = [1u8, self.exp[i]]; // x + α^i, high→low
            let mut next = vec![0u8; g.len() + 1];
            for (a, &ga) in g.iter().enumerate() {
                for (b, &fb) in factor.iter().enumerate() {
                    next[a + b] ^= self.mul(ga, fb);
                }
            }
            g = next;
        }
        g
    }

    /// The `ec_len` Reed-Solomon error-correction codewords for `data`.
    fn rs_ecc(&self, data: &[u8], ec_len: usize) -> Vec<u8> {
        let generator = self.rs_generator(ec_len);
        let mut res = vec![0u8; data.len() + ec_len];
        res[..data.len()].copy_from_slice(data);
        for i in 0..data.len() {
            let factor = res[i];
            if factor != 0 {
                for (j, &g) in generator.iter().enumerate() {
                    res[i + j] ^= self.mul(g, factor);
                }
            }
        }
        res[data.len()..].to_vec()
    }
}

fn getbit(x: u32, i: u32) -> bool {
    (x >> i) & 1 != 0
}

/// A square grid of dark (`true`) / light (`false`) modules, with the modules the
/// codeword stream must not overwrite tracked in `func`.
struct Canvas {
    size: usize,
    grid: Vec<bool>,
    func: Vec<bool>,
}

impl Canvas {
    fn new(version: usize) -> Self {
        let size = 17 + 4 * version;
        Self {
            size,
            grid: vec![false; size * size],
            func: vec![false; size * size],
        }
    }

    fn set_fn(&mut self, x: i32, y: i32, dark: bool) {
        if x < 0 || y < 0 || x as usize >= self.size || y as usize >= self.size {
            return;
        }
        let i = y as usize * self.size + x as usize;
        self.grid[i] = dark;
        self.func[i] = true;
    }

    fn is_func(&self, x: usize, y: usize) -> bool {
        self.func[y * self.size + x]
    }

    fn draw_finder(&mut self, cx: i32, cy: i32) {
        for dy in -4i32..=4 {
            for dx in -4i32..=4 {
                let dist = dx.abs().max(dy.abs());
                self.set_fn(cx + dx, cy + dy, dist != 2 && dist != 4);
            }
        }
    }

    fn draw_alignment(&mut self, cx: i32, cy: i32) {
        for dy in -2i32..=2 {
            for dx in -2i32..=2 {
                self.set_fn(cx + dx, cy + dy, dx.abs().max(dy.abs()) != 1);
            }
        }
    }

    fn draw_function_patterns(&mut self, version: usize) {
        let n = self.size as i32;
        // Timing patterns.
        for i in 0..self.size {
            let dark = i % 2 == 0;
            self.set_fn(6, i as i32, dark);
            self.set_fn(i as i32, 6, dark);
        }
        // Three finder patterns (each includes its separator ring).
        self.draw_finder(3, 3);
        self.draw_finder(n - 4, 3);
        self.draw_finder(3, n - 4);
        // Single interior alignment pattern for versions 2–4.
        if version >= 2 {
            let c = n - 7;
            self.draw_alignment(c, c);
        }
        // Reserve the format-info modules (and the dark module) by drawing a
        // placeholder set — the real bits are written per-mask later. Going
        // through `draw_format` reserves exactly the right modules and, unlike a
        // crude rectangle, leaves the timing crossings at (8,6)/(6,8) intact.
        self.draw_format(QrEcc::Low, 0);
    }

    fn draw_codewords(&mut self, data: &[u8]) {
        let size = self.size as i32;
        let mut i = 0usize; // bit index
        let total_bits = data.len() * 8;
        let mut right = size - 1;
        while right >= 1 {
            if right == 6 {
                right = 5;
            }
            for vert in 0..size {
                for j in 0..2 {
                    let x = (right - j) as usize;
                    let upward = (right + 1) & 2 == 0;
                    let y = if upward { size - 1 - vert } else { vert } as usize;
                    if !self.is_func(x, y) && i < total_bits {
                        self.grid[y * self.size + x] =
                            getbit(data[i >> 3] as u32, 7 - (i & 7) as u32);
                        i += 1;
                    }
                }
            }
            if right < 2 {
                break;
            }
            right -= 2;
        }
    }

    fn mask_condition(mask: u8, x: usize, y: usize) -> bool {
        match mask {
            0 => (x + y).is_multiple_of(2),
            1 => y.is_multiple_of(2),
            2 => x.is_multiple_of(3),
            3 => (x + y).is_multiple_of(3),
            4 => (x / 3 + y / 2).is_multiple_of(2),
            5 => (x * y) % 2 + (x * y) % 3 == 0,
            6 => ((x * y) % 2 + (x * y) % 3).is_multiple_of(2),
            _ => ((x + y) % 2 + (x * y) % 3).is_multiple_of(2),
        }
    }

    fn apply_mask(&mut self, mask: u8) {
        for y in 0..self.size {
            for x in 0..self.size {
                if !self.is_func(x, y) && Self::mask_condition(mask, x, y) {
                    self.grid[y * self.size + x] ^= true;
                }
            }
        }
    }

    fn draw_format(&mut self, ecc: QrEcc, mask: u8) {
        let data = (ecc.format_bits() << 3) | mask as u32;
        let mut rem = data;
        for _ in 0..10 {
            rem = (rem << 1) ^ ((rem >> 9) * 0x537);
        }
        let bits = ((data << 10) | rem) ^ 0x5412; // 15 bits
        let n = self.size as i32;
        // First copy, around the top-left finder.
        for i in 0..=5 {
            self.set_fn(8, i, getbit(bits, i as u32));
        }
        self.set_fn(8, 7, getbit(bits, 6));
        self.set_fn(8, 8, getbit(bits, 7));
        self.set_fn(7, 8, getbit(bits, 8));
        for i in 9..15 {
            self.set_fn(14 - i, 8, getbit(bits, i as u32));
        }
        // Second copy, split across the other two finders.
        for i in 0..8 {
            self.set_fn(n - 1 - i, 8, getbit(bits, i as u32));
        }
        for i in 8..15 {
            self.set_fn(8, n - 15 + i, getbit(bits, i as u32));
        }
        self.set_fn(8, n - 8, true); // dark module (again, harmless)
    }

    /// The QR penalty score used to pick the least-conspicuous mask.
    fn penalty(&self) -> u32 {
        let size = self.size;
        let dark = |x: usize, y: usize| self.grid[y * size + x];
        let mut score = 0u32;

        // Rule 1: runs of five+ same-color modules in each row and column.
        for y in 0..size {
            let (mut run, mut last) = (1u32, dark(0, y));
            for x in 1..size {
                let c = dark(x, y);
                if c == last {
                    run += 1;
                } else {
                    if run >= 5 {
                        score += 3 + (run - 5);
                    }
                    run = 1;
                    last = c;
                }
            }
            if run >= 5 {
                score += 3 + (run - 5);
            }
        }
        for x in 0..size {
            let (mut run, mut last) = (1u32, dark(x, 0));
            for y in 1..size {
                let c = dark(x, y);
                if c == last {
                    run += 1;
                } else {
                    if run >= 5 {
                        score += 3 + (run - 5);
                    }
                    run = 1;
                    last = c;
                }
            }
            if run >= 5 {
                score += 3 + (run - 5);
            }
        }

        // Rule 2: 2x2 blocks of a single color.
        for y in 0..size - 1 {
            for x in 0..size - 1 {
                let c = dark(x, y);
                if c == dark(x + 1, y) && c == dark(x, y + 1) && c == dark(x + 1, y + 1) {
                    score += 3;
                }
            }
        }

        // Rule 3: finder-like 1:1:3:1:1 patterns in rows and columns.
        let pat1 = [
            true, false, true, true, true, false, true, false, false, false, false,
        ];
        let pat2 = [
            false, false, false, false, true, false, true, true, true, false, true,
        ];
        for y in 0..size {
            for x in 0..size {
                if x + 11 <= size {
                    let row: Vec<bool> = (0..11).map(|k| dark(x + k, y)).collect();
                    if row == pat1 || row == pat2 {
                        score += 40;
                    }
                }
                if y + 11 <= size {
                    let col: Vec<bool> = (0..11).map(|k| dark(x, y + k)).collect();
                    if col == pat1 || col == pat2 {
                        score += 40;
                    }
                }
            }
        }

        // Rule 4: overall dark/light balance.
        let total = (size * size) as u32;
        let darks = self.grid.iter().filter(|&&d| d).count() as u32;
        let percent = darks * 100 / total;
        let k = if percent >= 50 {
            (percent - 50) / 5
        } else {
            (50 - percent).div_ceil(5)
        };
        score += k * 10;

        score
    }
}

/// Encode `data` into a QR module matrix at error-correction level `ecc`, or
/// `None` if it does not fit in versions 1–4 (byte mode, up to 78 bytes at
/// [`QrEcc::Low`]). Rows are top-to-bottom, `true` = a dark module.
pub fn encode(data: &[u8], ecc: QrEcc) -> Option<Vec<Vec<bool>>> {
    // Smallest version whose data capacity holds the payload.
    let (version, ec_len, blocks) = (1..=4).find_map(|v| {
        let (ec_len, blocks) = BLOCKS[v - 1][ecc.index()];
        let total_data: usize = blocks.iter().sum();
        // 4-bit mode + 8-bit count = 12 bits header → ~2 bytes overhead.
        (total_data >= data.len() + 2).then_some((v, ec_len, blocks))
    })?;

    let gf = Gf::new();
    let total_data: usize = blocks.iter().sum();

    // Bit stream: mode (byte=0100), count (8 bits), payload, terminator, pad.
    let mut bits: Vec<bool> = Vec::new();
    let push = |bits: &mut Vec<bool>, value: u32, n: u32| {
        for i in (0..n).rev() {
            bits.push(getbit(value, i));
        }
    };
    push(&mut bits, 0b0100, 4);
    push(&mut bits, data.len() as u32, 8);
    for &b in data {
        push(&mut bits, b as u32, 8);
    }
    let cap_bits = total_data * 8;
    let term = 4.min(cap_bits.saturating_sub(bits.len()));
    bits.resize(bits.len() + term, false);
    // Pad to a byte boundary.
    if !bits.len().is_multiple_of(8) {
        bits.resize(bits.len().next_multiple_of(8), false);
    }
    let mut codewords: Vec<u8> = bits
        .chunks(8)
        .map(|c| c.iter().fold(0u8, |acc, &b| (acc << 1) | b as u8))
        .collect();
    // Pad to capacity with the alternating 0xEC / 0x11 pad codewords.
    let need = total_data - codewords.len();
    codewords.extend([0xECu8, 0x11].into_iter().cycle().take(need));

    // Split into blocks, compute ECC, interleave.
    let mut data_blocks: Vec<&[u8]> = Vec::new();
    let mut cursor = 0;
    for &len in blocks {
        data_blocks.push(&codewords[cursor..cursor + len]);
        cursor += len;
    }
    let ecc_blocks: Vec<Vec<u8>> = data_blocks.iter().map(|b| gf.rs_ecc(b, ec_len)).collect();

    let max_data = blocks.iter().copied().max().unwrap_or(0);
    let mut stream: Vec<u8> = Vec::new();
    for i in 0..max_data {
        for blk in &data_blocks {
            if i < blk.len() {
                stream.push(blk[i]);
            }
        }
    }
    for i in 0..ec_len {
        for blk in &ecc_blocks {
            stream.push(blk[i]);
        }
    }

    // Lay out modules, then pick the lowest-penalty mask.
    let mut base = Canvas::new(version);
    base.draw_function_patterns(version);
    base.draw_codewords(&stream);

    let mut best: Option<(u32, Canvas)> = None;
    for mask in 0..8u8 {
        let mut c = Canvas::new(version);
        c.grid.copy_from_slice(&base.grid);
        c.func.copy_from_slice(&base.func);
        c.apply_mask(mask);
        c.draw_format(ecc, mask);
        let score = c.penalty();
        if best.as_ref().map(|(s, _)| score < *s).unwrap_or(true) {
            best = Some((score, c));
        }
    }
    let canvas = best.unwrap().1;
    let size = canvas.size;
    Some(
        (0..size)
            .map(|y| (0..size).map(|x| canvas.grid[y * size + x]).collect())
            .collect(),
    )
}

/// A QR code rendered with half-block cells.
///
/// ```no_run
/// use tuika::{QrCode, QrEcc, Theme};
/// let theme = Theme::default();
/// let qr = QrCode::encode("https://example.com", QrEcc::Medium).expect("fits in v1-4");
/// // `qr` is a `View`; paint it or embed it in a `Flex`.
/// # let _ = (theme, qr);
/// ```
pub struct QrCode {
    matrix: Vec<Vec<bool>>,
    quiet: u16,
    dark: Color,
    light: Color,
}

impl QrCode {
    /// Encode `text` at level `ecc`, returning `None` if it does not fit in
    /// versions 1–4 (see [`encode`]).
    pub fn encode(text: impl AsRef<str>, ecc: QrEcc) -> Option<Self> {
        Some(Self::from_matrix(encode(text.as_ref().as_bytes(), ecc)?))
    }

    /// Build a view from a pre-computed module matrix (`true` = dark), for hosts
    /// that encode with their own library.
    pub fn from_matrix(matrix: Vec<Vec<bool>>) -> Self {
        Self {
            matrix,
            quiet: 4,
            dark: Color::Rgb(0, 0, 0),
            light: Color::Rgb(255, 255, 255),
        }
    }

    /// Set the quiet-zone width in modules (default `4`, the spec minimum).
    pub fn quiet_zone(mut self, modules: u16) -> Self {
        self.quiet = modules;
        self
    }

    /// Override the dark/light module colors (defaults black on white).
    pub fn colors(mut self, dark: Color, light: Color) -> Self {
        self.dark = dark;
        self.light = light;
        self
    }

    /// Module count per side including the quiet zone on both edges.
    fn full_size(&self) -> u16 {
        self.matrix.len() as u16 + self.quiet * 2
    }

    /// Dark if module `(x, y)` (quiet-zone-inclusive coordinates) is set.
    fn module(&self, x: u16, y: u16) -> bool {
        let q = self.quiet;
        if x < q || y < q {
            return false;
        }
        let (mx, my) = ((x - q) as usize, (y - q) as usize);
        self.matrix
            .get(my)
            .and_then(|r| r.get(mx))
            .copied()
            .unwrap_or(false)
    }
}

impl View for QrCode {
    fn measure(&self, available: Size) -> Size {
        let side = self.full_size();
        // Two module rows per terminal row via the half-block glyph.
        Size::new(side, side.div_ceil(2)).clamp_to(available)
    }

    fn render(&self, area: Rect, surface: &mut Surface, _ctx: &RenderCtx) {
        if area.is_empty() {
            return;
        }
        let side = self.full_size();
        let color = |dark: bool| if dark { self.dark } else { self.light };
        for row in 0..side.div_ceil(2) {
            let y = area.y.saturating_add(row);
            if y >= area.bottom() {
                break;
            }
            let top_my = row * 2;
            let bot_my = top_my + 1;
            for x in 0..side {
                let cx = area.x.saturating_add(x);
                if cx >= area.right() {
                    break;
                }
                let top = self.module(x, top_my);
                // A missing bottom row (odd module count) reads as light quiet zone.
                let bottom = bot_my < side && self.module(x, bot_my);
                let style = Style::default().fg(color(top)).bg(color(bottom));
                surface.set(cx, y, '▀', style);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::style::Theme;

    #[test]
    fn gf_tables_are_consistent() {
        let gf = Gf::new();
        // exp/log are inverses over the multiplicative group.
        for a in 1u16..256 {
            assert_eq!(gf.exp[gf.log[a as usize] as usize], a as u8);
        }
        // A known product in GF(256): a^1 * a^1 = a^2 = 4.
        assert_eq!(gf.mul(2, 2), 4);
        // Multiplication is commutative and 1 is the identity.
        assert_eq!(gf.mul(0x53, 0xCA), gf.mul(0xCA, 0x53));
        assert_eq!(gf.mul(0xAB, 1), 0xAB);
    }

    #[test]
    fn rs_ecc_has_expected_length_and_is_deterministic() {
        let gf = Gf::new();
        let data = [0x10, 0x20, 0x0c, 0x56, 0x61, 0x80, 0xec, 0x11];
        let a = gf.rs_ecc(&data, 10);
        let b = gf.rs_ecc(&data, 10);
        assert_eq!(a.len(), 10);
        assert_eq!(a, b);
    }

    #[test]
    fn rs_generator_and_ecc_match_known_vectors() {
        let gf = Gf::new();
        // The degree-2 generator is (x + 1)(x + α) = x² + α^25·x + α^1, whose
        // GF(256) coefficients are [1, 3, 2] (high→low, monic).
        assert_eq!(gf.rs_generator(2), vec![1, 3, 2]);
        // The canonical QR spec example (Annex I.2): the 16 data codewords of a
        // 1-M "01234567" numeric message produce these 10 EC codewords.
        let data = [
            0x10, 0x20, 0x0c, 0x56, 0x61, 0x80, 0xec, 0x11, 0xec, 0x11, 0xec, 0x11, 0xec, 0x11,
            0xec, 0x11,
        ];
        assert_eq!(
            gf.rs_ecc(&data, 10),
            vec![0xa5, 0x24, 0xd4, 0xc1, 0xed, 0x36, 0xc7, 0x87, 0x2c, 0x55]
        );
    }

    fn finder_at(m: &[Vec<bool>], ox: usize, oy: usize) -> bool {
        // A finder is a 7x7 with a dark border, a light ring, and a 3x3 dark core.
        for dy in 0..7 {
            for dx in 0..7 {
                let dist = (dx as i32 - 3).abs().max((dy as i32 - 3).abs());
                let want = dist != 2; // dark except the ring at distance 2
                if m[oy + dy][ox + dx] != want {
                    return false;
                }
            }
        }
        true
    }

    #[test]
    fn encodes_url_to_valid_v1_matrix_with_finders() {
        let m = encode(b"HELLO", QrEcc::Medium).expect("fits v1");
        // Version 1 is 21x21.
        assert_eq!(m.len(), 21);
        assert!(m.iter().all(|r| r.len() == 21));
        // Three finder patterns in the corners.
        assert!(finder_at(&m, 0, 0), "top-left finder");
        assert!(finder_at(&m, 14, 0), "top-right finder");
        assert!(finder_at(&m, 0, 14), "bottom-left finder");
        // The dark module is always set.
        assert!(m[21 - 8][8], "dark module present");
    }

    #[test]
    fn version_grows_with_payload_and_caps_at_v4() {
        // Longer payloads select larger versions (bigger matrices).
        let small = encode(b"hi", QrEcc::Low).unwrap().len();
        let bigger = encode(&[b'x'; 40], QrEcc::Low).unwrap().len();
        assert!(bigger > small, "{bigger} should exceed {small}");
        // Version 4 at Low holds 78 bytes; 79 does not fit.
        assert!(encode(&[b'a'; 78], QrEcc::Low).is_some());
        assert!(encode(&[b'a'; 79], QrEcc::Low).is_none());
    }

    #[test]
    fn timing_patterns_alternate() {
        let m = encode(b"test", QrEcc::Low).unwrap();
        let size = m.len();
        // Row/col 6 alternate dark/light between the finders.
        #[allow(clippy::needless_range_loop)]
        for i in 8..size - 8 {
            assert_eq!(m[6][i], i % 2 == 0, "timing row at {i}");
            assert_eq!(m[i][6], i % 2 == 0, "timing col at {i}");
        }
    }

    #[test]
    fn renders_half_blocks_with_quiet_zone() {
        let theme = Theme::default();
        let qr = QrCode::encode("HI", QrEcc::Low).unwrap();
        // 21 modules + 8 quiet = 29 wide; 15 rows tall (ceil(29/2)).
        assert_eq!(qr.measure(Size::new(80, 40)), Size::new(29, 15));
        let buf = crate::testing::render(&qr, 29, 15, &theme);
        // The top-left cell is quiet zone → light background, half-block glyph.
        assert_eq!(buf[(0, 0)].symbol(), "▀");
        assert_eq!(buf[(0, 0)].fg, Color::Rgb(255, 255, 255));
    }

    #[test]
    fn from_matrix_and_custom_colors_do_not_panic() {
        let theme = Theme::default();
        let m = vec![vec![true, false], vec![false, true]];
        let qr = QrCode::from_matrix(m)
            .quiet_zone(1)
            .colors(Color::Rgb(1, 2, 3), Color::Rgb(4, 5, 6));
        for (w, h) in [(0u16, 0u16), (1, 1), (2, 2)] {
            let _ = crate::testing::render(&qr, w, h, &theme);
        }
    }
}

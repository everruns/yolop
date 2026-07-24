//! [`AsciiFont`] — large "figlet-style" block text from an embedded 5-row font.
//!
//! Each character is a 5-row bitmap painted with a solid block glyph in the
//! theme accent (overridable). Letters are case-insensitive (lowercase maps to
//! the uppercase form); unknown characters render as a blank space. This is the
//! banner/headline analog of OpenTUI's `ASCIIFont`, kept dependency-free by
//! bundling one compact font rather than parsing FIGlet files.

use ratatui_core::layout::Rect;
use ratatui_core::style::{Color, Style};

use crate::geometry::Size;
use crate::surface::Surface;
use crate::view::{RenderCtx, View};

/// Every glyph in the bundled font is this many rows tall.
pub const FONT_HEIGHT: u16 = 5;

/// Columns of blank space inserted between adjacent glyphs.
const LETTER_SPACING: u16 = 1;

/// The 5-row bitmap for `ch` (`#` = filled), or `None` if the font has no glyph.
/// Lowercase is folded to uppercase. A space returns a 3-wide blank.
fn glyph(ch: char) -> Option<[&'static str; 5]> {
    let up = ch.to_ascii_uppercase();
    Some(match up {
        ' ' => ["   ", "   ", "   ", "   ", "   "],
        'A' => [" ### ", "#   #", "#####", "#   #", "#   #"],
        'B' => ["#### ", "#   #", "#### ", "#   #", "#### "],
        'C' => [" ####", "#    ", "#    ", "#    ", " ####"],
        'D' => ["#### ", "#   #", "#   #", "#   #", "#### "],
        'E' => ["#####", "#    ", "#### ", "#    ", "#####"],
        'F' => ["#####", "#    ", "#### ", "#    ", "#    "],
        'G' => [" ####", "#    ", "#  ##", "#   #", " ####"],
        'H' => ["#   #", "#   #", "#####", "#   #", "#   #"],
        'I' => ["#####", "  #  ", "  #  ", "  #  ", "#####"],
        'J' => ["  ###", "   # ", "   # ", "#  # ", " ##  "],
        'K' => ["#   #", "#  # ", "###  ", "#  # ", "#   #"],
        'L' => ["#    ", "#    ", "#    ", "#    ", "#####"],
        'M' => ["#   #", "## ##", "# # #", "#   #", "#   #"],
        'N' => ["#   #", "##  #", "# # #", "#  ##", "#   #"],
        'O' => [" ### ", "#   #", "#   #", "#   #", " ### "],
        'P' => ["#### ", "#   #", "#### ", "#    ", "#    "],
        'Q' => [" ### ", "#   #", "# # #", "#  # ", " ## #"],
        'R' => ["#### ", "#   #", "#### ", "#  # ", "#   #"],
        'S' => [" ####", "#    ", " ### ", "    #", "#### "],
        'T' => ["#####", "  #  ", "  #  ", "  #  ", "  #  "],
        'U' => ["#   #", "#   #", "#   #", "#   #", " ### "],
        'V' => ["#   #", "#   #", "#   #", " # # ", "  #  "],
        'W' => ["#   #", "#   #", "# # #", "## ##", "#   #"],
        'X' => ["#   #", " # # ", "  #  ", " # # ", "#   #"],
        'Y' => ["#   #", " # # ", "  #  ", "  #  ", "  #  "],
        'Z' => ["#####", "   # ", "  #  ", " #   ", "#####"],
        '0' => [" ### ", "#  ##", "# # #", "##  #", " ### "],
        '1' => ["  #  ", " ##  ", "  #  ", "  #  ", "#####"],
        '2' => [" ### ", "#   #", "  ## ", " #   ", "#####"],
        '3' => ["#### ", "    #", " ### ", "    #", "#### "],
        '4' => ["#  # ", "#  # ", "#####", "   # ", "   # "],
        '5' => ["#####", "#    ", "#### ", "    #", "#### "],
        '6' => [" ####", "#    ", "#### ", "#   #", " ### "],
        '7' => ["#####", "   # ", "  #  ", " #   ", " #   "],
        '8' => [" ### ", "#   #", " ### ", "#   #", " ### "],
        '9' => [" ### ", "#   #", " ####", "    #", "#### "],
        '.' => [" ", " ", " ", " ", "#"],
        ',' => ["  ", "  ", "  ", " #", "# "],
        '!' => ["#", "#", "#", " ", "#"],
        '?' => [" ### ", "#   #", "  ## ", "     ", "  #  "],
        ':' => [" ", "#", " ", "#", " "],
        '-' => ["   ", "   ", "###", "   ", "   "],
        '+' => ["   ", " # ", "###", " # ", "   "],
        '=' => ["   ", "###", "   ", "###", "   "],
        '/' => ["    #", "   # ", "  #  ", " #   ", "#    "],
        _ => return None,
    })
}

/// The pixel width of `ch`'s glyph (0 for an unknown character).
fn glyph_width(ch: char) -> u16 {
    glyph(ch).map(|g| g[0].len() as u16).unwrap_or(0)
}

/// Big block-letter text rendered from the bundled 5-row font.
///
/// ```no_run
/// use tuika::{AsciiFont, Theme};
/// let theme = Theme::default();
/// let banner = AsciiFont::new("HELLO");
/// // `banner` is a `View` five rows tall; paint it or embed it in a `Flex`.
/// # let _ = (theme, banner);
/// ```
pub struct AsciiFont {
    text: String,
    color: Option<Color>,
    fill: char,
}

impl AsciiFont {
    /// A banner spelling `text`. Unknown characters render blank.
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            color: None,
            fill: '█',
        }
    }

    /// Override the glyph color (defaults to the theme accent).
    pub fn color(mut self, color: Color) -> Self {
        self.color = Some(color);
        self
    }

    /// Override the fill glyph painted for each set pixel (default `█`).
    pub fn fill(mut self, fill: char) -> Self {
        self.fill = fill;
        self
    }

    /// Total pixel width of the rendered text (glyphs plus inter-letter spacing).
    fn total_width(&self) -> u16 {
        let mut width = 0u16;
        let mut first = true;
        for ch in self.text.chars() {
            let gw = glyph_width(ch);
            if gw == 0 {
                continue;
            }
            if !first {
                width = width.saturating_add(LETTER_SPACING);
            }
            width = width.saturating_add(gw);
            first = false;
        }
        width
    }
}

impl View for AsciiFont {
    fn measure(&self, available: Size) -> Size {
        let width = self.total_width().min(available.width);
        let height = if self.text.is_empty() {
            0
        } else {
            FONT_HEIGHT.min(available.height)
        };
        Size::new(width, height)
    }

    fn render(&self, area: Rect, surface: &mut Surface, ctx: &RenderCtx) {
        if area.is_empty() {
            return;
        }
        let style = Style::default().fg(self.color.unwrap_or(ctx.theme.accent));
        let mut x = area.x;
        for ch in self.text.chars() {
            let Some(rows) = glyph(ch) else { continue };
            let gw = rows[0].len() as u16;
            for (r, row) in rows.iter().enumerate() {
                let y = area.y.saturating_add(r as u16);
                if y >= area.bottom() {
                    break;
                }
                for (c, cell) in row.chars().enumerate() {
                    if cell == '#' {
                        surface.set(x + c as u16, y, self.fill, style);
                    }
                }
            }
            x = x.saturating_add(gw).saturating_add(LETTER_SPACING);
            if x >= area.right() {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::style::Theme;
    use crate::test_support::{rainbow_theme, row};

    #[test]
    fn measures_five_rows_and_summed_width() {
        let font = AsciiFont::new("HI");
        // 'H' and 'I' are each 5 wide, plus one spacing column = 11.
        assert_eq!(font.measure(Size::new(80, 24)), Size::new(11, 5));
        // Empty string has no height.
        assert_eq!(AsciiFont::new("").measure(Size::new(80, 24)).height, 0);
    }

    #[test]
    fn renders_block_glyphs_case_insensitively() {
        let theme = Theme::default();
        // Lowercase folds to the uppercase glyph.
        let buf = crate::testing::render(&AsciiFont::new("a"), 6, 5, &theme);
        let filled: usize = (0..5)
            .map(|y| row(&buf, y).chars().filter(|&c| c == '█').count())
            .sum();
        assert!(filled > 0, "at least some pixels painted");
        // Top row of 'A' is " ### " → three blocks in the middle.
        assert_eq!(row(&buf, 0).matches('█').count(), 3);
    }

    #[test]
    fn color_defaults_to_accent_and_is_overridable() {
        let t = rainbow_theme();
        let buf = crate::testing::render(&AsciiFont::new("I"), 6, 5, &t);
        // The top row of 'I' is all five filled → accent foreground.
        assert_eq!(buf[(0, 0)].fg, t.accent);

        let custom = Color::Rgb(1, 2, 3);
        let buf = crate::testing::render(&AsciiFont::new("I").color(custom), 6, 5, &t);
        assert_eq!(buf[(0, 0)].fg, custom);
    }

    #[test]
    fn unknown_chars_and_tiny_areas_do_not_panic() {
        let theme = Theme::default();
        // A tilde has no glyph; it is skipped, not drawn.
        let _ = crate::testing::render(&AsciiFont::new("~"), 10, 5, &theme);
        for (w, h) in [(0u16, 0u16), (1, 1), (2, 3)] {
            let _ = crate::testing::render(&AsciiFont::new("HELLO 123"), w, h, &theme);
        }
    }
}

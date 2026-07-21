//! Grapheme-aware display-width measurement.
//!
//! A terminal cell holds one *grapheme cluster*, not one Unicode scalar, so
//! width must be measured per cluster — otherwise multi-scalar emoji (ZWJ
//! families, regional-indicator flags, skin-tone modifiers, and text glyphs
//! promoted to emoji presentation by U+FE0F) are miscounted. `unicode-width`
//! answers per scalar; this module folds those scalars into the width the
//! terminal will actually advance.
//!
//! Every layout, wrap, and paint path routes through [`str_cols`] /
//! [`grapheme_cols`] so measurement and rendering can never disagree — a
//! mismatch is what smears wide glyphs across the grid.

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthChar;

/// Emoji variation selector — forces the *emoji* (double-width) presentation of
/// an otherwise text-default glyph, e.g. `❤️` = U+2764 U+FE0F.
const VS16: char = '\u{FE0F}';
/// Text variation selector — forces the *text* (single-width) presentation of
/// an otherwise emoji-default glyph, e.g. `❤︎` = U+2764 U+FE0E.
const VS15: char = '\u{FE0E}';

/// Display columns a single grapheme cluster occupies in the terminal grid.
///
/// The cluster is painted into one starting cell and this reports how many
/// columns the cursor advances past it: `0` for an entirely zero-width cluster,
/// otherwise `1` or `2`. Emoji rules that `UnicodeWidthChar` cannot see at the
/// scalar level are applied here:
///
/// - a cluster carrying **VS16** (U+FE0F) renders in emoji presentation → 2;
/// - a cluster carrying **VS15** (U+FE0E) renders in text presentation → 1;
/// - a **ZWJ sequence / flag / emoji-modifier** cluster renders as one
///   double-width glyph — its scalar-width sum is ≥ 2, so the `min(2)` clamp
///   collapses it to a single 2-column cell instead of several;
/// - otherwise the width is the scalar-width sum (a base glyph plus zero-width
///   combining marks keeps the base's width).
pub fn grapheme_cols(cluster: &str) -> u16 {
    let mut vs16 = false;
    let mut vs15 = false;
    let mut sum: u16 = 0;
    for ch in cluster.chars() {
        match ch {
            VS16 => vs16 = true,
            VS15 => vs15 = true,
            _ => sum = sum.saturating_add(UnicodeWidthChar::width(ch).unwrap_or(0) as u16),
        }
    }
    if vs16 {
        return 2;
    }
    if vs15 {
        return 1;
    }
    // A printable cluster never occupies more than two cells; the clamp is what
    // turns a multi-emoji ZWJ/flag/modifier sequence into one wide cell.
    sum.min(2)
}

/// Display columns of a string, summed over its grapheme clusters.
pub fn str_cols(s: &str) -> u16 {
    s.graphemes(true)
        .map(grapheme_cols)
        .fold(0, u16::saturating_add)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_and_cjk_match_scalar_width() {
        assert_eq!(grapheme_cols("a"), 1);
        assert_eq!(grapheme_cols("世"), 2);
        assert_eq!(str_cols("ab世"), 4);
    }

    #[test]
    fn combining_marks_keep_base_width() {
        // "e" + combining acute accent is one grapheme, one column.
        assert_eq!(grapheme_cols("e\u{0301}"), 1);
    }

    #[test]
    fn single_scalar_emoji_is_wide() {
        assert_eq!(grapheme_cols("✅"), 2); // U+2705, emoji default
        assert_eq!(grapheme_cols("🚀"), 2); // U+1F680
    }

    #[test]
    fn variation_selectors_pick_presentation() {
        assert_eq!(grapheme_cols("❤\u{FE0F}"), 2, "VS16 forces emoji (wide)");
        assert_eq!(grapheme_cols("❤\u{FE0E}"), 1, "VS15 forces text (narrow)");
    }

    #[test]
    fn zwj_flag_and_modifier_clamp_to_one_wide_cell() {
        assert_eq!(grapheme_cols("👨\u{200D}👩\u{200D}👧"), 2, "ZWJ family");
        assert_eq!(grapheme_cols("🇺🇸"), 2, "regional-indicator flag");
        assert_eq!(grapheme_cols("👍🏽"), 2, "skin-tone modifier");
        assert_eq!(grapheme_cols("1\u{FE0F}\u{20E3}"), 2, "keycap");
    }

    #[test]
    fn str_cols_sums_emoji_run() {
        // family + space + flag = 2 + 1 + 2
        assert_eq!(str_cols("👨\u{200D}👩\u{200D}👧 🇺🇸"), 5);
    }
}

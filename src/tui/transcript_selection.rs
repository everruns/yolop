//! Content-anchored text selection for the full-screen transcript.
//!
//! A bare terminal — and [`tuika::mouse::SelectionState`] — track a selection in
//! *viewport* cells, so scrolling the transcript invalidates it and a drag is
//! capped at the single visible window. This anchors the selection in
//! *content-row* space instead: the wrapped-row index into the transcript cache,
//! which is stable as the view scrolls. The selection therefore survives
//! scrolling and can span the whole history; the host maps content rows back to
//! the current window for painting and reads the selected text out of the cache.
//!
//! Columns stay viewport-absolute (the transcript never scrolls horizontally),
//! so mapping a cell to/from the screen only shifts the row.

use std::time::{Duration, Instant};

const DOUBLE_CLICK_INTERVAL: Duration = Duration::from_millis(500);

/// A cell in content space: `(column, content_row)`. `column` is a
/// viewport-absolute column; `content_row` indexes the wrapped transcript rows.
pub(crate) type ContentCell = (u16, usize);

/// A normalized content-space selection in reading order: `start` is at or
/// before `end` ordered by `(row, column)`. Both endpoints are inclusive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ContentRange {
    pub start: ContentCell,
    pub end: ContentCell,
}

impl ContentRange {
    fn between(a: ContentCell, b: ContentCell) -> Self {
        let (start, end) = if (a.1, a.0) <= (b.1, b.0) {
            (a, b)
        } else {
            (b, a)
        };
        ContentRange { start, end }
    }
}

/// Tracks a click-drag / double-click transcript selection in content space.
///
/// Left `press` starts (and clears any previous selection); `drag` extends;
/// `release` finishes. Two plain releases on the same content cell within the
/// double-click interval queue a word selection, which the host resolves against
/// the freshly rendered buffer via [`take_pending_word`](Self::take_pending_word)
/// and [`select_word`](Self::select_word).
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct TranscriptSelection {
    anchor: ContentCell,
    cursor: ContentCell,
    pressed: bool,
    selecting: bool,
    last_click: Option<(ContentCell, Instant)>,
    pending_word: Option<ContentCell>,
}

impl TranscriptSelection {
    pub fn new() -> Self {
        Self::default()
    }

    /// Begin a gesture at `cell`. Returns `true` when a redraw is warranted
    /// (an existing selection was cleared).
    pub fn press(&mut self, cell: ContentCell) -> bool {
        self.anchor = cell;
        self.cursor = cell;
        self.pressed = true;
        let had = self.selecting;
        self.selecting = false;
        // Keep `last_click` so a press/release pair can complete a double click.
        had
    }

    /// Extend the in-progress gesture to `cell`. Returns `true` when handled.
    pub fn drag(&mut self, cell: ContentCell) -> bool {
        if !self.pressed {
            return false;
        }
        self.cursor = cell;
        self.selecting = self.cursor != self.anchor;
        self.last_click = None;
        self.pending_word = None;
        true
    }

    /// Finish the gesture at `cell`. Returns `true` when handled. A release with
    /// no drag records the click for double-click detection; a second click on
    /// the same cell queues a pending word selection.
    pub fn release(&mut self, cell: ContentCell) -> bool {
        if !self.pressed {
            return false;
        }
        self.cursor = cell;
        self.pressed = false;
        self.selecting = self.cursor != self.anchor;
        if self.selecting {
            self.last_click = None;
        } else {
            let now = Instant::now();
            let is_double = self.last_click.is_some_and(|(previous, at)| {
                previous == cell && now.duration_since(at) <= DOUBLE_CLICK_INTERVAL
            });
            self.last_click = Some((cell, now));
            self.pending_word = is_double.then_some(cell);
        }
        true
    }

    /// Take a queued double-click position so the host can resolve its word.
    pub fn take_pending_word(&mut self) -> Option<ContentCell> {
        self.pending_word.take()
    }

    /// Adopt a resolved word span as the active selection.
    pub fn select_word(&mut self, range: ContentRange) {
        self.anchor = range.start;
        self.cursor = range.end;
        self.selecting = true;
    }

    /// Clear the selection (e.g. on Esc, key input, or a new turn).
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// Whether a button is currently held (a drag is in progress).
    pub fn is_pressed(&self) -> bool {
        self.pressed
    }

    /// Whether a non-empty selection currently exists.
    pub fn is_active(&self) -> bool {
        self.selecting
    }

    /// The current selection in reading order, or `None` when nothing is
    /// selected.
    pub fn range(&self) -> Option<ContentRange> {
        self.selecting
            .then(|| ContentRange::between(self.anchor, self.cursor))
    }
}

/// Reconstruct selected wrapped text using the transcript source. Terminal
/// buffers expose visual rows only, so a soft wrap otherwise looks identical to
/// a source newline when copied.
pub(super) fn restore_source_breaks<'a>(
    sources: impl IntoIterator<Item = &'a str>,
    rendered: &str,
) -> String {
    let sources: Vec<&str> = sources.into_iter().collect();
    let rows: Vec<&str> = rendered.split('\n').collect();
    let Some(first) = rows.first() else {
        return String::new();
    };

    let mut restored = (*first).to_string();
    let mut matched_source = None;
    let mut search_start = 0;
    for pair in rows.windows(2) {
        let matched = matched_source
            .and_then(|index| source_separator(sources[index], search_start, pair[0], pair[1]))
            .map(|(separator, next_start)| (matched_source.unwrap(), separator, next_start))
            .or_else(|| {
                sources.iter().enumerate().find_map(|(index, source)| {
                    source_separator(source, 0, pair[0], pair[1])
                        .map(|(separator, next_start)| (index, separator, next_start))
                })
            });
        let separator = if let Some((index, separator, next_start)) = matched {
            matched_source = Some(index);
            search_start = next_start;
            separator
        } else {
            matched_source = None;
            search_start = 0;
            "\n"
        };
        restored.push_str(separator);
        restored.push_str(pair[1]);
    }
    restored
}

fn source_separator<'a>(
    source: &'a str,
    search_start: usize,
    left: &str,
    right: &str,
) -> Option<(&'a str, usize)> {
    let left = char_suffix(left.trim_end(), 32);
    let right = char_prefix(right.trim_start(), 32);
    if left.is_empty() || right.is_empty() {
        return None;
    }

    for (relative_start, _) in source[search_start..].match_indices(left) {
        let gap_start = search_start + relative_start + left.len();
        let tail = &source[gap_start..];
        let gap_len = tail
            .char_indices()
            .take_while(|(_, ch)| ch.is_whitespace())
            .map(|(index, ch)| index + ch.len_utf8())
            .last()
            .unwrap_or(0);
        if tail[gap_len..].starts_with(right) {
            let gap = &tail[..gap_len];
            let separator = if gap.contains(['\n', '\r']) {
                "\n"
            } else {
                gap
            };
            return Some((separator, gap_start + gap_len));
        }
    }
    None
}

fn char_suffix(value: &str, max_chars: usize) -> &str {
    let start = value
        .char_indices()
        .rev()
        .nth(max_chars.saturating_sub(1))
        .map_or(0, |(index, _)| index);
    &value[start..]
}

fn char_prefix(value: &str, max_chars: usize) -> &str {
    let end = value
        .char_indices()
        .nth(max_chars)
        .map_or(value.len(), |(index, _)| index);
    &value[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drag_across_rows_selects_in_reading_order() {
        let mut sel = TranscriptSelection::new();
        assert!(!sel.press((5, 40))); // fresh press: nothing to redraw yet
        assert!(sel.drag((2, 3))); // drag up-left, far above the anchor
        assert!(sel.release((2, 3)));
        let range = sel.range().expect("a drag selects");
        // Reading order: (col=2,row=3) precedes (col=5,row=40).
        assert_eq!(range.start, (2, 3));
        assert_eq!(range.end, (5, 40));
        assert!(sel.is_active());
    }

    #[test]
    fn selection_survives_a_conceptual_scroll() {
        // The rows are content-space, so they mean the same thing regardless of
        // where the viewport currently sits — nothing here clears on scroll.
        let mut sel = TranscriptSelection::new();
        sel.press((0, 100));
        sel.drag((10, 250));
        let range = sel.range().expect("selection");
        assert_eq!(range.start, (0, 100));
        assert_eq!(range.end, (10, 250));
    }

    #[test]
    fn plain_click_leaves_no_selection() {
        let mut sel = TranscriptSelection::new();
        sel.press((3, 7));
        sel.release((3, 7));
        assert!(sel.range().is_none());
        assert!(!sel.is_active());
    }

    #[test]
    fn a_new_press_clears_the_previous_selection() {
        let mut sel = TranscriptSelection::new();
        sel.press((0, 0));
        sel.drag((3, 2));
        sel.release((3, 2));
        assert!(sel.range().is_some());
        assert!(sel.press((1, 5)));
        assert!(sel.range().is_none());
    }

    #[test]
    fn double_click_queues_a_pending_word() {
        let mut sel = TranscriptSelection::new();
        sel.press((9, 4));
        sel.release((9, 4));
        sel.press((9, 4));
        assert!(sel.release((9, 4)));
        assert_eq!(sel.take_pending_word(), Some((9, 4)));
        sel.select_word(ContentRange {
            start: (7, 4),
            end: (17, 4),
        });
        let range = sel.range().expect("word selection");
        assert_eq!(range.start, (7, 4));
        assert_eq!(range.end, (17, 4));
    }

    #[test]
    fn copied_soft_wrap_uses_source_space() {
        let sources = ["Send an Agent a request early on, then gather the result."];
        let rendered = "Send an Agent a request early on, then\ngather the result.";
        assert_eq!(
            restore_source_breaks(sources, rendered),
            "Send an Agent a request early on, then gather the result."
        );
    }

    #[test]
    fn copied_hard_break_stays_a_newline() {
        let sources = ["first line\nsecond line"];
        assert_eq!(
            restore_source_breaks(sources, "first line\nsecond line"),
            "first line\nsecond line"
        );
    }

    #[test]
    fn copied_wrap_inside_a_long_word_has_no_separator() {
        let sources = ["supercalifragilistic"];
        assert_eq!(
            restore_source_breaks(sources, "supercali\nfragilistic"),
            "supercalifragilistic"
        );
    }

    #[test]
    fn unmatched_rendered_rows_keep_their_newline() {
        let sources = ["different source"];
        assert_eq!(
            restore_source_breaks(sources, "rendered heading\nrendered body"),
            "rendered heading\nrendered body"
        );
    }

    #[test]
    fn source_break_matching_is_unicode_safe() {
        let sources = ["🙂 café wraps here"];
        assert_eq!(
            restore_source_breaks(sources, "🙂 café\nwraps here"),
            "🙂 café wraps here"
        );
    }

    #[test]
    fn repeated_text_matches_in_source_order() {
        let sources = ["a b a\nb"];
        assert_eq!(restore_source_breaks(sources, "a\nb\na\nb"), "a b a\nb");
    }

    #[test]
    fn does_not_join_rows_across_transcript_entries() {
        let sources = ["first entry ends here", "here second entry begins"];
        assert_eq!(
            restore_source_breaks(sources, "first entry ends here\nhere second entry begins"),
            "first entry ends here\nhere second entry begins"
        );
    }
}

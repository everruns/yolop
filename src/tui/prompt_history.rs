//! Composer prompt history: shell-style Up/Down recall of previously submitted
//! prompts, mirroring the chat-composer history in OpenAI's Codex CLI.
//!
//! Entries persist across sessions in a JSONL file under the platform data dir
//! (next to the session logs), so recall survives restarts. The file is a
//! plain append log of `{"text": "..."}` lines; on startup we read the tail and
//! keep the most recent [`MAX_ENTRIES`]. Persistence is best-effort — any I/O
//! error degrades to in-memory-only history rather than surfacing to the user,
//! because losing composer recall must never block a turn.
//!
//! Navigation model (matches Codex): a single cursor walks the entry list from
//! newest to oldest. The in-progress draft is saved when browsing begins and
//! restored when the user pages back past the newest entry. The host gates when
//! Up/Down recall (vs. move the cursor within a multi-line composer) — see
//! `App::try_history_prev`/`try_history_next`.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;

/// Keep recall bounded so a long-lived history file never grows without limit
/// or slows startup. The newest entries are the ones worth recalling.
const MAX_ENTRIES: usize = 1000;

/// Recall store for previously submitted composer prompts.
pub(crate) struct PromptHistory {
    /// Submitted prompts, oldest first, with consecutive duplicates collapsed.
    entries: Vec<String>,
    /// Index into `entries` currently being viewed, or `None` when not browsing.
    cursor: Option<usize>,
    /// The composer text captured when browsing began, restored on page-past.
    draft: Option<String>,
    /// Persistence target; `None` disables the append log (tests, or when the
    /// platform data dir can't be resolved).
    path: Option<PathBuf>,
}

impl PromptHistory {
    /// In-memory-only history (no persistence). Used by tests and any host
    /// without a resolvable data dir.
    pub(crate) fn in_memory() -> Self {
        Self {
            entries: Vec::new(),
            cursor: None,
            draft: None,
            path: None,
        }
    }

    /// Load persisted history from the default data-dir path, seeding recall
    /// with prior sessions' prompts. Falls back to in-memory-only if the path
    /// can't be resolved or read.
    pub(crate) fn load() -> Self {
        let Some(path) = default_history_path() else {
            return Self::in_memory();
        };
        let mut entries = read_entries(&path);
        // Trim to the most recent MAX_ENTRIES; older lines stay on disk but are
        // not loaded into recall.
        if entries.len() > MAX_ENTRIES {
            entries.drain(0..entries.len() - MAX_ENTRIES);
        }
        Self {
            entries,
            cursor: None,
            draft: None,
            path: Some(path),
        }
    }

    /// Record a submitted prompt. Empty text and consecutive duplicates are
    /// ignored (recalling the same line twice is noise). Resets any in-progress
    /// browse so the next Up starts from the newest entry. Appends to the
    /// persistence log on a best-effort basis.
    pub(crate) fn record(&mut self, text: &str) {
        self.reset_navigation();
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        if self.entries.last().map(String::as_str) == Some(text) {
            return;
        }
        self.entries.push(text.to_string());
        if self.entries.len() > MAX_ENTRIES {
            self.entries.remove(0);
        }
        self.append_to_log(text);
    }

    /// Abandon any in-progress browse without touching the composer. Called on
    /// submit so a fresh Up begins from the newest entry.
    pub(crate) fn reset_navigation(&mut self) {
        self.cursor = None;
        self.draft = None;
    }

    /// Whether a browse is currently in progress (cursor parked on an entry).
    pub(crate) fn is_browsing(&self) -> bool {
        self.cursor.is_some()
    }

    /// The entry currently being viewed, if browsing. The host compares this to
    /// the live composer text to tell an unmodified recall from a user edit.
    pub(crate) fn current_entry(&self) -> Option<&str> {
        self.cursor.map(|i| self.entries[i].as_str())
    }

    /// Whether there is anything to recall or search.
    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The entry at `index`, if it exists.
    pub(crate) fn entry_at(&self, index: usize) -> Option<&str> {
        self.entries.get(index).map(String::as_str)
    }

    /// Reverse (newest-first) case-insensitive substring search for the most
    /// recent entry at or below `start_at` that contains `query`. An empty query
    /// matches every entry, so a bare Ctrl+R walks straight back through history.
    /// Returns the matched `(index, entry)`.
    pub(crate) fn reverse_search(&self, query: &str, start_at: usize) -> Option<(usize, String)> {
        if self.entries.is_empty() {
            return None;
        }
        let start = start_at.min(self.entries.len() - 1);
        let needle = query.to_lowercase();
        (0..=start)
            .rev()
            .find(|&i| self.entries[i].to_lowercase().contains(&needle))
            .map(|i| (i, self.entries[i].clone()))
    }

    /// The newest entry's index, if any — the natural starting point for a fresh
    /// reverse search.
    pub(crate) fn newest_index(&self) -> Option<usize> {
        self.entries.len().checked_sub(1)
    }

    /// Move to the previous (older) entry, saving `current` as the draft on the
    /// first step. Returns the entry to place in the composer, or `None` when
    /// there is nothing older (history empty, or already at the oldest entry).
    pub(crate) fn navigate_up(&mut self, current: &str) -> Option<String> {
        match self.cursor {
            None => {
                let last = self.entries.len().checked_sub(1)?;
                self.draft = Some(current.to_string());
                self.cursor = Some(last);
                Some(self.entries[last].clone())
            }
            Some(0) => None,
            Some(i) => {
                self.cursor = Some(i - 1);
                Some(self.entries[i - 1].clone())
            }
        }
    }

    /// Move to the next (newer) entry. Paging past the newest entry ends the
    /// browse and restores the saved draft. Returns the text to place in the
    /// composer, or `None` when not browsing.
    pub(crate) fn navigate_down(&mut self, _current: &str) -> Option<String> {
        let i = self.cursor?;
        if i + 1 < self.entries.len() {
            self.cursor = Some(i + 1);
            Some(self.entries[i + 1].clone())
        } else {
            self.cursor = None;
            Some(self.draft.take().unwrap_or_default())
        }
    }

    fn append_to_log(&self, text: &str) {
        let Some(path) = &self.path else {
            return;
        };
        // Best-effort: create the parent dir and append one JSON line. Any error
        // is swallowed — recall still works for the rest of this session.
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let line = serde_json::json!({ "text": text }).to_string();
        if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
            let _ = writeln!(file, "{line}");
        }
    }
}

/// Default persistence path: `<data_dir>/yolop/prompt_history.jsonl`, alongside
/// the per-session logs. Mirrors [`crate::runtime::session_log::default_sessions_dir`]'s
/// use of the platform data dir.
fn default_history_path() -> Option<PathBuf> {
    dirs::data_dir().map(|p| p.join("yolop").join("prompt_history.jsonl"))
}

/// Read `{"text": "..."}` lines from the log, skipping blank or malformed lines.
fn read_entries(path: &PathBuf) -> Vec<String> {
    let Ok(file) = File::open(path) else {
        return Vec::new();
    };
    let mut entries = Vec::new();
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Ok(serde_json::Value::Object(obj)) = serde_json::from_str::<serde_json::Value>(line)
            && let Some(serde_json::Value::String(text)) = obj.get("text")
            && !text.is_empty()
        {
            entries.push(text.clone());
        }
    }
    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seeded(entries: &[&str]) -> PromptHistory {
        let mut h = PromptHistory::in_memory();
        for e in entries {
            h.record(e);
        }
        h
    }

    #[test]
    fn record_skips_empty_and_consecutive_duplicates() {
        let mut h = PromptHistory::in_memory();
        h.record("hello");
        h.record("hello"); // dup
        h.record("   "); // blank
        h.record("world");
        h.record("hello"); // non-consecutive dup is kept
        assert_eq!(h.entries, vec!["hello", "world", "hello"]);
    }

    #[test]
    fn record_trims_surrounding_whitespace() {
        let mut h = PromptHistory::in_memory();
        h.record("  spaced  ");
        assert_eq!(h.entries, vec!["spaced"]);
    }

    #[test]
    fn up_walks_from_newest_to_oldest_then_stops() {
        let mut h = seeded(&["one", "two", "three"]);
        assert_eq!(h.navigate_up(""), Some("three".to_string()));
        assert_eq!(h.navigate_up("three"), Some("two".to_string()));
        assert_eq!(h.navigate_up("two"), Some("one".to_string()));
        // At the oldest entry, further Up is a no-op.
        assert_eq!(h.navigate_up("one"), None);
        assert_eq!(h.current_entry(), Some("one"));
    }

    #[test]
    fn down_restores_draft_after_paging_past_newest() {
        let mut h = seeded(&["one", "two"]);
        assert_eq!(h.navigate_up("draft"), Some("two".to_string()));
        assert_eq!(h.navigate_up("two"), Some("one".to_string()));
        // Page back down: one -> two -> draft.
        assert_eq!(h.navigate_down("one"), Some("two".to_string()));
        assert_eq!(h.navigate_down("two"), Some("draft".to_string()));
        assert!(!h.is_browsing());
    }

    #[test]
    fn down_without_browsing_is_a_no_op() {
        let mut h = seeded(&["one"]);
        assert_eq!(h.navigate_down("whatever"), None);
    }

    #[test]
    fn up_on_empty_history_does_not_begin_browsing() {
        let mut h = PromptHistory::in_memory();
        assert_eq!(h.navigate_up("draft"), None);
        assert!(!h.is_browsing());
    }

    #[test]
    fn record_resets_an_in_progress_browse() {
        let mut h = seeded(&["one", "two"]);
        assert_eq!(h.navigate_up(""), Some("two".to_string()));
        assert!(h.is_browsing());
        h.record("three");
        assert!(!h.is_browsing());
        // The next Up starts from the newest (freshly recorded) entry.
        assert_eq!(h.navigate_up(""), Some("three".to_string()));
    }

    #[test]
    fn reverse_search_finds_newest_match_and_cycles_older() {
        let h = seeded(&["deploy staging", "run tests", "deploy prod", "run lint"]);
        let newest = h.newest_index().unwrap();
        // From the newest, "deploy" matches "deploy prod" (index 2) first.
        let (i, entry) = h.reverse_search("deploy", newest).unwrap();
        assert_eq!((i, entry.as_str()), (2, "deploy prod"));
        // Cycling older (search below index 2) reaches "deploy staging".
        let (i, entry) = h.reverse_search("deploy", i - 1).unwrap();
        assert_eq!((i, entry.as_str()), (0, "deploy staging"));
        // A query absent from index 0 (and below) yields no match there.
        assert_eq!(h.reverse_search("prod", 0), None);
    }

    #[test]
    fn reverse_search_is_case_insensitive_and_empty_matches_newest() {
        let h = seeded(&["First", "SECOND"]);
        assert_eq!(h.reverse_search("second", 1).unwrap().1, "SECOND");
        // Empty query matches the newest entry.
        assert_eq!(h.reverse_search("", 1).unwrap(), (1, "SECOND".to_string()));
    }

    #[test]
    fn reverse_search_on_empty_history_is_none() {
        let h = PromptHistory::in_memory();
        assert!(h.newest_index().is_none());
        assert_eq!(h.reverse_search("x", 0), None);
    }

    #[test]
    fn persistence_round_trips_through_the_log_file() {
        let dir = std::env::temp_dir().join(format!("yolop-hist-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("prompt_history.jsonl");

        let mut h = PromptHistory {
            entries: Vec::new(),
            cursor: None,
            draft: None,
            path: Some(path.clone()),
        };
        h.record("first");
        h.record("second\nwith newline");

        let reloaded = read_entries(&path);
        assert_eq!(reloaded, vec!["first", "second\nwith newline"]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

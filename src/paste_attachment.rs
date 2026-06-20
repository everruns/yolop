// Large clipboard/terminal paste handling for the interactive TUI.
//
// Mirrors Codex TUI: pastes above `LARGE_PASTE_CHAR_THRESHOLD` characters
// insert a compact placeholder in the composer while the full payload is
// stored in `pending_pastes` and expanded on submit.

/// Pastes larger than this attach as a placeholder instead of inline text.
pub const LARGE_PASTE_CHAR_THRESHOLD: usize = 1000;

/// Upper bound on attached paste payload size (512 KiB).
pub const MAX_PASTE_ATTACHMENT_BYTES: usize = 512 * 1024;

/// Normalize line endings from clipboard or terminal paste events.
pub fn normalize_pasted_text(pasted: &str) -> String {
    pasted.replace("\r\n", "\n").replace('\r', "\n")
}

/// Whether pasted text should attach as a placeholder rather than inline.
pub fn is_large_paste(text: &str) -> bool {
    text.chars().count() > LARGE_PASTE_CHAR_THRESHOLD
}

/// Build the next placeholder label for a large paste of `char_count` chars.
pub fn next_large_paste_placeholder(
    char_count: usize,
    pending_pastes: &[(String, String)],
) -> String {
    let base = format!("[Pasted Content {char_count} chars]");
    let prefix = format!("{base} #");
    let mut max_suffix = 0usize;

    for (placeholder, _) in pending_pastes {
        if placeholder == &base {
            max_suffix = max_suffix.max(1);
            continue;
        }
        if let Some(suffix) = placeholder.strip_prefix(&prefix)
            && let Ok(value) = suffix.parse::<usize>()
        {
            max_suffix = max_suffix.max(value);
        }
    }

    if max_suffix == 0 {
        base
    } else {
        format!("{base} #{}", max_suffix + 1)
    }
}

/// Drop pending pastes whose placeholder no longer appears in the composer.
pub fn prune_pending_pastes(text: &str, pending_pastes: &mut Vec<(String, String)>) {
    pending_pastes.retain(|(placeholder, _)| text.contains(placeholder));
}

/// Replace placeholder labels in `text` with their stored payloads.
pub fn expand_pending_pastes(text: &str, pending_pastes: &[(String, String)]) -> String {
    let mut expanded = text.to_string();
    for (placeholder, payload) in pending_pastes {
        expanded = expanded.replace(placeholder, payload);
    }
    expanded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_pasted_text_unifies_line_endings() {
        assert_eq!(normalize_pasted_text("a\r\nb\rc"), "a\nb\nc");
    }

    #[test]
    fn is_large_paste_uses_char_count() {
        let exact = "x".repeat(LARGE_PASTE_CHAR_THRESHOLD);
        assert!(!is_large_paste(&exact));
        assert!(is_large_paste(&format!("{exact}x")));
    }

    #[test]
    fn next_large_paste_placeholder_adds_suffix_for_duplicates() {
        let pending = vec![
            ("[Pasted Content 1200 chars]".to_string(), "first".to_string()),
            (
                "[Pasted Content 1200 chars] #2".to_string(),
                "second".to_string(),
            ),
        ];
        assert_eq!(
            next_large_paste_placeholder(1200, &pending),
            "[Pasted Content 1200 chars] #3"
        );
    }

    #[test]
    fn expand_pending_pastes_replaces_placeholders_in_order() {
        let first = "a".repeat(LARGE_PASTE_CHAR_THRESHOLD + 1);
        let second = "b".repeat(LARGE_PASTE_CHAR_THRESHOLD + 2);
        let first_ph = next_large_paste_placeholder(first.chars().count(), &[]);
        let pending = vec![
            (first_ph.clone(), first.clone()),
            (
                next_large_paste_placeholder(second.chars().count(), &[(first_ph, first.clone())]),
                second.clone(),
            ),
        ];
        let composed = format!("before {} middle {} after", pending[0].0, pending[1].0);
        let expanded = expand_pending_pastes(&composed, &pending);
        assert_eq!(expanded, format!("before {first} middle {second} after"));
    }

    #[test]
    fn prune_pending_pastes_drops_removed_placeholders() {
        let placeholder = "[Pasted Content 1500 chars]";
        let mut pending = vec![(placeholder.to_string(), "payload".to_string())];
        prune_pending_pastes("typed only", &mut pending);
        assert!(pending.is_empty());
    }
}

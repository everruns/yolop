// Unified-diff helper shared by the approval prompt and the tool-result
// rendering. Extracted to a tiny module so both call sites stay readable.

use similar::TextDiff;

pub fn unified(old: &str, new: &str, label: &str, context: usize) -> String {
    let diff = TextDiff::from_lines(old, new);
    diff.unified_diff()
        .header(&format!("a/{label}"), &format!("b/{label}"))
        .context_radius(context)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_uses_a_and_b_prefix_with_label() {
        let out = unified("old\n", "new\n", "foo.txt", 3);
        assert!(out.contains("--- a/foo.txt"), "missing a/ header: {out}");
        assert!(out.contains("+++ b/foo.txt"), "missing b/ header: {out}");
    }

    #[test]
    fn identical_input_produces_no_hunks() {
        let out = unified("same\n", "same\n", "x", 3);
        assert!(
            !out.contains("@@"),
            "expected no hunk markers for identical input: {out}"
        );
    }

    #[test]
    fn single_line_change_shows_minus_and_plus() {
        let out = unified("hello\n", "world\n", "x", 3);
        assert!(out.contains("-hello"), "missing -hello in {out}");
        assert!(out.contains("+world"), "missing +world in {out}");
    }

    #[test]
    fn context_radius_zero_omits_unchanged_neighbours() {
        let old = "a\nb\nc\nd\ne\n";
        let new = "a\nb\nCHANGED\nd\ne\n";
        let out = unified(old, new, "x", 0);
        // With zero context, surrounding unchanged lines must not appear as
        // ` a` / ` b` / ` d` / ` e` context lines in the hunk body.
        let context_line = out.lines().find(|l| l.starts_with(' '));
        assert!(
            context_line.is_none(),
            "expected zero context lines, got: {context_line:?} in {out}"
        );
    }

    #[test]
    fn empty_old_treats_change_as_addition() {
        let out = unified("", "new line\n", "x", 3);
        assert!(out.contains("+new line"), "missing addition: {out}");
        assert!(!out.contains("-new line"), "unexpected deletion: {out}");
    }
}

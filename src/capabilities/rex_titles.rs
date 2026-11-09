//! Rex tab title projection for session titles.
//!
//! When yolop runs inside Rex (`REX_BLOCK` is set), every effective session
//! title mutation is mirrored into the Rex UI. The main yolop renames its
//! window (the tab); a secondary instance renames only its block (the pane).
//! Rex exposes no icon API (window and block records carry a label only), so
//! the title text is projected verbatim and no icon is set.
//!
//! Main means solo and focused: the window holds exactly this block, has no
//! split layout, and this block is the focused one. Anything else (a split
//! pane, an unfocused block, a multi-block window) is secondary.
//!
//! The sync adopts the upstream title contract: it only fires on the typed
//! `session.title.updated` event, which is the durable record of an effective
//! mutation. Explicit titles keep winning upstream, repeated writes stay
//! no-ops there and emit no event here. Projection is best effort and never
//! feeds back into the title pipeline: no `rex` binary, no live block, or a
//! failing daemon only logs at debug level and never breaks the session.

/// Longest title projected into a Rex label; tab chrome stays readable.
const MAX_TITLE_CHARS: usize = 120;
/// Per-request budget for Rex server RPCs. The sync runs on a detached
/// thread, so this only bounds that helper, never the agent loop.
const REX_TIMEOUT: &str = "3s";

/// Where one session title lands in the Rex UI.
pub(crate) enum RexTitleTarget {
    /// Main instance: rename the window, which is the visible tab title.
    Window { window_id: String },
    /// Secondary instance: rename only this block, which is the pane label.
    Block { block_id: String },
}

/// Mirror `title` into Rex when running inside it; no-op anywhere else.
///
/// Cheap when Rex is absent (a single env read, no thread). Inside Rex the
/// work moves to a detached thread so event commit never waits on the daemon.
pub(crate) fn sync_rex_title(title: &str) {
    let title = sanitize_title(title);
    if title.is_empty() {
        return;
    }
    let self_block = match std::env::var("REX_BLOCK") {
        Ok(block) if !block.is_empty() => block,
        _ => return,
    };
    if std::thread::Builder::new()
        .name("rex-title-sync".to_string())
        .spawn(move || apply_title(&self_block, &title))
        .is_err()
    {
        tracing::debug!("rex title sync: could not spawn helper thread");
    }
}

/// Pick the Rex rename target for one block: window when it is the main
/// instance, otherwise just its own block.
pub(crate) fn decide_title_target(
    self_block: &str,
    window_id: &str,
    focused_block_id: &str,
    block_count: usize,
    has_split: bool,
) -> RexTitleTarget {
    if !has_split && block_count <= 1 && focused_block_id == self_block {
        RexTitleTarget::Window {
            window_id: window_id.to_string(),
        }
    } else {
        RexTitleTarget::Block {
            block_id: self_block.to_string(),
        }
    }
}

/// Collapse whitespace and cap length so labels stay one short line.
pub(crate) fn sanitize_title(title: &str) -> String {
    let collapsed = title.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= MAX_TITLE_CHARS {
        collapsed
    } else {
        collapsed.chars().take(MAX_TITLE_CHARS).collect()
    }
}

fn apply_title(self_block: &str, title: &str) {
    let window_id = match block_window_id(self_block) {
        Some(window_id) => window_id,
        None => return,
    };
    let target = match window_title_target(self_block, &window_id) {
        Some(target) => target,
        None => return,
    };
    let args: Vec<&str> = match &target {
        RexTitleTarget::Window { window_id } => {
            vec!["window", "rename", window_id, title]
        }
        RexTitleTarget::Block { block_id } => {
            vec!["block", "rename", block_id, title]
        }
    };
    rename(args);
}

/// Direct argv, never a shell, so title text cannot inject a command.
fn rename(subcommand: Vec<&str>) {
    let mut args = vec!["--timeout", REX_TIMEOUT];
    args.extend(subcommand);
    match std::process::Command::new("rex").args(args).output() {
        Ok(output) if output.status.success() => {}
        Ok(output) => {
            tracing::debug!(
                status = ?output.status,
                stderr = %String::from_utf8_lossy(&output.stderr),
                "rex title sync: rename rejected"
            );
        }
        Err(error) => {
            tracing::debug!(%error, "rex title sync: rex binary unavailable");
        }
    }
}

fn rex_json(global_args: &[&str]) -> Option<serde_json::Value> {
    let mut args = vec!["--timeout", REX_TIMEOUT];
    args.extend(global_args);
    let output = std::process::Command::new("rex").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    serde_json::from_slice(&output.stdout).ok()
}

fn block_window_id(block_id: &str) -> Option<String> {
    let value = rex_json(&["block", "inspect", block_id, "--json"])?;
    value
        .pointer("/block/window_id")
        .and_then(|id| id.as_str())
        .map(str::to_string)
}

fn window_title_target(self_block: &str, window_id: &str) -> Option<RexTitleTarget> {
    let value = rex_json(&["window", "inspect", window_id, "--json"])?;
    let focused = value.get("focused_block_id")?.as_str()?;
    let layers = value.get("layers")?.as_array()?;
    let layer = layers.first()?;
    let block_count = layer.get("blocks")?.as_array().map(|blocks| blocks.len())?;
    let has_split = layer
        .get("layout")
        .and_then(|layout| layout.get("split"))
        .is_some();
    Some(decide_title_target(
        self_block,
        window_id,
        focused,
        block_count,
        has_split,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn solo_focused_block_takes_the_window() {
        let target = decide_title_target("block:a", "window:w", "block:a", 1, false);
        assert!(
            matches!(target, RexTitleTarget::Window { window_id } if window_id == "window:w"),
            "solo focused block is the main instance"
        );
    }

    #[test]
    fn split_pane_stays_on_its_block() {
        let target = decide_title_target("block:a", "window:w", "block:a", 2, true);
        assert!(
            matches!(target, RexTitleTarget::Block { block_id } if block_id == "block:a"),
            "focused block in a split window must not rename the tab"
        );
    }

    #[test]
    fn unfocused_block_stays_on_its_block() {
        let target = decide_title_target("block:b", "window:w", "block:a", 2, true);
        assert!(
            matches!(target, RexTitleTarget::Block { block_id } if block_id == "block:b"),
            "unfocused block must not rename the tab"
        );
    }

    #[test]
    fn sanitize_collapses_whitespace_and_caps_length() {
        assert_eq!(
            sanitize_title("  Batch   independent\nwork "),
            "Batch independent work"
        );
        assert_eq!(sanitize_title(""), "");
        assert_eq!(sanitize_title("   "), "");
        let long = "w".repeat(MAX_TITLE_CHARS + 10);
        assert_eq!(sanitize_title(&long).chars().count(), MAX_TITLE_CHARS);
    }
}

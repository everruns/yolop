//! What v2 reports about tool activity that v1 had no update for: file
//! changes as structured diffs, shell commands as agent-owned terminals, and
//! background tasks as listed background work.
//!
//! Decision: diffs come from file snapshots, not from tool arguments. The
//! approval gate's observer runs before every tool call, so the link reads
//! each file an edit names before it changes and again when the tool
//! completes. That covers `write_file`, `edit_file`, `delete_file` and
//! `apply_patch` the same way and never guesses what an edit did.

use std::path::{Path, PathBuf};

use base64::Engine as _;
use everruns_contracts::runtime::session_task::{SessionTask, SessionTaskState};
use serde_json::{Value, json};

/// Files above this size are listed as changed without a patch.
const MAX_DIFF_BYTES: u64 = 1024 * 1024;

/// One file as it was before an edit ran.
pub(super) struct FileSnapshot {
    pub path: PathBuf,
    pub before: Snapshot,
}

#[derive(Clone, PartialEq, Eq)]
pub(super) enum Snapshot {
    Missing,
    Text(String),
    /// Too large or not UTF-8: the change is listed, the patch omitted.
    Opaque(Vec<u8>),
}

pub(super) fn read_snapshot(path: &Path) -> Snapshot {
    match std::fs::metadata(path) {
        Ok(meta) if meta.is_file() => {}
        _ => return Snapshot::Missing,
    }
    let Ok(bytes) = std::fs::read(path) else {
        return Snapshot::Missing;
    };
    if bytes.len() as u64 > MAX_DIFF_BYTES {
        // Large files are compared by content, never rendered.
        return Snapshot::Opaque(bytes);
    }
    match String::from_utf8(bytes) {
        Ok(text) => Snapshot::Text(text),
        Err(err) => Snapshot::Opaque(err.into_bytes()),
    }
}

/// The files a tool call is about to change, resolved against the workspace
/// root, or `None` for tools that do not edit files.
pub(super) fn edited_paths(name: &str, arguments: &Value, root: &Path) -> Option<Vec<PathBuf>> {
    let paths = match name {
        "write_file" | "edit_file" | "delete_file" => {
            vec![arguments.get("path")?.as_str()?.to_string()]
        }
        "apply_patch" => patch_paths(arguments.get("input")?.as_str()?),
        _ => return None,
    };
    let mut resolved: Vec<PathBuf> = paths.iter().map(|path| resolve(root, path)).collect();
    resolved.dedup();
    (!resolved.is_empty()).then_some(resolved)
}

/// Paths named by an `apply_patch` envelope (`*** Add File:`, `*** Update
/// File:`, `*** Delete File:`, `*** Move to:`).
fn patch_paths(patch: &str) -> Vec<String> {
    patch
        .lines()
        .filter_map(|line| {
            [
                "*** Add File:",
                "*** Update File:",
                "*** Delete File:",
                "*** Move to:",
            ]
            .iter()
            .find_map(|prefix| line.strip_prefix(prefix))
            .map(|path| path.trim().to_string())
        })
        .filter(|path| !path.is_empty())
        .collect()
}

/// File tools take paths relative to the workspace, often written with a
/// leading `/` (the session filesystem's root), or absolute host paths.
pub(super) fn resolve(root: &Path, path: &str) -> PathBuf {
    let candidate = Path::new(path);
    if !candidate.is_absolute() {
        return root.join(candidate);
    }
    if candidate.starts_with(root) {
        return candidate.to_path_buf();
    }
    let relative = root.join(candidate.strip_prefix("/").unwrap_or(candidate));
    let host_parent_exists = candidate.parent().is_some_and(Path::exists);
    let workspace_parent_exists = relative.parent().is_some_and(Path::exists);
    if relative.exists() || (workspace_parent_exists && !host_parent_exists) {
        relative
    } else {
        candidate.to_path_buf()
    }
}

/// A v2 `diff` tool call content for the files that changed, or `None` when
/// none did (a failed or no-op edit).
pub(super) fn diff_content(snapshots: &[FileSnapshot]) -> Option<Value> {
    let mut changes = Vec::new();
    let mut patch = String::new();
    for snapshot in snapshots {
        let after = read_snapshot(&snapshot.path);
        if after == snapshot.before {
            continue;
        }
        let path = snapshot.path.to_string_lossy().to_string();
        let operation = match (&snapshot.before, &after) {
            (Snapshot::Missing, _) => "add",
            (_, Snapshot::Missing) => "delete",
            _ => "modify",
        };
        let binary =
            matches!(snapshot.before, Snapshot::Opaque(_)) || matches!(after, Snapshot::Opaque(_));
        let mut change = json!({ "operation": operation, "path": path });
        change["fileType"] = json!(if binary { "binary" } else { "text" });
        changes.push(change);
        if !binary {
            patch.push_str(&git_patch(&path, operation, &snapshot.before, &after));
        }
    }
    if changes.is_empty() {
        return None;
    }
    let mut diff = json!({ "type": "diff", "changes": changes });
    if !patch.is_empty() {
        diff["patch"] = json!({ "format": "git_patch", "text": patch });
    }
    Some(diff)
}

fn git_patch(path: &str, operation: &str, before: &Snapshot, after: &Snapshot) -> String {
    let text = |snapshot: &Snapshot| match snapshot {
        Snapshot::Text(text) => text.clone(),
        _ => String::new(),
    };
    let (old, new) = (text(before), text(after));
    let old_name = if operation == "add" {
        "/dev/null".to_string()
    } else {
        format!("a{path}")
    };
    let new_name = if operation == "delete" {
        "/dev/null".to_string()
    } else {
        format!("b{path}")
    };
    let mut out = format!("diff --git a{path} b{path}\n");
    match operation {
        "add" => out.push_str("new file mode 100644\n"),
        "delete" => out.push_str("deleted file mode 100644\n"),
        _ => {}
    }
    let diff = similar::TextDiff::from_lines(&old, &new);
    let body = diff
        .unified_diff()
        .context_radius(3)
        .header(&old_name, &new_name)
        .to_string();
    out.push_str(&body);
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

pub(super) fn base64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// The shell command a `bash` call runs, if it is one.
pub(super) fn shell_command(name: &str, arguments: &Value) -> Option<String> {
    (name == "bash")
        .then(|| arguments.get("command")?.as_str().map(str::to_string))
        .flatten()
}

/// Final terminal output and exit code from a `bash` result payload, used
/// when the command produced no streamed output.
pub(super) fn shell_result(result: Option<&Value>) -> (Vec<u8>, Option<i64>) {
    let Some(result) = result else {
        return (Vec::new(), None);
    };
    let mut output = String::new();
    for key in ["stdout", "stderr"] {
        if let Some(text) = result.get(key).and_then(Value::as_str)
            && !text.is_empty()
        {
            output.push_str(text);
            if !text.ends_with('\n') {
                output.push('\n');
            }
        }
    }
    (
        output.into_bytes(),
        result.get("exit_code").and_then(Value::as_i64),
    )
}

/// The tool call id a background task is listed under.
pub(super) fn task_tool_call_id(task: &str) -> String {
    format!("task_{task}")
}

/// A `tool_call_update` that announces or updates a background task.
pub(super) fn task_update(task: &SessionTask) -> Value {
    let status = match task.state {
        SessionTaskState::Queued | SessionTaskState::AwaitingInput => "pending",
        SessionTaskState::Running => "in_progress",
        SessionTaskState::Succeeded => "completed",
        SessionTaskState::Failed => "failed",
        SessionTaskState::Canceled => "cancelled",
    };
    let title = if task.display_name.trim().is_empty() {
        format!("Background task {}", task.id)
    } else {
        task.display_name.clone()
    };
    let mut update = json!({
        "sessionUpdate": "tool_call_update",
        "toolCallId": task_tool_call_id(&task.id),
        "name": "background_task",
        "title": title,
        "kind": "execute",
        "status": status,
    });
    if task.state.is_terminal() {
        let text = task
            .error
            .as_ref()
            .map(|error| error.message.clone())
            .or_else(|| task.summary.clone())
            .unwrap_or_else(|| format!("background task {status}"));
        update["content"] =
            json!([{ "type": "content", "content": { "type": "text", "text": text } }]);
    }
    update
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patch_envelope_names_every_touched_file() {
        let patch = "*** Begin Patch\n*** Update File: src/a.rs\n*** Move to: src/b.rs\n@@\n-x\n+y\n*** Add File: c.txt\n+hi\n*** Delete File: d.txt\n*** End Patch";
        assert_eq!(
            patch_paths(patch),
            vec!["src/a.rs", "src/b.rs", "c.txt", "d.txt"]
        );
    }

    #[test]
    fn workspace_paths_with_a_leading_slash_resolve_inside_the_root() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("src")).unwrap();
        assert_eq!(
            resolve(root.path(), "src/x.rs"),
            root.path().join("src/x.rs")
        );
        assert_eq!(
            resolve(root.path(), "/src/x.rs"),
            root.path().join("src/x.rs")
        );
        let inside = root.path().join("src/y.rs");
        assert_eq!(resolve(root.path(), inside.to_str().unwrap()), inside);
    }

    #[test]
    fn edits_become_structured_changes_with_a_git_patch() {
        let root = tempfile::tempdir().unwrap();
        let edited = root.path().join("a.txt");
        let created = root.path().join("b.txt");
        let deleted = root.path().join("c.txt");
        std::fs::write(&edited, "one\ntwo\n").unwrap();
        std::fs::write(&deleted, "gone\n").unwrap();
        let snapshots = [&edited, &created, &deleted]
            .into_iter()
            .map(|path| FileSnapshot {
                path: path.clone(),
                before: read_snapshot(path),
            })
            .collect::<Vec<_>>();
        std::fs::write(&edited, "one\nTWO\n").unwrap();
        std::fs::write(&created, "new\n").unwrap();
        std::fs::remove_file(&deleted).unwrap();

        let diff = diff_content(&snapshots).expect("changes");
        let operations: Vec<&str> = diff["changes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|change| change["operation"].as_str().unwrap())
            .collect();
        assert_eq!(operations, vec!["modify", "add", "delete"]);
        let patch = diff["patch"]["text"].as_str().unwrap();
        assert!(patch.contains("-two\n+TWO"), "{patch}");
        assert!(patch.contains("new file mode"), "{patch}");
        assert!(patch.contains("+++ /dev/null"), "{patch}");
        assert_eq!(diff["patch"]["format"], "git_patch");
    }

    #[test]
    fn an_unchanged_file_is_no_diff() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("a.txt");
        std::fs::write(&path, "same\n").unwrap();
        let snapshot = FileSnapshot {
            before: read_snapshot(&path),
            path,
        };
        assert!(diff_content(&[snapshot]).is_none());
    }

    fn task(state: &str, summary: Option<&str>) -> SessionTask {
        serde_json::from_value(json!({
            "id": "task_1",
            "session_id": everruns_contracts::typed_id::SessionId::new().to_string(),
            "kind": "background_tool",
            "display_name": "cargo test",
            "state": state,
            "summary": summary,
            "created_at": "2026-11-12T00:00:00Z",
            "updated_at": "2026-11-12T00:00:00Z",
        }))
        .unwrap()
    }

    #[test]
    fn background_tasks_are_tool_calls_that_settle_with_their_outcome() {
        let running = task_update(&task("running", None));
        assert_eq!(running["toolCallId"], "task_task_1");
        assert_eq!(running["status"], "in_progress");
        assert_eq!(running["title"], "cargo test");
        assert!(running.get("content").is_none());
        let done = task_update(&task("succeeded", Some("42 passed")));
        assert_eq!(done["status"], "completed");
        assert_eq!(done["content"][0]["content"]["text"], "42 passed");
        let _: agent_client_protocol::v2::schema::SessionUpdate =
            serde_json::from_value(done).unwrap();
    }

    #[test]
    fn shell_results_fall_back_to_captured_streams() {
        let (output, code) = shell_result(Some(&json!({
            "stdout": "ok", "stderr": "warn\n", "exit_code": 2
        })));
        assert_eq!(String::from_utf8(output).unwrap(), "ok\nwarn\n");
        assert_eq!(code, Some(2));
    }
}

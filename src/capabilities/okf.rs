//! Native awareness of an Open Knowledge Format (OKF) bundle in the workspace.
//!
//! OKF represents knowledge as a plain directory of markdown files with YAML
//! frontmatter (spec: <https://okf.md/spec/>). This capability is a passive
//! *detector*: at session start it looks for a bundle and, only when it finds
//! one, contributes a short system-prompt note pointing the agent at the bundle
//! and the bundled `okf` skill. When no bundle is present it is completely
//! inert, so non-OKF repositories behave exactly as before.
//!
//! Detection is deliberately conservative because OKF has **no canonical bundle
//! marker** — the spec mandates only a per-file `type` field, and neither a root
//! manifest nor a fixed directory name. We therefore never assume `.okf/` exists
//! or is authoritative. Resolution order:
//!
//!   1. An explicit override (`YOLOP_OKF_BUNDLE_DIR`, workspace-relative or
//!      absolute) — trusted when it resolves to a directory.
//!   2. Otherwise a short list of conventional roots (`.okf/`, `okf/`), each
//!      accepted **only** if it passes a content signature: it contains an
//!      `index.md`, or at least one non-reserved `.md` whose frontmatter carries
//!      a non-empty `type`. This is what keeps a plain markdown directory
//!      (Jekyll, Obsidian, docs/) from being mistaken for an OKF bundle.
//!   3. Otherwise nothing.

use crate::workspace_host::WorkspaceHost;
use async_trait::async_trait;
use everruns_core::capabilities::{Capability, CapabilityStatus, SystemPromptContext};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub(crate) const OKF_CAPABILITY_ID: &str = "okf";

/// Env override naming the OKF bundle directory (workspace-relative or absolute).
const OKF_BUNDLE_DIR_ENV: &str = "YOLOP_OKF_BUNDLE_DIR";

/// Conventional bundle directory names, tried in order when no override is set.
const CONVENTIONAL_ROOTS: &[&str] = &[".okf", "okf"];

/// Reserved OKF filenames — not concept documents, and exempt from the `type`
/// rule. `index.md` doubles as a strong presence signal.
const RESERVED: &[&str] = &["index.md", "log.md"];

/// Directory names never descended into while scanning a candidate bundle.
const SKIP_DIRS: &[&str] = &["target", "node_modules", "dist", "build", "venv", ".git"];

/// Upper bound on files inspected while validating/counting a candidate, so a
/// mistakenly-pointed-at huge tree can't stall session startup.
const MAX_SCAN_FILES: usize = 4000;

/// A bundle the detector is confident about.
#[derive(Clone, Debug, PartialEq, Eq)]
struct DetectedBundle {
    /// Workspace-relative path of the bundle root (forward-slashed).
    rel_path: String,
    /// Number of non-reserved `.md` concept documents found.
    concept_count: usize,
}

#[derive(Clone)]
pub(crate) struct OkfCapability {
    bundle: Option<DetectedBundle>,
}

impl OkfCapability {
    /// Detect a bundle for `workspace` at construction time. Reads the env
    /// override once here; the detection logic itself is a pure function of
    /// `(workspace_root, override)` so it can be unit-tested without env state.
    pub(crate) fn new(workspace: Arc<WorkspaceHost>) -> Self {
        let bundle = match workspace.host_root() {
            Ok(root) => {
                let override_dir = std::env::var(OKF_BUNDLE_DIR_ENV)
                    .ok()
                    .map(|value| value.trim().to_string())
                    .filter(|value| !value.is_empty())
                    .map(PathBuf::from);
                detect_bundle(&root, override_dir.as_deref())
            }
            Err(_) => None,
        };
        Self { bundle }
    }

    fn prompt(&self) -> Option<String> {
        let bundle = self.bundle.as_ref()?;
        let concepts = if bundle.concept_count == 1 {
            "1 concept document".to_string()
        } else {
            format!("{} concept documents", bundle.concept_count)
        };
        Some(format!(
            "<capability id=\"okf\">\n\
             This workspace contains an Open Knowledge Format (OKF) bundle at \
             `{path}` ({concepts}) — curated, high-signal knowledge as markdown \
             with YAML frontmatter. Before broad exploration for facts it may \
             already cover (architecture, data models, processes, conventions), \
             read `{path}/index.md` first, then follow its links into the \
             relevant concept documents. Use the `okf` skill to read, author, \
             validate, or maintain the bundle.\n\
             </capability>",
            path = bundle.rel_path,
            concepts = concepts,
        ))
    }
}

#[async_trait]
impl Capability for OkfCapability {
    fn id(&self) -> &str {
        OKF_CAPABILITY_ID
    }

    fn name(&self) -> &str {
        "OKF"
    }

    fn description(&self) -> &str {
        "Detects an Open Knowledge Format bundle in the workspace and points the agent at it."
    }

    fn status(&self) -> CapabilityStatus {
        CapabilityStatus::Available
    }

    fn category(&self) -> Option<&str> {
        Some("Knowledge")
    }

    async fn system_prompt_contribution(&self, _ctx: &SystemPromptContext) -> Option<String> {
        self.prompt()
    }

    fn system_prompt_preview(&self) -> Option<String> {
        self.prompt()
    }
}

/// Resolve a bundle for `workspace_root`, honoring an optional explicit
/// `override_dir` (workspace-relative or absolute). Returns `None` when no
/// bundle is confidently identified.
fn detect_bundle(workspace_root: &Path, override_dir: Option<&Path>) -> Option<DetectedBundle> {
    if let Some(dir) = override_dir {
        // An explicit pointer is trusted: resolve it, require a real directory,
        // but do not demand the content signature — the user declared it OKF.
        let abs = if dir.is_absolute() {
            dir.to_path_buf()
        } else {
            workspace_root.join(dir)
        };
        if !abs.is_dir() {
            return None;
        }
        let (concepts, _has_index) = scan_bundle(&abs);
        return Some(DetectedBundle {
            rel_path: rel_display(workspace_root, &abs),
            concept_count: concepts,
        });
    }

    // No override: try conventional roots, accepting one only on a content match.
    for name in CONVENTIONAL_ROOTS {
        let candidate = workspace_root.join(name);
        if !candidate.is_dir() {
            continue;
        }
        let (concepts, has_index) = scan_bundle(&candidate);
        if has_index || concepts > 0 {
            return Some(DetectedBundle {
                rel_path: (*name).to_string(),
                concept_count: concepts,
            });
        }
    }
    None
}

/// Scan a candidate bundle directory. Returns `(concept_count, has_index)` where
/// `concept_count` is the number of non-reserved `.md` files whose frontmatter
/// carries a non-empty `type`, and `has_index` is whether a root `index.md`
/// exists. The walk is bounded by [`MAX_SCAN_FILES`].
fn scan_bundle(root: &Path) -> (usize, bool) {
    let has_index = root.join("index.md").is_file();
    let mut concept_count = 0usize;
    let mut budget = MAX_SCAN_FILES;
    walk(root, &mut budget, &mut concept_count);
    (concept_count, has_index)
}

fn walk(dir: &Path, budget: &mut usize, concept_count: &mut usize) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if *budget == 0 {
            return;
        }
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if file_type.is_dir() {
            // Skip build/dep output and nested hidden directories; the bundle
            // root itself may be hidden (`.okf`), but its subtree should not
            // sprawl into `.git`/caches.
            if SKIP_DIRS.contains(&name.as_ref()) || name.starts_with('.') {
                continue;
            }
            walk(&path, budget, concept_count);
        } else if file_type.is_file() {
            *budget = budget.saturating_sub(1);
            if !name.ends_with(".md") || RESERVED.contains(&name.as_ref()) {
                continue;
            }
            if frontmatter_has_type(&path) {
                *concept_count += 1;
            }
        }
    }
}

/// Whether a markdown file opens with a YAML frontmatter block containing a
/// non-empty top-level `type:` key. Only the frontmatter block is read; the rest
/// of the file is ignored. Mirrors the conformance check in the `okf` skill's
/// validator without pulling in a YAML dependency.
fn frontmatter_has_type(path: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(path) else {
        return false;
    };
    let mut lines = text.lines();
    if lines.next().map(str::trim) != Some("---") {
        return false;
    }
    for line in lines {
        let trimmed = line.trim();
        if trimmed == "---" {
            return false; // closed the block without a `type`
        }
        if let Some(rest) = trimmed.strip_prefix("type:") {
            return !rest.trim().is_empty();
        }
    }
    false
}

/// Forward-slashed workspace-relative display path (falls back to the absolute
/// path when `path` is not under `root`).
fn rel_display(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent");
        }
        fs::write(path, content).expect("write file");
    }

    #[test]
    fn detects_dot_okf_bundle_with_index() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join(".okf/index.md"), "# Index\n- users\n");
        write(
            &dir.path().join(".okf/tables/users.md"),
            "---\ntype: Table\n---\n# users\n",
        );

        let bundle = detect_bundle(dir.path(), None).expect("bundle detected");
        assert_eq!(bundle.rel_path, ".okf");
        assert_eq!(bundle.concept_count, 1);
    }

    #[test]
    fn detects_okf_bundle_by_typed_concept_without_index() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("okf/metrics/signups.md"),
            "---\ntype: Metric\ntitle: Signups\n---\n# Signups\n",
        );

        let bundle = detect_bundle(dir.path(), None).expect("bundle detected");
        assert_eq!(bundle.rel_path, "okf");
        assert_eq!(bundle.concept_count, 1);
    }

    #[test]
    fn ignores_plain_markdown_directory() {
        // A conventional-looking dir with markdown but no `type` and no index
        // must NOT be treated as a bundle — this is the adoption-safety guard.
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("okf/readme.md"),
            "---\ntitle: not okf\n---\n# hi\n",
        );
        write(
            &dir.path().join("okf/notes.md"),
            "just prose, no frontmatter\n",
        );

        assert_eq!(detect_bundle(dir.path(), None), None);
    }

    #[test]
    fn ignores_workspace_without_any_bundle() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("src/main.rs"), "fn main() {}\n");
        assert_eq!(detect_bundle(dir.path(), None), None);
    }

    #[test]
    fn override_points_at_custom_directory() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("knowledge/api.md"),
            "---\ntype: API\n---\n# API\n",
        );
        // Non-conventional location: only found because the override names it.
        assert_eq!(detect_bundle(dir.path(), None), None);

        let bundle = detect_bundle(dir.path(), Some(Path::new("knowledge")))
            .expect("override bundle detected");
        assert_eq!(bundle.rel_path, "knowledge");
        assert_eq!(bundle.concept_count, 1);
    }

    #[test]
    fn override_to_missing_directory_yields_nothing() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(detect_bundle(dir.path(), Some(Path::new("nope"))), None);
    }

    #[test]
    fn prefers_dot_okf_over_okf() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join(".okf/a.md"),
            "---\ntype: Concept\n---\n# a\n",
        );
        write(
            &dir.path().join("okf/b.md"),
            "---\ntype: Concept\n---\n# b\n",
        );
        let bundle = detect_bundle(dir.path(), None).unwrap();
        assert_eq!(bundle.rel_path, ".okf");
    }

    #[test]
    fn counts_multiple_concepts_and_excludes_reserved() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join(".okf/index.md"), "# Index\n");
        write(&dir.path().join(".okf/log.md"), "# Log\n");
        write(&dir.path().join(".okf/a.md"), "---\ntype: X\n---\n");
        write(&dir.path().join(".okf/sub/b.md"), "---\ntype: Y\n---\n");
        write(&dir.path().join(".okf/notype.md"), "---\ntitle: z\n---\n");

        let bundle = detect_bundle(dir.path(), None).unwrap();
        // a.md + sub/b.md count; index.md, log.md, notype.md do not.
        assert_eq!(bundle.concept_count, 2);
    }

    #[test]
    fn frontmatter_type_detection_edges() {
        let dir = tempfile::tempdir().unwrap();
        let with_type = dir.path().join("t.md");
        write(&with_type, "---\ntype:  Table \n---\nbody\n");
        assert!(frontmatter_has_type(&with_type));

        let empty_type = dir.path().join("e.md");
        write(&empty_type, "---\ntype:\n---\n");
        assert!(!frontmatter_has_type(&empty_type));

        let no_fm = dir.path().join("n.md");
        write(&no_fm, "# just a heading\n");
        assert!(!frontmatter_has_type(&no_fm));
    }

    #[test]
    fn contributes_prompt_only_when_bundle_present() {
        let with = OkfCapability {
            bundle: Some(DetectedBundle {
                rel_path: ".okf".to_string(),
                concept_count: 3,
            }),
        };
        let prompt = with.prompt().expect("prompt when bundle present");
        assert!(prompt.contains(".okf"));
        assert!(prompt.contains("3 concept documents"));
        assert!(prompt.contains("okf` skill"));

        let without = OkfCapability { bundle: None };
        assert!(without.prompt().is_none());
    }

    #[test]
    fn prompt_uses_singular_for_one_concept() {
        let cap = OkfCapability {
            bundle: Some(DetectedBundle {
                rel_path: ".okf".to_string(),
                concept_count: 1,
            }),
        };
        assert!(cap.prompt().unwrap().contains("(1 concept document)"));
    }
}

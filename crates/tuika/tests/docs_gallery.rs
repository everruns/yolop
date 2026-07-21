//! Integrity checks for the component gallery (`docs/`).
//!
//! The demos are a three-part system that can drift silently: a VHS tape, the
//! GIF it records, and the `![demo](…/docs/demos/<name>.gif)` references in the
//! component `struct` docs and the gallery README. A renamed scene or a missing
//! recording would leave a broken image on docs.rs with nothing else failing.
//! These tests keep the three in lockstep.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

fn crate_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// File stems (without extension) of every entry in `dir` with `ext`.
fn stems(dir: &Path, ext: &str) -> BTreeSet<String> {
    fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("read_dir {}: {e}", dir.display()))
        .filter_map(Result::ok)
        .filter_map(|e| {
            let path = e.path();
            if path.extension().and_then(|s| s.to_str()) == Some(ext) {
                path.file_stem().and_then(|s| s.to_str()).map(str::to_owned)
            } else {
                None
            }
        })
        .collect()
}

/// Every `demos/<name>.gif` reference in `text` — matches both the relative
/// `demos/x.gif` form in the README and the absolute `.../docs/demos/x.gif`
/// URLs embedded in component docs.
fn referenced_gifs(text: &str) -> BTreeSet<String> {
    const MARKER: &str = "demos/";
    let mut found = BTreeSet::new();
    for (idx, _) in text.match_indices(MARKER) {
        let rest = &text[idx + MARKER.len()..];
        let Some(end) = rest.find(".gif") else {
            continue;
        };
        let name = &rest[..end];
        if !name.is_empty() && name.chars().all(|c| c.is_ascii_lowercase() || c == '_') {
            found.insert(name.to_owned());
        }
    }
    found
}

#[test]
fn every_tape_has_a_recorded_gif_and_vice_versa() {
    let dir = crate_dir().join("docs");
    let tapes = stems(&dir.join("tapes"), "tape");
    let gifs = stems(&dir.join("demos"), "gif");

    assert!(!tapes.is_empty(), "no tapes found under docs/tapes");
    assert_eq!(
        tapes, gifs,
        "docs/tapes/*.tape and docs/demos/*.gif are out of sync; \
         run crates/tuika/docs/generate.sh"
    );
}

#[test]
fn recorded_gifs_are_non_empty() {
    let demos = crate_dir().join("docs/demos");
    for name in stems(&demos, "gif") {
        let path = demos.join(format!("{name}.gif"));
        let len = fs::metadata(&path).unwrap().len();
        assert!(len > 0, "{} is empty", path.display());
    }
}

#[test]
fn component_doc_embeds_point_at_existing_gifs() {
    let dir = crate_dir();
    let demos = dir.join("docs/demos");
    let components = dir.join("src/components");

    let mut referenced = BTreeSet::new();
    for entry in fs::read_dir(&components).unwrap().filter_map(Result::ok) {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) == Some("rs") {
            let src = fs::read_to_string(&path).unwrap();
            referenced.extend(referenced_gifs(&src));
        }
    }

    assert!(
        !referenced.is_empty(),
        "expected inline demo embeds in src/components/*.rs but found none"
    );
    for name in &referenced {
        let gif = demos.join(format!("{name}.gif"));
        assert!(
            gif.exists(),
            "component docs embed {name}.gif but {} is missing",
            gif.display()
        );
    }
}

#[test]
fn gallery_readme_references_existing_gifs() {
    let dir = crate_dir();
    let readme = fs::read_to_string(dir.join("docs/README.md")).unwrap();
    let demos = dir.join("docs/demos");

    let referenced = referenced_gifs(&readme);
    assert!(
        !referenced.is_empty(),
        "docs/README.md references no demo GIFs"
    );
    for name in &referenced {
        let gif = demos.join(format!("{name}.gif"));
        assert!(
            gif.exists(),
            "docs/README.md references {name}.gif but {} is missing",
            gif.display()
        );
    }
}

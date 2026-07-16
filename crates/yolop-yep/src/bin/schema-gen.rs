//! Emit the machine-readable protocol artifacts under `schema/yep/v1/`:
//! `meta.json` (method + capability vocabulary) always, and `schema.json`
//! (full JSON Schema of the wire payloads) when built with `--features schema`.
//!
//! `cargo run -p yolop-yep --features schema --bin schema-gen` regenerates
//! both; pass `--check` to fail (non-zero) when a committed file is stale,
//! which CI runs so a wire change can't merge without matching artifacts (the
//! mira pattern). Paths are relative to the repo root.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

fn main() -> ExitCode {
    let check = std::env::args().any(|a| a == "--check");
    let root = repo_root();

    // (relative path, expected contents). `mut` is used only when the
    // `schema` feature adds schema.json below.
    #[cfg_attr(not(feature = "schema"), allow(unused_mut))]
    let mut artifacts: Vec<(&str, String)> =
        vec![("schema/yep/v1/meta.json", yolop_yep::meta_json())];
    #[cfg(feature = "schema")]
    artifacts.push(("schema/yep/v1/schema.json", yolop_yep::schema_json()));

    let mut ok = true;
    for (rel, expected) in &artifacts {
        let path = root.join(rel);
        if check {
            let actual = std::fs::read_to_string(&path).unwrap_or_default();
            if &actual == expected {
                eprintln!("{rel} is up to date");
            } else {
                eprintln!(
                    "{rel} is STALE — run `cargo run -p yolop-yep --features schema --bin schema-gen`"
                );
                ok = false;
            }
        } else if let Err(err) = write_file(&path, expected) {
            eprintln!("failed to write {}: {err}", path.display());
            ok = false;
        } else {
            eprintln!("wrote {}", path.display());
        }
    }

    #[cfg(not(feature = "schema"))]
    eprintln!("note: schema.json skipped — rebuild with `--features schema` to include it");

    if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn write_file(path: &Path, contents: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, contents)
}

/// The workspace root: this crate is at `<root>/crates/yolop-yep`.
fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

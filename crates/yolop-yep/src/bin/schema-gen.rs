//! Emit the machine-readable protocol metadata to `schema/yep/v1/meta.json`.
//!
//! `cargo run -p yolop-yep --bin schema-gen` regenerates the file; pass
//! `--check` to fail (non-zero) when the committed file is stale, which CI
//! runs so a wire-vocabulary change can't merge without a matching artifact
//! (the mira pattern). The path is relative to the repo root.

use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    let check = std::env::args().any(|a| a == "--check");
    let path = repo_root().join("schema/yep/v1/meta.json");
    let expected = yolop_yep::meta_json();

    if check {
        let actual = std::fs::read_to_string(&path).unwrap_or_default();
        if actual == expected {
            eprintln!("schema/yep/v1/meta.json is up to date");
            ExitCode::SUCCESS
        } else {
            eprintln!(
                "schema/yep/v1/meta.json is STALE — run `cargo run -p yolop-yep --bin schema-gen`"
            );
            ExitCode::FAILURE
        }
    } else {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match std::fs::write(&path, &expected) {
            Ok(()) => {
                eprintln!("wrote {}", path.display());
                ExitCode::SUCCESS
            }
            Err(err) => {
                eprintln!("failed to write {}: {err}", path.display());
                ExitCode::FAILURE
            }
        }
    }
}

/// The workspace root: this crate is at `<root>/crates/yolop-yep`.
fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

//! Guards the published `.crate` contents.
//!
//! The `docs/` demo/theme/styling GIFs are ~8 MiB and serve only the
//! GitHub-rendered README and `docs/*.md`; docs.rs builds from the hand-written
//! `//!` header in `lib.rs`, which references no images. The `exclude` in
//! `Cargo.toml` keeps them out of the tarball. This test drives the real
//! packaging path (`cargo package --list`) so a stray asset — or a regression
//! that drops an image the crates.io README needs — fails loudly instead of
//! silently re-inflating the crate.

use std::process::Command;

/// Ask cargo which files it would put in tuika's `.crate`, exactly as `publish`
/// would. `--list` resolves the manifest's `include`/`exclude` without building.
fn packaged_files() -> Vec<String> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let output = Command::new(cargo)
        .args([
            "package",
            "--list",
            "--quiet",
            "--allow-dirty",
            "-p",
            "tuika",
        ])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("run `cargo package --list`");
    assert!(
        output.status.success(),
        "`cargo package --list` failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("utf8 file list")
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect()
}

#[test]
fn heavy_doc_gifs_are_excluded_from_the_package() {
    let files = packaged_files();

    // No demo/theme/styling GIF may ship: those are GitHub-only assets.
    let bundled_heavy_gifs: Vec<&String> = files
        .iter()
        .filter(|f| {
            f.ends_with(".gif")
                && (f.starts_with("docs/demos/")
                    || f.starts_with("docs/styling/")
                    || f.starts_with("docs/themes/"))
        })
        .collect();
    assert!(
        bundled_heavy_gifs.is_empty(),
        "these GIFs must not ship in the crate (see Cargo.toml `exclude`): {bundled_heavy_gifs:?}"
    );
}

#[test]
fn crates_io_readme_assets_and_source_are_kept() {
    let files = packaged_files();
    let has = |p: &str| files.iter().any(|f| f == p);

    // The two assets the crates.io README embeds by relative path.
    assert!(has("docs/hero.gif"), "README hero image must ship");
    assert!(
        has("docs/demos/image.svg"),
        "README image-protocol asset must ship"
    );
    // Sanity: the crate still carries its source and manifest.
    assert!(has("src/lib.rs"), "library source must ship");
    assert!(has("Cargo.toml"), "manifest must ship");
    assert!(has("README.md"), "README must ship");
}

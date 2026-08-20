//! The local model store.
//!
//! Weights for the `local` provider live under `<data_dir>/yolop/models/`,
//! one directory per Hugging Face repo. Yolop owns this directory rather than
//! deferring to the engine's own cache so the bytes are inspectable: `yolop
//! weights list` can report what is on disk and how much it costs, and `yolop
//! weights rm` can reclaim it. An engine-managed cache would leave users with
//! gigabytes they can neither see nor delete through yolop.
//!
//! Listing and removal are plain filesystem work and compile into every build.
//! Downloading needs the Hugging Face client and lives in [`download`], behind
//! the `local-inference` feature.

use anyhow::{Context, Result, anyhow};
use std::path::{Path, PathBuf};

#[cfg(feature = "local-inference")]
pub mod download;

// Spec parsing and installed-checks serve the driver and the downloader, both
// of which are feature-gated; the store's list/remove half is not. Rather than
// split the module, the few feature-only items carry an allow so a default
// build stays warning-free.
/// Separator between a repo and a specific GGUF file inside it, as it appears
/// in a model spec: `unsloth/Qwen3-8B-GGUF::Qwen3-8B-Q4_K_M.gguf`.
#[cfg_attr(not(feature = "local-inference"), allow(dead_code))]
pub const GGUF_SEPARATOR: &str = "::";

/// Root of the model store, or `None` when the platform has no data directory.
pub fn store_root() -> Option<PathBuf> {
    crate::config::paths::data_dir().map(|dir| dir.join("models"))
}

/// Split a model spec into its repo and, for GGUF specs, the file within it.
#[cfg_attr(not(feature = "local-inference"), allow(dead_code))]
pub fn split_spec(spec: &str) -> (&str, Option<&str>) {
    match spec.split_once(GGUF_SEPARATOR) {
        Some((repo, file)) => (repo, Some(file)),
        None => (spec, None),
    }
}

/// On-disk directory name for a repo.
///
/// Maps `/` to `__` so every repo is one flat entry that [`installed`] can
/// enumerate without walking a tree. `\` gets the same treatment because it is
/// a separator on Windows, and a leading `.` is escaped so a repo named `..`
/// cannot name a parent directory.
pub fn dir_name(repo: &str) -> String {
    let flattened = repo.replace(['/', '\\'], "__");
    match flattened.strip_prefix('.') {
        Some(rest) => format!("_{rest}"),
        None => flattened,
    }
}

/// Inverse of [`dir_name`], for reporting what is installed.
fn repo_from_dir_name(name: &str) -> String {
    name.replace("__", "/")
}

/// Where a repo's files live, whether or not it has been downloaded.
pub fn repo_dir(repo: &str) -> Result<PathBuf> {
    let root =
        store_root().ok_or_else(|| anyhow!("no platform data directory for the model store"))?;
    Ok(root.join(dir_name(repo)))
}

/// Join a repo-relative filename onto `dir`, refusing anything that could
/// land outside it.
///
/// Filenames are not trusted input. The GGUF half of a spec comes from the
/// command line, and for a safetensors repo the list comes from the Hugging
/// Face API — a repo can advertise a sibling called `../../../.bashrc`, and
/// a plain `dir.join(..)` would happily write there. Only ordinary path
/// components are allowed: no root, no prefix, no `..`, and the joined result
/// must still be under `dir`.
#[cfg_attr(not(feature = "local-inference"), allow(dead_code))]
pub fn safe_join(dir: &Path, filename: &str) -> Result<PathBuf> {
    use std::path::Component;

    if filename.trim().is_empty() {
        return Err(anyhow!("empty filename in model repo listing"));
    }
    let relative = Path::new(filename);
    for component in relative.components() {
        match component {
            Component::Normal(_) => {}
            _ => {
                return Err(anyhow!(
                    "refusing model file '{filename}': only plain relative paths are allowed"
                ));
            }
        }
    }

    let joined = dir.join(relative);
    // Belt and braces: the component check already rejects traversal, but a
    // symlinked parent or an odd platform encoding should not slip past.
    if !joined.starts_with(dir) {
        return Err(anyhow!(
            "refusing model file '{filename}': it resolves outside the model store"
        ));
    }
    Ok(joined)
}

/// A repo present in the store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledModel {
    pub repo: String,
    pub path: PathBuf,
    /// Total bytes of the repo's files.
    pub bytes: u64,
}

/// Every repo in the store, sorted by repo id. An absent store is not an
/// error — it just means nothing has been pulled yet.
pub fn installed() -> Result<Vec<InstalledModel>> {
    let Some(root) = store_root() else {
        return Ok(Vec::new());
    };
    if !root.exists() {
        return Ok(Vec::new());
    }

    let mut models = Vec::new();
    for entry in std::fs::read_dir(&root).with_context(|| format!("read {}", root.display()))? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        models.push(InstalledModel {
            repo: repo_from_dir_name(name),
            bytes: dir_size(&path)?,
            path,
        });
    }
    models.sort_by(|a, b| a.repo.cmp(&b.repo));
    Ok(models)
}

/// Whether a spec's weights are already on disk.
///
/// A GGUF spec needs its named file; a safetensors repo needs at least one
/// `.safetensors` file. Checking for content rather than for the directory
/// keeps an interrupted download from reading as installed.
#[cfg_attr(not(feature = "local-inference"), allow(dead_code))]
pub fn is_installed(spec: &str) -> bool {
    let (repo, file) = split_spec(spec);
    let Ok(dir) = repo_dir(repo) else {
        return false;
    };
    match file {
        Some(file) => safe_join(&dir, file).is_ok_and(|path| path.is_file()),
        None => has_extension(&dir, "safetensors"),
    }
}

#[cfg_attr(not(feature = "local-inference"), allow(dead_code))]
fn has_extension(dir: &Path, extension: &str) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    entries.flatten().any(|entry| {
        entry
            .path()
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case(extension))
    })
}

/// Delete a repo from the store, returning the bytes reclaimed. Errors when the
/// repo is not installed, so a typo reports itself instead of silently
/// succeeding.
pub fn remove(repo: &str) -> Result<u64> {
    let dir = repo_dir(repo)?;
    if !dir.is_dir() {
        return Err(anyhow!("{repo} is not in the model store"));
    }
    let bytes = dir_size(&dir)?;
    std::fs::remove_dir_all(&dir).with_context(|| format!("remove {}", dir.display()))?;
    Ok(bytes)
}

/// Recursive byte total for a directory. Symlinks are counted by their own
/// entry rather than followed, so a link into another store cannot inflate the
/// total or loop.
fn dir_size(dir: &Path) -> Result<u64> {
    let mut total = 0;
    for entry in std::fs::read_dir(dir).with_context(|| format!("read {}", dir.display()))? {
        let entry = entry?;
        let meta = entry.metadata()?;
        if meta.is_dir() {
            total += dir_size(&entry.path())?;
        } else {
            total += meta.len();
        }
    }
    Ok(total)
}

/// Human-readable byte count for CLI output.
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", UNITS[0])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn specs_split_into_repo_and_optional_file() {
        assert_eq!(
            split_spec("unsloth/Qwen3-8B-GGUF::Qwen3-8B-Q4_K_M.gguf"),
            ("unsloth/Qwen3-8B-GGUF", Some("Qwen3-8B-Q4_K_M.gguf"))
        );
        assert_eq!(split_spec("Qwen/Qwen3-8B"), ("Qwen/Qwen3-8B", None));
    }

    #[test]
    fn repo_ids_round_trip_through_directory_names() {
        for repo in ["Qwen/Qwen3-8B", "unsloth/Qwen3-8B-GGUF", "single-segment"] {
            assert_eq!(repo_from_dir_name(&dir_name(repo)), repo);
        }
    }

    #[test]
    fn directory_names_stay_flat() {
        // A nested name would put the repo outside the store's single level and
        // hide it from `installed`.
        assert!(!dir_name("Qwen/Qwen3-8B").contains('/'));
    }

    #[test]
    fn a_repo_id_cannot_escape_the_store_directory() {
        // Repo ids reach the filesystem as a single directory name; a
        // separator or a leading dot must not turn one into a parent.
        for hostile in ["../../etc", "..\\..\\windows", "..", "a/../../b"] {
            let name = dir_name(hostile);
            assert!(!name.contains('/'), "{hostile} -> {name}");
            assert!(!name.contains('\\'), "{hostile} -> {name}");
            assert_ne!(name, "..", "{hostile} -> {name}");
            assert!(!name.starts_with('.'), "{hostile} -> {name}");
        }
    }

    #[test]
    fn hostile_filenames_are_refused_rather_than_joined() {
        // The safetensors file list comes from the remote repo, so a malicious
        // repo could advertise a sibling that writes outside the store.
        let dir = Path::new("/store/repo");
        for hostile in [
            "../../../.bashrc",
            "/etc/passwd",
            "a/../../../b",
            "..",
            "",
            "   ",
        ] {
            assert!(
                safe_join(dir, hostile).is_err(),
                "should have refused {hostile:?}"
            );
        }
    }

    #[test]
    fn ordinary_filenames_still_join() {
        let dir = Path::new("/store/repo");
        assert_eq!(
            safe_join(dir, "model.safetensors").expect("plain name"),
            Path::new("/store/repo/model.safetensors")
        );
        // Sharded repos legitimately nest one level.
        assert_eq!(
            safe_join(dir, "shards/model-00001.safetensors").expect("nested name"),
            Path::new("/store/repo/shards/model-00001.safetensors")
        );
    }

    #[test]
    fn byte_counts_scale_to_readable_units() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(2048), "2.0 KiB");
        assert_eq!(human_bytes(5 * 1024 * 1024 * 1024), "5.0 GiB");
    }

    #[test]
    fn a_directory_without_weights_is_not_installed() {
        // Guards the interrupted-download case: the directory exists but holds
        // no weights, and reporting it installed would send the engine at an
        // empty folder.
        let temp = tempfile::tempdir().expect("tempdir");
        assert!(!has_extension(temp.path(), "safetensors"));
        std::fs::write(temp.path().join("config.json"), "{}").expect("write");
        assert!(!has_extension(temp.path(), "safetensors"));
        std::fs::write(temp.path().join("model.safetensors"), "w").expect("write");
        assert!(has_extension(temp.path(), "safetensors"));
    }

    #[test]
    fn directory_sizes_sum_recursively() {
        let temp = tempfile::tempdir().expect("tempdir");
        std::fs::write(temp.path().join("a"), vec![0u8; 10]).expect("write");
        let nested = temp.path().join("nested");
        std::fs::create_dir(&nested).expect("mkdir");
        std::fs::write(nested.join("b"), vec![0u8; 32]).expect("write");
        assert_eq!(dir_size(temp.path()).expect("size"), 42);
    }
}

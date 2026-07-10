//! Install and uninstall extensions from a local directory.

use super::discovery::{ExtensionScope, install_dir};
use super::manifest::{ExtensionManifest, MANIFEST_FILE};
use anyhow::{Context, Result, bail};
use std::fs;
use std::path::{Path, PathBuf};

/// Copy an extension bundle from `source` into the target scope.
///
/// `source` must be a directory containing `extension.toml`. The manifest
/// `id` determines the install directory name.
pub fn install_extension(
    source: &Path,
    scope: ExtensionScope,
    workspace_root: &Path,
    force: bool,
) -> Result<PathBuf> {
    if !source.is_dir() {
        bail!("extension source must be a directory: {}", source.display());
    }
    let manifest_path = source.join(MANIFEST_FILE);
    if !manifest_path.is_file() {
        bail!(
            "extension source missing {MANIFEST_FILE}: {}",
            source.display()
        );
    }

    let manifest = ExtensionManifest::from_file(&manifest_path)?;
    if !manifest.has_contributions() {
        tracing::warn!(
            id = %manifest.id,
            "extension has no contributions; installing anyway"
        );
    }

    let dest = install_dir(scope, workspace_root, &manifest.id)?;
    if dest.exists() {
        if force {
            fs::remove_dir_all(&dest)
                .with_context(|| format!("remove existing extension {}", dest.display()))?;
        } else {
            bail!(
                "extension `{}` already installed at {} (pass --force to replace)",
                manifest.id,
                dest.display()
            );
        }
    }

    fs::create_dir_all(dest.parent().unwrap()).with_context(|| {
        format!(
            "create extensions parent {}",
            dest.parent().unwrap().display()
        )
    })?;
    copy_dir_recursive(source, &dest)?;

    Ok(dest)
}

/// Remove an installed extension by id from the given scope.
pub fn uninstall_extension(id: &str, scope: ExtensionScope, workspace_root: &Path) -> Result<()> {
    if id.trim().is_empty() {
        bail!("extension id must not be empty");
    }
    let dest = install_dir(scope, workspace_root, id)?;
    if !dest.exists() {
        bail!(
            "extension `{id}` is not installed in {} scope at {}",
            scope.label(),
            dest.display()
        );
    }
    fs::remove_dir_all(&dest).with_context(|| format!("remove extension {}", dest.display()))?;
    Ok(())
}

fn copy_dir_recursive(src: &Path, dest: &Path) -> Result<()> {
    fs::create_dir_all(dest)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let src_path = entry.path();
        let dest_path = dest.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir_recursive(&src_path, &dest_path)?;
        } else if file_type.is_file() {
            fs::copy(&src_path, &dest_path).with_context(|| {
                format!("copy {} to {}", src_path.display(), dest_path.display())
            })?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn install_copies_manifest_and_rejects_duplicate() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("bundle");
        fs::create_dir_all(&source).unwrap();
        fs::write(
            source.join(MANIFEST_FILE),
            r#"
id = "demo-ext"
name = "Demo"
version = "0.1.0"

[contributions.capabilities.lsp.servers.demo]
command = "demo-lsp"
extensions = ["demo"]
"#,
        )
        .unwrap();

        let ws = tmp.path().join("workspace");
        fs::create_dir_all(&ws).unwrap();
        let dest = install_extension(&source, ExtensionScope::Workspace, &ws, false).unwrap();
        assert!(dest.join(MANIFEST_FILE).is_file());
        assert!(install_extension(&source, ExtensionScope::Workspace, &ws, false).is_err());
        install_extension(&source, ExtensionScope::Workspace, &ws, true).unwrap();
        uninstall_extension("demo-ext", ExtensionScope::Workspace, &ws).unwrap();
        assert!(!dest.exists());
    }
}

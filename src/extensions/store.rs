//! Extension installation: sources, the `extensions.lock` pin file, and
//! install/remove into the global extensions directory. See
//! `specs/extensions.md`, "Packaging, install, trust".
//!
//! Trust: installing is consent by action (the user ran the install verb),
//! mirroring how authoring `.mcp.json` consents to its stdio servers. The
//! lockfile records what was approved (source, resolved commit, content
//! hash) so `update` can show a contribution diff and re-ask on a widened
//! grant. There is one scope — global — because repos never carry yolop
//! extensions.

use super::package::{ExtensionManifest, MANIFEST_FILE, parse_manifest};
use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const LOCKFILE: &str = "extensions.lock";

/// Where an installed extension came from, as the user spelled it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum Source {
    /// A git repository, pinned to the resolved commit.
    Git { url: String, rev: String },
    /// A local directory, referenced by absolute path (dev loop).
    Path { path: String },
    /// A crates.io crate (`yolop-extension-<name>`), pinned to a version.
    Crate { crate_name: String, version: String },
}

impl Source {
    /// Parse a user-supplied `<source>` argument.
    ///
    /// - `crates.io:<crate>[@ver]` or a bare `<name>` → crate
    ///   (`<name>` expands to `yolop-extension-<name>`)
    /// - `<git-url>[@rev]` (contains `://` or `git@`) → git
    /// - anything else that exists on disk → path
    pub fn parse(spec: &str) -> Result<Self> {
        let spec = spec.trim();
        if let Some(rest) = spec.strip_prefix("crates.io:") {
            let (crate_name, version) = split_at_version(rest);
            return Ok(Source::Crate {
                crate_name: crate_name.to_string(),
                version: version.unwrap_or_default().to_string(),
            });
        }
        if spec.contains("://") || spec.starts_with("git@") {
            let (url, rev) = split_at_version(spec);
            return Ok(Source::Git {
                url: url.to_string(),
                rev: rev.unwrap_or_default().to_string(),
            });
        }
        let path = Path::new(spec);
        if path.exists() {
            let absolute = std::fs::canonicalize(path)
                .with_context(|| format!("resolving extension path {spec}"))?;
            return Ok(Source::Path {
                path: absolute.display().to_string(),
            });
        }
        // Bare name shorthand: `lsp` → crate `yolop-extension-lsp`.
        if spec
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            let (name, version) = split_at_version(spec);
            return Ok(Source::Crate {
                crate_name: format!("yolop-extension-{name}"),
                version: version.unwrap_or_default().to_string(),
            });
        }
        bail!("cannot interpret `{spec}` as a git URL, crate, or existing path")
    }
}

fn split_at_version(spec: &str) -> (&str, Option<&str>) {
    // Split on the LAST `@` so `git@host:org/repo@rev` keeps the scp-form
    // user intact and only the trailing ref is taken.
    match spec.rsplit_once('@') {
        // `git@host:...` with no ref has its only `@` right after `git`.
        Some((left, right)) if !right.contains('/') && !right.contains(':') => (left, Some(right)),
        _ => (spec, None),
    }
}

/// One `extensions.lock` entry: what was installed and the hash approved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockEntry {
    pub name: String,
    pub source: Source,
    /// Content hash of the installed package tree at approval time.
    pub content_hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Lockfile {
    #[serde(default)]
    pub extensions: Vec<LockEntry>,
}

impl Lockfile {
    pub fn load(dir: &Path) -> Self {
        let path = dir.join(LOCKFILE);
        std::fs::read_to_string(&path)
            .ok()
            .and_then(|raw| toml::from_str(&raw).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, dir: &Path) -> Result<()> {
        std::fs::create_dir_all(dir)?;
        let toml = toml::to_string_pretty(self).context("serializing extensions.lock")?;
        std::fs::write(dir.join(LOCKFILE), toml).context("writing extensions.lock")?;
        Ok(())
    }

    pub fn get(&self, name: &str) -> Option<&LockEntry> {
        self.extensions.iter().find(|e| e.name == name)
    }

    fn upsert(&mut self, entry: LockEntry) {
        match self.extensions.iter_mut().find(|e| e.name == entry.name) {
            Some(existing) => *existing = entry,
            None => self.extensions.push(entry),
        }
        self.extensions.sort_by(|a, b| a.name.cmp(&b.name));
    }

    fn remove(&mut self, name: &str) -> bool {
        let before = self.extensions.len();
        self.extensions.retain(|e| e.name != name);
        self.extensions.len() != before
    }
}

/// A stable content hash of a package tree: every file's relative path and
/// bytes, order-independent. Used to pin what was approved and to detect a
/// widened grant on update.
pub fn hash_package_dir(dir: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    let mut files: Vec<(String, Vec<u8>)> = Vec::new();
    collect_files(dir, dir, &mut files)?;
    files.sort_by(|a, b| a.0.cmp(&b.0));
    let mut hasher = Sha256::new();
    for (rel, bytes) in files {
        hasher.update(rel.as_bytes());
        hasher.update([0u8]);
        hasher.update((bytes.len() as u64).to_le_bytes());
        hasher.update(&bytes);
    }
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(7 + digest.len() * 2);
    hex.push_str("sha256:");
    for byte in digest {
        use std::fmt::Write;
        let _ = write!(hex, "{byte:02x}");
    }
    Ok(hex)
}

fn collect_files(root: &Path, dir: &Path, out: &mut Vec<(String, Vec<u8>)>) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            // Skip VCS metadata so a git-cloned tree hashes to the same value
            // a tarball would.
            if entry.file_name() == ".git" {
                continue;
            }
            collect_files(root, &path, out)?;
        } else if file_type.is_file() {
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            out.push((rel, std::fs::read(&path)?));
        }
    }
    Ok(())
}

/// Result of an install/update, for the caller to summarize to the user.
#[derive(Debug, Clone)]
pub struct Installed {
    pub manifest: ExtensionManifest,
    pub content_hash: String,
    /// Present on an update: the previously approved hash, if the grant changed.
    pub previous_hash: Option<String>,
}

/// Install (or update) an extension into `extensions_dir` from `source`.
///
/// Git and path sources are handled natively; crates.io is deferred (returns
/// a clear error pointing at the follow-up) so this lands testable offline.
/// The package is staged, its manifest parsed (approval boundary — no binary
/// is executed), then swapped into place and pinned in the lockfile.
pub fn install(extensions_dir: &Path, source: &Source, git: &dyn GitRunner) -> Result<Installed> {
    std::fs::create_dir_all(extensions_dir)?;
    let staging = extensions_dir.join(".staging");
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging).context("creating staging dir")?;
    let cleanup = StagingGuard(staging.clone());

    let resolved_source = match source {
        Source::Path { path } => {
            copy_dir(Path::new(path), &staging)
                .with_context(|| format!("copying extension from {path}"))?;
            source.clone()
        }
        Source::Git { url, rev } => {
            let resolved_rev = git.clone_into(url, rev.as_str_opt(), &staging)?;
            Source::Git {
                url: url.clone(),
                rev: resolved_rev,
            }
        }
        Source::Crate { .. } => {
            bail!(
                "crates.io installs are not yet wired (native sparse-index + tarball fetch is a \
                 follow-up); install from a git URL or local path for now"
            );
        }
    };

    let manifest_raw = std::fs::read_to_string(staging.join(MANIFEST_FILE))
        .with_context(|| format!("extension package has no {MANIFEST_FILE} at its root"))?;
    let manifest = parse_manifest(&manifest_raw).map_err(|e| anyhow!(e))?;

    let content_hash = hash_package_dir(&staging)?;
    let dest = extensions_dir.join(&manifest.name);
    let previous_hash = Lockfile::load(extensions_dir)
        .get(&manifest.name)
        .map(|e| e.content_hash.clone())
        .filter(|prev| prev != &content_hash);

    // Swap staging into place atomically-ish: remove old, rename new.
    let _ = std::fs::remove_dir_all(&dest);
    std::fs::rename(&staging, &dest)
        .with_context(|| format!("installing into {}", dest.display()))?;
    std::mem::forget(cleanup); // renamed away; nothing to clean.

    let mut lock = Lockfile::load(extensions_dir);
    lock.upsert(LockEntry {
        name: manifest.name.clone(),
        source: resolved_source,
        content_hash: content_hash.clone(),
        version: manifest.version.clone(),
    });
    lock.save(extensions_dir)?;

    Ok(Installed {
        manifest,
        content_hash,
        previous_hash,
    })
}

/// Remove an installed extension and its lockfile pin. Returns whether it
/// existed.
pub fn remove(extensions_dir: &Path, name: &str) -> Result<bool> {
    let dir = extensions_dir.join(name);
    let existed = dir.is_dir();
    if existed {
        std::fs::remove_dir_all(&dir).with_context(|| format!("removing {}", dir.display()))?;
    }
    let mut lock = Lockfile::load(extensions_dir);
    let unpinned = lock.remove(name);
    if unpinned {
        lock.save(extensions_dir)?;
    }
    Ok(existed || unpinned)
}

struct StagingGuard(PathBuf);
impl Drop for StagingGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn copy_dir(src: &Path, dst: &Path) -> Result<()> {
    if !src.is_dir() {
        bail!("{} is not a directory", src.display());
    }
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            if entry.file_name() == ".git" {
                continue;
            }
            std::fs::create_dir_all(&to)?;
            copy_dir(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

/// Seam for git operations, so install logic is testable without a network.
pub trait GitRunner: Send + Sync {
    /// Clone `url` (optionally at `rev`) into `dest`, returning the resolved
    /// commit sha.
    fn clone_into(&self, url: &str, rev: Option<&str>, dest: &Path) -> Result<String>;
}

/// Real git via the `git` binary (shallow clone, pinned checkout).
pub struct SystemGit;

impl GitRunner for SystemGit {
    fn clone_into(&self, url: &str, rev: Option<&str>, dest: &Path) -> Result<String> {
        use std::process::Command;
        let run = |args: &[&str], cwd: Option<&Path>| -> Result<String> {
            let mut cmd = Command::new("git");
            cmd.args(args);
            if let Some(cwd) = cwd {
                cmd.current_dir(cwd);
            }
            let out = cmd.output().context("running git")?;
            if !out.status.success() {
                bail!(
                    "git {} failed: {}",
                    args.join(" "),
                    String::from_utf8_lossy(&out.stderr).trim()
                );
            }
            Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
        };
        run(
            &["clone", "--depth", "1", url, &dest.display().to_string()],
            None,
        )?;
        if let Some(rev) = rev {
            // Fetch and checkout the pinned ref; depth-1 clones don't have it.
            let _ = run(&["fetch", "--depth", "1", "origin", rev], Some(dest));
            run(&["checkout", rev], Some(dest))?;
        }
        run(&["rev-parse", "HEAD"], Some(dest))
    }
}

trait RevOpt {
    fn as_str_opt(&self) -> Option<&str>;
}
impl RevOpt for String {
    fn as_str_opt(&self) -> Option<&str> {
        (!self.is_empty()).then_some(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn write_pkg(dir: &Path, name: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join(MANIFEST_FILE),
            json!({
                "name": name,
                "description": "Test.",
                "version": "0.1.0",
                "yolop": {
                    "protocol_version": "1.0",
                    "capabilityServer": { "command": "yolop-extension-x" },
                    "tools": [{ "name": "t" }]
                }
            })
            .to_string(),
        )
        .unwrap();
    }

    #[test]
    fn source_parsing() {
        assert_eq!(
            Source::parse("crates.io:yolop-extension-lsp@0.1.0").unwrap(),
            Source::Crate {
                crate_name: "yolop-extension-lsp".into(),
                version: "0.1.0".into()
            }
        );
        assert_eq!(
            Source::parse("lsp").unwrap(),
            Source::Crate {
                crate_name: "yolop-extension-lsp".into(),
                version: String::new()
            }
        );
        assert_eq!(
            Source::parse("https://github.com/acme/x@v1").unwrap(),
            Source::Git {
                url: "https://github.com/acme/x".into(),
                rev: "v1".into()
            }
        );
    }

    #[test]
    fn install_from_path_pins_and_hashes() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src-pkg");
        write_pkg(&src, "echo");
        let ext_dir = tmp.path().join("extensions");

        let source = Source::parse(src.to_str().unwrap()).unwrap();
        let installed = install(&ext_dir, &source, &SystemGit).unwrap();
        assert_eq!(installed.manifest.name, "echo");
        assert!(installed.content_hash.starts_with("sha256:"));
        assert!(installed.previous_hash.is_none());
        assert!(ext_dir.join("echo").join(MANIFEST_FILE).is_file());

        let lock = Lockfile::load(&ext_dir);
        assert_eq!(
            lock.get("echo").unwrap().content_hash,
            installed.content_hash
        );

        // Reinstall unchanged: no grant change.
        let again = install(&ext_dir, &source, &SystemGit).unwrap();
        assert!(again.previous_hash.is_none());

        // Change a file, reinstall: previous hash surfaces.
        std::fs::write(src.join("extra.txt"), "new").unwrap();
        let changed = install(&ext_dir, &source, &SystemGit).unwrap();
        assert!(changed.previous_hash.is_some());
    }

    #[test]
    fn remove_deletes_dir_and_pin() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("p");
        write_pkg(&src, "echo");
        let ext_dir = tmp.path().join("extensions");
        install(
            &ext_dir,
            &Source::parse(src.to_str().unwrap()).unwrap(),
            &SystemGit,
        )
        .unwrap();

        assert!(remove(&ext_dir, "echo").unwrap());
        assert!(!ext_dir.join("echo").exists());
        assert!(Lockfile::load(&ext_dir).get("echo").is_none());
        assert!(!remove(&ext_dir, "echo").unwrap());
    }

    struct FakeGit;
    impl GitRunner for FakeGit {
        fn clone_into(&self, _url: &str, _rev: Option<&str>, dest: &Path) -> Result<String> {
            write_pkg(dest, "gitext");
            Ok("abc123".into())
        }
    }

    #[test]
    fn install_from_git_records_resolved_rev() {
        let tmp = tempfile::tempdir().unwrap();
        let ext_dir = tmp.path().join("extensions");
        let source = Source::parse("https://example.com/acme/gitext").unwrap();
        let installed = install(&ext_dir, &source, &FakeGit).unwrap();
        assert_eq!(installed.manifest.name, "gitext");
        match Lockfile::load(&ext_dir)
            .get("gitext")
            .unwrap()
            .source
            .clone()
        {
            Source::Git { rev, .. } => assert_eq!(rev, "abc123"),
            other => panic!("expected git source, got {other:?}"),
        }
    }
}

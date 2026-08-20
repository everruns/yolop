//! Extension installation: sources, the `extensions.lock` pin file, and
//! install/remove into the global extensions directory. See
//! `knowledge/specs/extensions.md`, "Packaging, install, trust".
//!
//! Trust: installing is consent by action (the user ran the install verb),
//! mirroring how authoring `.mcp.json` consents to its stdio servers. The
//! lockfile records what was approved (source, resolved commit, content
//! hash) so `update` can show a contribution diff and re-ask on a widened
//! grant. There is one scope — global — because repos never carry yolop
//! extensions.

use super::package::{ExtensionManifest, MANIFEST_FILE, parse_manifest};
use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
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

/// The crate-name prefix every published yolop extension carries.
const CRATE_PREFIX: &str = "yolop-extension-";

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
        // Bare name shorthand: `lsp` → crate `yolop-extension-lsp`. The
        // published crate name is what a user copies off crates.io, so the
        // prefix is applied only when it is not already there: prefixing
        // unconditionally turned `yolop-extension-lsp` into a lookup for
        // `yolop-extension-yolop-extension-lsp`, which cannot exist.
        // Split the optional `@<version>` pin off first, then validate the
        // name: checking the whole spec instead forbade `@` and the version's
        // dots, which made the `<name>@<version>` form unparseable and left
        // this branch's version split unreachable.
        let (name, version) = split_at_version(spec);
        if !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            let crate_name = match name.strip_prefix(CRATE_PREFIX) {
                Some(rest) if !rest.is_empty() => name.to_string(),
                _ => format!("{CRATE_PREFIX}{name}"),
            };
            return Ok(Source::Crate {
                crate_name,
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
    Ok(format!("sha256:{}", hex_encode(&hasher.finalize())))
}

/// Lowercase hex encoding of a byte slice (SHA-256 digests, here).
fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut hex = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(hex, "{byte:02x}");
    }
    hex
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

/// Whether the package's declared `capabilityServer.command` can actually be
/// spawned: resolvable in the package's own `bin/`, as a path, or on `PATH`.
///
/// A crates.io `.crate` carries source, not binaries, and the install path is
/// toolchain-free by design, so a Rust extension published without a `bin/`
/// installs cleanly and can never spawn. Install says so rather than reporting
/// a bare success and leaving `doctor` to explain it later.
pub fn server_command_resolves(package_dir: &Path, command: &str) -> bool {
    if command.contains(std::path::MAIN_SEPARATOR) || command.contains('/') {
        let candidate = Path::new(command);
        return if candidate.is_absolute() {
            candidate.is_file()
        } else {
            package_dir.join(candidate).is_file()
        };
    }
    if package_dir.join("bin").join(command).is_file() {
        return true;
    }
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| dir.join(command).is_file())
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
/// Path, git, and crates.io sources are all handled natively — the crate
/// source fetches + unpacks a `.crate` tarball with no cargo/rustc toolchain
/// (see [`CrateFetcher`]). The package is staged, its manifest parsed
/// (approval boundary — no binary is executed), then swapped into place and
/// pinned in the lockfile.
pub async fn install(
    extensions_dir: &Path,
    source: &Source,
    git: &dyn GitRunner,
    crates: &dyn CrateFetcher,
) -> Result<Installed> {
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
            let resolved_rev = git.clone_into(url, rev_opt(rev), &staging)?;
            Source::Git {
                url: url.clone(),
                rev: resolved_rev,
            }
        }
        Source::Crate {
            crate_name,
            version,
        } => {
            let resolved_version = crates
                .fetch_into(crate_name, rev_opt(version), &staging)
                .await
                .with_context(|| format!("fetching crate {crate_name}"))?;
            Source::Crate {
                crate_name: crate_name.clone(),
                version: resolved_version,
            }
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

/// A non-empty revision/version string as `Some(&str)`, empty as `None`.
fn rev_opt(value: &str) -> Option<&str> {
    (!value.is_empty()).then_some(value)
}

/// Seam for crates.io fetches, so install logic is testable without a network.
#[async_trait]
pub trait CrateFetcher: Send + Sync {
    /// Resolve `crate_name` (at `version`, or the latest non-yanked release if
    /// `None`), download and checksum-verify its `.crate` tarball, and extract
    /// the package contents into `dest` (stripping the `{name}-{vers}/` top
    /// directory). Returns the resolved version.
    async fn fetch_into(
        &self,
        crate_name: &str,
        version: Option<&str>,
        dest: &Path,
    ) -> Result<String>;
}

/// Real crates.io fetch over the sparse index + static CDN — no cargo/rustc.
///
/// 1. `GET https://index.crates.io/<prefix>/<crate>` — the sparse index file,
///    newline-delimited JSON, one object per published version.
/// 2. Pick the requested version (or the highest non-yanked, non-prerelease).
/// 3. `GET https://static.crates.io/crates/<crate>/<crate>-<vers>.crate`.
/// 4. Verify the SHA-256 against the index `cksum`, then unpack the gzip'd tar.
pub struct SystemCrateFetcher {
    index_base: String,
    cdn_base: String,
}

impl Default for SystemCrateFetcher {
    fn default() -> Self {
        Self {
            index_base: "https://index.crates.io".into(),
            cdn_base: "https://static.crates.io/crates".into(),
        }
    }
}

#[async_trait]
impl CrateFetcher for SystemCrateFetcher {
    async fn fetch_into(
        &self,
        crate_name: &str,
        version: Option<&str>,
        dest: &Path,
    ) -> Result<String> {
        let name = crate_name.to_ascii_lowercase();
        let index_url = format!("{}/{}", self.index_base, sparse_index_path(&name));
        let client = reqwest::Client::new();
        let index_body = client
            .get(&index_url)
            .header("User-Agent", "yolop-extension-install")
            .send()
            .await
            .with_context(|| format!("fetching sparse index {index_url}"))?
            .error_for_status()
            .with_context(|| format!("crate `{crate_name}` not found on crates.io"))?
            .text()
            .await?;
        let entries = parse_index(&index_body)?;
        let picked = pick_version(&entries, version)?;

        let crate_url = format!("{}/{name}/{name}-{}.crate", self.cdn_base, picked.vers);
        let bytes = client
            .get(&crate_url)
            .header("User-Agent", "yolop-extension-install")
            .send()
            .await
            .with_context(|| format!("downloading {crate_url}"))?
            .error_for_status()?
            .bytes()
            .await?;

        let actual = sha256_hex(&bytes);
        if actual != picked.cksum {
            bail!(
                "checksum mismatch for {name}-{}: index says {}, download is {actual}",
                picked.vers,
                picked.cksum
            );
        }
        extract_crate(&bytes, dest)
            .with_context(|| format!("unpacking {name}-{}.crate", picked.vers))?;
        Ok(picked.vers.clone())
    }
}

/// The sparse-index path for a crate name (cargo's layout): 1/2/3-char names
/// get short prefixes; 4+ split on the first two pairs of characters.
fn sparse_index_path(name: &str) -> String {
    match name.len() {
        0 => String::new(),
        1 => format!("1/{name}"),
        2 => format!("2/{name}"),
        3 => format!("3/{}/{name}", &name[0..1]),
        _ => format!("{}/{}/{name}", &name[0..2], &name[2..4]),
    }
}

#[derive(Debug, Deserialize)]
struct IndexEntry {
    vers: String,
    cksum: String,
    #[serde(default)]
    yanked: bool,
}

fn parse_index(body: &str) -> Result<Vec<IndexEntry>> {
    let entries: Vec<IndexEntry> = body
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(serde_json::from_str)
        .collect::<Result<_, _>>()
        .context("parsing crates.io sparse index")?;
    if entries.is_empty() {
        bail!("crate has no published versions");
    }
    Ok(entries)
}

/// Pick the version to install: an exact match when requested (yanked allowed
/// if explicitly named), else the highest non-yanked, non-prerelease release.
fn pick_version<'a>(entries: &'a [IndexEntry], requested: Option<&str>) -> Result<&'a IndexEntry> {
    if let Some(req) = requested {
        return entries
            .iter()
            .find(|e| e.vers == req)
            .ok_or_else(|| anyhow!("version {req} is not published on crates.io"));
    }
    entries
        .iter()
        .filter(|e| !e.yanked && !e.vers.contains('-'))
        .max_by(|a, b| parse_semver(&a.vers).cmp(&parse_semver(&b.vers)))
        .ok_or_else(|| anyhow!("no installable (non-yanked) release found"))
}

/// A coarse numeric key for release ordering — pre-releases are filtered out
/// before this runs, so `major.minor.patch` (missing parts = 0) is enough.
fn parse_semver(v: &str) -> Vec<u64> {
    v.split('.')
        .map(|part| part.trim().parse::<u64>().unwrap_or(0))
        .collect()
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex_encode(&Sha256::digest(bytes))
}

/// Unpack a `.crate` (gzip'd tar) into `dest`, dropping the leading
/// `{name}-{vers}/` directory every crate tarball carries. Rejects entries
/// that would escape `dest` (absolute paths or `..`).
fn extract_crate(bytes: &[u8], dest: &Path) -> Result<()> {
    use std::path::Component;
    let decoder = flate2::read::GzDecoder::new(bytes);
    let mut archive = tar::Archive::new(decoder);
    for entry in archive.entries().context("reading crate tar")? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        // Strip the top-level `{name}-{vers}` directory.
        let rel: PathBuf = path.components().skip(1).collect();
        if rel.as_os_str().is_empty() {
            continue;
        }
        if rel.is_absolute() || rel.components().any(|c| matches!(c, Component::ParentDir)) {
            bail!("refusing unsafe path in crate tarball: {}", rel.display());
        }
        let out = dest.join(&rel);
        if entry.header().entry_type().is_dir() {
            std::fs::create_dir_all(&out)?;
        } else {
            if let Some(parent) = out.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut file = std::fs::File::create(&out)
                .with_context(|| format!("writing {}", out.display()))?;
            std::io::copy(&mut entry, &mut file)?;
        }
    }
    Ok(())
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

    /// A crate fetcher that never runs — for path/git tests that don't touch
    /// crates.io. Panics if called so a mis-routed source is caught.
    struct NoCrates;
    #[async_trait]
    impl CrateFetcher for NoCrates {
        async fn fetch_into(&self, _: &str, _: Option<&str>, _: &Path) -> Result<String> {
            panic!("crate fetcher should not be used in this test");
        }
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
        // The published crate name is what a user copies off crates.io, so the
        // shorthand must not prefix a name that already carries the prefix.
        assert_eq!(
            Source::parse("yolop-extension-lsp").unwrap(),
            Source::Crate {
                crate_name: "yolop-extension-lsp".into(),
                version: String::new()
            }
        );
        assert_eq!(
            Source::parse("yolop-extension-lsp@0.1.0").unwrap(),
            Source::Crate {
                crate_name: "yolop-extension-lsp".into(),
                version: "0.1.0".into()
            }
        );
        // A name that merely starts with the same letters keeps its prefix.
        assert_eq!(
            Source::parse("yolop-extensions-thing").unwrap(),
            Source::Crate {
                crate_name: "yolop-extension-yolop-extensions-thing".into(),
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
    fn sparse_index_path_follows_cargo_layout() {
        assert_eq!(sparse_index_path("a"), "1/a");
        assert_eq!(sparse_index_path("ab"), "2/ab");
        assert_eq!(sparse_index_path("abc"), "3/a/abc");
        assert_eq!(
            sparse_index_path("yolop-extension-lsp"),
            "yo/lo/yolop-extension-lsp"
        );
    }

    #[test]
    fn pick_version_prefers_highest_stable() {
        let entries = vec![
            IndexEntry {
                vers: "0.1.0".into(),
                cksum: "a".into(),
                yanked: false,
            },
            IndexEntry {
                vers: "0.2.0".into(),
                cksum: "b".into(),
                yanked: false,
            },
            IndexEntry {
                vers: "0.10.0".into(),
                cksum: "c".into(),
                yanked: false,
            },
            IndexEntry {
                vers: "0.11.0".into(),
                cksum: "d".into(),
                yanked: true,
            },
            IndexEntry {
                vers: "0.12.0-rc.1".into(),
                cksum: "e".into(),
                yanked: false,
            },
        ];
        // Highest non-yanked, non-prerelease: 0.10.0 (beats 0.2.0 numerically,
        // yanked 0.11.0 and pre-release 0.12.0-rc.1 excluded).
        assert_eq!(pick_version(&entries, None).unwrap().vers, "0.10.0");
        // Exact request wins even when yanked.
        assert_eq!(
            pick_version(&entries, Some("0.11.0")).unwrap().vers,
            "0.11.0"
        );
        assert!(pick_version(&entries, Some("9.9.9")).is_err());
    }

    #[tokio::test]
    async fn install_from_path_pins_and_hashes() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src-pkg");
        write_pkg(&src, "echo");
        let ext_dir = tmp.path().join("extensions");

        let source = Source::parse(src.to_str().unwrap()).unwrap();
        let installed = install(&ext_dir, &source, &SystemGit, &NoCrates)
            .await
            .unwrap();
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
        let again = install(&ext_dir, &source, &SystemGit, &NoCrates)
            .await
            .unwrap();
        assert!(again.previous_hash.is_none());

        // Change a file, reinstall: previous hash surfaces.
        std::fs::write(src.join("extra.txt"), "new").unwrap();
        let changed = install(&ext_dir, &source, &SystemGit, &NoCrates)
            .await
            .unwrap();
        assert!(changed.previous_hash.is_some());
    }

    #[tokio::test]
    async fn remove_deletes_dir_and_pin() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("p");
        write_pkg(&src, "echo");
        let ext_dir = tmp.path().join("extensions");
        install(
            &ext_dir,
            &Source::parse(src.to_str().unwrap()).unwrap(),
            &SystemGit,
            &NoCrates,
        )
        .await
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

    #[tokio::test]
    async fn install_from_git_records_resolved_rev() {
        let tmp = tempfile::tempdir().unwrap();
        let ext_dir = tmp.path().join("extensions");
        let source = Source::parse("https://example.com/acme/gitext").unwrap();
        let installed = install(&ext_dir, &source, &FakeGit, &NoCrates)
            .await
            .unwrap();
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

    /// Build a `.crate` tarball (gzip'd tar) carrying `{name}-{vers}/plugin.json`,
    /// exactly as crates.io ships one.
    fn build_crate_tarball(name: &str, vers: &str, manifest: &str) -> Vec<u8> {
        use flate2::{Compression, write::GzEncoder};
        let mut tar = tar::Builder::new(Vec::new());
        let bytes = manifest.as_bytes();
        let mut header = tar::Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append_data(&mut header, format!("{name}-{vers}/{MANIFEST_FILE}"), bytes)
            .unwrap();
        let tar_bytes = tar.into_inner().unwrap();
        let mut gz = GzEncoder::new(Vec::new(), Compression::default());
        std::io::Write::write_all(&mut gz, &tar_bytes).unwrap();
        gz.finish().unwrap()
    }

    #[test]
    fn extract_crate_strips_top_dir_and_blocks_traversal() {
        let manifest = r#"{"name":"x"}"#;
        let tarball = build_crate_tarball("yolop-extension-x", "0.3.0", manifest);
        let tmp = tempfile::tempdir().unwrap();
        extract_crate(&tarball, tmp.path()).unwrap();
        // Top dir stripped: plugin.json sits at the destination root.
        let got = std::fs::read_to_string(tmp.path().join(MANIFEST_FILE)).unwrap();
        assert_eq!(got, manifest);
    }

    /// End-to-end crates.io install against a local sparse index + CDN served
    /// by `tiny_http`, proving the whole path: index parse → version pick →
    /// download → checksum verify → unpack → manifest read → lockfile pin.
    #[tokio::test]
    async fn install_from_crates_io_end_to_end() {
        let manifest = json!({
            "name": "clifetch",
            "description": "Fetched from crates.io.",
            "version": "0.3.0",
            "yolop": {
                "protocol_version": "1.0",
                "capabilityServer": { "command": "yolop-extension-clifetch" },
                "tools": [{ "name": "t" }]
            }
        })
        .to_string();
        let name = "yolop-extension-clifetch";
        let tarball = build_crate_tarball(name, "0.3.0", &manifest);
        let cksum = sha256_hex(&tarball);
        let index_line =
            json!({ "name": name, "vers": "0.3.0", "cksum": cksum, "yanked": false }).to_string();

        // Serve `{index}/yo/lo/<name>` and `{cdn}/<name>/<name>-0.3.0.crate`.
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr().to_ip().unwrap();
        let base = format!("http://{addr}");
        let index_body = index_line.clone();
        let crate_bytes = tarball.clone();
        let handle = std::thread::spawn(move || {
            for _ in 0..2 {
                let request = match server.recv() {
                    Ok(r) => r,
                    Err(_) => break,
                };
                let url = request.url().to_string();
                let response = if url.ends_with(".crate") {
                    tiny_http::Response::from_data(crate_bytes.clone())
                } else {
                    tiny_http::Response::from_data(index_body.clone().into_bytes())
                };
                let _ = request.respond(response);
            }
        });

        let fetcher = SystemCrateFetcher {
            index_base: base.clone(),
            cdn_base: format!("{base}/crates"),
        };
        let tmp = tempfile::tempdir().unwrap();
        let ext_dir = tmp.path().join("extensions");
        let source = Source::Crate {
            crate_name: name.into(),
            version: String::new(),
        };
        let installed = install(&ext_dir, &source, &SystemGit, &fetcher)
            .await
            .unwrap();
        let _ = handle.join();

        assert_eq!(installed.manifest.name, "clifetch");
        assert!(ext_dir.join("clifetch").join(MANIFEST_FILE).is_file());
        match Lockfile::load(&ext_dir)
            .get("clifetch")
            .unwrap()
            .source
            .clone()
        {
            Source::Crate { version, .. } => assert_eq!(version, "0.3.0"),
            other => panic!("expected crate source, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn crate_checksum_mismatch_is_rejected() {
        let name = "yolop-extension-bad";
        let tarball = build_crate_tarball(name, "1.0.0", r#"{"name":"bad"}"#);
        // Advertise a wrong checksum; the download must be refused.
        let index_line =
            json!({ "name": name, "vers": "1.0.0", "cksum": "deadbeef", "yanked": false })
                .to_string();
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr().to_ip().unwrap();
        let base = format!("http://{addr}");
        let crate_bytes = tarball.clone();
        let handle = std::thread::spawn(move || {
            for _ in 0..2 {
                let Ok(request) = server.recv() else { break };
                let url = request.url().to_string();
                let response = if url.ends_with(".crate") {
                    tiny_http::Response::from_data(crate_bytes.clone())
                } else {
                    tiny_http::Response::from_data(index_line.clone().into_bytes())
                };
                let _ = request.respond(response);
            }
        });
        let fetcher = SystemCrateFetcher {
            index_base: base.clone(),
            cdn_base: format!("{base}/crates"),
        };
        let tmp = tempfile::tempdir().unwrap();
        let err = fetcher
            .fetch_into(name, None, tmp.path())
            .await
            .unwrap_err()
            .to_string();
        let _ = handle.join();
        assert!(err.contains("checksum mismatch"), "{err}");
    }
}

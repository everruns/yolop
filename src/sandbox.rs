//! Pluggable containment for arbitrary child-process execution.
//!
//! Structured filesystem tools remain in the trusted host broker. Every shell
//! entry point receives one of these providers, so foreground, background and
//! interactive commands share the same kernel boundary.

use crate::settings::SandboxMode;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::process::Command;

pub(crate) trait SandboxProvider: Send + Sync {
    fn command(&self, cwd: &Path, script: &str) -> Result<Command>;
}

pub(crate) fn provider(mode: SandboxMode) -> std::sync::Arc<dyn SandboxProvider> {
    match mode {
        SandboxMode::Native => std::sync::Arc::new(NativeSandbox),
        SandboxMode::Off => std::sync::Arc::new(UnsafeHost),
    }
}

pub(crate) fn danger_warning(mode: SandboxMode) -> Option<&'static str> {
    (mode == SandboxMode::Off).then_some(
        "DANGER: sandbox disabled (UNSAFE HOST) — shell commands can access and modify files, processes, and the network outside the workspace",
    )
}

struct UnsafeHost;

impl SandboxProvider for UnsafeHost {
    fn command(&self, cwd: &Path, script: &str) -> Result<Command> {
        let mut command = Command::new("bash");
        command.arg("-lc").arg(script).current_dir(cwd);
        Ok(command)
    }
}

struct NativeSandbox;

impl SandboxProvider for NativeSandbox {
    fn command(&self, cwd: &Path, script: &str) -> Result<Command> {
        native_command(cwd, script)
    }
}

#[cfg(target_os = "macos")]
fn native_command(cwd: &Path, script: &str) -> Result<Command> {
    let executable = Path::new("/usr/bin/sandbox-exec");
    if !executable.is_file() {
        anyhow::bail!(
            "native sandbox unavailable: /usr/bin/sandbox-exec is missing; refusing to run unsandboxed. Set `sandbox = \"off\"` only inside an already isolated environment"
        )
    }

    // Seatbelt denies network and all writes by default. Reads stay available
    // so compilers, SDKs, package caches and system skills continue to work;
    // only the active workspace and a private temporary directory are writable.
    let temp = sandbox_temp_dir()?;
    let home = sandbox_home_dir(&temp)?;
    let profile = format!(
        "(version 1)\n(deny default)\n(allow process*)\n(allow file-read*)\n(allow sysctl-read)\n(allow mach-lookup)\n(allow file-write* (subpath \"{}\") (subpath \"{}\"))\n(deny network*)\n(deny file-write* (literal \"{}\") (subpath \"{}\"))",
        seatbelt_escape(cwd),
        seatbelt_escape(&temp),
        seatbelt_escape(&cwd.join(".git")),
        seatbelt_escape(&cwd.join(".git")),
    );
    let mut command = Command::new(executable);
    command
        .arg("-p")
        .arg(profile)
        .arg("/bin/bash")
        .arg("-lc")
        .arg(script)
        .current_dir(cwd);
    apply_native_environment(&mut command, &home, &temp);
    Ok(command)
}

#[cfg(target_os = "linux")]
fn native_command(cwd: &Path, script: &str) -> Result<Command> {
    let temp = sandbox_temp_dir()?;
    let home = sandbox_home_dir(&temp)?;
    let executable = std::env::current_exe().context("resolve yolop sandbox worker executable")?;
    let mut command = Command::new(executable);
    command
        .arg("__sandbox-exec")
        .arg("--cwd")
        .arg(cwd)
        .arg("--temp")
        .arg(&temp)
        .arg("--script")
        .arg(script)
        .current_dir(cwd);
    apply_native_environment(&mut command, &home, &temp);
    Ok(command)
}

/// Apply Linux kernel restrictions in a fresh helper process, then replace it
/// with bash. Keeping this out of `pre_exec` avoids allocation and locking in a
/// post-fork callback of Tokio's multi-threaded parent.
#[cfg(target_os = "linux")]
pub(crate) fn run_linux_worker(cwd: &Path, temp: &Path, script: &str) -> Result<()> {
    use landlock::{
        ABI, Access, AccessFs, PathBeneath, PathFd, Ruleset, RulesetAttr, RulesetCreatedAttr,
        RulesetStatus,
    };
    use seccompiler::{
        BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter,
        SeccompRule,
    };
    use std::convert::TryInto;
    use std::os::unix::process::CommandExt;

    std::env::set_current_dir(cwd)
        .with_context(|| format!("enter sandbox workspace: {}", cwd.display()))?;

    // ABI V3 is the oldest policy that also mediates truncate(2); requiring
    // full enforcement avoids silently weakening the write boundary.
    let abi = ABI::V3;
    let status = Ruleset::default()
        .handle_access(AccessFs::from_all(abi))?
        .create()?
        .add_rule(PathBeneath::new(
            PathFd::new("/")?,
            AccessFs::from_read(abi),
        ))?
        .add_rule(PathBeneath::new(PathFd::new(cwd)?, AccessFs::from_all(abi)))?
        .add_rule(PathBeneath::new(
            PathFd::new(temp)?,
            AccessFs::from_all(abi),
        ))?
        .restrict_self()?;
    if status.ruleset != RulesetStatus::FullyEnforced || !status.no_new_privs {
        anyhow::bail!(
            "native sandbox unavailable: Landlock ABI v3 is not fully enforced by this Linux kernel; refusing to run unsandboxed. Set `sandbox = \"off\"` only inside an already isolated environment"
        );
    }

    // Internet and packet sockets are denied. Unix sockets remain available
    // for local toolchains and agents; the sanitized environment removes
    // credential-bearing agent socket paths before this worker starts.
    let socket_rules = [libc::AF_INET, libc::AF_INET6, libc::AF_PACKET]
        .into_iter()
        .map(|family| {
            SeccompRule::new(vec![SeccompCondition::new(
                0,
                SeccompCmpArgLen::Dword,
                SeccompCmpOp::Eq,
                family as u64,
            )?])
        })
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let filter: BpfProgram = SeccompFilter::new(
        [(libc::SYS_socket, socket_rules)].into_iter().collect(),
        SeccompAction::Allow,
        SeccompAction::Errno(libc::EACCES as u32),
        std::env::consts::ARCH.try_into()?,
    )?
    .try_into()?;
    seccompiler::apply_filter(&filter)?;

    Err(std::process::Command::new("/bin/bash")
        .arg("-lc")
        .arg(script)
        .exec()
        .into())
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn native_command(_cwd: &Path, _script: &str) -> Result<Command> {
    anyhow::bail!(
        "native sandbox is supported only on macOS and Linux; refusing to run unsandboxed. Set `sandbox = \"off\"` only inside an already isolated environment"
    )
}

fn sandbox_temp_dir() -> Result<PathBuf> {
    let path = std::env::temp_dir().join(format!("yolop-sandbox-{}", std::process::id()));
    prepare_sandbox_temp(&path)?;
    Ok(path)
}

fn prepare_sandbox_temp(path: &Path) -> Result<()> {
    use std::io::ErrorKind;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    match std::fs::create_dir(path) {
        Ok(()) => {}
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {
            let metadata = std::fs::symlink_metadata(path)
                .with_context(|| format!("inspect sandbox temp directory: {}", path.display()))?;
            if !metadata.file_type().is_dir() {
                anyhow::bail!(
                    "sandbox temp path is not a directory (possible path-alias attack): {}",
                    path.display()
                );
            }
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("create sandbox temp directory: {}", path.display()));
        }
    }
    #[cfg(unix)]
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("secure sandbox temp directory: {}", path.display()))?;
    Ok(())
}

fn sandbox_home_dir(temp: &Path) -> Result<PathBuf> {
    let path = temp.join("home");
    std::fs::create_dir_all(&path)
        .with_context(|| format!("create sandbox home directory: {}", path.display()))?;
    Ok(path)
}

fn apply_native_environment(command: &mut Command, home: &Path, temp: &Path) {
    command.env_clear();
    for (key, value) in std::env::vars_os() {
        if safe_environment_key(&key.to_string_lossy()) {
            command.env(key, value);
        }
    }
    command.env("HOME", home).env("TMPDIR", temp);
}

fn safe_environment_key(key: &str) -> bool {
    matches!(
        key,
        "PATH"
            | "LANG"
            | "LC_ALL"
            | "LC_CTYPE"
            | "TERM"
            | "COLORTERM"
            | "NO_COLOR"
            | "FORCE_COLOR"
            | "CARGO_HOME"
            | "RUSTUP_HOME"
            | "SDKROOT"
            | "DEVELOPER_DIR"
            | "PKG_CONFIG_PATH"
            | "CPATH"
            | "LIBRARY_PATH"
            | "C_INCLUDE_PATH"
            | "CPLUS_INCLUDE_PATH"
            | "JAVA_HOME"
            | "GOPATH"
            | "GOROOT"
    ) || key.starts_with("LC_")
}

#[cfg(target_os = "macos")]
fn seatbelt_escape(path: &Path) -> String {
    path.to_string_lossy()
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
}

pub(crate) fn configure_stdio(command: &mut Command) {
    command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn sandbox_temp_rejects_a_preexisting_symlink() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("target");
        let alias = root.path().join("alias");
        std::fs::create_dir(&target).unwrap();
        std::os::unix::fs::symlink(&target, &alias).unwrap();

        let error = prepare_sandbox_temp(&alias).unwrap_err().to_string();
        assert!(error.contains("path-alias attack"), "{error}");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn seatbelt_paths_are_escaped() {
        assert_eq!(
            seatbelt_escape(Path::new("/tmp/a \\\"b")),
            "/tmp/a \\\\\\\"b"
        );
    }

    #[test]
    fn native_is_the_safe_default_provider() {
        assert!(danger_warning(SandboxMode::Native).is_none());
        assert!(
            danger_warning(SandboxMode::Off)
                .unwrap()
                .contains("UNSAFE HOST")
        );
    }

    #[test]
    fn native_environment_allowlist_excludes_credentials_and_agents() {
        for key in [
            "OPENAI_API_KEY",
            "ANTHROPIC_API_KEY",
            "DOPPLER_TOKEN",
            "GITHUB_TOKEN",
            "AWS_SECRET_ACCESS_KEY",
            "SSH_AUTH_SOCK",
            "GPG_AGENT_INFO",
        ] {
            assert!(!safe_environment_key(key), "unexpectedly allowed {key}");
        }
        for key in [
            "PATH",
            "CARGO_HOME",
            "RUSTUP_HOME",
            "SDKROOT",
            "LC_MESSAGES",
        ] {
            assert!(safe_environment_key(key), "expected build env {key}");
        }
    }
}

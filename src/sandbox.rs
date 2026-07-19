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
    let bwrap = find_in_path("bwrap").ok_or_else(|| anyhow::anyhow!(
        "native sandbox unavailable: bubblewrap (`bwrap`) is required on Linux; refusing to run unsandboxed. Install bubblewrap or set `sandbox = \"off\"` only inside an already isolated environment"
    ))?;
    let temp = sandbox_temp_dir()?;
    let home = sandbox_home_dir(&temp)?;
    let mut command = Command::new(bwrap);
    command
        .args(["--die-with-parent", "--new-session", "--unshare-net"])
        .args(["--ro-bind", "/", "/"])
        .arg("--bind")
        .arg(cwd)
        .arg(cwd)
        .arg("--bind")
        .arg(&temp)
        .arg(&temp)
        .arg("--chdir")
        .arg(cwd);
    let dot_git = cwd.join(".git");
    if dot_git.exists() {
        command.arg("--ro-bind").arg(&dot_git).arg(&dot_git);
    }
    command.arg("/bin/bash").arg("-lc").arg(script);
    apply_native_environment(&mut command, &home, &temp);
    Ok(command)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn native_command(_cwd: &Path, _script: &str) -> Result<Command> {
    anyhow::bail!(
        "native sandbox is supported only on macOS and Linux; refusing to run unsandboxed. Set `sandbox = \"off\"` only inside an already isolated environment"
    )
}

fn sandbox_temp_dir() -> Result<PathBuf> {
    let path = std::env::temp_dir().join(format!("yolop-sandbox-{}", std::process::id()));
    std::fs::create_dir_all(&path)
        .with_context(|| format!("create sandbox temp directory: {}", path.display()))?;
    Ok(path)
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

#[cfg(target_os = "linux")]
fn find_in_path(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|path| path.join(name))
            .find(|path| path.is_file())
    })
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

//! Windows shell containment on Microsoft's MXC library.
//!
//! Decisions:
//! - MXC runs in-process (`mxc_sdk::v1::spawn`): no helper executable ships
//!   next to Yolop, matching macOS, where the OS supplies `sandbox-exec`.
//! - The ProcessContainer backend is used: AppContainer-based process
//!   isolation with Windows Filtering Platform egress rules. It needs Windows
//!   11 24H2 or later; older builds fail closed, like a missing Landlock ABI.
//! - AppContainer denies reads by default, unlike Seatbelt and Landlock, so
//!   the policy grants the workspace, the private temp, toolchain directories
//!   found on `PATH` and well-known SDK variables, and per-user installs under
//!   `%LOCALAPPDATA%\Programs`. Everything else in the profile stays unreadable.
//! - All network is denied, host loopback included, so the attached `yolop`
//!   control endpoint (loopback TCP on Windows) is not offered in here; see
//!   `SandboxProvider::supports_control_endpoint`.
//! - Workspace `.git` stays writable, matching Linux rather than macOS.
//! - Scripts travel as `-EncodedCommand` so no PowerShell quoting is involved.

use std::io::Read;
use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{Context, Result};
use base64::Engine as _;
use mxc_sdk::v1::{
    self, ContainerRequest, FilesystemPolicy, MxcProcess, NetworkAction, NetworkEgressPolicy,
    NetworkIngressPolicy, NetworkPolicy, StreamCloser,
    policy::filesystem::{
        ToolsPolicyContainerType, ToolsPolicyOptions, available_tools_policy, user_profile_policy,
    },
};
use tokio::io::AsyncWriteExt;

use crate::config::SandboxMode;
use crate::exec::sandbox::ChildOutput;

/// Whether this host can create a ProcessContainer. Probed once per process.
pub(crate) fn available() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(
        || matches!(v1::probe(None), Ok(probe) if probe.error.is_none() && probe.tier.is_some()),
    )
}

/// A contained PowerShell launch for `script` in `cwd`.
pub(crate) fn command(cwd: &Path, script: &str, mode: SandboxMode) -> Result<ContainedCommand> {
    if mode == SandboxMode::DangerFullAccess {
        anyhow::bail!("the Windows sandbox cannot run danger-full-access");
    }
    if !available() {
        anyhow::bail!(
            "native sandbox unavailable: this Windows build cannot create an MXC process container (Windows 11 24H2 or later is required); refusing to run unsandboxed. Set `sandbox_mode = \"danger-full-access\"` only inside an already isolated environment"
        );
    }

    let temp = super::sandbox::sandbox_temp_dir()?;
    let home = super::sandbox::sandbox_home_dir(&temp)?;
    let mut filesystem = FilesystemPolicy {
        readwrite_paths: vec![path_string(&temp)],
        ..Default::default()
    };
    if mode == SandboxMode::WorkspaceWrite {
        filesystem.readwrite_paths.push(path_string(cwd));
        if let Some(skills) = super::sandbox::prepared_global_skills_dir()? {
            filesystem.readwrite_paths.push(path_string(&skills));
        }
    } else {
        filesystem.readonly_paths.push(path_string(cwd));
    }
    let tools = available_tools_policy(
        None,
        ToolsPolicyOptions {
            container_type: Some(ToolsPolicyContainerType::ProcessContainer),
        },
    );
    filesystem.readonly_paths.extend(tools.readonly_paths);
    filesystem
        .readonly_paths
        .extend(user_profile_policy(None).readonly_paths);
    dedupe_granted(&mut filesystem);

    let request = ContainerRequest {
        filesystem: Some(filesystem),
        network: Some(NetworkPolicy {
            egress: Some(NetworkEgressPolicy {
                default: Some(NetworkAction::Deny),
                ..Default::default()
            }),
            ingress: Some(NetworkIngressPolicy {
                default: Some(NetworkAction::Deny),
                host_loopback: Some(NetworkAction::Deny),
            }),
            runtime_config: None,
        }),
        working_directory: Some(path_string(cwd)),
        environment: Some(environment(&home, &temp)),
        inherit_default_environment: Some(false),
        ..ContainerRequest::new(powershell_command_line(script))
    };
    Ok(ContainedCommand { request })
}

/// A prepared, not yet started, contained command.
pub(crate) struct ContainedCommand {
    request: ContainerRequest,
}

impl ContainedCommand {
    /// Start the container. Output arrives through async readers fed by
    /// threads, because MXC exposes blocking pipes.
    pub(crate) fn spawn(self) -> std::io::Result<(ContainedChild, ChildOutput, ChildOutput)> {
        let mut process = v1::spawn(self.request, Default::default())
            .map_err(|error| std::io::Error::other(format!("MXC container: {error}")))?;
        let stdout = pump(process.take_stdout());
        let stderr = pump(process.take_stderr());
        let closers = [process.stdout_closer(), process.stderr_closer()]
            .into_iter()
            .flatten()
            .collect();
        let id = process.id();
        Ok((
            ContainedChild {
                process,
                closers,
                id,
                exited: false,
            },
            stdout,
            stderr,
        ))
    }
}

/// A running contained command. Killed when dropped before it exits, like a
/// `kill_on_drop` host command.
pub(crate) struct ContainedChild {
    process: MxcProcess,
    closers: Vec<StreamCloser>,
    id: u32,
    exited: bool,
}

impl ContainedChild {
    pub(crate) fn id(&self) -> u32 {
        self.id
    }

    pub(crate) fn kill(&mut self) -> std::io::Result<()> {
        let result = self.process.kill();
        // A descendant may still hold the pipes open; closing them ends the
        // reader threads so the caller's read loop finishes.
        for closer in &self.closers {
            closer.close();
        }
        result
    }

    /// Wait for exit. MXC has no async wait, so poll the non-blocking check.
    pub(crate) async fn wait(&mut self) -> std::io::Result<i32> {
        loop {
            if let Some(code) = self.process.try_wait()? {
                self.exited = true;
                return Ok(code);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

impl Drop for ContainedChild {
    fn drop(&mut self) {
        if !self.exited && !matches!(self.process.try_wait(), Ok(Some(_))) {
            let _ = self.kill();
        }
    }
}

fn pump(reader: Option<Box<dyn Read + Send>>) -> ChildOutput {
    let Some(mut reader) = reader else {
        return Box::new(tokio::io::empty());
    };
    let (mut writer, output) = tokio::io::duplex(64 * 1024);
    let runtime = tokio::runtime::Handle::current();
    std::thread::spawn(move || {
        let mut buffer = [0u8; 8192];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(read) => {
                    if runtime.block_on(writer.write_all(&buffer[..read])).is_err() {
                        break;
                    }
                }
            }
        }
    });
    Box::new(output)
}

/// `powershell.exe` by absolute path, with the script base64-encoded as
/// UTF-16LE, which is what `-EncodedCommand` expects.
fn powershell_command_line(script: &str) -> String {
    let root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".to_string());
    let utf16: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    let encoded = base64::engine::general_purpose::STANDARD.encode(utf16);
    format!(
        "\"{root}\\System32\\WindowsPowerShell\\v1.0\\powershell.exe\" -NoProfile -NonInteractive -EncodedCommand {encoded}"
    )
}

/// The sandbox environment: the shared build allowlist plus the variables
/// Windows programs need to start, with home and temp pointed at the private
/// sandbox directories. Credentials never pass through.
fn environment(home: &Path, temp: &Path) -> Vec<(String, String)> {
    let mut environment: Vec<(String, String)> = std::env::vars()
        .filter(|(key, _)| windows_environment_key(key))
        .collect();
    let home = path_string(home);
    let temp = path_string(temp);
    for (key, value) in [
        ("HOME", &home),
        ("TEMP", &temp),
        ("TMP", &temp),
        ("TMPDIR", &temp),
    ] {
        environment.retain(|(existing, _)| !existing.eq_ignore_ascii_case(key));
        environment.push((key.to_string(), value.clone()));
    }
    environment
}

/// Windows variable names are case-insensitive (`Path` vs `PATH`).
fn windows_environment_key(key: &str) -> bool {
    let upper = key.to_ascii_uppercase();
    super::sandbox::safe_environment_key(&upper)
        || matches!(
            upper.as_str(),
            "SYSTEMROOT"
                | "SYSTEMDRIVE"
                | "WINDIR"
                | "COMSPEC"
                | "PATHEXT"
                | "PSMODULEPATH"
                | "OS"
                | "PROCESSOR_ARCHITECTURE"
                | "PROCESSOR_IDENTIFIER"
                | "NUMBER_OF_PROCESSORS"
                | "PROGRAMFILES"
                | "PROGRAMFILES(X86)"
                | "PROGRAMW6432"
                | "COMMONPROGRAMFILES"
                | "COMMONPROGRAMFILES(X86)"
                | "PROGRAMDATA"
                | "ALLUSERSPROFILE"
        )
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// Drop read-only grants already covered by a read-write grant, so a tool
/// directory inside the workspace keeps write access.
fn dedupe_granted(filesystem: &mut FilesystemPolicy) {
    let writable: Vec<String> = filesystem
        .readwrite_paths
        .iter()
        .map(|path| path.to_ascii_lowercase())
        .collect();
    let mut seen = std::collections::HashSet::new();
    filesystem.readonly_paths.retain(|path| {
        let lower = path.to_ascii_lowercase();
        let covered = writable
            .iter()
            .any(|root| Path::new(&lower).starts_with(Path::new(root)));
        !covered && seen.insert(lower)
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_is_encoded_as_utf16le_base64() {
        let line = powershell_command_line("Write-Output 'hé'");
        let encoded = line.rsplit(' ').next().unwrap();
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .unwrap();
        let units: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        assert_eq!(String::from_utf16(&units).unwrap(), "Write-Output 'hé'");
        assert!(line.to_ascii_lowercase().contains("powershell.exe"));
    }

    #[test]
    fn environment_drops_credentials_and_points_temp_at_the_sandbox() {
        assert!(windows_environment_key("Path"));
        assert!(windows_environment_key("SystemRoot"));
        assert!(!windows_environment_key("GITHUB_TOKEN"));
        assert!(!windows_environment_key("OPENAI_API_KEY"));
        assert!(!windows_environment_key("USERPROFILE"));

        let environment = environment(Path::new(r"C:\sb\home"), Path::new(r"C:\sb"));
        let temp = environment
            .iter()
            .filter(|(key, _)| key.eq_ignore_ascii_case("TEMP"))
            .collect::<Vec<_>>();
        assert_eq!(temp, [&("TEMP".to_string(), r"C:\sb".to_string())]);
    }

    #[test]
    fn readonly_grants_inside_the_writable_workspace_are_dropped() {
        let mut filesystem = FilesystemPolicy {
            readwrite_paths: vec![r"C:\work".to_string()],
            readonly_paths: vec![
                r"C:\Work\tools".to_string(),
                r"C:\Tools".to_string(),
                r"c:\tools".to_string(),
            ],
            ..Default::default()
        };
        dedupe_granted(&mut filesystem);
        assert_eq!(filesystem.readonly_paths, [r"C:\Tools".to_string()]);
    }

    /// End to end on a host that supports MXC; skips elsewhere, per the
    /// repository's no-`#[ignore]` rule.
    #[tokio::test]
    async fn workspace_write_contains_writes_and_network() {
        if !available() {
            eprintln!("skipping: MXC process container unavailable on this Windows build");
            return;
        }
        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let outside_file = outside.path().join("escape.txt");
        let script = format!(
            "Set-Content -Path inside.txt -Value ok; \
             try {{ Set-Content -Path '{}' -Value no -ErrorAction Stop; 'outside-written' }} catch {{ 'outside-denied' }}; \
             try {{ (New-Object Net.Sockets.TcpClient).Connect('1.1.1.1', 443); 'net-open' }} catch {{ 'net-denied' }}",
            outside_file.display()
        );
        let workspace_path = std::fs::canonicalize(workspace.path()).unwrap();
        let mut child = crate::exec::sandbox::SandboxCommand::Windows(
            command(&workspace_path, &script, SandboxMode::WorkspaceWrite).unwrap(),
        )
        .spawn()
        .unwrap();
        let (mut output, _) = child.take_output();
        let mut stdout = String::new();
        tokio::io::AsyncReadExt::read_to_string(&mut output, &mut stdout)
            .await
            .unwrap();
        let code = child.wait().await.unwrap();
        assert_eq!(code, Some(0), "{stdout}");
        assert!(workspace_path.join("inside.txt").exists(), "{stdout}");
        assert!(!outside_file.exists(), "{stdout}");
        assert!(stdout.contains("outside-denied"), "{stdout}");
        assert!(stdout.contains("net-denied"), "{stdout}");
    }
}

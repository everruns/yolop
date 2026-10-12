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
//! - All network is denied, host loopback included. The attached `yolop`
//!   control endpoint is a named pipe whose ACL admits AppContainers, so
//!   `yolop` subcommands still reach the session from inside.
//! - Workspace `.git` stays writable, matching Linux rather than macOS.
//! - Scripts travel as `-EncodedCommand` so no PowerShell quoting is involved.
//! - Drive roots are never granted. MXC's tools policy adds `C:\` when `pwsh`
//!   is on `PATH`, which would expose the whole drive and, on the
//!   AppContainer+DACL tier, rewrite the ACL of every file on it per command.
//!   The shell is Windows PowerShell, which does not need the root.
//! - Win32k stays enabled (`ui.disable = false`): `powershell.exe` and most
//!   console tools load `user32.dll` and exit with STATUS_DLL_INIT_FAILED
//!   without it. Window creation is not a filesystem or network escape.

use std::io::Read;
use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{Context, Result};
use base64::Engine as _;
use mxc_sdk::v1::{
    self, ContainerRequest, FilesystemPolicy, MxcProcess, NetworkAction, NetworkEgressPolicy,
    NetworkIngressPolicy, NetworkPolicy, StreamCloser, UiPolicy,
    policy::filesystem::{
        ToolsPolicyContainerType, ToolsPolicyOptions, available_tools_policy, user_profile_policy,
    },
};
use tokio::io::AsyncWriteExt;

use crate::config::SandboxMode;
use crate::control::ControlEndpoint;
use crate::exec::sandbox::ChildOutput;

/// Whether this host can create a ProcessContainer. Probed once per process.
pub(crate) fn available() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(
        || matches!(v1::probe(None), Ok(probe) if probe.error.is_none() && probe.tier.is_some()),
    )
}

/// A contained PowerShell launch for `script` in `cwd`.
pub(crate) fn command(
    cwd: &Path,
    script: &str,
    mode: SandboxMode,
    control: Option<&ControlEndpoint>,
) -> Result<ContainedCommand> {
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
    for profile_dir in [["AppData", "Local"], ["AppData", "Roaming"]] {
        let path = profile_dir
            .iter()
            .fold(home.clone(), |path, part| path.join(part));
        std::fs::create_dir_all(&path)
            .with_context(|| format!("create sandbox profile directory: {}", path.display()))?;
    }
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
    filesystem.readonly_paths.extend(
        tools
            .readonly_paths
            .into_iter()
            .filter(|path| !is_drive_root(path)),
    );
    if let Some(control) = control {
        // The `yolop` shim and the executable it runs; the pipe itself is
        // admitted by its own ACL (`control::control_pipe`).
        filesystem
            .readonly_paths
            .push(path_string(control.bin_dir()));
        if let Some(exe_dir) = std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(Path::to_path_buf))
        {
            filesystem.readonly_paths.push(path_string(&exe_dir));
        }
    }
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
        ui: Some(UiPolicy {
            disable: false,
            ..Default::default()
        }),
        working_directory: Some(path_string(cwd)),
        environment: Some({
            let mut environment = environment(&home, &temp);
            if let Some(control) = control {
                environment.extend(
                    control
                        .child_environment()
                        .map(|(key, value)| (key.to_string(), value.to_string())),
                );
            }
            environment
        }),
        inherit_default_environment: Some(false),
        ..ContainerRequest::new(powershell_command_line(&contained_script(cwd, script)))
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

/// The script with a prelude that enters `cwd` explicitly. The container's
/// launch directory is not reliable (a contained process that cannot open it
/// is silently started at the drive root), and a relative write must never
/// land outside the workspace, so a failed `Set-Location` stops the script.
/// Progress records are silenced so they do not reach stderr as CLIXML.
fn contained_script(cwd: &Path, script: &str) -> String {
    let cwd = path_string(cwd).replace('\'', "''");
    format!(
        "$ProgressPreference = 'SilentlyContinue'\ntry {{ Set-Location -LiteralPath '{cwd}' -ErrorAction Stop }} catch {{ [Console]::Error.WriteLine(\"yolop sandbox: cannot enter the workspace: $_\"); exit 1 }}\n{script}"
    )
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
    let local_app_data = path_string(&home.join("AppData").join("Local"));
    let app_data = path_string(&home.join("AppData").join("Roaming"));
    let home = path_string(home);
    let temp = path_string(temp);
    // MXC refuses to create a container without LOCALAPPDATA. The profile
    // directories point inside the sandbox home so per-user caches stay
    // writable without exposing the real profile.
    for (key, value) in [
        ("HOME", &home),
        ("USERPROFILE", &home),
        ("LOCALAPPDATA", &local_app_data),
        ("APPDATA", &app_data),
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

/// The plain form of a path. `canonicalize` returns verbatim `\\?\C:\...`
/// paths, which PowerShell cannot use as its working directory, so a drive
/// path loses that prefix.
fn path_string(path: &Path) -> String {
    let path = path.to_string_lossy().into_owned();
    if let Some(rest) = path.strip_prefix(r"\\?\")
        && rest.as_bytes().get(1) == Some(&b':')
    {
        return rest.to_string();
    }
    path
}

/// `C:`, `C:\` or `C:/`.
fn is_drive_root(path: &str) -> bool {
    let trimmed = path.trim_end_matches(['\\', '/']);
    trimmed.len() == 2 && trimmed.ends_with(':')
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
        assert!(
            environment
                .iter()
                .any(|(key, value)| key == "LOCALAPPDATA" && value.starts_with(r"C:\sb\home"))
        );
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

    #[test]
    fn contained_script_enters_the_workspace_first() {
        let script = contained_script(Path::new(r"C:\it's"), "Get-ChildItem");
        assert_eq!(
            script,
            "$ProgressPreference = 'SilentlyContinue'\ntry { Set-Location -LiteralPath 'C:\\it''s' -ErrorAction Stop } catch { [Console]::Error.WriteLine(\"yolop sandbox: cannot enter the workspace: $_\"); exit 1 }\nGet-ChildItem"
        );
    }

    #[test]
    fn drive_roots_are_never_granted() {
        assert!(is_drive_root(r"C:\"));
        assert!(is_drive_root("D:"));
        assert!(is_drive_root("C:/"));
        assert!(!is_drive_root(r"C:\Tools"));
        assert!(!is_drive_root(r"\\server\share"));
    }

    #[test]
    fn verbatim_drive_paths_lose_their_prefix() {
        assert_eq!(path_string(Path::new(r"\\?\C:\work")), r"C:\work");
        assert_eq!(path_string(Path::new(r"C:\work")), r"C:\work");
        assert_eq!(
            path_string(Path::new(r"\\?\UNC\server\share")),
            r"\\?\UNC\server\share"
        );
    }

    /// The control pipe's ACL is what lets a contained shell reach the
    /// session, so open it from inside a real container.
    #[tokio::test]
    async fn workspace_write_container_can_open_the_control_pipe() {
        if !available() {
            eprintln!("skipping: MXC process container unavailable on this Windows build");
            return;
        }
        let name = format!(r"\\.\pipe\yolop-sandbox-test-{}", std::process::id());
        let server = crate::control::control_pipe::create(&name, true).unwrap();
        let accept = tokio::spawn(async move {
            server.connect().await.unwrap();
            let mut line = String::new();
            let mut reader = tokio::io::BufReader::new(server);
            tokio::io::AsyncBufReadExt::read_line(&mut reader, &mut line)
                .await
                .unwrap();
            line
        });
        let pipe = name.trim_start_matches(r"\\.\pipe\");
        let script = format!(
            "$pipe = New-Object IO.Pipes.NamedPipeClientStream('.', '{pipe}', [IO.Pipes.PipeDirection]::InOut); \
             $pipe.Connect(10000); $writer = New-Object IO.StreamWriter($pipe); \
             $writer.WriteLine('hello-from-container'); $writer.Flush(); 'pipe-connected'"
        );
        let workspace = tempfile::tempdir().unwrap();
        let workspace_path = std::fs::canonicalize(workspace.path()).unwrap();
        let mut child = crate::exec::sandbox::SandboxCommand::Windows(
            command(&workspace_path, &script, SandboxMode::WorkspaceWrite, None).unwrap(),
        )
        .spawn()
        .unwrap();
        let (mut output, mut errors) = child.take_output();
        let mut stdout = String::new();
        let mut stderr = String::new();
        tokio::io::AsyncReadExt::read_to_string(&mut output, &mut stdout)
            .await
            .unwrap();
        tokio::io::AsyncReadExt::read_to_string(&mut errors, &mut stderr)
            .await
            .unwrap();
        let code = child.wait().await.unwrap();
        assert_eq!(code, Some(0), "stdout={stdout} stderr={stderr}");
        assert!(stdout.contains("pipe-connected"), "{stdout} {stderr}");
        let line = tokio::time::timeout(Duration::from_secs(10), accept)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(line.trim(), "hello-from-container");
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
        let started = std::time::Instant::now();
        let prepared =
            command(&workspace_path, &script, SandboxMode::WorkspaceWrite, None).unwrap();
        eprintln!("sandbox policy prepared in {:?}", started.elapsed());
        if let Ok(probe) = v1::probe(None) {
            eprintln!("sandbox tier: {:?}", probe.tier);
        }
        if let Some(filesystem) = &prepared.request.filesystem {
            eprintln!("sandbox read-only grants: {:?}", filesystem.readonly_paths);
            eprintln!(
                "sandbox read-write grants: {:?}",
                filesystem.readwrite_paths
            );
        }
        let started = std::time::Instant::now();
        let mut child = crate::exec::sandbox::SandboxCommand::Windows(prepared)
            .spawn()
            .unwrap();
        eprintln!("sandbox container started in {:?}", started.elapsed());
        let (mut output, mut errors) = child.take_output();
        let mut stdout = String::new();
        let mut stderr = String::new();
        tokio::io::AsyncReadExt::read_to_string(&mut output, &mut stdout)
            .await
            .unwrap();
        tokio::io::AsyncReadExt::read_to_string(&mut errors, &mut stderr)
            .await
            .unwrap();
        let code = child.wait().await.unwrap();
        assert_eq!(code, Some(0), "stdout={stdout} stderr={stderr}");
        assert!(
            workspace_path.join("inside.txt").exists(),
            "stdout={stdout} stderr={stderr}"
        );
        assert!(!outside_file.exists(), "{stdout}");
        assert!(stdout.contains("outside-denied"), "{stdout}");
        assert!(stdout.contains("net-denied"), "{stdout}");
    }

    /// Temporary: what the contained token can reach on this host's tier.
    #[tokio::test]
    async fn container_access_diagnostics() {
        if !available() {
            return;
        }
        if let Ok(probe) = v1::probe(None) {
            eprintln!("diag tier: {:?} warnings: {:?}", probe.tier, probe.warnings);
        }
        eprintln!(
            "diag bfscfg.exe present: {}",
            Path::new(r"C:\Windows\System32\bfscfg.exe").exists()
        );
        let workspace = tempfile::tempdir().unwrap();
        let ws = path_string(workspace.path());
        let script = format!(
            "$ProgressPreference = 'SilentlyContinue'\n\
             'cwd=' + [Environment]::CurrentDirectory\n\
             try {{ [IO.Directory]::GetFiles('{ws}').Count | Out-Null; 'ws-list-ok' }} catch {{ \"ws-list: $($_.Exception.Message)\" }}\n\
             try {{ [IO.File]::WriteAllText('{ws}\\abs.txt', 'x'); 'ws-write-ok' }} catch {{ \"ws-write: $($_.Exception.Message)\" }}\n\
             try {{ [IO.File]::WriteAllText(\"$env:TEMP\\t.txt\", 'x'); 'temp-write-ok' }} catch {{ \"temp-write: $($_.Exception.Message)\" }}\n\
             try {{ [IO.Directory]::SetCurrentDirectory('{ws}'); 'setcwd-ok' }} catch {{ \"setcwd: $($_.Exception.Message)\" }}\n\
             try {{ Set-Location -LiteralPath '{ws}' -ErrorAction Stop; 'setloc-ok' }} catch {{ \"setloc: $($_.Exception.Message)\" }}\n\
             try {{ Get-Item -LiteralPath 'C:\\Users' -ErrorAction Stop | Out-Null; 'users-ok' }} catch {{ \"users: $($_.Exception.Message)\" }}\n\
             whoami /groups | Select-String 'Label|APPLICATION'"
        );
        let mut prepared =
            command(workspace.path(), "", SandboxMode::WorkspaceWrite, None).unwrap();
        prepared.request.command = powershell_command_line(&script);
        let mut child = crate::exec::sandbox::SandboxCommand::Windows(prepared)
            .spawn()
            .unwrap();
        let (mut output, mut errors) = child.take_output();
        let mut stdout = String::new();
        let mut stderr = String::new();
        tokio::io::AsyncReadExt::read_to_string(&mut output, &mut stdout)
            .await
            .unwrap();
        tokio::io::AsyncReadExt::read_to_string(&mut errors, &mut stderr)
            .await
            .unwrap();
        let code = child.wait().await.unwrap();
        eprintln!("diag code={code:?}\ndiag stdout:\n{stdout}\ndiag stderr:\n{stderr}");
    }
}

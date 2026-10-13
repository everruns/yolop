//! Pluggable containment for arbitrary child-process execution.
//!
//! Structured filesystem tools remain in the trusted host broker. Every shell
//! entry point receives one of these providers, so foreground, background and
//! interactive commands share the same kernel boundary.

use crate::config::SandboxMode;
use crate::control::ControlEndpoint;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

use crate::capabilities::skills::global_skills_dir;
use std::process::Stdio;
use tokio::process::Command;

pub(crate) trait SandboxProvider: Send + Sync {
    fn mode(&self) -> SandboxMode;
    /// `control` is the attached `yolop` endpoint for this execution, when one
    /// exists; a contained provider admits exactly that endpoint and nothing
    /// else on the network.
    fn command(
        &self,
        cwd: &Path,
        script: &str,
        control: Option<&ControlEndpoint>,
    ) -> Result<SandboxCommand>;
}

/// A shell launch prepared by a provider.
///
/// macOS and Linux contain an ordinary child process, so they hand back a
/// `tokio` command. Windows containment is created in-process by the MXC
/// library, which owns process creation, so it hands back its own launch.
pub(crate) enum SandboxCommand {
    Host(Command),
    #[cfg(windows)]
    Windows(crate::exec::sandbox_windows::ContainedCommand),
}

impl SandboxCommand {
    /// The underlying host command, when the provider launches one.
    pub(crate) fn host_mut(&mut self) -> Option<&mut Command> {
        match self {
            Self::Host(command) => Some(command),
            #[cfg(windows)]
            Self::Windows(_) => None,
        }
    }

    /// Start the command with piped stdout and stderr and no stdin.
    pub(crate) fn spawn(self) -> std::io::Result<SandboxChild> {
        match self {
            Self::Host(mut command) => {
                configure_stdio(&mut command);
                #[cfg(unix)]
                command.process_group(0);
                let mut child = command.spawn()?;
                let stdout = child.stdout.take().expect("stdout is piped");
                let stderr = child.stderr.take().expect("stderr is piped");
                Ok(SandboxChild {
                    stdout: Box::new(stdout),
                    stderr: Box::new(stderr),
                    process: ChildProcess::Host(child),
                })
            }
            #[cfg(windows)]
            Self::Windows(command) => {
                let (child, stdout, stderr) = command.spawn()?;
                Ok(SandboxChild {
                    stdout,
                    stderr,
                    process: ChildProcess::Windows(child),
                })
            }
        }
    }
}

pub(crate) type ChildOutput = Box<dyn tokio::io::AsyncRead + Unpin + Send>;

/// A running shell command, whichever provider started it.
pub(crate) struct SandboxChild {
    stdout: ChildOutput,
    stderr: ChildOutput,
    process: ChildProcess,
}

enum ChildProcess {
    Host(tokio::process::Child),
    #[cfg(windows)]
    Windows(crate::exec::sandbox_windows::ContainedChild),
}

impl SandboxChild {
    /// Take stdout and stderr, leaving empty readers behind.
    pub(crate) fn take_output(&mut self) -> (ChildOutput, ChildOutput) {
        (
            std::mem::replace(&mut self.stdout, Box::new(tokio::io::empty())),
            std::mem::replace(&mut self.stderr, Box::new(tokio::io::empty())),
        )
    }

    pub(crate) fn id(&self) -> Option<u32> {
        match &self.process {
            ChildProcess::Host(child) => child.id(),
            #[cfg(windows)]
            ChildProcess::Windows(child) => Some(child.id()),
        }
    }

    pub(crate) fn start_kill(&mut self) -> std::io::Result<()> {
        match &mut self.process {
            ChildProcess::Host(child) => child.start_kill(),
            #[cfg(windows)]
            ChildProcess::Windows(child) => child.kill(),
        }
    }

    /// Wait for exit. `None` when the process ended without an exit code.
    pub(crate) async fn wait(&mut self) -> std::io::Result<Option<i32>> {
        match &mut self.process {
            ChildProcess::Host(child) => child.wait().await.map(|status| status.code()),
            #[cfg(windows)]
            ChildProcess::Windows(child) => child.wait().await.map(Some),
        }
    }
}

pub(crate) fn provider(mode: SandboxMode) -> std::sync::Arc<dyn SandboxProvider> {
    match mode {
        SandboxMode::ReadOnly | SandboxMode::WorkspaceWrite => {
            std::sync::Arc::new(NativeSandbox { mode })
        }
        SandboxMode::DangerFullAccess => std::sync::Arc::new(UnsafeHost),
    }
}

pub(crate) fn danger_warning(mode: SandboxMode) -> Option<&'static str> {
    #[cfg(windows)]
    {
        if mode == SandboxMode::DangerFullAccess {
            Some(
                "DANGER: danger-full-access (UNSAFE HOST) — shell commands can access and modify files, processes, and the network outside the workspace",
            )
        } else if !crate::exec::sandbox_windows::available() {
            Some(
                "WARNING: the Windows sandbox is unavailable on this Windows build — shell commands will be refused. Windows 11 24H2 or later is required; set `sandbox_mode = \"danger-full-access\"` to run commands uncontained",
            )
        } else if crate::exec::sandbox_windows::rewrites_host_permissions() {
            Some(
                "NOTE: this Windows build only offers MXC's compatibility sandbox, which re-permissions the granted folders for each command, so shell commands start slowly",
            )
        } else {
            None
        }
    }
    #[cfg(not(windows))]
    {
        (mode == SandboxMode::DangerFullAccess).then_some(
            "DANGER: danger-full-access (UNSAFE HOST) — shell commands can access and modify files, processes, and the network outside the workspace",
        )
    }
}

/// Network availability of the shell process the selected provider launches.
pub(crate) fn network_access(mode: SandboxMode) -> &'static str {
    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    {
        if mode == SandboxMode::DangerFullAccess {
            "enabled"
        } else {
            "disabled"
        }
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
    {
        if mode == SandboxMode::DangerFullAccess {
            "enabled"
        } else {
            "unavailable"
        }
    }
}

/// Base shell invocation for the current platform. This is the single place the
/// host shell is chosen, so the sandbox providers stay platform-agnostic. Unix
/// wraps the script in a bash login shell (`bash -lc`); Windows runs it through
/// PowerShell.
fn shell_command(cwd: &Path, script: &str) -> Command {
    #[cfg(windows)]
    {
        // `powershell.exe` (Windows PowerShell 5.1) ships in-box on every
        // supported Windows, so it needs no install step. `-Command` runs
        // inline script text and is exempt from the script-file execution
        // policy. If argument quoting ever bites, `-EncodedCommand` (base64
        // UTF-16LE) is the hardening path.
        let mut command = Command::new("powershell.exe");
        command
            .arg("-NoProfile")
            .arg("-NonInteractive")
            .arg("-Command")
            .arg(script)
            .current_dir(cwd);
        command
    }
    #[cfg(not(windows))]
    {
        let mut command = Command::new("bash");
        command.arg("-lc").arg(script).current_dir(cwd);
        command
    }
}

struct UnsafeHost;

impl SandboxProvider for UnsafeHost {
    fn mode(&self) -> SandboxMode {
        SandboxMode::DangerFullAccess
    }

    fn command(
        &self,
        cwd: &Path,
        script: &str,
        _control: Option<&ControlEndpoint>,
    ) -> Result<SandboxCommand> {
        Ok(SandboxCommand::Host(shell_command(cwd, script)))
    }
}

struct NativeSandbox {
    mode: SandboxMode,
}

impl SandboxProvider for NativeSandbox {
    fn mode(&self) -> SandboxMode {
        self.mode
    }

    fn command(
        &self,
        cwd: &Path,
        script: &str,
        control: Option<&ControlEndpoint>,
    ) -> Result<SandboxCommand> {
        #[cfg(windows)]
        {
            crate::exec::sandbox_windows::command(cwd, script, self.mode, control)
                .map(SandboxCommand::Windows)
        }
        #[cfg(not(windows))]
        {
            native_command(cwd, script, self.mode, control).map(SandboxCommand::Host)
        }
    }
}

#[cfg(target_os = "macos")]
fn native_command(
    cwd: &Path,
    script: &str,
    mode: SandboxMode,
    control: Option<&ControlEndpoint>,
) -> Result<Command> {
    let executable = Path::new("/usr/bin/sandbox-exec");
    if !executable.is_file() {
        anyhow::bail!(
            "native sandbox unavailable: /usr/bin/sandbox-exec is missing; refusing to run unsandboxed. Set `sandbox_mode = \"danger-full-access\"` only inside an already isolated environment"
        )
    }

    // Seatbelt denies network and all writes by default. Reads stay available
    // so compilers, SDKs, package caches and system skills continue to work;
    // the active workspace, private temp, and conventional shared /tmp are
    // writable for tool compatibility. /dev/null is the sole writable device,
    // matching Codex's default Seatbelt policy. The explicit /private/tmp spelling
    // covers macOS path canonicalization through the /tmp symlink.
    let temp = sandbox_temp_dir()?;
    let home = sandbox_home_dir(&temp)?;
    if mode == SandboxMode::WorkspaceWrite {
        prepared_global_skills_dir()?;
    }
    let control_socket = control.and_then(ControlEndpoint::socket_path);
    let profile = macos_profile(cwd, &temp, mode, control_socket);
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

/// Landlock leaves Unix-socket connects alone and the seccomp filter blocks
/// internet and packet socket creation and io_uring, so the control socket needs no
/// grant here.
#[cfg(target_os = "linux")]
fn native_command(
    cwd: &Path,
    script: &str,
    mode: SandboxMode,
    _control: Option<&ControlEndpoint>,
) -> Result<Command> {
    let temp = sandbox_temp_dir()?;
    let home = sandbox_home_dir(&temp)?;
    let executable =
        crate::exec::self_exe::reexec_path().context("resolve yolop sandbox worker executable")?;
    let mut command = Command::new(executable);
    command
        .arg("__sandbox-exec")
        .arg("--cwd")
        .arg(cwd)
        .arg("--temp")
        .arg(&temp)
        .arg("--mode")
        .arg(mode.as_str())
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
pub(crate) fn run_linux_worker(
    cwd: &Path,
    temp: &Path,
    mode: SandboxMode,
    script: &str,
) -> Result<()> {
    use landlock::{
        ABI, Access, AccessFs, PathBeneath, PathFd, Ruleset, RulesetAttr, RulesetCreatedAttr,
        RulesetStatus,
    };
    use std::os::unix::process::CommandExt;

    if mode == SandboxMode::DangerFullAccess {
        anyhow::bail!("the sandbox worker cannot run danger-full-access");
    }

    std::env::set_current_dir(cwd)
        .with_context(|| format!("enter sandbox workspace: {}", cwd.display()))?;

    // ABI V3 is the oldest policy that also mediates truncate(2); requiring
    // full enforcement avoids silently weakening the write boundary.
    let abi = ABI::V3;
    let ruleset = Ruleset::default()
        .handle_access(AccessFs::from_all(abi))?
        .create()?
        .add_rule(PathBeneath::new(
            PathFd::new("/")?,
            AccessFs::from_read(abi),
        ))?
        .add_rule(PathBeneath::new(
            PathFd::new(temp)?,
            AccessFs::from_all(abi),
        ))?;
    let ruleset = if mode == SandboxMode::WorkspaceWrite {
        let ruleset = ruleset
            .add_rule(PathBeneath::new(PathFd::new(cwd)?, AccessFs::from_all(abi)))?
            .add_rule(PathBeneath::new(
                PathFd::new("/tmp")?,
                AccessFs::from_all(abi),
            ))?;
        if let Some(skills) = prepared_global_skills_dir()? {
            ruleset.add_rule(PathBeneath::new(
                PathFd::new(skills)?,
                AccessFs::from_all(abi),
            ))?
        } else {
            ruleset
        }
    } else {
        ruleset
    };
    let status = ruleset.restrict_self()?;
    if status.ruleset != RulesetStatus::FullyEnforced || !status.no_new_privs {
        anyhow::bail!(
            "native sandbox unavailable: Landlock ABI v3 is not fully enforced by this Linux kernel; refusing to run unsandboxed. Set `sandbox_mode = \"danger-full-access\"` only inside an already isolated environment"
        );
    }

    let filter = linux_network_filter()?;
    seccompiler::apply_filter(&filter)?;

    Err(std::process::Command::new("/bin/bash")
        .arg("-lc")
        .arg(script)
        .exec()
        .into())
}

#[cfg(target_os = "linux")]
fn linux_network_filter() -> std::result::Result<seccompiler::BpfProgram, seccompiler::BackendError>
{
    use seccompiler::{
        SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter, SeccompRule,
    };
    use std::convert::TryInto;
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
    SeccompFilter::new(
        [
            (libc::SYS_socket, socket_rules),
            // TM-BASH: asynchronous operations are not mediated by SYS_socket.
            // Deny setup, submission and registration, including inherited rings.
            (libc::SYS_io_uring_setup, vec![]),
            (libc::SYS_io_uring_enter, vec![]),
            (libc::SYS_io_uring_register, vec![]),
        ]
        .into_iter()
        .collect(),
        SeccompAction::Allow,
        SeccompAction::Errno(libc::EACCES as u32),
        std::env::consts::ARCH.try_into()?,
    )?
    .try_into()
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
fn native_command(
    _cwd: &Path,
    _script: &str,
    _mode: SandboxMode,
    _control: Option<&ControlEndpoint>,
) -> Result<Command> {
    anyhow::bail!(
        "native sandbox is supported only on macOS and Linux; refusing to run unsandboxed. Set `sandbox_mode = \"danger-full-access\"` only inside an already isolated environment"
    )
}

pub(super) fn prepared_global_skills_dir() -> Result<Option<PathBuf>> {
    let Some(path) = global_skills_dir() else {
        return Ok(None);
    };
    std::fs::create_dir_all(&path)
        .with_context(|| format!("create global skills directory: {}", path.display()))?;
    Ok(Some(std::fs::canonicalize(&path).with_context(|| {
        format!("canonicalize global skills directory: {}", path.display())
    })?))
}

pub(super) fn sandbox_temp_dir() -> Result<PathBuf> {
    let path = std::env::temp_dir().join(format!("yolop-sandbox-{}", std::process::id()));
    prepare_sandbox_temp(&path)?;
    std::fs::canonicalize(&path)
        .with_context(|| format!("canonicalize sandbox temp directory: {}", path.display()))
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

pub(super) fn sandbox_home_dir(temp: &Path) -> Result<PathBuf> {
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

pub(super) fn safe_environment_key(key: &str) -> bool {
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
fn macos_profile(
    cwd: &Path,
    temp: &Path,
    mode: SandboxMode,
    control_socket: Option<&Path>,
) -> String {
    let workspace_write = if mode == SandboxMode::WorkspaceWrite {
        format!(" (subpath \"{}\")", seatbelt_escape(cwd))
    } else {
        String::new()
    };
    let shared_temp_write = if mode == SandboxMode::WorkspaceWrite {
        " (subpath \"/tmp\") (subpath \"/private/tmp\")"
    } else {
        ""
    };
    let global_skills_write = if mode == SandboxMode::WorkspaceWrite {
        global_skills_dir()
            .map(|path| format!(" (subpath \"{}\")", seatbelt_escape(&path)))
            .unwrap_or_default()
    } else {
        String::new()
    };
    // The one network exception: connecting to this execution's attached
    // `yolop` control socket. Listed under both spellings because /tmp is a
    // symlink to /private/tmp.
    let control_connect = control_socket
        .map(|socket| {
            let literal = socket.to_string_lossy();
            let private = literal
                .strip_prefix("/tmp/")
                .map(|rest| format!("/private/tmp/{rest}"))
                .unwrap_or_else(|| literal.to_string());
            format!(
                "\n(allow network-outbound (remote unix-socket (path-literal \"{}\")) (remote unix-socket (path-literal \"{}\")))",
                seatbelt_escape(socket),
                seatbelt_escape(Path::new(&private)),
            )
        })
        .unwrap_or_default();
    format!(
        "(version 1)\n(deny default)\n(allow process*)\n(allow file-read*)\n(allow sysctl-read)\n(allow mach-lookup)\n(allow file-write-data (require-all (path \"/dev/null\") (vnode-type CHARACTER-DEVICE)))\n(allow file-write*{} (subpath \"{}\"){}{})\n(deny network*){}\n(deny file-write* (literal \"{}\") (subpath \"{}\"))",
        workspace_write,
        seatbelt_escape(temp),
        shared_temp_write,
        global_skills_write,
        control_connect,
        seatbelt_escape(&cwd.join(".git")),
        seatbelt_escape(&cwd.join(".git")),
    )
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

    // Interpret only the instructions emitted by this policy on inert data.
    // No filter is installed and no syscall is submitted by these tests.
    #[cfg(target_os = "linux")]
    fn linux_policy_action(syscall: i64, first_arg: u64, valid_arch: bool) -> u32 {
        let arch = match std::env::consts::ARCH {
            "x86_64" => 0xc000003e,
            "aarch64" => 0xc00000b7,
            "riscv64" => 0xc00000f3,
            other => panic!("unsupported seccomp architecture: {other}"),
        };
        let mut data = [0_u32; 16];
        data[0] = syscall as u32;
        data[1] = if valid_arch { arch } else { 0 };
        data[4] = first_arg as u32;
        data[5] = (first_arg >> 32) as u32;
        let program = linux_network_filter().unwrap();
        let mut accumulator = 0;
        let mut pc = 0;
        for _ in 0..program.len() {
            let instruction = &program[pc];
            pc += 1;
            match instruction.code {
                0x20 => accumulator = data[instruction.k as usize / 4],
                0x15 => {
                    pc += if accumulator == instruction.k {
                        instruction.jt
                    } else {
                        instruction.jf
                    } as usize;
                }
                0x05 => pc += instruction.k as usize,
                0x06 => return instruction.k,
                other => panic!("unexpected policy instruction: {other:#x}"),
            }
        }
        panic!("policy did not return an action");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_network_policy_denies_async_syscall_numbers() {
        for syscall in [
            libc::SYS_io_uring_setup,
            libc::SYS_io_uring_enter,
            libc::SYS_io_uring_register,
        ] {
            for argument in [0, u64::MAX] {
                assert_eq!(
                    linux_policy_action(syscall, argument, true),
                    libc::SECCOMP_RET_ERRNO | libc::EACCES as u32,
                    "syscall {syscall}, argument {argument}"
                );
            }
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_network_policy_preserves_direct_socket_and_normal_syscall_rules() {
        for family in [libc::AF_INET, libc::AF_INET6, libc::AF_PACKET] {
            assert_eq!(
                linux_policy_action(libc::SYS_socket, family as u64, true),
                libc::SECCOMP_RET_ERRNO | libc::EACCES as u32,
            );
        }
        assert_eq!(
            linux_policy_action(libc::SYS_socket, libc::AF_UNIX as u64, true),
            libc::SECCOMP_RET_ALLOW,
        );
        assert_eq!(
            linux_policy_action(libc::SYS_getpid, 0, true),
            libc::SECCOMP_RET_ALLOW,
        );
        assert_eq!(
            linux_policy_action(libc::SYS_getpid, 0, false),
            libc::SECCOMP_RET_KILL_PROCESS,
        );
    }

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

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_policy_allows_only_dev_null_device_writes() {
        let profile = macos_profile(
            Path::new("/workspace"),
            Path::new("/private/tmp/sandbox"),
            SandboxMode::WorkspaceWrite,
            None,
        );

        assert!(profile.contains(
            r#"(allow file-write-data (require-all (path "/dev/null") (vnode-type CHARACTER-DEVICE)))"#
        ));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_workspace_write_policy_allows_global_skills_only_when_writable() {
        let skills = global_skills_dir().expect("home directory should resolve");
        let writable = macos_profile(
            Path::new("/workspace"),
            Path::new("/private/tmp/sandbox"),
            SandboxMode::WorkspaceWrite,
            None,
        );
        let read_only = macos_profile(
            Path::new("/workspace"),
            Path::new("/private/tmp/sandbox"),
            SandboxMode::ReadOnly,
            None,
        );
        let rule = format!(r#"(subpath "{}")"#, seatbelt_escape(&skills));

        assert!(writable.contains(&rule));
        assert!(!read_only.contains(&rule));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_sandbox_temp_is_canonical_and_allowed_by_the_policy() {
        let temp = sandbox_temp_dir().expect("sandbox temp should be created");
        assert_eq!(
            temp,
            std::fs::canonicalize(&temp).expect("sandbox temp should remain canonical")
        );

        let profile = macos_profile(
            Path::new("/workspace"),
            &temp,
            SandboxMode::WorkspaceWrite,
            None,
        );
        assert!(profile.contains(&format!(r#"(subpath "{}")"#, temp.display())));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_sandbox_admits_only_the_attached_control_socket() {
        let socket = Path::new("/tmp/yolop-control-1-abc/control.sock");
        let profile = macos_profile(
            Path::new("/workspace"),
            Path::new("/private/tmp/sandbox"),
            SandboxMode::WorkspaceWrite,
            Some(socket),
        );
        assert!(profile.contains(
            r#"(allow network-outbound (remote unix-socket (path-literal "/tmp/yolop-control-1-abc/control.sock")) (remote unix-socket (path-literal "/private/tmp/yolop-control-1-abc/control.sock")))"#
        ));
        assert!(profile.contains("(deny network*)"));
        let without = macos_profile(
            Path::new("/workspace"),
            Path::new("/private/tmp/sandbox"),
            SandboxMode::WorkspaceWrite,
            None,
        );
        assert!(!without.contains("network-outbound"));
    }

    #[cfg(not(windows))]
    #[test]
    fn native_is_the_safe_default_provider() {
        assert!(danger_warning(SandboxMode::WorkspaceWrite).is_none());
        assert!(
            danger_warning(SandboxMode::DangerFullAccess)
                .unwrap()
                .contains("UNSAFE HOST")
        );
    }

    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    #[test]
    fn network_access_matches_native_and_unsafe_execution() {
        assert_eq!(network_access(SandboxMode::ReadOnly), "disabled");
        assert_eq!(network_access(SandboxMode::WorkspaceWrite), "disabled");
        assert_eq!(network_access(SandboxMode::DangerFullAccess), "enabled");
    }

    #[cfg(windows)]
    #[test]
    fn windows_warns_only_when_commands_are_not_contained() {
        assert!(
            danger_warning(SandboxMode::DangerFullAccess)
                .unwrap()
                .contains("UNSAFE HOST")
        );
        for mode in [SandboxMode::ReadOnly, SandboxMode::WorkspaceWrite] {
            assert_eq!(
                danger_warning(mode).is_some(),
                !crate::exec::sandbox_windows::available()
                    || crate::exec::sandbox_windows::rewrites_host_permissions()
            );
        }
    }

    #[test]
    fn shell_command_uses_the_platform_shell() {
        let command = shell_command(Path::new("."), "echo hi");
        let program = command.as_std().get_program().to_string_lossy();
        #[cfg(windows)]
        assert!(
            program.to_ascii_lowercase().contains("powershell"),
            "expected PowerShell on Windows, got {program}"
        );
        #[cfg(not(windows))]
        assert_eq!(program, "bash");
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

//! Attached CLI control plane.
//!
//! This is intentionally separate from YEP: YEP is the host↔extension data
//! plane, while this module lets a Yolop CLI invoked anywhere below a session's
//! shell ask that session to mutate live state. A short-lived authenticated
//! local endpoint belongs to one shell execution. The child sends raw argv,
//! then the host's exact executable parses it into the typed capability request.

use async_trait::async_trait;
use clap::{ArgMatches, Command};
use everruns_core::Capability;
use everruns_core::ToolExecutionResult;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::ffi::OsString;
use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};

pub const CONTROL_VERSION: u32 = 1;
const MAX_CONTROL_FRAME_BYTES: usize = 64 * 1024;
const CONTROL_TIMEOUT: Duration = Duration::from_secs(120);
/// Cap for output relayed from a child that ran as an ordinary CLI (help,
/// version, usage errors) rather than speaking the protocol.
const MAX_RELAY_BYTES: usize = 256 * 1024;
const CONTROL_ENDPOINT_ENV: &str = "YOLOP_CONTROL_ENDPOINT";
const CONTROL_TOKEN_ENV: &str = "YOLOP_CONTROL_TOKEN";
const CONTROL_ROUTES_ENV: &str = "YOLOP_CONTROL_ROUTES";

#[derive(Debug, Serialize, Deserialize)]
struct EndpointRequest {
    version: u32,
    token: String,
    client_version: String,
    argv: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct EndpointResponse {
    version: u32,
    host_version: String,
    stdout: String,
    stderr: String,
    exit_code: i32,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ControlRequest {
    pub version: u32,
    pub resource: String,
    pub action: Value,
}

impl ControlRequest {
    pub fn new(resource: impl Into<String>, action: impl Serialize) -> serde_json::Result<Self> {
        Ok(Self {
            version: CONTROL_VERSION,
            resource: resource.into(),
            action: serde_json::to_value(action)?,
        })
    }
}

/// A capability-owned control route exposed through a direct `yolop` CLI
/// invocation. Read-only operations may cross a contained Bash boundary
/// without approval; every other operation is treated as consequential.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ControlRoute {
    pub resource: &'static str,
    pub cli_subcommand: &'static str,
    pub read_only_operations: &'static [&'static str],
    /// One clause naming what the subcommand administers, rendered into the
    /// single shared `yolop` prompt block. Capabilities contribute this datum
    /// only: the framing, the attachment rules, and the discovery pointer are
    /// written once in `YolopCapability` (`src/capabilities/yolop.rs`), so a
    /// new CLI route adds a line rather than a prompt section of its own.
    pub summary: &'static str,
}

/// Optional control-plane facet for an ordinary runtime capability.
///
/// Keeping this as an adjunct to `Capability` lets MCP, skills, configuration,
/// or another administrative domain opt in without adding model tools or a
/// resource-specific branch to the transport.
#[async_trait]
pub trait ControlCapability: Capability {
    fn control_route(&self) -> ControlRoute;
    async fn execute_control(&self, action: &Value) -> ToolExecutionResult;
    fn render_control(&self, action: &Value, response: &ControlResponse) -> String;
}

/// Capability facet for contributing a top-level Yolop CLI command.
///
/// The command definition and its conversion into the typed control envelope
/// live together. The root binary only assembles registered contributions; it
/// has no resource-specific command variant or dispatch branch.
#[async_trait]
pub trait CliCapability: ControlCapability {
    fn cli_command(&self) -> Command;
    fn control_request_from_cli(&self, matches: &ArgMatches) -> anyhow::Result<ControlRequest>;
    async fn execute_cli(&self, request: &ControlRequest) -> anyhow::Result<()>;
}

pub struct CliInvocation {
    capability: Arc<dyn CliCapability>,
    pub request: ControlRequest,
}

impl CliInvocation {
    pub async fn execute(self) -> anyhow::Result<()> {
        self.capability.execute_cli(&self.request).await
    }
}

#[derive(Default)]
pub struct CliRegistry {
    capabilities: HashMap<String, Arc<dyn CliCapability>>,
}

impl CliRegistry {
    pub fn register<C>(&mut self, capability: Arc<C>) -> anyhow::Result<()>
    where
        C: CliCapability + 'static,
    {
        let name = capability.cli_command().get_name().to_string();
        let route = capability.control_route();
        if name != route.cli_subcommand {
            anyhow::bail!(
                "CLI command `{name}` does not match control route `{}`",
                route.cli_subcommand
            );
        }
        if self.capabilities.contains_key(&name) {
            anyhow::bail!("duplicate contributed CLI command `{name}`");
        }
        self.capabilities.insert(name, capability);
        Ok(())
    }

    pub fn augment(&self, mut root: Command) -> anyhow::Result<Command> {
        let mut names = self.capabilities.keys().collect::<Vec<_>>();
        names.sort();
        for name in names {
            if root
                .get_subcommands()
                .any(|command| command.get_name() == name)
            {
                anyhow::bail!("contributed CLI command `{name}` shadows a built-in command");
            }
            root = root.subcommand(self.capabilities[name].cli_command());
        }
        Ok(root)
    }

    pub fn invocation(&self, matches: &ArgMatches) -> anyhow::Result<Option<CliInvocation>> {
        let Some((name, submatches)) = matches.subcommand() else {
            return Ok(None);
        };
        let Some(capability) = self.capabilities.get(name) else {
            return Ok(None);
        };
        Ok(Some(CliInvocation {
            request: capability.control_request_from_cli(submatches)?,
            capability: capability.clone(),
        }))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ControlResponse {
    pub version: u32,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl ControlResponse {
    pub fn success(value: Value) -> Self {
        Self {
            version: CONTROL_VERSION,
            ok: true,
            value: Some(value),
            error: None,
        }
    }

    pub fn error(error: impl Into<String>) -> Self {
        Self {
            version: CONTROL_VERSION,
            ok: false,
            value: None,
            error: Some(error.into()),
        }
    }

    pub fn from_tool_result(result: ToolExecutionResult) -> Self {
        match result {
            ToolExecutionResult::Success(value)
            | ToolExecutionResult::SuccessWithImages { result: value, .. } => Self::success(value),
            ToolExecutionResult::ToolError(error) => Self::error(error),
            ToolExecutionResult::InternalError(_) => {
                Self::error("administration failed internally")
            }
            ToolExecutionResult::ConnectionRequired { provider, .. } => {
                Self::error(format!("connection `{provider}` is required"))
            }
        }
    }

    pub fn render_default(&self) -> String {
        if !self.ok {
            return self
                .error
                .clone()
                .unwrap_or_else(|| "control request failed".to_string());
        }
        let value = self.value.as_ref().unwrap_or(&Value::Null);
        serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
    }
}

#[async_trait]
pub trait ControlService: Send + Sync {
    /// Every registered route, ordered by subcommand so the derived prompt
    /// block is stable across sessions (and stays cacheable).
    fn routes(&self) -> Vec<ControlRoute>;
    async fn execute(&self, request: ControlRequest) -> ControlResponse;
    fn render(&self, request: &ControlRequest, response: &ControlResponse) -> String;
    fn is_read_only(&self, request: &ControlRequest) -> bool;
}

#[derive(Default)]
pub struct ControlRegistry {
    resources: RwLock<HashMap<String, Arc<dyn ControlCapability>>>,
    routes: RwLock<HashMap<String, String>>,
}

impl ControlRegistry {
    pub fn register<C>(&self, capability: Arc<C>) -> anyhow::Result<()>
    where
        C: ControlCapability + 'static,
    {
        let route = capability.control_route();
        let mut resources = self.resources.write().expect("control resources lock");
        let mut routes = self.routes.write().expect("control routes lock");
        if resources.contains_key(route.resource) {
            anyhow::bail!("duplicate control resource `{}`", route.resource);
        }
        if routes.contains_key(route.cli_subcommand) {
            anyhow::bail!("duplicate control CLI route `{}`", route.cli_subcommand);
        }
        routes.insert(route.cli_subcommand.to_string(), route.resource.to_string());
        resources.insert(route.resource.to_string(), capability);
        Ok(())
    }

    /// Replace a temporary route owner with the complete capability that uses
    /// the same resource and top-level CLI route.
    pub fn replace<C>(&self, capability: Arc<C>) -> anyhow::Result<()>
    where
        C: ControlCapability + 'static,
    {
        let route = capability.control_route();
        let mut resources = self.resources.write().expect("control resources lock");
        let routes = self.routes.read().expect("control routes lock");
        let Some(resource) = routes.get(route.cli_subcommand) else {
            anyhow::bail!(
                "control CLI route `{}` is not registered",
                route.cli_subcommand
            );
        };
        if resource != route.resource || !resources.contains_key(route.resource) {
            anyhow::bail!(
                "control route `{}` does not match its replacement",
                route.resource
            );
        }
        resources.insert(route.resource.to_string(), capability);
        Ok(())
    }
}

#[async_trait]
impl ControlService for ControlRegistry {
    fn routes(&self) -> Vec<ControlRoute> {
        let mut routes: Vec<ControlRoute> = self
            .resources
            .read()
            .expect("control resources lock")
            .values()
            .map(|capability| capability.control_route())
            .collect();
        routes.sort_by_key(|route| route.cli_subcommand);
        routes
    }

    async fn execute(&self, request: ControlRequest) -> ControlResponse {
        if request.version != CONTROL_VERSION {
            return ControlResponse::error(format!(
                "unsupported control protocol version {}; expected {CONTROL_VERSION}",
                request.version
            ));
        }
        let capability = self
            .resources
            .read()
            .expect("control resources lock")
            .get(&request.resource)
            .cloned();
        let Some(capability) = capability else {
            return ControlResponse::error(format!(
                "control resource `{}` is not available in this session",
                request.resource
            ));
        };
        ControlResponse::from_tool_result(capability.execute_control(&request.action).await)
    }

    fn render(&self, request: &ControlRequest, response: &ControlResponse) -> String {
        self.resources
            .read()
            .expect("control resources lock")
            .get(&request.resource)
            .map(|capability| capability.render_control(&request.action, response))
            .unwrap_or_else(|| response.render_default())
    }

    fn is_read_only(&self, request: &ControlRequest) -> bool {
        let capability = self
            .resources
            .read()
            .expect("control resources lock")
            .get(&request.resource)
            .cloned();
        let Some(capability) = capability else {
            return false;
        };
        let route = capability.control_route();
        request
            .action
            .get("operation")
            .or_else(|| request.action.get("action"))
            .and_then(Value::as_str)
            .is_some_and(|operation| route.read_only_operations.contains(&operation))
    }
}

pub struct AttachedCommandOutput {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

/// One authenticated endpoint inherited by every descendant of a shell call.
/// The listener is local-only and the directory is private to this process.
pub(crate) struct ControlEndpoint {
    address: String,
    token: String,
    routes: String,
    bin_dir: PathBuf,
    task: tokio::task::JoinHandle<()>,
}

impl ControlEndpoint {
    pub(crate) async fn start(
        service: Arc<dyn ControlService>,
        approval_policy: crate::config::ApprovalPolicy,
        approval_gate: Arc<crate::sandbox_approval::ApprovalGate>,
        sandbox_mode: crate::config::SandboxMode,
    ) -> anyhow::Result<Self> {
        use rand::RngExt;

        let nonce = format!("{:032x}", rand::rng().random::<u128>());
        let token = format!("{:032x}", rand::rng().random::<u128>());
        #[cfg(unix)]
        let temp_root = PathBuf::from("/tmp");
        #[cfg(windows)]
        let temp_root = std::env::temp_dir();
        let root = temp_root.join(format!("yolop-control-{}-{nonce}", std::process::id()));
        std::fs::create_dir(&root)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))?;
        }
        let bin_dir = root.join("bin");
        std::fs::create_dir(&bin_dir)?;
        install_current_executable_shim(&bin_dir)?;

        let routes = service
            .routes()
            .into_iter()
            .map(|route| route.cli_subcommand)
            .collect::<Vec<_>>()
            .join(",");

        #[cfg(unix)]
        let (address, task) = {
            let path = root.join("control.sock");
            let listener = tokio::net::UnixListener::bind(&path)?;
            let address = format!("unix:{}", path.display());
            let expected_token = token.clone();
            let task = tokio::spawn(async move {
                while let Ok((stream, _)) = listener.accept().await {
                    let service = service.clone();
                    let token = expected_token.clone();
                    let gate = approval_gate.clone();
                    tokio::spawn(async move {
                        let _ = handle_endpoint_stream(
                            stream,
                            &token,
                            service,
                            approval_policy,
                            gate,
                            sandbox_mode,
                        )
                        .await;
                    });
                }
            });
            (address, task)
        };

        #[cfg(windows)]
        let (address, task) = {
            // Windows shell containment is not available yet. Loopback plus a
            // per-execution random token retains process-local authentication.
            let listener =
                tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
            let address = format!("tcp:{}", listener.local_addr()?);
            let expected_token = token.clone();
            let task = tokio::spawn(async move {
                while let Ok((stream, _)) = listener.accept().await {
                    let service = service.clone();
                    let token = expected_token.clone();
                    let gate = approval_gate.clone();
                    tokio::spawn(async move {
                        let _ = handle_endpoint_stream(
                            stream,
                            &token,
                            service,
                            approval_policy,
                            gate,
                            sandbox_mode,
                        )
                        .await;
                    });
                }
            });
            (address, task)
        };

        Ok(Self {
            address,
            token,
            routes,
            bin_dir,
            task,
        })
    }

    pub(crate) fn apply_to(&self, command: &mut tokio::process::Command) {
        command
            .env(CONTROL_ENDPOINT_ENV, &self.address)
            .env(CONTROL_TOKEN_ENV, &self.token)
            .env(CONTROL_ROUTES_ENV, &self.routes);
        let mut paths = vec![self.bin_dir.clone()];
        if let Some(existing) = std::env::var_os("PATH") {
            paths.extend(std::env::split_paths(&existing));
        }
        if let Ok(path) = std::env::join_paths(paths) {
            command.env("PATH", path);
        }
    }

    pub(crate) fn wrap_shell_script(&self, script: &str) -> String {
        #[cfg(unix)]
        {
            let path = self.bin_dir.to_string_lossy().replace('\'', "'\"'\"'");
            format!("export PATH='{path}':\"$PATH\"\n{script}")
        }
        #[cfg(windows)]
        {
            let path = self.bin_dir.to_string_lossy().replace('\'', "''");
            format!("$env:Path = '{path};' + $env:Path\n{script}")
        }
    }
}

impl Drop for ControlEndpoint {
    fn drop(&mut self) {
        self.task.abort();
        if let Some(root) = self.bin_dir.parent() {
            let _ = std::fs::remove_dir_all(root);
        }
    }
}

#[cfg(unix)]
fn install_current_executable_shim(bin_dir: &Path) -> anyhow::Result<()> {
    std::os::unix::fs::symlink(std::env::current_exe()?, bin_dir.join("yolop"))?;
    Ok(())
}

#[cfg(windows)]
fn install_current_executable_shim(bin_dir: &Path) -> anyhow::Result<()> {
    let target = std::env::current_exe()?;
    std::fs::write(
        bin_dir.join("yolop.cmd"),
        format!("@\"{}\" %*\r\n", target.display()),
    )?;
    Ok(())
}

/// Relay a child that answered as an ordinary CLI rather than a control client.
///
/// Its stdout, stderr, and exit code become the command's, so `yolop extensions
/// --help` prints help and exits 0 the way it would outside a session. Output is
/// bounded like every other frame on this transport.
async fn relay_plain_cli_child(
    mut child: tokio::process::Child,
    first_line: Vec<u8>,
    reader: tokio::io::Take<BufReader<tokio::process::ChildStdout>>,
) -> anyhow::Result<AttachedCommandOutput> {
    let mut stdout = first_line;
    let mut reader = reader.into_inner().take(MAX_RELAY_BYTES as u64);
    reader.read_to_end(&mut stdout).await?;
    let mut stderr = Vec::new();
    if let Some(handle) = child.stderr.take() {
        let mut handle = BufReader::new(handle).take(MAX_RELAY_BYTES as u64);
        handle.read_to_end(&mut stderr).await?;
    }
    // The child never asked for a response; closing stdin lets it exit.
    drop(child.stdin.take());
    let status = child.wait().await?;
    Ok(AttachedCommandOutput {
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
        exit_code: status.code().unwrap_or(1),
    })
}

async fn handle_endpoint_stream<S>(
    stream: S,
    expected_token: &str,
    service: Arc<dyn ControlService>,
    approval_policy: crate::config::ApprovalPolicy,
    approval_gate: Arc<crate::sandbox_approval::ApprovalGate>,
    sandbox_mode: crate::config::SandboxMode,
) -> anyhow::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let reader = BufReader::new(stream);
    let mut reader = reader.take((MAX_CONTROL_FRAME_BYTES + 1) as u64);
    let mut frame = Vec::new();
    reader.read_until(b'\n', &mut frame).await?;
    if frame.len() > MAX_CONTROL_FRAME_BYTES {
        anyhow::bail!("control request exceeded {MAX_CONTROL_FRAME_BYTES} bytes");
    }
    let request: EndpointRequest = serde_json::from_slice(&frame)?;
    let output = if request.version != CONTROL_VERSION {
        AttachedCommandOutput {
            stdout: String::new(),
            stderr: format!(
                "attached control protocol {} is incompatible with host protocol {CONTROL_VERSION} (child {}, host {})\n",
                request.version,
                request.client_version,
                env!("CARGO_PKG_VERSION")
            ),
            exit_code: 1,
        }
    } else if request.token != expected_token {
        AttachedCommandOutput {
            stdout: String::new(),
            stderr: "attached control authentication failed\n".to_string(),
            exit_code: 1,
        }
    } else {
        let args = request.argv.into_iter().map(OsString::from).collect();
        match tokio::time::timeout(
            CONTROL_TIMEOUT,
            invoke_attached_inner(args, service, approval_policy, approval_gate, sandbox_mode),
        )
        .await
        {
            Ok(Ok(output)) => output,
            Ok(Err(error)) => AttachedCommandOutput {
                stdout: String::new(),
                stderr: format!("attached control failed: {error}\n"),
                exit_code: 1,
            },
            Err(_) => AttachedCommandOutput {
                stdout: String::new(),
                stderr: "attached control timed out\n".to_string(),
                exit_code: 1,
            },
        }
    };
    let response = EndpointResponse {
        version: CONTROL_VERSION,
        host_version: env!("CARGO_PKG_VERSION").to_string(),
        stdout: output.stdout,
        stderr: output.stderr,
        exit_code: output.exit_code,
    };
    let mut encoded = serde_json::to_vec(&response)?;
    encoded.push(b'\n');
    let mut stream = reader.into_inner().into_inner();
    stream.write_all(&encoded).await?;
    stream.shutdown().await?;
    Ok(())
}

async fn invoke_attached_inner(
    args: Vec<OsString>,
    service: Arc<dyn ControlService>,
    approval_policy: crate::config::ApprovalPolicy,
    approval_gate: Arc<crate::sandbox_approval::ApprovalGate>,
    sandbox_mode: crate::config::SandboxMode,
) -> anyhow::Result<AttachedCommandOutput> {
    let executable = std::env::current_exe()?;
    let mut child = tokio::process::Command::new(executable)
        .arg("--__attached-control-child")
        .args(&args)
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;

    let stdout = child.stdout.take().expect("piped stdout");
    let reader = BufReader::new(stdout);
    let mut reader = reader.take((MAX_CONTROL_FRAME_BYTES + 1) as u64);
    let mut frame = Vec::new();
    reader.read_until(b'\n', &mut frame).await?;
    if frame.len() > MAX_CONTROL_FRAME_BYTES {
        anyhow::bail!("control request exceeded {MAX_CONTROL_FRAME_BYTES} bytes");
    }
    // `--help`, `--version`, and usage errors never reach the protocol: clap
    // prints and exits inside the child. Relaying its own output keeps those
    // invocations working exactly like an ordinary CLI run (and keeps them on
    // this binary), instead of failing to parse help text as a control frame.
    let request = match serde_json::from_slice::<ControlRequest>(&frame) {
        Ok(request) => request,
        Err(_) => return relay_plain_cli_child(child, frame, reader).await,
    };
    let response = if sandbox_mode != crate::config::SandboxMode::DangerFullAccess
        && !service.is_read_only(&request)
    {
        let command = format!(
            "yolop {}",
            args.iter()
                .map(|arg| arg.to_string_lossy())
                .collect::<Vec<_>>()
                .join(" ")
        );
        if approval_policy == crate::config::ApprovalPolicy::Never {
            ControlResponse::error(
                "approval_policy=never forbids attached administration outside the shell sandbox",
            )
        } else if approval_gate
            .approve(crate::sandbox_approval::ApprovalRequest {
                command,
                reason:
                    "Yolop administration writes or executes outside the arbitrary-shell sandbox"
                        .to_string(),
                full_access: false,
            })
            .await
        {
            service.execute(request.clone()).await
        } else {
            ControlResponse::error("attached administration was not approved")
        }
    } else {
        service.execute(request.clone()).await
    };

    let mut stdin = child.stdin.take().expect("piped stdin");
    let mut reply = serde_json::to_vec(&response)?;
    reply.push(b'\n');
    stdin.write_all(&reply).await?;
    stdin.shutdown().await?;
    let status = child.wait().await?;
    if !status.success() {
        anyhow::bail!("control child rejected the host response");
    }

    let rendered = service.render(&request, &response);
    Ok(if response.ok {
        AttachedCommandOutput {
            stdout: format!("{rendered}\n"),
            stderr: String::new(),
            exit_code: 0,
        }
    } else {
        AttachedCommandOutput {
            stdout: String::new(),
            stderr: format!("{rendered}\n"),
            exit_code: 1,
        }
    })
}

/// Return raw argv when this process is an administrative CLI below a running
/// session. Route selection is the only child-side interpretation. The host's
/// exact executable owns the full grammar and typed decoding.
pub(crate) fn endpoint_client_args() -> anyhow::Result<Option<Vec<String>>> {
    let Some(_) = std::env::var_os(CONTROL_ENDPOINT_ENV) else {
        return Ok(None);
    };
    let mut args = std::env::args_os().skip(1);
    let Some(first) = args.next() else {
        return Ok(None);
    };
    let first = first
        .into_string()
        .map_err(|_| anyhow::anyhow!("attached Yolop arguments must be valid UTF-8"))?;
    let routes = std::env::var(CONTROL_ROUTES_ENV)
        .map_err(|_| anyhow::anyhow!("attached control route metadata is missing"))?;
    if !routes.split(',').any(|route| route == first) {
        return Ok(None);
    }
    let mut raw = vec![first];
    for arg in args {
        raw.push(
            arg.into_string()
                .map_err(|_| anyhow::anyhow!("attached Yolop arguments must be valid UTF-8"))?,
        );
    }
    Ok(Some(raw))
}

pub(crate) async fn run_endpoint_client(argv: Vec<String>) -> anyhow::Result<i32> {
    let address = std::env::var(CONTROL_ENDPOINT_ENV)
        .map_err(|_| anyhow::anyhow!("attached control endpoint is missing"))?;
    let token = std::env::var(CONTROL_TOKEN_ENV)
        .map_err(|_| anyhow::anyhow!("attached control token is missing"))?;
    let request = EndpointRequest {
        version: CONTROL_VERSION,
        token,
        client_version: env!("CARGO_PKG_VERSION").to_string(),
        argv,
    };

    #[cfg(unix)]
    let stream = {
        let path = address
            .strip_prefix("unix:")
            .ok_or_else(|| anyhow::anyhow!("unsupported attached control endpoint `{address}`"))?;
        tokio::net::UnixStream::connect(path).await.map_err(|error| {
            anyhow::anyhow!(
                "cannot reach the running Yolop session at `{path}`: {error}; refusing detached fallback"
            )
        })?
    };
    #[cfg(windows)]
    let stream = {
        let target = address
            .strip_prefix("tcp:")
            .ok_or_else(|| anyhow::anyhow!("unsupported attached control endpoint `{address}`"))?;
        tokio::net::TcpStream::connect(target).await.map_err(|error| {
            anyhow::anyhow!(
                "cannot reach the running Yolop session at `{target}`: {error}; refusing detached fallback"
            )
        })?
    };
    exchange_endpoint_request(stream, request).await
}

async fn exchange_endpoint_request<S>(
    mut stream: S,
    request: EndpointRequest,
) -> anyhow::Result<i32>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut frame = serde_json::to_vec(&request)?;
    if frame.len() > MAX_CONTROL_FRAME_BYTES {
        anyhow::bail!("control request exceeded {MAX_CONTROL_FRAME_BYTES} bytes");
    }
    frame.push(b'\n');
    stream.write_all(&frame).await?;
    stream.flush().await?;
    let mut reader = BufReader::new(stream).take((MAX_CONTROL_FRAME_BYTES + 1) as u64);
    let mut reply = Vec::new();
    reader.read_until(b'\n', &mut reply).await?;
    if reply.len() > MAX_CONTROL_FRAME_BYTES {
        anyhow::bail!("control response exceeded {MAX_CONTROL_FRAME_BYTES} bytes");
    }
    let response: EndpointResponse = serde_json::from_slice(&reply)?;
    if response.version != CONTROL_VERSION {
        anyhow::bail!(
            "host {} uses unsupported attached control protocol {}; child {} expects {CONTROL_VERSION}",
            response.host_version,
            response.version,
            env!("CARGO_PKG_VERSION")
        );
    }
    let mut stdout = tokio::io::stdout();
    stdout.write_all(response.stdout.as_bytes()).await?;
    stdout.flush().await?;
    let mut stderr = tokio::io::stderr();
    stderr.write_all(response.stderr.as_bytes()).await?;
    stderr.flush().await?;
    Ok(response.exit_code)
}

/// Child half of the anonymous pipe handshake. It emits exactly one bounded
/// request and accepts exactly one version-matched response.
pub async fn run_control_child(request: ControlRequest) -> anyhow::Result<()> {
    let mut frame = serde_json::to_vec(&request)?;
    if frame.len() > MAX_CONTROL_FRAME_BYTES {
        anyhow::bail!("control request exceeded {MAX_CONTROL_FRAME_BYTES} bytes");
    }
    frame.push(b'\n');
    let mut stdout = tokio::io::stdout();
    stdout.write_all(&frame).await?;
    stdout.flush().await?;

    let reader = BufReader::new(tokio::io::stdin());
    let mut reader = reader.take((MAX_CONTROL_FRAME_BYTES + 1) as u64);
    let mut reply = Vec::new();
    reader.read_until(b'\n', &mut reply).await?;
    if reply.len() > MAX_CONTROL_FRAME_BYTES {
        anyhow::bail!("control response exceeded {MAX_CONTROL_FRAME_BYTES} bytes");
    }
    let response: ControlResponse = serde_json::from_slice(&reply)?;
    if response.version != CONTROL_VERSION {
        anyhow::bail!(
            "control response version {} does not match {CONTROL_VERSION}",
            response.version
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestControlCapability;

    #[async_trait]
    impl Capability for TestControlCapability {
        fn id(&self) -> &str {
            "extensions"
        }

        fn name(&self) -> &str {
            "Extensions"
        }

        fn description(&self) -> &str {
            "Test extension administration."
        }
    }

    #[async_trait]
    impl ControlCapability for TestControlCapability {
        fn control_route(&self) -> ControlRoute {
            ControlRoute {
                resource: "extensions",
                cli_subcommand: "extensions",
                read_only_operations: &["list"],
                summary: "administer extension packages",
            }
        }

        async fn execute_control(&self, action: &Value) -> ToolExecutionResult {
            ToolExecutionResult::Success(action.clone())
        }

        fn render_control(&self, _action: &Value, response: &ControlResponse) -> String {
            response.render_default()
        }
    }

    #[async_trait]
    impl CliCapability for TestControlCapability {
        fn cli_command(&self) -> Command {
            Command::new("extensions")
                .subcommand(Command::new("enable").arg(clap::Arg::new("name").required(true)))
        }

        fn control_request_from_cli(&self, matches: &ArgMatches) -> anyhow::Result<ControlRequest> {
            let (operation, matches) = matches.subcommand().expect("operation");
            ControlRequest::new(
                "extensions",
                serde_json::json!({
                    "operation": operation,
                    "name": matches.get_one::<String>("name"),
                }),
            )
            .map_err(Into::into)
        }

        async fn execute_cli(&self, _request: &ControlRequest) -> anyhow::Result<()> {
            Ok(())
        }
    }

    fn service() -> ControlRegistry {
        let service = ControlRegistry::default();
        service.register(Arc::new(TestControlCapability)).unwrap();
        service
    }

    #[test]
    fn request_round_trips_as_versioned_resource_envelope() {
        let request = ControlRequest::new(
            "extensions",
            serde_json::json!({ "operation": "enable", "name": "demo" }),
        )
        .unwrap();
        let encoded = serde_json::to_string(&request).unwrap();
        assert!(encoded.contains("\"version\":1"));
        assert!(encoded.contains("\"resource\":\"extensions\""));
        assert_eq!(
            serde_json::from_str::<ControlRequest>(&encoded).unwrap(),
            request
        );
    }

    #[tokio::test]
    async fn endpoint_rejects_protocol_skew_with_both_product_versions() {
        let (mut client, server) = tokio::io::duplex(4096);
        let task = tokio::spawn(handle_endpoint_stream(
            server,
            "expected-token",
            Arc::new(service()),
            crate::config::ApprovalPolicy::Never,
            crate::sandbox_approval::ApprovalGate::deny(),
            crate::config::SandboxMode::DangerFullAccess,
        ));
        let mut frame = serde_json::to_vec(&EndpointRequest {
            version: 0,
            token: "expected-token".to_string(),
            client_version: "0.1.0".to_string(),
            argv: vec!["extensions".to_string(), "list".to_string()],
        })
        .unwrap();
        frame.push(b'\n');
        client.write_all(&frame).await.unwrap();

        let mut reply = String::new();
        BufReader::new(client).read_line(&mut reply).await.unwrap();
        let response: EndpointResponse = serde_json::from_str(&reply).unwrap();
        assert_eq!(response.exit_code, 1);
        assert!(response.stderr.contains("child 0.1.0"));
        assert!(response.stderr.contains(env!("CARGO_PKG_VERSION")));
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn registry_routes_requests_to_the_declaring_capability() {
        let service = service();
        let request = ControlRequest::new(
            "extensions",
            serde_json::json!({ "operation": "enable", "name": "demo" }),
        )
        .unwrap();
        let response = service.execute(request.clone()).await;
        assert!(response.ok);
        assert_eq!(response.value, Some(request.action));

        let unavailable = service
            .execute(ControlRequest::new("mcp", serde_json::json!({})).unwrap())
            .await;
        assert!(!unavailable.ok);
        assert!(
            unavailable
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("not available")
        );
        assert!(service.is_read_only(
            &ControlRequest::new("extensions", serde_json::json!({ "operation": "list" })).unwrap()
        ));
        assert!(
            !service.is_read_only(
                &ControlRequest::new(
                    "extensions",
                    serde_json::json!({ "operation": "enable", "name": "demo" })
                )
                .unwrap()
            )
        );
    }

    #[test]
    fn registries_reject_duplicate_capability_routes() {
        let control = ControlRegistry::default();
        control.register(Arc::new(TestControlCapability)).unwrap();
        assert!(
            control
                .register(Arc::new(TestControlCapability))
                .unwrap_err()
                .to_string()
                .contains("duplicate control resource")
        );

        let mut cli = CliRegistry::default();
        cli.register(Arc::new(TestControlCapability)).unwrap();
        assert!(
            cli.register(Arc::new(TestControlCapability))
                .unwrap_err()
                .to_string()
                .contains("duplicate contributed CLI command")
        );
    }

    #[test]
    fn cli_registry_contributes_and_decodes_the_capability_command() {
        let mut registry = CliRegistry::default();
        registry.register(Arc::new(TestControlCapability)).unwrap();
        let matches = registry
            .augment(Command::new("yolop").subcommand(Command::new("version")))
            .unwrap()
            .try_get_matches_from(["yolop", "extensions", "enable", "demo"])
            .unwrap();
        let invocation = registry.invocation(&matches).unwrap().unwrap();
        assert_eq!(invocation.request.resource, "extensions");
        assert_eq!(invocation.request.action["operation"], "enable");
        assert_eq!(invocation.request.action["name"], "demo");
    }

    #[test]
    fn routes_are_ordered_so_the_prompt_prefix_stays_stable() {
        let registry = ControlRegistry::default();
        registry
            .register(Arc::new(SecondTestControlCapability))
            .expect("register");
        registry
            .register(Arc::new(TestControlCapability))
            .expect("register");
        let subcommands: Vec<&str> = ControlService::routes(&registry)
            .iter()
            .map(|route| route.cli_subcommand)
            .collect();
        assert_eq!(subcommands, vec!["coordination", "extensions"]);
    }

    struct SecondTestControlCapability;

    #[async_trait]
    impl Capability for SecondTestControlCapability {
        fn id(&self) -> &str {
            "coordination"
        }

        fn name(&self) -> &str {
            "Coordination"
        }

        fn description(&self) -> &str {
            "Test coordination administration."
        }
    }

    #[async_trait]
    impl ControlCapability for SecondTestControlCapability {
        fn control_route(&self) -> ControlRoute {
            ControlRoute {
                resource: "coordination",
                cli_subcommand: "coordination",
                read_only_operations: &["list"],
                summary: "steer local sessions",
            }
        }

        async fn execute_control(&self, action: &Value) -> ToolExecutionResult {
            ToolExecutionResult::Success(action.clone())
        }

        fn render_control(&self, _action: &Value, response: &ControlResponse) -> String {
            response.render_default()
        }
    }
}

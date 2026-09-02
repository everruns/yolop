//! One-shot attached CLI control plane.
//!
//! This is intentionally separate from YEP: YEP is the host↔extension data
//! plane, while this module lets a directly invoked Yolop CLI ask its parent
//! Yolop session to mutate live state. There is no listening socket, endpoint
//! path, token, or inherited general-shell environment. For one conservative
//! foreground invocation the host spawns its exact executable with anonymous
//! stdin/stdout pipes, accepts one versioned request, replies once, and closes.

use async_trait::async_trait;
use clap::{ArgMatches, Command};
use everruns_core::Capability;
use everruns_core::ToolExecutionResult;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::ffi::OsString;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

pub const CONTROL_VERSION: u32 = 1;
const MAX_CONTROL_FRAME_BYTES: usize = 64 * 1024;
const CONTROL_TIMEOUT: Duration = Duration::from_secs(120);
/// Cap for output relayed from a child that ran as an ordinary CLI (help,
/// version, usage errors) rather than speaking the protocol.
const MAX_RELAY_BYTES: usize = 256 * 1024;

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
    /// single shared control-plane prompt block. Capabilities contribute this
    /// datum only: the prose, the attachment rules, and the discovery pointer
    /// are written once in `ControlPlaneCapability`, so a new CLI route adds a
    /// line rather than a prompt section of its own.
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
            ToolExecutionResult::ConnectionRequired { provider } => {
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
    fn route(&self, cli_subcommand: &str) -> Option<ControlRoute>;
    /// Every registered route, ordered by subcommand so the derived prompt
    /// block is stable across sessions (and stays cacheable).
    fn routes(&self) -> Vec<ControlRoute>;
    async fn execute(&self, request: ControlRequest) -> ControlResponse;
    fn render(&self, request: &ControlRequest, response: &ControlResponse) -> String;
}

#[derive(Default)]
pub struct ControlRegistry {
    resources: HashMap<String, Arc<dyn ControlCapability>>,
    routes: HashMap<String, String>,
}

impl ControlRegistry {
    pub fn register<C>(&mut self, capability: Arc<C>) -> anyhow::Result<()>
    where
        C: ControlCapability + 'static,
    {
        let route = capability.control_route();
        if self.resources.contains_key(route.resource) {
            anyhow::bail!("duplicate control resource `{}`", route.resource);
        }
        if self.routes.contains_key(route.cli_subcommand) {
            anyhow::bail!("duplicate control CLI route `{}`", route.cli_subcommand);
        }
        self.routes
            .insert(route.cli_subcommand.to_string(), route.resource.to_string());
        self.resources
            .insert(route.resource.to_string(), capability);
        Ok(())
    }
}

#[async_trait]
impl ControlService for ControlRegistry {
    fn route(&self, cli_subcommand: &str) -> Option<ControlRoute> {
        self.routes
            .get(cli_subcommand)
            .and_then(|resource| self.resources.get(resource))
            .map(|capability| capability.control_route())
    }

    fn routes(&self) -> Vec<ControlRoute> {
        let mut routes: Vec<ControlRoute> = self
            .resources
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
        let Some(capability) = self.resources.get(&request.resource) else {
            return ControlResponse::error(format!(
                "control resource `{}` is not available in this session",
                request.resource
            ));
        };
        ControlResponse::from_tool_result(capability.execute_control(&request.action).await)
    }

    fn render(&self, request: &ControlRequest, response: &ControlResponse) -> String {
        self.resources
            .get(&request.resource)
            .map(|capability| capability.render_control(&request.action, response))
            .unwrap_or_else(|| response.render_default())
    }
}

pub const CONTROL_PLANE_CAPABILITY_ID: &str = "control_plane";

/// The single system-prompt contribution describing attached administration.
///
/// Administration is deliberately not a set of model tools (their schemas would
/// cost context every turn), so the agent has to learn the affordance some other
/// way. It is stated once, here, and derived from the routes actually registered
/// in this session: a session without a control route registers no capability
/// and says nothing, and a new `ControlCapability` is described by its own
/// `ControlRoute::summary` without touching this text or adding a block of its
/// own. The Bash tool description stays free of resource-specific vocabulary.
pub struct ControlPlaneCapability {
    prompt: String,
}

impl ControlPlaneCapability {
    /// `None` when nothing is registered, so the block is never a promise the
    /// session cannot keep.
    pub fn new(routes: &[ControlRoute]) -> Option<Self> {
        if routes.is_empty() {
            return None;
        }
        // No `<capability>` wrapper here: the default
        // `Capability::system_prompt_contribution` wraps this text in
        // `<capability id="control_plane">` when it assembles the prompt.
        let mut prompt = String::from(
            "Administer this session by running `yolop <subcommand> ...` in the foreground bash \
             tool: it attaches to the running session and takes effect live. Run \
             `yolop <subcommand> --help` for the operations; there are no equivalent tools.\n",
        );
        for route in routes {
            prompt.push_str(&format!(
                "- `{}`, {}\n",
                route.cli_subcommand, route.summary
            ));
        }
        prompt.push_str(
            "Invoke directly: shell composition loses attachment. Do not repeat `--help`.",
        );
        Some(Self { prompt })
    }
}

#[async_trait]
impl Capability for ControlPlaneCapability {
    fn id(&self) -> &str {
        CONTROL_PLANE_CAPABILITY_ID
    }

    fn name(&self) -> &str {
        "Control plane"
    }

    fn description(&self) -> &str {
        "Attached administration of the running session through the `yolop` CLI."
    }

    fn system_prompt_addition(&self) -> Option<&str> {
        Some(&self.prompt)
    }

    fn tools(&self) -> Vec<Box<dyn everruns_core::Tool>> {
        Vec::new()
    }
}

pub struct AttachedCommandOutput {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

/// Return argv only for the deliberately narrow direct-invocation grammar.
/// Shell composition and quoting run as ordinary shell commands without an
/// attached channel, so a capability cannot leak into pipelines/backgrounds.
fn direct_control_args(
    command: &str,
    service: &dyn ControlService,
) -> Option<(ControlRoute, Vec<OsString>)> {
    if command.len() > 8192
        || command.chars().any(|c| {
            matches!(
                c,
                '|' | '&' | ';' | '<' | '>' | '`' | '\n' | '\r' | '\\' | '\'' | '"'
            )
        })
        || command.contains("$(")
    {
        return None;
    }
    let words = command.split_ascii_whitespace().collect::<Vec<_>>();
    if words.len() < 2 || Path::new(words[0]).file_name().and_then(|v| v.to_str()) != Some("yolop")
    {
        return None;
    }
    let route = service.route(words[1])?;
    Some((
        route,
        words.into_iter().skip(1).map(OsString::from).collect(),
    ))
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

/// Warn when a `yolop` administration command was written in a form the
/// attached recognizer rejects.
///
/// The two forms differ only by punctuation, and the difference is invisible in
/// the output: `yolop extensions enable x` mutates this session, while
/// `yolop extensions enable x | cat` is ordinary shell that touches only
/// persisted global state. Without this notice the agent (and the user reading
/// the transcript) sees a success either way and cannot tell which happened.
///
/// `None` for anything that is genuinely attached, and for `yolop` commands that
/// name no registered control route (`yolop --version | cat` administers
/// nothing).
pub fn detached_control_notice(command: &str, service: &dyn ControlService) -> Option<String> {
    if direct_control_args(command, service).is_some() {
        return None;
    }
    let words = command.split_ascii_whitespace().collect::<Vec<_>>();
    // Help and version administer nothing, so a composed one is not a mistake.
    if words
        .iter()
        .any(|word| matches!(*word, "--help" | "-h" | "--version" | "-V"))
    {
        return None;
    }
    // Scan every token, not just argv[0]: `... | yolop extensions list` runs
    // detached just as surely as `yolop extensions list | ...`.
    let subcommand = words.windows(2).find_map(|pair| {
        let is_yolop = Path::new(pair[0].trim_matches(['"', '\'']))
            .file_name()
            .and_then(|value| value.to_str())
            == Some("yolop");
        let route = service.route(pair[1].trim_matches(['"', '\'']))?;
        is_yolop.then_some(route.cli_subcommand)
    })?;
    Some(format!(
        "`yolop {subcommand}` ran as an ordinary shell command. Shell composition (a pipeline, \
         redirection, quoting, substitution, or `&`) is never attached to this session, so this \
         changed only persisted global state and left the running session untouched. Re-run it \
         directly, without composition, to administer this session."
    ))
}

/// Typed administration other than a list crosses the arbitrary-shell
/// sandbox boundary into a host broker and therefore needs an explicit shell
/// approval whenever containment is enabled.
pub fn requires_host_approval(command: &str, service: &dyn ControlService) -> bool {
    let Some((route, args)) = direct_control_args(command, service) else {
        return false;
    };
    !args
        .get(1)
        .and_then(|value| value.to_str())
        .is_some_and(|operation| route.read_only_operations.contains(&operation))
}

/// Invoke the exact running Yolop executable as a one-shot protocol client.
/// `None` means this is not an eligible direct control command and the caller
/// should execute it through the ordinary shell.
pub async fn invoke_attached(
    command: &str,
    service: Arc<dyn ControlService>,
) -> Option<AttachedCommandOutput> {
    let (_, args) = direct_control_args(command, service.as_ref())?;
    Some(
        match tokio::time::timeout(CONTROL_TIMEOUT, invoke_attached_inner(args, service)).await {
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
        },
    )
}

async fn invoke_attached_inner(
    args: Vec<OsString>,
    service: Arc<dyn ControlService>,
) -> anyhow::Result<AttachedCommandOutput> {
    let executable = std::env::current_exe()?;
    let mut child = tokio::process::Command::new(executable)
        .arg("--__attached-control-child")
        .args(args)
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
    let response = service.execute(request.clone()).await;

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
        let mut service = ControlRegistry::default();
        service.register(Arc::new(TestControlCapability)).unwrap();
        service
    }

    #[test]
    fn only_plain_foreground_extension_invocations_are_attached() {
        let service = service();
        assert!(direct_control_args("yolop extensions", &service).is_some());
        assert!(direct_control_args("yolop extensions list", &service).is_some());
        assert!(
            direct_control_args("/usr/local/bin/yolop extensions enable demo", &service).is_some()
        );
        assert!(direct_control_args("yolop extensions enable demo | cat", &service).is_none());
        assert!(direct_control_args("yolop extensions enable demo &", &service).is_none());
        assert!(direct_control_args("yolop extensions enable 'demo'", &service).is_none());
        assert!(direct_control_args("yolop mcp list", &service).is_none());
        assert!(!requires_host_approval("yolop extensions list", &service));
        assert!(requires_host_approval(
            "yolop extensions doctor demo",
            &service
        ));
        assert!(requires_host_approval(
            "yolop extensions enable demo",
            &service
        ));
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
    }

    #[test]
    fn registries_reject_duplicate_capability_routes() {
        let mut control = ControlRegistry::default();
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
    fn control_plane_prompt_is_absent_without_registered_routes() {
        assert!(ControlPlaneCapability::new(&[]).is_none());
    }

    /// Drives the real assembly entry point (`system_prompt_contribution`, what
    /// the runtime calls when it builds the prompt), not just the constructor:
    /// the default wraps the addition, so the block must not wrap itself.
    #[tokio::test]
    async fn control_plane_contribution_is_wrapped_exactly_once() {
        let registry = service();
        let capability = ControlPlaneCapability::new(&ControlService::routes(&registry))
            .expect("a registered route contributes the block");
        let ctx = everruns_core::capabilities::SystemPromptContext::without_file_store(
            everruns_provider::typed_id::SessionId::new(),
        );
        let contribution = capability
            .system_prompt_contribution(&ctx)
            .await
            .expect("the capability contributes to the assembled prompt");
        assert_eq!(contribution.matches("<capability id=").count(), 1);
        assert!(contribution.starts_with("<capability id=\"control_plane\">\n"));
        assert!(contribution.ends_with("</capability>"));
        assert!(contribution.contains("- `extensions`, administer extension packages"));
    }

    #[test]
    fn control_plane_prompt_lists_every_registered_route() {
        let registry = service();
        let capability = ControlPlaneCapability::new(&ControlService::routes(&registry))
            .expect("a registered route contributes the block");
        let prompt = capability
            .system_prompt_addition()
            .expect("the block is the capability's contribution");
        assert!(prompt.contains("- `extensions`, administer extension packages"));
        // Discovery points at the contributed help, not at a duplicated grammar.
        assert!(prompt.contains("`yolop <subcommand> --help`"));
        assert!(prompt.contains("loses attachment"));
    }

    #[test]
    fn control_plane_prompt_discourages_repeated_help_probes() {
        let registry = service();
        let capability = ControlPlaneCapability::new(&ControlService::routes(&registry))
            .expect("a registered route contributes the block");
        let prompt = capability
            .system_prompt_addition()
            .expect("the block is the capability's contribution");

        assert!(prompt.contains("Do not repeat `--help`"));
    }

    #[test]
    fn routes_are_ordered_so_the_prompt_prefix_stays_stable() {
        let mut registry = ControlRegistry::default();
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

    #[test]
    fn attached_administration_gets_no_detached_notice() {
        let service = service();
        assert!(detached_control_notice("yolop extensions enable demo", &service).is_none());
        assert!(detached_control_notice("yolop extensions list", &service).is_none());
    }

    #[test]
    fn composed_administration_is_reported_as_detached() {
        let service = service();
        // Each of these is rejected by the recognizer and silently runs as
        // ordinary shell, which is exactly what the agent cannot otherwise see.
        for command in [
            "yolop extensions enable demo | cat",
            "yolop extensions enable demo > out.txt",
            "yolop extensions enable 'demo'",
            "yolop extensions enable demo &",
            "/usr/local/bin/yolop extensions list | grep demo",
            // Not argv[0]: still a detached administration attempt.
            "echo hi | yolop extensions list",
        ] {
            let notice = detached_control_notice(command, &service)
                .unwrap_or_else(|| panic!("expected a detached notice for `{command}`"));
            assert!(notice.contains("`yolop extensions`"), "{command}: {notice}");
            assert!(
                notice.contains("left the running session untouched"),
                "{command}"
            );
        }
    }

    #[test]
    fn unrelated_and_non_administration_commands_are_quiet() {
        let service = service();
        // No registered route named, so nothing about the session is at stake.
        assert!(detached_control_notice("yolop --version | cat", &service).is_none());
        assert!(detached_control_notice("yolop worktree list | cat", &service).is_none());
        assert!(detached_control_notice("cargo test | tee log", &service).is_none());
        // A bare word that happens to match a route is not a yolop invocation.
        assert!(detached_control_notice("git extensions list", &service).is_none());
    }
}

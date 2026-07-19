//! Persistent capability-server processes. One process per extension per
//! yolop process, spawned lazily on first use, kept for the session,
//! `kill_on_drop`, respawned with a fresh handshake on the call after a
//! crash — the same lifecycle policy `capabilities/lsp/manager.rs` applies
//! to language servers. Only this module knows about processes; everything
//! else drives the transport-generic `YepConnection`.

use super::client::{AskSink, StatusSink, YepConnection};
use super::package::ExtensionManifest;
use super::protocol::{InitializeParams, InitializeResult, PROTOCOL_VERSION};
use anyhow::{Context, Result, anyhow};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Weak};
use std::time::Duration;
use tokio::process::Command;
use tokio::sync::Mutex;

pub const DEFAULT_REQUEST_TIMEOUT_MS: u64 = 60_000;

/// Everything needed to spawn and clamp one extension's server.
pub struct ExtensionProcessSpec {
    pub manifest: ExtensionManifest,
    /// Package directory; its `bin/` is prepended to `PATH` for resolution.
    pub package_dir: PathBuf,
    pub workspace_root: PathBuf,
    pub config: Value,
    pub request_timeout: Duration,
    /// Where the server's `status/changed` notifications go (status bar).
    /// Only wired when the extension declares `status` and the host has a
    /// status bar; `None` otherwise.
    pub status_sink: Option<StatusSink>,
    /// Handles the server's `ui/ask` requests. Only wired when the extension
    /// declares `ui_ask` and the host has a prompt surface; `None` otherwise.
    pub ask_sink: Option<AskSink>,
}

/// A live (or lazily spawned) capability server.
pub struct ExtensionProcess {
    spec: ExtensionProcessSpec,
    state: Mutex<Option<Live>>,
}

struct Live {
    connection: Arc<YepConnection>,
    /// Tool names the server declared this run, already clamped to the
    /// manifest (D4: the handshake may only narrow the approved grant).
    served_tools: HashSet<String>,
    prompt: Option<String>,
    // Held so the child dies with us (`kill_on_drop`).
    _child: tokio::process::Child,
}

impl ExtensionProcess {
    pub fn new(spec: ExtensionProcessSpec) -> Self {
        Self {
            spec,
            state: Mutex::new(None),
        }
    }

    pub fn name(&self) -> &str {
        &self.spec.manifest.name
    }

    /// Call one tool on the server, spawning/handshaking it first if needed.
    /// A dead connection is dropped so the next call respawns.
    pub async fn call_tool(&self, tool_call_id: &str, name: &str, args: Value) -> Result<Value> {
        let (connection, served) = {
            let mut state = self.state.lock().await;
            self.ensure_live(&mut state).await?;
            let live = state.as_ref().expect("ensured live");
            (live.connection.clone(), live.served_tools.contains(name))
        };
        if !served {
            return Err(anyhow!(
                "tool `{name}` is declared by extension `{}` but not served by its running \
                 server (feature-gated build?)",
                self.name()
            ));
        }
        let params = serde_json::to_value(super::protocol::ToolCallParams {
            tool_call_id: tool_call_id.to_string(),
            name: name.to_string(),
            args,
        })?;
        let result = connection.request("tool/call", params).await;
        if connection.is_closed() {
            *self.state.lock().await = None;
        }
        result
    }

    /// Run a manifest-declared slash command on the server (`command/execute`).
    /// Spawns the server if needed. `name` is the command's own name (no
    /// namespace prefix).
    pub async fn execute_command(
        &self,
        name: &str,
        arguments: &str,
    ) -> Result<super::protocol::CommandExecuteResult> {
        let connection = {
            let mut state = self.state.lock().await;
            self.ensure_live(&mut state).await?;
            state.as_ref().expect("ensured live").connection.clone()
        };
        let params = serde_json::to_value(super::protocol::CommandExecuteParams {
            name: name.to_string(),
            arguments: arguments.to_string(),
        })?;
        let result = connection.request("command/execute", params).await;
        if connection.is_closed() {
            *self.state.lock().await = None;
        }
        Ok(serde_json::from_value(result?).unwrap_or_default())
    }

    /// The server's static system-prompt contribution, from the handshake.
    /// Spawns the server if needed; a failure is a warning, never a session
    /// sink — the prompt facet degrades to absent.
    pub async fn prompt_contribution(&self) -> Option<String> {
        let mut state = self.state.lock().await;
        if let Err(err) = self.ensure_live(&mut state).await {
            tracing::warn!(
                target: "yolop::ext", ext = %self.name(),
                "extension server unavailable for prompt contribution: {err}"
            );
            return None;
        }
        state.as_ref().and_then(|live| live.prompt.clone())
    }

    /// A fresh dynamic system-prompt contribution (`prompt/contribution`),
    /// recomputed per turn. Failure degrades to `None` (the caller falls back
    /// to the static prompt / last-known-good).
    pub async fn dynamic_prompt(&self) -> Option<String> {
        let connection = {
            let mut state = self.state.lock().await;
            self.ensure_live(&mut state).await.ok()?;
            state.as_ref().expect("ensured live").connection.clone()
        };
        match connection
            .request("prompt/contribution", serde_json::Value::Null)
            .await
        {
            Ok(value) => {
                let contribution: super::protocol::PromptContribution =
                    serde_json::from_value(value).unwrap_or_default();
                let text = contribution.text.trim();
                (!text.is_empty()).then(|| text.to_string())
            }
            Err(err) => {
                tracing::warn!(
                    target: "yolop::ext", ext = %self.name(),
                    "dynamic prompt/contribution failed: {err}"
                );
                None
            }
        }
    }

    /// Fire one subscribed lifecycle hook (`hook/fire`) and return the
    /// server's decision. `Err` is a transport/server failure the caller
    /// resolves per the subscription's `on_error` policy.
    pub async fn fire_hook(
        &self,
        event: &str,
        tool_name: &str,
        args: &serde_json::Value,
    ) -> Result<super::protocol::HookDecision> {
        let connection = {
            let mut state = self.state.lock().await;
            self.ensure_live(&mut state).await?;
            state.as_ref().expect("ensured live").connection.clone()
        };
        let params = serde_json::to_value(super::protocol::HookFireParams {
            event: event.to_string(),
            tool_name: tool_name.to_string(),
            args: args.clone(),
        })?;
        let value = connection.request("hook/fire", params).await?;
        Ok(serde_json::from_value(value).unwrap_or_default())
    }

    /// Drop the running server (if any) so the next call respawns it from
    /// disk with a fresh handshake — the live-reload seam. Picks up edits to
    /// the server *implementation*; the approved surface (manifest tools,
    /// prompt) is the session snapshot, so reload can never widen the grant
    /// (D4). Returns whether a live server was actually torn down.
    pub async fn reload(&self) -> bool {
        // Taking the `Live` drops its `_child` (`kill_on_drop`), ending the old
        // process; `ensure_live` respawns on the next call.
        self.state.lock().await.take().is_some()
    }

    async fn ensure_live(&self, state: &mut Option<Live>) -> Result<()> {
        if let Some(live) = state.as_ref()
            && !live.connection.is_closed()
        {
            return Ok(());
        }
        *state = Some(self.spawn().await?);
        Ok(())
    }

    async fn spawn(&self) -> Result<Live> {
        let server = &self.spec.manifest.capability_server;
        let mut command = Command::new(&server.command);
        command
            .args(&server.args)
            .current_dir(&self.spec.workspace_root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // stderr is the server's free log channel, never parsed. Pipe it and
            // drain to tracing rather than inherit: inheriting writes the
            // server's logs straight onto the terminal, which corrupts the TUI
            // frame (most visibly the `--fullscreen` alternate screen). Routed to
            // tracing, the logs still surface via `RUST_LOG` — just never on the
            // raw screen.
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // Resolve the command against the package's own `bin/` first.
        let bin_dir = self.spec.package_dir.join("bin");
        if bin_dir.is_dir() {
            let mut paths: Vec<PathBuf> = vec![bin_dir];
            if let Some(path) = std::env::var_os("PATH") {
                paths.extend(std::env::split_paths(&path));
            }
            if let Ok(joined) = std::env::join_paths(paths) {
                command.env("PATH", joined);
            }
        }
        let mut child = command.spawn().with_context(|| {
            format!(
                "extension `{}`: cannot spawn `{}` (install it or fix `capabilityServer.command`)",
                self.name(),
                server.command
            )
        })?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("extension server has no stdout"))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("extension server has no stdin"))?;
        // Drain the server's stderr into tracing so its logs never touch the
        // terminal the TUI owns. The task ends when the child exits and stderr
        // hits EOF.
        if let Some(stderr) = child.stderr.take() {
            let ext = self.name().to_string();
            tokio::spawn(async move {
                use tokio::io::{AsyncBufReadExt, BufReader};
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::debug!(target: "yolop::ext", ext = %ext, "{line}");
                }
            });
        }

        let init = InitializeParams {
            protocol_version: PROTOCOL_VERSION.to_string(),
            session_id: String::new(),
            workspace_root: self.spec.workspace_root.display().to_string(),
            config: self.spec.config.clone(),
            capabilities: vec!["cancel".into(), "streaming".into()],
        };
        let (connection, handshake) = YepConnection::connect(
            stdout,
            stdin,
            self.name(),
            init,
            self.spec.request_timeout,
            self.spec.status_sink.clone(),
            self.spec.ask_sink.clone(),
        )
        .await?;

        if !handshake.name.is_empty() && handshake.name != self.spec.manifest.name {
            tracing::warn!(
                target: "yolop::ext", ext = %self.name(),
                "server introduced itself as `{}`; the installed manifest name wins",
                handshake.name
            );
        }
        tracing::debug!(
            target: "yolop::ext", ext = %self.name(),
            capabilities = ?handshake.capabilities, "extension server connected"
        );
        let (served_tools, prompt) = self.clamp(&handshake);
        Ok(Live {
            connection,
            served_tools,
            prompt,
            _child: child,
        })
    }

    /// Apply the D4 clamp: anything the handshake declares beyond the
    /// approved manifest is refused (and logged), never widened.
    fn clamp(&self, handshake: &InitializeResult) -> (HashSet<String>, Option<String>) {
        let manifest = &self.spec.manifest;
        let approved: HashSet<&str> = manifest.tools.iter().map(|t| t.name.as_str()).collect();
        let mut served = HashSet::new();
        for tool in &handshake.capability_params.tools {
            if approved.contains(tool.name.as_str()) {
                served.insert(tool.name.clone());
            } else {
                tracing::warn!(
                    target: "yolop::ext", ext = %self.name(),
                    "refusing tool `{}` not in the approved manifest", tool.name
                );
            }
        }
        let prompt = match &handshake.capability_params.prompt {
            Some(prompt) if manifest.prompt => {
                let text = prompt.static_text.trim();
                (!text.is_empty()).then(|| text.to_string())
            }
            Some(_) => {
                tracing::warn!(
                    target: "yolop::ext", ext = %self.name(),
                    "refusing prompt contribution: manifest does not declare `prompt = true`"
                );
                None
            }
            None => None,
        };
        (served, prompt)
    }
}

/// Shared handle to the live server processes, keyed by extension name. Each
/// `ExtensionCapability` registers its process here on spawn; the management
/// capability (`reload_extension`) looks one up to restart it mid-session
/// without a yolop restart. Weak references so the registry never keeps a
/// process alive past its owning capability's own cache.
#[derive(Clone, Default)]
pub struct LiveProcessRegistry {
    processes: Arc<std::sync::Mutex<HashMap<String, Weak<ExtensionProcess>>>>,
}

impl LiveProcessRegistry {
    /// Record the current process for `name`, replacing any prior entry (the
    /// capability rebuilds its process when config changes).
    pub fn register(&self, name: &str, process: &Arc<ExtensionProcess>) {
        self.processes
            .lock()
            .expect("live-process registry lock")
            .insert(name.to_string(), Arc::downgrade(process));
    }

    /// Reload the named extension's server. Returns `Some(true)` if a running
    /// server was torn down (next call respawns), `Some(false)` if the
    /// extension is registered but hasn't spawned a server yet (nothing to do
    /// — the next call spawns current code anyway), and `None` if no such
    /// extension is live in this session (not enabled, or unknown).
    pub async fn reload(&self, name: &str) -> Option<bool> {
        let process = self
            .processes
            .lock()
            .expect("live-process registry lock")
            .get(name)
            .and_then(Weak::upgrade)?;
        Some(process.reload().await)
    }
}

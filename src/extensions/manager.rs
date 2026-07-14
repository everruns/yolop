//! Persistent capability-server processes. One process per extension per
//! yolop process, spawned lazily on first use, kept for the session,
//! `kill_on_drop`, respawned with a fresh handshake on the call after a
//! crash — the same lifecycle policy `capabilities/lsp/manager.rs` applies
//! to language servers. Only this module knows about processes; everything
//! else drives the transport-generic `YepConnection`.

use super::client::YepConnection;
use super::package::ExtensionManifest;
use super::protocol::{InitializeParams, InitializeResult, PROTOCOL_VERSION};
use anyhow::{Context, Result, anyhow};
use serde_json::Value;
use std::collections::HashSet;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
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
            // stderr is the server's free log channel, never parsed; inherit
            // so `RUST_LOG`-style server logs land with yolop's own stderr.
            .stderr(Stdio::inherit())
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

        let init = InitializeParams {
            protocol_version: PROTOCOL_VERSION.to_string(),
            session_id: String::new(),
            workspace_root: self.spec.workspace_root.display().to_string(),
            config: self.spec.config.clone(),
            capabilities: vec!["cancel".into(), "streaming".into()],
        };
        let (connection, handshake) =
            YepConnection::connect(stdout, stdin, self.name(), init, self.spec.request_timeout)
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

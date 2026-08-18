//! ACP server: SDK transport/dispatch plus yolop session execution.
//!
//! yolop acts as an ACP *agent*: it reads newline-delimited JSON-RPC 2.0
//! messages from a client (an editor such as Zed) through the upstream ACP SDK
//! and drives the everruns runtime in response. [`serve`] is generic over byte
//! streams and a [`RuntimeFactory`], so the production binary wires it to real
//! stdin/stdout while tests drive it over in-memory pipes with a scripted
//! runtime.
//!
//! Concurrency model:
//!   * The SDK serialises outbound lines, dispatches typed requests, and
//!     correlates responses.
//!   * `session/prompt` runs in its own Tokio task, so `session/cancel`
//!     keeps flowing while a turn is in progress.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::time::Duration;

use agent_client_protocol::{Agent, Client, ConnectionTo, Lines, Responder};
use anyhow::Result;
use async_trait::async_trait;
use everruns_core::command::{CommandDescriptor, CommandSource, ExecuteCommandRequest};
use everruns_core::{ContentPart, ImageContentPart};
use everruns_core::{InputMessage, ScopedMcpServers};
use everruns_core::{McpServerTransportType, ScopedMcpServer};
use everruns_provider::tool_types::{
    ToolCall as RuntimeToolCall, ToolDefinition as RuntimeToolDefinition,
};
use everruns_provider::typed_id::SessionId as RuntimeSessionId;
use futures::{AsyncBufReadExt, AsyncWriteExt, StreamExt};
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{oneshot, watch};
use tokio::task::JoinHandle;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use crate::capabilities::{ApprovalDecision, ToolApprover};
use crate::config::{ApprovalMode, SettingsStore};
use crate::exec::worktree::WorktreeManager;
use crate::runtime::background_wake::{WakeReceiver, coalesce_pending_wakes, frame_wake_prompt};
use crate::runtime::{BuiltRuntime, ModelState, RuntimeHandles};
use crate::session_state::task_completion::{CompletionBudget, GateDecision};
use crate::session_state::user_ask::{AskOutcome, UserAskStore};

use super::bridge::{Translator, tool_kind};
use super::modes;
use super::protocol::{
    self, AgentCapabilities, AuthMethod, AuthenticateParams, AuthenticateResult, AvailableCommand,
    AvailableCommandInput, ConfigOptionUpdate, CurrentModeUpdate, InitializeParams,
    InitializeResult, LoadSessionParams, LoadSessionResult, McpCapabilities, McpServer,
    NewSessionParams, NewSessionResult, PermissionOption, PermissionOptionKind, PromptCapabilities,
    PromptParams, PromptResult, RequestPermissionOutcome, RequestPermissionParams, SessionConfigId,
    SessionConfigOption, SessionConfigOptionCategory, SessionConfigSelectOption,
    SessionNotification, SessionUpdate, SetSessionConfigOptionRequest,
    SetSessionConfigOptionResponse, SetSessionModeParams, SetSessionModeResult, StopReason,
    ToolCall, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields, ToolKind,
    UnstructuredCommandInput,
};

/// How often the prompt loop wakes to check whether the turn task finished,
/// in case the final event was already drained from the broadcast.
const TURN_POLL_INTERVAL: Duration = Duration::from_millis(150);

/// Builds a runtime for a freshly opened ACP session. Abstracted so tests can
/// substitute a scripted llmsim runtime for the real provider wiring.
#[async_trait]
pub trait RuntimeFactory: Send + Sync + 'static {
    fn session_exists(&self, session_id: RuntimeSessionId) -> bool;

    /// URL of a live loopback setup page, when this run serves one (the
    /// `--acp-setup-page` flag or the `acp_setup_page` setting). `None`
    /// disables the no-provider hint.
    async fn setup_page_url(&self) -> Option<String> {
        None
    }

    /// Resolve once the live setup page finishes; `true` when it connected a
    /// provider, so open sessions should refresh their model options.
    async fn wait_for_setup_page(&self) -> bool {
        false
    }

    fn auth_methods(&self) -> Vec<AuthMethod> {
        Vec::new()
    }

    async fn authenticate(&self, method_id: &str) -> Result<()> {
        anyhow::bail!("unknown authentication method `{method_id}`")
    }

    async fn build(
        &self,
        cwd: PathBuf,
        resume_session_id: Option<RuntimeSessionId>,
        client_mcp_servers: ScopedMcpServers,
        tool_approver: Option<Arc<dyn ToolApprover>>,
    ) -> Result<BuiltRuntime>;
}

/// Translate the ACP `mcpServers` list into the runtime's scoped MCP config.
///
/// yolop advertises `mcp_capabilities.http` and the always-mandatory stdio
/// transport, so only `http` and `stdio` entries are expected. An `sse` (or any
/// other) transport is rejected with `InvalidParams` rather than silently
/// dropped, so a client that ignored our capabilities gets a clear error
/// instead of a server that quietly went missing. Values are passed through
/// literally — the client already resolved any of its own placeholders.
fn scoped_mcp_servers_from_acp(
    servers: &[McpServer],
) -> std::result::Result<ScopedMcpServers, agent_client_protocol::Error> {
    let mut scoped = ScopedMcpServers::new();
    for server in servers {
        let (name, entry) = match server {
            McpServer::Http(http) => (
                http.name.clone(),
                ScopedMcpServer {
                    transport_type: McpServerTransportType::Http,
                    url: http.url.clone(),
                    headers: http
                        .headers
                        .iter()
                        .map(|h| (h.name.clone(), h.value.clone()))
                        .collect(),
                    ..ScopedMcpServer::default()
                },
            ),
            McpServer::Stdio(stdio) => (
                stdio.name.clone(),
                ScopedMcpServer {
                    transport_type: McpServerTransportType::Stdio,
                    command: Some(stdio.command.to_string_lossy().into_owned()),
                    args: stdio.args.clone(),
                    env: stdio
                        .env
                        .iter()
                        .map(|e| (e.name.clone(), e.value.clone()))
                        .collect(),
                    ..ScopedMcpServer::default()
                },
            ),
            other => {
                return Err(invalid_params(format!(
                    "unsupported mcp transport for server: {other:?}"
                )));
            }
        };
        scoped.insert(name, entry);
    }
    Ok(scoped)
}

/// SDK connection wrapper plus yolop-local ids for synthetic command tool calls.
struct Peer {
    cx: ConnectionTo<Client>,
    next_id: Arc<AtomicI64>,
}

impl Peer {
    fn session_update(&self, session_id: &str, update: SessionUpdate) {
        let notification = SessionNotification::new(session_id.to_string(), update);
        if let Err(err) = self.cx.send_notification(notification) {
            tracing::warn!(%err, "acp: failed to send session update");
        }
    }
}

/// Backs the tool-approval gate with the ACP client: each gated tool becomes a
/// `session/request_permission` the client renders, and the turn suspends on the
/// answer. Safe because the whole turn runs in a spawned task (see
/// `respond_prompt`), off the SDK event loop, so awaiting the client reply never
/// deadlocks dispatch.
struct AcpToolApprover {
    peer: Arc<Peer>,
}

/// Option ids for the four ACP permission choices, mapped back to a decision.
const PERMISSION_ALLOW_ONCE: &str = "allow_once";
const PERMISSION_ALLOW_ALWAYS: &str = "allow_always";
const PERMISSION_REJECT_ONCE: &str = "reject_once";
const PERMISSION_REJECT_ALWAYS: &str = "reject_always";

#[async_trait]
impl ToolApprover for AcpToolApprover {
    async fn approve(
        &self,
        session_id: RuntimeSessionId,
        tool_call: &RuntimeToolCall,
        tool_def: &RuntimeToolDefinition,
    ) -> ApprovalDecision {
        let fields = ToolCallUpdateFields::new()
            .title(tool_def.name().to_string())
            .kind(tool_kind(&tool_call.name))
            .status(ToolCallStatus::Pending);
        let request = RequestPermissionParams::new(
            session_id.to_string(),
            ToolCallUpdate::new(tool_call.id.clone(), fields),
            vec![
                PermissionOption::new(
                    PERMISSION_ALLOW_ONCE,
                    "Allow",
                    PermissionOptionKind::AllowOnce,
                ),
                PermissionOption::new(
                    PERMISSION_ALLOW_ALWAYS,
                    "Allow, don't ask again",
                    PermissionOptionKind::AllowAlways,
                ),
                PermissionOption::new(
                    PERMISSION_REJECT_ONCE,
                    "Reject",
                    PermissionOptionKind::RejectOnce,
                ),
                PermissionOption::new(
                    PERMISSION_REJECT_ALWAYS,
                    "Reject, don't ask again",
                    PermissionOptionKind::RejectAlways,
                ),
            ],
        );

        match self.peer.cx.send_request(request).block_task().await {
            Ok(response) => match response.outcome {
                RequestPermissionOutcome::Cancelled => ApprovalDecision::Cancelled,
                RequestPermissionOutcome::Selected(selected) => {
                    match selected.option_id.to_string().as_str() {
                        PERMISSION_ALLOW_ONCE => ApprovalDecision::Allow,
                        PERMISSION_ALLOW_ALWAYS => ApprovalDecision::AllowAlways,
                        PERMISSION_REJECT_ONCE => ApprovalDecision::Reject,
                        PERMISSION_REJECT_ALWAYS => ApprovalDecision::RejectAlways,
                        other => {
                            tracing::warn!(option = other, "acp: unknown permission option");
                            ApprovalDecision::Reject
                        }
                    }
                }
                // `RequestPermissionOutcome` is non-exhaustive: treat any future
                // variant as a rejection rather than silently allowing.
                _ => ApprovalDecision::Reject,
            },
            Err(err) => {
                // The client could not answer (no permission UI, or the
                // connection is winding down). Fall back to allowing so a client
                // without `session/request_permission` keeps working rather than
                // having every mutating tool blocked; the soft-approval prompt
                // still nudges the model.
                tracing::warn!(%err, "acp: request_permission failed; allowing tool");
                ApprovalDecision::Unavailable
            }
        }
    }
}

/// State for one open ACP session: the runtime handles plus a one-shot cancel
/// channel armed for the duration of each in-flight prompt.
struct Session {
    acp_id: String,
    handles: RuntimeHandles,
    model: ModelState,
    worktree: Arc<WorktreeManager>,
    commands: StdMutex<Vec<CommandDescriptor>>,
    cancel: StdMutex<Option<oneshot::Sender<()>>>,
    /// Settings source, read for the `proactive_wake` opt-out and the
    /// approval-level ↔ session-mode mapping.
    settings: Arc<SettingsStore>,
    goal_store: Arc<crate::session_state::goal::GoalStore>,
    /// Last approval level reported to the client as the current session mode.
    /// Compared after each turn so a level changed out of band (the
    /// `set_approval_mode` tool, `/setup approval`) surfaces as a
    /// `current_mode_update` instead of silently drifting from the picker.
    last_mode: StdMutex<ApprovalMode>,
    /// Retained for the ACP session lifetime so due local schedules keep polling.
    _schedule_runner: everruns::local::LocalScheduleRunnerHandle,
    user_ask_store: Arc<UserAskStore>,
    user_ask_enabled: bool,
    task_registry: Arc<dyn everruns_core::session_task::SessionTaskRegistry>,
    completion_budget: StdMutex<CompletionBudget>,
    /// Serializes turns for this session. Both a client prompt and a background
    /// wake turn take it, so two `run_turn`s never overlap.
    turn_lock: tokio::sync::Mutex<()>,
}

impl Session {
    /// Arm a fresh cancel channel for a new prompt, returning the receiver the
    /// prompt loop selects on. Replaces any stale sender.
    fn arm_cancel(&self) -> oneshot::Receiver<()> {
        let (tx, rx) = oneshot::channel();
        *self.cancel.lock().unwrap() = Some(tx);
        rx
    }

    fn trigger_cancel(&self) {
        if let Some(tx) = self.cancel.lock().unwrap().take() {
            let _ = tx.send(());
        }
    }
}

struct Server<F: RuntimeFactory> {
    factory: Arc<F>,
    sessions: StdMutex<HashMap<String, Arc<Session>>>,
    /// Flipped to `true` when the connection winds down, so each session's wake
    /// poller exits instead of looping against a dead client.
    shutdown: watch::Receiver<bool>,
    /// Handles to the per-session wake pollers. `serve` awaits these on teardown
    /// so each poller drops its `Arc<Session>` (and the runtime it keeps alive)
    /// *before* `serve` returns. Without that join, a poller could outlive the
    /// connection and hold the session open while a later `serve` loads the same
    /// session id from disk — a data race on the session's on-disk state.
    poller_handles: StdMutex<Vec<JoinHandle<()>>>,
    /// Whether a task is already awaiting the loopback setup page, so several
    /// sessions opened with nothing connected share one watcher.
    setup_page_watch: AtomicBool,
}

impl<F: RuntimeFactory> Server<F> {
    fn session(&self, id: &str) -> Option<Arc<Session>> {
        self.sessions.lock().unwrap().get(id).cloned()
    }

    fn sessions(&self) -> Vec<Arc<Session>> {
        self.sessions.lock().unwrap().values().cloned().collect()
    }
}

/// Run the ACP agent over the given byte streams until the client closes its
/// end (EOF on `reader`). Returns once the SDK connection winds down.
pub async fn serve<R, W, F>(reader: R, writer: W, factory: Arc<F>) -> Result<()>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
    F: RuntimeFactory,
{
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let server = Arc::new(Server {
        factory,
        sessions: StdMutex::new(HashMap::new()),
        shutdown: shutdown_rx,
        poller_handles: StdMutex::new(Vec::new()),
        setup_page_watch: AtomicBool::new(false),
    });
    let next_tool_id = Arc::new(AtomicI64::new(1));
    let (eof_tx, eof_rx) = oneshot::channel::<()>();
    let incoming_lines = futures::io::BufReader::new(reader.compat()).lines();
    let incoming = futures::stream::unfold(
        (incoming_lines, Some(eof_tx)),
        |(mut lines, mut eof_tx)| async move {
            match lines.next().await {
                Some(line) => Some((line, (lines, eof_tx))),
                None => {
                    if let Some(tx) = eof_tx.take() {
                        let _ = tx.send(());
                    }
                    None
                }
            }
        },
    );
    let outgoing = futures::sink::unfold(
        writer.compat_write(),
        async move |mut writer, line: String| {
            writer.write_all(line.as_bytes()).await?;
            writer.write_all(b"\n").await?;
            writer.flush().await?;
            Ok::<_, std::io::Error>(writer)
        },
    );
    let transport = Lines::new(outgoing, incoming);

    let result = Agent
        .builder()
        .name("yolop")
        .on_receive_request(
            {
                let server = server.clone();
                async move |params: InitializeParams, responder, _cx| {
                    responder.respond(handle_initialize(params, server.factory.auth_methods()))
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let server = server.clone();
                let next_tool_id = next_tool_id.clone();
                async move |params: AuthenticateParams, responder, cx| {
                    let method_id = params.method_id.to_string();
                    if !server
                        .factory
                        .auth_methods()
                        .iter()
                        .any(|method| method.id().to_string() == method_id)
                    {
                        responder.respond_with_error(invalid_params(format!(
                            "unknown authentication method `{method_id}`"
                        )))?;
                        return Ok(());
                    }
                    match server.factory.authenticate(&method_id).await {
                        Ok(()) => {
                            responder.respond(AuthenticateResult::new())?;
                            let peer = Peer {
                                cx: cx.clone(),
                                next_id: next_tool_id.clone(),
                            };
                            for session in server.sessions() {
                                emit_config_options(&peer, &session).await;
                            }
                        }
                        Err(err) => responder
                            .respond_with_error(internal_error(format!("authenticate: {err}")))?,
                    }
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let server = server.clone();
                let next_tool_id = next_tool_id.clone();
                async move |params: NewSessionParams, responder, cx| {
                    let peer = Arc::new(Peer {
                        cx: cx.clone(),
                        next_id: next_tool_id.clone(),
                    });
                    match handle_new_session(&server, &peer, params).await {
                        Ok(result) => {
                            let session_id = result.session_id.to_string();
                            responder.respond(result)?;
                            if let Some(session) = server.session(&session_id) {
                                let commands = session.commands.lock().unwrap().clone();
                                notify_available_commands(&peer, &session_id, &commands);
                                offer_setup_page(&server, &peer, &session).await;
                            }
                        }
                        Err(err) => responder.respond_with_error(err)?,
                    }
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let server = server.clone();
                let next_tool_id = next_tool_id.clone();
                async move |params: LoadSessionParams, responder, cx| {
                    let peer = Arc::new(Peer {
                        cx: cx.clone(),
                        next_id: next_tool_id.clone(),
                    });
                    match handle_load_session(&server, &peer, params).await {
                        Ok((result, session_id)) => {
                            responder.respond(result)?;
                            if let Some(session) = server.session(&session_id) {
                                let commands = session.commands.lock().unwrap().clone();
                                notify_available_commands(&peer, &session_id, &commands);
                            }
                        }
                        Err(err) => responder.respond_with_error(err)?,
                    }
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let server = server.clone();
                let next_tool_id = next_tool_id.clone();
                async move |params: PromptParams, responder, cx| {
                    let peer = Arc::new(Peer {
                        cx: cx.clone(),
                        next_id: next_tool_id.clone(),
                    });
                    tokio::spawn({
                        let server = server.clone();
                        async move {
                            respond_prompt(&server, peer, params, responder).await;
                        }
                    });
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let server = server.clone();
                async move |params: SetSessionModeParams, responder, _cx| {
                    match apply_set_mode(&server, &params) {
                        Ok(()) => responder.respond(SetSessionModeResult::new())?,
                        Err(err) => responder.respond_with_error(err)?,
                    }
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let server = server.clone();
                async move |params: SetSessionConfigOptionRequest, responder, _cx| {
                    match apply_set_config_option(&server, &params).await {
                        Ok(result) => responder.respond(result)?,
                        Err(err) => responder.respond_with_error(err)?,
                    }
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            {
                let server = server.clone();
                async move |params: protocol::CancelNotification, _cx| {
                    if let Some(session) = server.session(&params.session_id.to_string()) {
                        session.trigger_cancel();
                    }
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_with(transport, async move |_cx| {
            let _ = eof_rx.await;
            Ok(())
        })
        .await;

    // The connection has wound down: stop every session's wake poller so no
    // detached task keeps polling (and sending to a dead client) after return,
    // then join them so each drops its `Arc<Session>` — and the runtime it holds
    // open — before `serve` returns.
    let _ = shutdown_tx.send(true);
    let pollers: Vec<JoinHandle<()>> = server.poller_handles.lock().unwrap().drain(..).collect();
    for poller in pollers {
        let _ = poller.await;
    }

    match result {
        Ok(()) => Ok(()),
        Err(err) if is_client_disconnect_error(&err) => {
            tracing::debug!(%err, "acp: client disconnected while transport was closing");
            Ok(())
        }
        Err(err) => Err(err.into()),
    }
}

fn invalid_params(message: impl Into<String>) -> agent_client_protocol::Error {
    agent_client_protocol::Error::invalid_params().data(message.into())
}

fn internal_error(message: impl Into<String>) -> agent_client_protocol::Error {
    agent_client_protocol::Error::internal_error().data(message.into())
}

fn is_client_disconnect_error(err: &agent_client_protocol::Error) -> bool {
    err.code == agent_client_protocol::ErrorCode::InternalError
        && err.data.as_ref().is_some_and(value_mentions_broken_pipe)
}

fn value_mentions_broken_pipe(value: &Value) -> bool {
    match value {
        Value::String(text) => text.to_ascii_lowercase().contains("broken pipe"),
        Value::Array(values) => values.iter().any(value_mentions_broken_pipe),
        Value::Object(map) => map.values().any(value_mentions_broken_pipe),
        _ => false,
    }
}

fn handle_initialize(params: InitializeParams, auth_methods: Vec<AuthMethod>) -> InitializeResult {
    // Echo a supported version: honour the client's request when it is one we
    // speak, otherwise advertise our own.
    let version = match params.protocol_version {
        v if v == protocol::PROTOCOL_VERSION => v,
        _ => protocol::PROTOCOL_VERSION,
    };
    InitializeResult::new(version)
        .auth_methods(auth_methods)
        .agent_capabilities(
            AgentCapabilities::new()
                .load_session(true)
                .prompt_capabilities(
                    PromptCapabilities::new()
                        .image(true)
                        .audio(false)
                        .embedded_context(true),
                )
                // Client-configured MCP servers: `http` is advertised; the `stdio`
                // transport is mandatory for all agents and needs no flag. `sse` is
                // not advertised (the runtime has no SSE transport).
                .mcp_capabilities(McpCapabilities::new().http(true))
                .meta(protocol::meta(json!({
                    "yolop.dev/acp": {
                        "commandMetadata": true,
                        "commandArgSuggestions": true,
                        "commandToolLifecycle": true
                    }
                }))),
        )
}

async fn handle_new_session<F: RuntimeFactory>(
    server: &Arc<Server<F>>,
    peer: &Arc<Peer>,
    params: NewSessionParams,
) -> std::result::Result<NewSessionResult, agent_client_protocol::Error> {
    let cwd = params.cwd;
    let client_mcp_servers = scoped_mcp_servers_from_acp(&params.mcp_servers)?;
    let tool_approver: Option<Arc<dyn ToolApprover>> =
        Some(Arc::new(AcpToolApprover { peer: peer.clone() }));

    let built = server
        .factory
        .build(cwd, None, client_mcp_servers, tool_approver)
        .await
        .map_err(|e| internal_error(format!("build runtime: {e}")))?;

    let mode = built.settings.snapshot().approval_mode();
    let acp_id = register_session(server, peer, built);
    let session = server
        .session(&acp_id)
        .ok_or_else(|| internal_error("new session was not registered"))?;

    Ok(NewSessionResult::new(acp_id)
        .modes(modes::session_mode_state(mode))
        .config_options(session_config_options(&session.model).await))
}

async fn handle_load_session<F: RuntimeFactory>(
    server: &Arc<Server<F>>,
    peer: &Arc<Peer>,
    params: LoadSessionParams,
) -> std::result::Result<(LoadSessionResult, String), agent_client_protocol::Error> {
    let requested_id = params.session_id.to_string();
    let resume_session_id = requested_id
        .parse::<RuntimeSessionId>()
        .map_err(|e| invalid_params(format!("invalid session id `{requested_id}`: {e}")))?;

    let session = match server.session(&requested_id) {
        Some(session) => session,
        None => {
            if !server.factory.session_exists(resume_session_id) {
                return Err(invalid_params(format!(
                    "unknown session id `{requested_id}`"
                )));
            }
            let client_mcp_servers = scoped_mcp_servers_from_acp(&params.mcp_servers)?;
            let tool_approver: Option<Arc<dyn ToolApprover>> =
                Some(Arc::new(AcpToolApprover { peer: peer.clone() }));
            let built = server
                .factory
                .build(
                    params.cwd,
                    Some(resume_session_id),
                    client_mcp_servers,
                    tool_approver,
                )
                .await
                .map_err(|e| internal_error(format!("load runtime: {e}")))?;
            let acp_id = register_session(server, peer, built);
            server
                .session(&acp_id)
                .ok_or_else(|| internal_error("loaded session was not registered"))?
        }
    };

    replay_session_history(peer, &session).await?;
    let mode = session.settings.snapshot().approval_mode();
    Ok((
        LoadSessionResult::new()
            .modes(modes::session_mode_state(mode))
            .config_options(session_config_options(&session.model).await),
        session.acp_id.clone(),
    ))
}

fn register_session<F: RuntimeFactory>(
    server: &Arc<Server<F>>,
    peer: &Arc<Peer>,
    built: BuiltRuntime,
) -> String {
    let acp_id = built.handles.session_id.to_string();
    let commands = built.startup.capability_commands.clone();
    let user_ask_store = built.user_ask_store.clone();
    let user_ask_enabled = built.user_ask_enabled;
    let task_registry = built.task_registry.clone();
    let session = Arc::new(Session {
        acp_id: acp_id.clone(),
        handles: built.handles,
        model: built.model,
        worktree: built.worktree,
        commands: StdMutex::new(commands.clone()),
        cancel: StdMutex::new(None),
        last_mode: StdMutex::new(built.settings.snapshot().approval_mode()),
        settings: built.settings,
        goal_store: built.goal_store,
        _schedule_runner: built.schedule_runner,
        user_ask_store,
        user_ask_enabled,
        task_registry,
        completion_budget: StdMutex::new(CompletionBudget::default()),
        turn_lock: tokio::sync::Mutex::new(()),
    });
    server
        .sessions
        .lock()
        .unwrap()
        .insert(acp_id.clone(), session.clone());

    let wake_drain = spawn_background_wake_drain(
        session,
        peer.clone(),
        built.background_wake,
        server.shutdown.clone(),
    );
    server.poller_handles.lock().unwrap().push(wake_drain);

    acp_id
}

/// Drain this session's everruns `spawn_background` completion wakes and drive a
/// streamed turn for each. The ACP request/response loop only runs turns while a
/// client prompt is in flight, so — unlike the TUI's idle event loop — nothing
/// otherwise reacts to a background task finishing between prompts. This closes
/// that gap: it awaits the wake channel (fed by the platform-store wake seam,
/// `crate::runtime::background_wake`) and takes the same `turn_lock` as client prompts so
/// a wake turn never overlaps one. Stops on connection teardown or when the
/// runtime (and its wake sender) drops. See knowledge/specs/background.md.
fn spawn_background_wake_drain(
    session: Arc<Session>,
    peer: Arc<Peer>,
    mut wake_rx: WakeReceiver,
    mut shutdown: watch::Receiver<bool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let message = tokio::select! {
                _ = shutdown.changed() => break,
                recv = wake_rx.recv() => match recv {
                    Some(message) => message,
                    None => break,
                },
            };
            if *shutdown.borrow() {
                break;
            }
            // Serialize with client prompts so two turns never overlap.
            let _turn = session.turn_lock.lock().await;
            // Completions that accumulated while the foreground turn held the
            // lock are one observation point, not separate model obligations.
            let message = coalesce_pending_wakes(message, &mut wake_rx)
                .with_active_goal(
                    session
                        .goal_store
                        .active_condition(session.handles.session_id),
                )
                .with_active_ask(
                    session
                        .user_ask_store
                        .active_text(session.handles.session_id),
                );
            if !message.is_coordination() && !session.settings.snapshot().proactive_wake_enabled() {
                peer.session_update(
                    &session.acp_id,
                    SessionUpdate::AgentMessageChunk(protocol::text_chunk(
                        "✓ background task finished — see /background (proactive wake off)",
                    )),
                );
                continue;
            }
            peer.session_update(
                &session.acp_id,
                SessionUpdate::AgentMessageChunk(protocol::text_chunk(message.notice())),
            );
            let prompt = frame_wake_prompt(&message);
            let input = crate::runtime::background_wake::input_for_wake(&message);
            run_prompt(peer.clone(), session.clone(), prompt, input).await;
        }
    })
}

async fn replay_session_history(
    peer: &Arc<Peer>,
    session: &Arc<Session>,
) -> std::result::Result<(), agent_client_protocol::Error> {
    let events = session
        .handles
        .runtime
        .events()
        .await
        .map_err(|e| internal_error(format!("load session history: {e}")))?;
    let mut translator = Translator::for_replay();
    for event in events {
        if event.session_id != session.handles.session_id {
            continue;
        }
        for update in translator.on_event(&event) {
            peer.session_update(&session.acp_id, update);
        }
    }
    Ok(())
}

const MODEL_CONFIG_ID: &str = "model";
const REASONING_EFFORT_CONFIG_ID: &str = "reasoning_effort";

async fn session_config_options(model: &ModelState) -> Vec<SessionConfigOption> {
    let model_options = model
        .model_options()
        .await
        .into_iter()
        .map(|(value, name, group)| {
            SessionConfigSelectOption::new(value, format!("{group}: {name}"))
        })
        .collect::<Vec<_>>();
    let selected_model = format!("{}:{}", model.provider_name(), model.model_id());
    let mut options = vec![
        SessionConfigOption::select(
            SessionConfigId::new(MODEL_CONFIG_ID),
            "Model",
            selected_model,
            model_options,
        )
        .category(SessionConfigOptionCategory::Model),
    ];

    let efforts = model.reasoning_effort_options();
    if !efforts.is_empty() {
        let selected = model
            .reasoning_effort()
            .or_else(|| model.default_reasoning_effort())
            .unwrap_or_else(|| efforts[0].value.clone());
        let effort_options = efforts
            .into_iter()
            .map(|effort| SessionConfigSelectOption::new(effort.value, effort.label))
            .collect::<Vec<_>>();
        options.push(
            SessionConfigOption::select(
                SessionConfigId::new(REASONING_EFFORT_CONFIG_ID),
                "Reasoning effort",
                selected,
                effort_options,
            )
            .category(SessionConfigOptionCategory::ThoughtLevel),
        );
    }
    options
}

async fn apply_set_config_option<F: RuntimeFactory>(
    server: &Arc<Server<F>>,
    params: &SetSessionConfigOptionRequest,
) -> std::result::Result<SetSessionConfigOptionResponse, agent_client_protocol::Error> {
    let session = server
        .session(&params.session_id.to_string())
        .ok_or_else(|| invalid_params("unknown session id"))?;
    let value = params
        .value
        .as_value_id()
        .ok_or_else(|| invalid_params("session config option requires a value id"))?;
    match params.config_id.0.as_ref() {
        MODEL_CONFIG_ID => session.model.select_model_id(value.0.as_ref()).await,
        REASONING_EFFORT_CONFIG_ID => {
            session
                .model
                .select_reasoning_effort(value.0.as_ref())
                .await
        }
        other => Err(anyhow::anyhow!("unknown session config option `{other}`")),
    }
    .map_err(|err| invalid_params(err.to_string()))?;

    Ok(SetSessionConfigOptionResponse::new(
        session_config_options(&session.model).await,
    ))
}

async fn handle_prompt<F: RuntimeFactory>(
    server: &Arc<Server<F>>,
    peer: Arc<Peer>,
    params: PromptParams,
) -> std::result::Result<PromptResult, agent_client_protocol::Error> {
    let session_id = params.session_id.to_string();
    let session = server
        .session(&session_id)
        .ok_or_else(|| invalid_params("unknown session id"))?;
    let prompt = protocol::prompt_text(&params.prompt);
    let input = prompt_input(&session.model, &params.prompt);

    // Serialize with any proactive background wake turn (and any other in-flight
    // prompt) so two turns never run for one session at once. Held for the whole
    // dispatch; `run_prompt` does not take the lock itself, so the poller can
    // reuse it under its own guard.
    let _turn = session.turn_lock.lock().await;
    let mode_peer = peer.clone();
    let parsed_command = parse_command_prompt(&prompt);
    if parsed_command.is_none() && session.user_ask_enabled {
        if let Err(err) = session
            .user_ask_store
            .record_user_prompt(session.handles.session_id, &prompt)
        {
            tracing::warn!(%err, "acp: record user ask failed");
        }
        session.completion_budget.lock().unwrap().reset();
    }
    let stop_reason = match parsed_command {
        Some(command) => run_slash_command(server, peer, session.clone(), command).await,
        None => run_prompt(peer, session.clone(), prompt, input).await,
    };
    // A level changed mid-turn (the `set_approval_mode` tool, `/setup approval`)
    // must reach the client's mode picker, not just the settings file.
    emit_mode_change_if_needed(&mode_peer, &session);
    Ok(PromptResult::new(stop_reason))
}

/// Persist a client `session/set_mode` by mapping the mode id to an approval
/// level (globally — see [`modes`]). Refreshes `last_mode` so the change the
/// client just made is not re-echoed as an out-of-band `current_mode_update`.
fn apply_set_mode<F: RuntimeFactory>(
    server: &Arc<Server<F>>,
    params: &SetSessionModeParams,
) -> std::result::Result<(), agent_client_protocol::Error> {
    let session = server
        .session(&params.session_id.to_string())
        .ok_or_else(|| invalid_params("unknown session id"))?;
    let mode = modes::approval_mode_from_id(&params.mode_id.to_string())
        .ok_or_else(|| invalid_params(format!("unknown session mode `{}`", params.mode_id)))?;
    session
        .settings
        .set_approval_mode(mode)
        .map_err(|e| internal_error(format!("set approval mode: {e}")))?;
    *session.last_mode.lock().unwrap() = mode;
    Ok(())
}

/// Push a `current_mode_update` when the approval level changed out of band
/// since it was last reported (the `set_approval_mode` tool, `/setup approval`),
/// so the client's mode picker stays in sync with the setting.
fn emit_mode_change_if_needed(peer: &Peer, session: &Session) {
    let current = session.settings.snapshot().approval_mode();
    let changed = {
        let mut last = session.last_mode.lock().unwrap();
        let changed = *last != current;
        *last = current;
        changed
    };
    if changed {
        peer.session_update(
            &session.acp_id,
            SessionUpdate::CurrentModeUpdate(CurrentModeUpdate::new(modes::mode_id(current))),
        );
    }
}

async fn emit_config_options(peer: &Peer, session: &Session) {
    peer.session_update(
        &session.acp_id,
        SessionUpdate::ConfigOptionUpdate(ConfigOptionUpdate::new(
            session_config_options(&session.model).await,
        )),
    );
}

async fn respond_prompt<F: RuntimeFactory>(
    server: &Arc<Server<F>>,
    peer: Arc<Peer>,
    params: PromptParams,
    responder: Responder<PromptResult>,
) {
    match handle_prompt(server, peer, params).await {
        Ok(result) => {
            let _ = responder.respond(result);
        }
        Err(err) => {
            let _ = responder.respond_with_error(err);
        }
    }
}

fn available_commands(commands: &[CommandDescriptor]) -> Vec<AvailableCommand> {
    commands
        .iter()
        .map(|command| {
            AvailableCommand::new(command.name.clone(), command.description.clone())
                .input(command_input(command))
                .meta(command_meta(command))
        })
        .collect()
}

fn command_input(command: &CommandDescriptor) -> Option<AvailableCommandInput> {
    if command.args.is_empty() {
        return None;
    }
    let hint = command
        .args
        .iter()
        .map(|arg| format!("<{}>", arg.name))
        .collect::<Vec<_>>()
        .join(" ");
    Some(AvailableCommandInput::Unstructured(
        UnstructuredCommandInput::new(hint),
    ))
}

fn notify_available_commands(peer: &Arc<Peer>, session_id: &str, commands: &[CommandDescriptor]) {
    peer.session_update(
        session_id,
        SessionUpdate::AvailableCommandsUpdate(
            protocol::AvailableCommandsUpdate::new(available_commands(commands)).meta(
                protocol::meta(json!({
                    "yolop.dev/acp": {
                        "argSuggestions": true
                    }
                })),
            ),
        ),
    );
}

fn command_meta(command: &CommandDescriptor) -> Option<serde_json::Map<String, Value>> {
    if command.args.is_empty() {
        return None;
    }
    let source = match command.source {
        CommandSource::System => "system",
        CommandSource::Skill => "skill",
    };
    protocol::meta(json!({
        "yolop.dev/command": {
            "source": source,
            "args": command.args.iter().map(|arg| {
                let mut suggestions = arg.suggestions.clone();
                if command.name == "setup" && !suggestions.iter().any(|value| value == "login") {
                    suggestions.insert(0, "login".to_string());
                }
                json!({
                    "name": arg.name,
                    "description": arg.description,
                    "required": arg.required,
                    "suggestions": suggestions,
                })
            }).collect::<Vec<_>>()
        }
    }))
}

#[derive(Debug, PartialEq, Eq)]
struct ParsedCommand {
    name: String,
    args: String,
    title: String,
}

fn parse_command_prompt(prompt: &str) -> Option<ParsedCommand> {
    let trimmed = prompt.trim();
    if let Some(rest) = trimmed.strip_prefix('/') {
        return parse_slash_command(rest.trim_start());
    }
    if let Some(rest) = trimmed.strip_prefix('!') {
        return Some(parse_shell_shortcut(rest));
    }
    None
}

fn parse_slash_command(rest: &str) -> Option<ParsedCommand> {
    let mut parts = rest.splitn(2, char::is_whitespace);
    let name = parts.next()?.trim();
    if name.is_empty() {
        return None;
    }
    let args = parts.next().unwrap_or_default().trim();
    Some(ParsedCommand {
        name: name.to_string(),
        args: args.to_string(),
        title: command_title("/", name, args),
    })
}

fn parse_shell_shortcut(rest: &str) -> ParsedCommand {
    let args = rest
        .trim_start()
        .strip_prefix("shell")
        .and_then(|tail| {
            tail.chars()
                .next()
                .is_none_or(char::is_whitespace)
                .then_some(tail)
        })
        .unwrap_or(rest)
        .trim();
    ParsedCommand {
        name: "shell".to_string(),
        args: args.to_string(),
        title: command_title("!", "shell", args),
    }
}

async fn run_slash_command<F: RuntimeFactory>(
    server: &Arc<Server<F>>,
    peer: Arc<Peer>,
    session: Arc<Session>,
    command: ParsedCommand,
) -> StopReason {
    let name = command.name;
    let args = command.args;
    let title = command.title;
    if name == "setup"
        && let Some(stop_reason) = run_setup_login(server, &peer, &session, &args).await
    {
        return stop_reason;
    }
    if name == "setup" && args.split_whitespace().next() == Some("token") {
        let setup_page_offered = server
            .factory
            .auth_methods()
            .iter()
            .any(|method| method.id().to_string() == super::SETUP_PAGE_AUTH_METHOD);
        let guidance = if setup_page_offered {
            "API keys cannot be entered over ACP because the protocol does not provide secure secret input; run `/setup login` to open the local setup page instead"
        } else {
            "API keys cannot be entered over ACP because the protocol does not provide secure secret input; configure the provider key in the agent process environment"
        };
        peer.session_update(
            &session.acp_id,
            SessionUpdate::AgentMessageChunk(protocol::text_chunk(guidance)),
        );
        return StopReason::EndTurn;
    }
    let commands = session.commands.lock().unwrap().clone();
    let Some(descriptor) = commands.iter().find(|c| c.name == name).cloned() else {
        peer.session_update(
            &session.acp_id,
            SessionUpdate::AgentMessageChunk(protocol::text_chunk(format!(
                "unknown command: /{name}"
            ))),
        );
        return StopReason::EndTurn;
    };

    let required_missing = descriptor
        .args
        .iter()
        .any(|a| a.required && args.is_empty());
    if required_missing {
        let needed = descriptor
            .args
            .iter()
            .filter(|a| a.required)
            .map(|a| a.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        peer.session_update(
            &session.acp_id,
            SessionUpdate::AgentMessageChunk(protocol::text_chunk(format!(
                "/{name} requires: {needed}"
            ))),
        );
        return StopReason::EndTurn;
    }

    match descriptor.source {
        CommandSource::System => {
            let tool_call_id = format!("command_{}", peer.next_id.fetch_add(1, Ordering::Relaxed));
            peer.session_update(
                &session.acp_id,
                SessionUpdate::ToolCall(
                    ToolCall::new(tool_call_id.clone(), title)
                        .kind(ToolKind::Execute)
                        .status(ToolCallStatus::InProgress)
                        .raw_input(json!({
                        "command": descriptor.name,
                        "arguments": if args.is_empty() { Value::Null } else { Value::String(args.clone()) },
                        "source": "system",
                    })),
                ),
            );

            let request = ExecuteCommandRequest {
                name: descriptor.name.clone(),
                arguments: if args.is_empty() { None } else { Some(args) },
                controls: None,
            };
            let (success, message, raw_output) = match session
                .handles
                .runtime
                .execute_command(session.handles.session_id, request)
                .await
            {
                Ok(result) => {
                    let prefix = if result.success { "" } else { "error: " };
                    (
                        result.success,
                        format!("{prefix}{}", result.message),
                        serde_json::to_value(result).expect("command result serializes"),
                    )
                }
                Err(err) => (
                    false,
                    format!("/{name} failed: {err}"),
                    json!({ "success": false, "message": format!("{err}") }),
                ),
            };
            peer.session_update(
                &session.acp_id,
                SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                    tool_call_id,
                    ToolCallUpdateFields::new()
                        .status(if success {
                            ToolCallStatus::Completed
                        } else {
                            ToolCallStatus::Failed
                        })
                        .content(vec![protocol::content(message)])
                        .raw_output(raw_output),
                )),
            );
            refresh_available_commands(&peer, &session).await;
            emit_config_options(&peer, &session).await;
            StopReason::EndTurn
        }
        CommandSource::Skill => {
            let text = if args.is_empty() {
                format!("/{name}")
            } else {
                format!("/{name} {args}")
            };
            let input = session.model.input_message(text.clone());
            run_prompt(peer, session, text, input).await
        }
    }
}

/// ACP has no secure generic setup form, but it does have agent-handled auth
/// methods. Treat plain `/setup` as the host's guided entry point, selecting
/// the current provider's method when several are advertised, and expose
/// `/setup login` as the explicit form.
fn select_setup_auth_method(
    methods: &[AuthMethod],
    requested: Option<&str>,
    active_provider: &str,
) -> std::result::Result<Option<String>, String> {
    let available = methods
        .iter()
        .map(|method| method.id().to_string())
        .collect::<Vec<_>>();
    if let Some(id) = requested {
        return available
            .contains(&id.to_string())
            .then(|| Some(id.to_string()))
            .ok_or_else(|| {
                format!(
                    "unknown authentication method `{id}`; available: {}",
                    available.join(", ")
                )
            });
    }

    let active_method_id = format!("{active_provider}_browser");
    if available.contains(&active_method_id) {
        return Ok(Some(active_method_id));
    }
    // The setup page covers every provider, so it is the right default once the
    // active provider has no login of its own.
    if available
        .iter()
        .any(|id| id == super::SETUP_PAGE_AUTH_METHOD)
    {
        return Ok(Some(super::SETUP_PAGE_AUTH_METHOD.to_string()));
    }
    if let [method_id] = available.as_slice() {
        return Ok(Some(method_id.clone()));
    }
    Ok(None)
}

async fn run_setup_login<F: RuntimeFactory>(
    server: &Arc<Server<F>>,
    peer: &Arc<Peer>,
    session: &Arc<Session>,
    args: &str,
) -> Option<StopReason> {
    let mut parts = args.split_whitespace();
    let requested = match parts.next() {
        None => None,
        Some("login" | "reauthenticate") => parts.next(),
        Some(_) => return None,
    };
    let methods = server.factory.auth_methods();
    if methods.is_empty() {
        return None;
    }
    let method_id =
        match select_setup_auth_method(&methods, requested, &session.model.provider_name()) {
            Ok(Some(method_id)) => method_id,
            Err(message) => {
                peer.session_update(
                    &session.acp_id,
                    SessionUpdate::AgentMessageChunk(protocol::text_chunk(message)),
                );
                return Some(StopReason::EndTurn);
            }
            Ok(None) => {
                peer.session_update(
                    &session.acp_id,
                    SessionUpdate::AgentMessageChunk(protocol::text_chunk(format!(
                        "choose an authentication method with `/setup login <method>`: {}",
                        methods
                            .iter()
                            .map(|method| method.id().to_string())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ))),
                );
                return Some(StopReason::EndTurn);
            }
        };

    let tool_call_id = format!("command_{}", peer.next_id.fetch_add(1, Ordering::Relaxed));
    peer.session_update(
        &session.acp_id,
        SessionUpdate::ToolCall(
            ToolCall::new(tool_call_id.clone(), "/setup login")
                .kind(ToolKind::Execute)
                .status(ToolCallStatus::InProgress)
                .raw_input(json!({
                    "command": "setup",
                    "arguments": "login",
                    "source": "system",
                    "authenticationMethod": method_id,
                })),
        ),
    );

    let result = server.factory.authenticate(&method_id).await;
    let (success, message) = match &result {
        Ok(()) => (
            true,
            "Authentication refreshed. Retry your request.".to_string(),
        ),
        Err(err) => (false, format!("Authentication failed: {err}")),
    };
    peer.session_update(
        &session.acp_id,
        SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
            tool_call_id,
            ToolCallUpdateFields::new()
                .status(if success {
                    ToolCallStatus::Completed
                } else {
                    ToolCallStatus::Failed
                })
                .content(vec![protocol::content(message.clone())])
                .raw_output(json!({ "success": success, "message": message })),
        )),
    );
    if success {
        for open_session in server.sessions() {
            emit_config_options(peer, &open_session).await;
        }
    }
    Some(StopReason::EndTurn)
}

/// With nothing connected, an ACP session can only run the offline `llmsim`
/// model, and the protocol gives the client no way to type an API key. Post the
/// local setup page so the conversation itself carries a way out. A run with the
/// page turned off serves no URL, so this is a no-op there.
async fn offer_setup_page<F: RuntimeFactory>(
    server: &Arc<Server<F>>,
    peer: &Arc<Peer>,
    session: &Arc<Session>,
) {
    if crate::capabilities::model_discovery::any_provider_connected(&session.settings.snapshot()) {
        return;
    }
    let Some(url) = server.factory.setup_page_url().await else {
        return;
    };
    peer.session_update(
        &session.acp_id,
        SessionUpdate::AgentMessageChunk(protocol::text_chunk(format!(
            "No AI provider is connected, so this session can only run the offline `llmsim` model. \
             Open {url} to connect one, then pick its model in this client."
        ))),
    );
    // One watcher per process, not per session: the page writes global settings,
    // and every open session is refreshed when it saves.
    if server.setup_page_watch.swap(true, Ordering::Relaxed) {
        return;
    }
    tokio::spawn({
        let server = server.clone();
        let peer = peer.clone();
        let mut shutdown = server.shutdown.clone();
        async move {
            // The page outlives a turn by design (it waits on a human), so the
            // watcher also drops out on teardown. Otherwise it would hold the
            // session map open for the page's whole lifetime, and a later
            // `serve` could load the same session id from disk underneath it.
            let connected = tokio::select! {
                connected = server.factory.wait_for_setup_page() => connected,
                _ = shutdown.wait_for(|done| *done) => false,
            };
            server.setup_page_watch.store(false, Ordering::Relaxed);
            if connected {
                for session in server.sessions() {
                    emit_config_options(&peer, &session).await;
                }
            }
        }
    });
}

fn command_title(prefix: &str, name: &str, args: &str) -> String {
    if args.is_empty() {
        format!("{prefix}{name}")
    } else {
        format!("{prefix}{name} {args}")
    }
}

async fn refresh_available_commands(peer: &Arc<Peer>, session: &Arc<Session>) {
    match session
        .handles
        .runtime
        .list_commands(session.handles.session_id)
        .await
    {
        Ok(commands) => {
            *session.commands.lock().unwrap() = commands.clone();
            notify_available_commands(peer, &session.acp_id, &commands);
        }
        Err(err) => tracing::warn!(%err, "acp: command refresh failed"),
    }
}

fn prompt_input(model: &ModelState, blocks: &[protocol::ContentBlock]) -> InputMessage {
    model.input_message_with_images(
        protocol::prompt_model_text(blocks),
        prompt_image_parts(blocks),
    )
}

fn prompt_image_parts(blocks: &[protocol::ContentBlock]) -> Vec<ContentPart> {
    blocks
        .iter()
        .filter_map(|block| match block {
            protocol::ContentBlock::Image(image) => Some(ContentPart::Image(
                ImageContentPart::from_base64(image.data.clone(), image.mime_type.clone()),
            )),
            _ => None,
        })
        .collect()
}

/// Drive one prompt turn: stream the runtime's events to the client as
/// `session/update`s and resolve a stop reason. Honours `session/cancel`.
async fn run_prompt(
    peer: Arc<Peer>,
    session: Arc<Session>,
    mut prompt: String,
    mut input: InputMessage,
) -> StopReason {
    loop {
        let (stop, result) = run_prompt_once(peer.clone(), session.clone(), prompt, input).await;
        if stop == StopReason::Cancelled {
            return stop;
        }
        let Some(result) = result else {
            if session.user_ask_enabled
                && session.user_ask_store.is_active(session.handles.session_id)
            {
                let evaluation = crate::session_state::task_completion::evaluation_for_state(
                    crate::session_state::task_completion::CompletionState::Failed,
                );
                let _ = session
                    .user_ask_store
                    .record_evaluation(session.handles.session_id, &evaluation);
                peer.session_update(
                    &session.acp_id,
                    SessionUpdate::AgentMessageChunk(protocol::text_chunk("task failed")),
                );
            }
            return stop;
        };
        let Some(next) = completion_followup(&peer, &session, &result).await else {
            return stop;
        };
        prompt = next;
        input = crate::session_state::task_completion::tag_continuation(
            session.model.input_message(prompt.clone()),
        );
    }
}

async fn run_prompt_once(
    peer: Arc<Peer>,
    session: Arc<Session>,
    prompt: String,
    input: InputMessage,
) -> (StopReason, Option<everruns_host::TurnResult>) {
    let handles = session.handles.clone();
    let session_id = handles.session_id;
    let acp_id = session.acp_id.clone();

    // Subscribe before launching the turn so no early events are missed; the
    // broadcast only delivers events emitted after `subscribe`.
    let mut live = handles.events.subscribe();
    let events_before = handles.runtime.events().await.map(|e| e.len()).unwrap_or(0);

    if let Err(err) = session.worktree.ensure_before_turn(&prompt) {
        tracing::warn!(%err, "acp: worktree activation failed");
    }
    let turn_handles = handles.clone();
    let turn =
        tokio::spawn(async move { turn_handles.run_checkpointed_turn(&prompt, input).await });

    let mut translator = Translator::new();
    let mut cancel_rx = session.arm_cancel();
    let mut cancelled = false;

    loop {
        tokio::select! {
            biased;
            _ = &mut cancel_rx => {
                cancelled = true;
                break;
            }
            recv = live.recv() => match recv {
                Ok(event) => {
                    if event.session_id == session_id {
                        for update in translator.on_event(&event) {
                            peer.session_update(&acp_id, update);
                        }
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    // Overflow: catch up from the canonical event log and
                    // resubscribe at the current head.
                    live = handles.events.subscribe();
                    drain_events(&peer, &handles, events_before, &mut translator, &acp_id).await;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            },
            _ = tokio::time::sleep(TURN_POLL_INTERVAL) => {
                if turn.is_finished() {
                    break;
                }
            }
        }
    }

    // Flush any tail events emitted between the last poll and completion. The
    // translator dedups by event id, so already-streamed events are skipped.
    drain_events(&peer, &handles, events_before, &mut translator, &acp_id).await;

    if cancelled {
        // Dropping the runtime's active act future cancels its per-tool
        // ToolContext token. Await teardown before replying so child work has
        // observed cooperative cancellation when the client sees `cancelled`.
        turn.abort();
        let _ = turn.await;
        handles.report_herdr_state(crate::capabilities::herdr::HerdrState::Idle);
        return (StopReason::Cancelled, None);
    }

    let outcome = turn.await;
    if let Some(notice) = handles.checkpoints.take_notice() {
        peer.session_update(
            &acp_id,
            SessionUpdate::AgentMessageChunk(protocol::text_chunk(notice)),
        );
    }
    match outcome {
        Ok(Ok(result)) if result.success => (StopReason::EndTurn, Some(result)),
        Ok(Ok(result)) => {
            if let Some(error) = &result.error {
                peer.session_update(
                    &acp_id,
                    SessionUpdate::AgentMessageChunk(protocol::text_chunk(format!(
                        "turn error: {error}"
                    ))),
                );
            }
            (StopReason::EndTurn, Some(result))
        }
        Ok(Err(err)) => {
            peer.session_update(
                &acp_id,
                SessionUpdate::AgentMessageChunk(protocol::text_chunk(format!(
                    "turn failed: {err}"
                ))),
            );
            (StopReason::EndTurn, None)
        }
        Err(_) => (StopReason::Cancelled, None),
    }
}

async fn completion_followup(
    peer: &Arc<Peer>,
    session: &Arc<Session>,
    result: &everruns_host::TurnResult,
) -> Option<String> {
    let session_id = session.handles.session_id;
    if !session.user_ask_enabled || !session.user_ask_store.is_active(session_id) {
        return None;
    }
    if let Some(evaluation) = crate::session_state::task_completion::failed_turn_evaluation(result)
    {
        let _ = session
            .user_ask_store
            .record_evaluation(session_id, &evaluation);
        peer.session_update(
            &session.acp_id,
            SessionUpdate::AgentMessageChunk(protocol::text_chunk("task failed")),
        );
        return None;
    }
    let tokens = session.handles.turn_tokens(result.turn_id).await;
    if !session
        .completion_budget
        .lock()
        .unwrap()
        .observe_turn(tokens)
    {
        peer.session_update(
            &session.acp_id,
            SessionUpdate::AgentMessageChunk(protocol::text_chunk(
                "user ask budget exhausted; send another prompt to resume",
            )),
        );
        return None;
    }
    let has_background = session
        .task_registry
        .list(session_id, None)
        .await
        .unwrap_or_default()
        .iter()
        .any(|task| !task.state.is_terminal());

    let evaluation = match crate::session_state::task_completion::gate_turn(result, has_background)
    {
        GateDecision::Evaluate => {
            let command = session
                .handles
                .runtime
                .execute_command(
                    session_id,
                    ExecuteCommandRequest {
                        name: "ask".to_string(),
                        arguments: Some(
                            crate::session_state::user_ask::USER_ASK_EVALUATE_ARG.to_string(),
                        ),
                        controls: None,
                    },
                )
                .await
                .ok()?;
            if !command.success {
                return None;
            }
            crate::session_state::user_ask::parse_evaluation_response(&command.message).ok()?
        }
        GateDecision::Conclusive(state) => {
            let evaluation = crate::session_state::task_completion::evaluation_for_state(state);
            if session
                .user_ask_store
                .record_evaluation(session_id, &evaluation)
                .is_err()
            {
                return None;
            }
            evaluation
        }
    };

    match evaluation.outcome {
        AskOutcome::InProgress => Some(crate::session_state::task_completion::continuation_prompt(
            &evaluation.reason,
        )),
        AskOutcome::Blocked => {
            session
                .handles
                .report_herdr_state(crate::capabilities::herdr::HerdrState::Blocked);
            None
        }
        AskOutcome::Failed => {
            peer.session_update(
                &session.acp_id,
                SessionUpdate::AgentMessageChunk(protocol::text_chunk("task failed")),
            );
            None
        }
        AskOutcome::WaitingOnBackground => {
            peer.session_update(
                &session.acp_id,
                SessionUpdate::AgentMessageChunk(protocol::text_chunk(
                    "task waiting on background",
                )),
            );
            None
        }
        AskOutcome::Achieved => None,
    }
}

/// Feed every not-yet-seen runtime event through the translator and emit the
/// resulting updates. Used to recover from broadcast lag and to flush the
/// turn's tail.
async fn drain_events(
    peer: &Arc<Peer>,
    handles: &RuntimeHandles,
    events_before: usize,
    translator: &mut Translator,
    acp_id: &str,
) {
    let events = handles.runtime.events().await.unwrap_or_default();
    for event in events.iter().skip(events_before) {
        if event.session_id != handles.session_id {
            continue;
        }
        for update in translator.on_event(event) {
            peer.session_update(acp_id, update);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initialize_advertises_agent_handled_authentication() {
        let result = handle_initialize(
            InitializeParams::new(protocol::PROTOCOL_VERSION),
            super::super::advertised_auth_methods(false),
        );

        let value = serde_json::to_value(result).expect("serialize initialize response");
        assert_eq!(value["authMethods"][0]["id"], "codex_browser");
        assert_eq!(value["authMethods"][0]["name"], "Sign in with ChatGPT");
        assert_eq!(value["authMethods"][1]["id"], "openrouter_browser");
        assert_eq!(value["authMethods"][1]["name"], "Sign in with OpenRouter");
        assert_eq!(
            value["authMethods"].as_array().map(Vec::len),
            Some(2),
            "the setup page must not be advertised when it is off"
        );
    }

    /// The page covers every provider, so it is the fallback once the active
    /// provider has no login of its own. It only exists when enabled.
    #[test]
    fn setup_page_is_the_fallback_method_only_when_enabled() {
        let with_page = super::super::advertised_auth_methods(true);
        assert_eq!(
            select_setup_auth_method(&with_page, None, "openai").unwrap(),
            Some(super::super::SETUP_PAGE_AUTH_METHOD.to_string())
        );
        assert_eq!(
            select_setup_auth_method(&with_page, None, "codex").unwrap(),
            Some("codex_browser".to_string())
        );

        let without_page = super::super::advertised_auth_methods(false);
        assert_eq!(
            select_setup_auth_method(&without_page, None, "openai").unwrap(),
            None
        );
    }

    #[test]
    fn setup_authentication_prefers_the_active_provider() {
        let methods = super::super::advertised_auth_methods(false);

        assert_eq!(
            select_setup_auth_method(&methods, None, "codex").unwrap(),
            Some("codex_browser".to_string())
        );
        assert_eq!(
            select_setup_auth_method(&methods, None, "openrouter").unwrap(),
            Some("openrouter_browser".to_string())
        );
    }

    #[test]
    fn prompt_image_parts_preserve_acp_inline_images() {
        let blocks = vec![
            protocol::ContentBlock::Text(protocol::TextContent::new("look")),
            protocol::ContentBlock::Image(protocol::ImageContent::new("ZmFrZQ==", "image/png")),
        ];

        let parts = prompt_image_parts(&blocks);

        assert_eq!(parts.len(), 1);
        match &parts[0] {
            ContentPart::Image(image) => {
                assert_eq!(image.media_type.as_deref(), Some("image/png"));
                assert_eq!(image.base64.as_deref(), Some("ZmFrZQ=="));
                assert!(image.url.is_none());
            }
            other => panic!("expected image content part, got {other:?}"),
        }
    }

    #[test]
    fn prompt_image_parts_ignores_text_only_prompts() {
        let blocks = vec![protocol::ContentBlock::Text(protocol::TextContent::new(
            "hello",
        ))];

        assert!(prompt_image_parts(&blocks).is_empty());
    }
    #[test]
    fn translates_http_and_stdio_mcp_servers() {
        let servers: Vec<McpServer> = serde_json::from_value(json!([
            {
                "type": "http",
                "name": "docs",
                "url": "https://example.com/mcp",
                "headers": [{ "name": "Authorization", "value": "Bearer t" }]
            },
            {
                "name": "fs",
                "command": "/usr/bin/mcp-fs",
                "args": ["--root", "/tmp"],
                "env": [{ "name": "RUST_LOG", "value": "info" }]
            }
        ]))
        .expect("valid ACP mcp servers");

        let scoped = scoped_mcp_servers_from_acp(&servers).expect("translated");

        let docs = scoped.get("docs").expect("http server present");
        assert_eq!(docs.transport_type, McpServerTransportType::Http);
        assert_eq!(docs.url, "https://example.com/mcp");
        assert_eq!(
            docs.headers.get("Authorization").map(String::as_str),
            Some("Bearer t")
        );

        let fs = scoped.get("fs").expect("stdio server present");
        assert_eq!(fs.transport_type, McpServerTransportType::Stdio);
        assert_eq!(fs.command.as_deref(), Some("/usr/bin/mcp-fs"));
        assert_eq!(fs.args, vec!["--root".to_string(), "/tmp".to_string()]);
        assert_eq!(fs.env.get("RUST_LOG").map(String::as_str), Some("info"));
    }

    #[test]
    fn rejects_unsupported_sse_mcp_transport() {
        let servers: Vec<McpServer> = serde_json::from_value(json!([
            { "type": "sse", "name": "stream", "url": "https://example.com/sse", "headers": [] }
        ]))
        .expect("valid ACP sse server");

        let error =
            scoped_mcp_servers_from_acp(&servers).expect_err("sse transport must be rejected");
        assert_eq!(error.code, agent_client_protocol::ErrorCode::InvalidParams);
    }
}

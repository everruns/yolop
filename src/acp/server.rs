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
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use agent_client_protocol::{Agent, Client, ConnectionTo, Lines, Responder};
use anyhow::Result;
use async_trait::async_trait;
use everruns_core::InputMessage;
use everruns_core::command::{CommandDescriptor, CommandSource, ExecuteCommandRequest};
use everruns_core::message::{ContentPart, ImageContentPart};
use everruns_core::typed_id::SessionId as RuntimeSessionId;
use futures::{AsyncBufReadExt, AsyncWriteExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{oneshot, watch};
use tokio::task::JoinHandle;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use crate::background_wake::{WakeReceiver, frame_wake_prompt};
use crate::runtime::{BuiltRuntime, ModelState, ProviderChoice, RuntimeHandles};
use crate::settings::SettingsStore;
use crate::worktree::WorktreeManager;

use super::bridge::Translator;
use super::protocol::{
    self, AgentCapabilities, AuthenticateParams, AuthenticateResult, AvailableCommand,
    AvailableCommandInput, InitializeParams, InitializeResult, LoadSessionParams,
    LoadSessionResult, NewSessionParams, NewSessionResult, PromptCapabilities, PromptParams,
    PromptResult, SessionNotification, SessionUpdate, StopReason, ToolCall, ToolCallStatus,
    ToolCallUpdate, ToolCallUpdateFields, ToolKind, UnstructuredCommandInput,
};

/// How often the prompt loop wakes to check whether the turn task finished,
/// in case the final event was already drained from the broadcast.
const TURN_POLL_INTERVAL: Duration = Duration::from_millis(150);

/// Builds a runtime for a freshly opened ACP session. Abstracted so tests can
/// substitute a scripted llmsim runtime for the real provider wiring.
#[async_trait]
pub trait RuntimeFactory: Send + Sync + 'static {
    fn session_exists(&self, session_id: RuntimeSessionId) -> bool;

    async fn build(
        &self,
        cwd: PathBuf,
        resume_session_id: Option<RuntimeSessionId>,
        model_selection: Option<ProviderChoice>,
    ) -> Result<BuiltRuntime>;
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AcpSelectedModel {
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
}

impl AcpSelectedModel {
    fn is_empty(&self) -> bool {
        self.provider.as_deref().is_none_or(str::is_empty)
            && self.model.as_deref().is_none_or(str::is_empty)
            && self.reasoning_effort.as_deref().is_none_or(str::is_empty)
    }
}

fn selected_model_from_new_session(
    params: &NewSessionParams,
) -> Result<Option<ProviderChoice>, agent_client_protocol::Error> {
    let meta = params.meta.as_ref().map(|meta| Value::Object(meta.clone()));
    selected_model_from_meta(meta.as_ref())
}

fn selected_model_from_meta(
    meta: Option<&Value>,
) -> Result<Option<ProviderChoice>, agent_client_protocol::Error> {
    let Some(yolop_meta) = meta.and_then(|meta| meta.get("yolop.dev/acp")) else {
        return Ok(None);
    };
    let Some(model_meta) = yolop_meta
        .get("model")
        .or_else(|| yolop_meta.get("selectedModel"))
    else {
        return Ok(None);
    };

    let selected: AcpSelectedModel = serde_json::from_value(model_meta.clone())
        .map_err(|err| invalid_params(format!("invalid yolop model selection metadata: {err}")))?;
    if selected.is_empty() {
        return Ok(None);
    }
    let provider = selected
        .provider
        .as_deref()
        .ok_or_else(|| invalid_params("model selection requires `provider`"))
        .and_then(|provider| {
            ProviderChoice::default_for_provider_name(provider)
                .map_err(|err| invalid_params(err.to_string()))
        })?;
    let model = selected
        .model
        .unwrap_or_else(|| provider.model_id().to_string());
    let spec = match selected.reasoning_effort {
        Some(effort) => format!("{model} {effort}"),
        None => model,
    };
    provider
        .resolve_model_spec(&spec)
        .map(Some)
        .map_err(|err| invalid_params(err.to_string()))
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

/// State for one open ACP session: the runtime handles plus a one-shot cancel
/// channel armed for the duration of each in-flight prompt.
struct Session {
    acp_id: String,
    handles: RuntimeHandles,
    model: ModelState,
    worktree: Arc<WorktreeManager>,
    commands: StdMutex<Vec<CommandDescriptor>>,
    cancel: StdMutex<Option<oneshot::Sender<()>>>,
    /// Settings source, read for the `proactive_wake` opt-out.
    settings: Arc<SettingsStore>,
    /// Retained for the ACP session lifetime so due local schedules keep polling.
    _schedule_runner: everruns_local::LocalScheduleRunnerHandle,
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
}

impl<F: RuntimeFactory> Server<F> {
    fn session(&self, id: &str) -> Option<Arc<Session>> {
        self.sessions.lock().unwrap().get(id).cloned()
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
            async |params: InitializeParams, responder, _cx| {
                responder.respond(handle_initialize(params))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async |_params: AuthenticateParams, responder, _cx| {
                responder.respond(AuthenticateResult::new())
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

fn handle_initialize(params: InitializeParams) -> InitializeResult {
    // Echo a supported version: honour the client's request when it is one we
    // speak, otherwise advertise our own.
    let version = match params.protocol_version {
        v if v == protocol::PROTOCOL_VERSION => v,
        _ => protocol::PROTOCOL_VERSION,
    };
    InitializeResult::new(version).agent_capabilities(
        AgentCapabilities::new()
            .load_session(true)
            .prompt_capabilities(
                PromptCapabilities::new()
                    .image(true)
                    .audio(false)
                    .embedded_context(true),
            )
            .meta(protocol::meta(json!({
                "yolop.dev/acp": {
                    "commandMetadata": true,
                    "commandArgSuggestions": true,
                    "commandToolLifecycle": true,
                    "modelSelection": {
                        "requestMetaKey": "selectedModel",
                        "supportsReasoningEffort": true
                    }
                }
            }))),
    )
}

async fn handle_new_session<F: RuntimeFactory>(
    server: &Arc<Server<F>>,
    peer: &Arc<Peer>,
    params: NewSessionParams,
) -> std::result::Result<NewSessionResult, agent_client_protocol::Error> {
    let model_selection = selected_model_from_new_session(&params)?;
    let cwd = params.cwd;

    let built = server
        .factory
        .build(cwd, None, model_selection)
        .await
        .map_err(|e| internal_error(format!("build runtime: {e}")))?;

    let acp_id = register_session(server, peer, built);

    Ok(NewSessionResult::new(acp_id))
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
            let built = server
                .factory
                .build(params.cwd, Some(resume_session_id), None)
                .await
                .map_err(|e| internal_error(format!("load runtime: {e}")))?;
            let acp_id = register_session(server, peer, built);
            server
                .session(&acp_id)
                .ok_or_else(|| internal_error("loaded session was not registered"))?
        }
    };

    replay_session_history(peer, &session).await?;
    Ok((LoadSessionResult::new(), session.acp_id.clone()))
}

fn register_session<F: RuntimeFactory>(
    server: &Arc<Server<F>>,
    peer: &Arc<Peer>,
    built: BuiltRuntime,
) -> String {
    let acp_id = built.handles.session_id.to_string();
    let commands = built.startup.capability_commands.clone();
    let session = Arc::new(Session {
        acp_id: acp_id.clone(),
        handles: built.handles,
        model: built.model,
        worktree: built.worktree,
        commands: StdMutex::new(commands.clone()),
        cancel: StdMutex::new(None),
        settings: built.settings,
        _schedule_runner: built.schedule_runner,
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
/// `crate::background_wake`) and takes the same `turn_lock` as client prompts so
/// a wake turn never overlaps one. Stops on connection teardown or when the
/// runtime (and its wake sender) drops. See specs/background.md.
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
            if !session.settings.snapshot().proactive_wake_enabled() {
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
                SessionUpdate::AgentMessageChunk(protocol::text_chunk(
                    "↻ background task finished — waking agent to review",
                )),
            );
            let prompt = frame_wake_prompt(&message);
            let input = session.model.input_message(prompt.clone());
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
    let stop_reason = match parse_command_prompt(&prompt) {
        Some(command) => run_slash_command(peer, session.clone(), command).await,
        None => run_prompt(peer, session.clone(), prompt, input).await,
    };
    Ok(PromptResult::new(stop_reason))
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
                json!({
                    "name": arg.name,
                    "description": arg.description,
                    "required": arg.required,
                    "suggestions": arg.suggestions,
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

async fn run_slash_command(
    peer: Arc<Peer>,
    session: Arc<Session>,
    command: ParsedCommand,
) -> StopReason {
    let name = command.name;
    let args = command.args;
    let title = command.title;
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
    model.input_message_with_images(protocol::prompt_text(blocks), prompt_image_parts(blocks))
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
    prompt: String,
    input: InputMessage,
) -> StopReason {
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
    let runtime = handles.runtime.clone();
    let turn = tokio::spawn(async move { runtime.run_turn(session_id, input).await });

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
        // run_turn has no in-flight cancellation hook; abandon the task and
        // report cancelled. The runtime may finish in the background but its
        // remaining events are ignored.
        turn.abort();
        handles.report_herdr_state(crate::capabilities::herdr::HerdrState::Idle);
        return StopReason::Cancelled;
    }

    match turn.await {
        Ok(Ok(result)) if result.success => StopReason::EndTurn,
        Ok(Ok(result)) => {
            if let Some(error) = result.error {
                peer.session_update(
                    &acp_id,
                    SessionUpdate::AgentMessageChunk(protocol::text_chunk(format!(
                        "turn error: {error}"
                    ))),
                );
            }
            StopReason::EndTurn
        }
        Ok(Err(err)) => {
            peer.session_update(
                &acp_id,
                SessionUpdate::AgentMessageChunk(protocol::text_chunk(format!(
                    "turn failed: {err}"
                ))),
            );
            StopReason::EndTurn
        }
        Err(_) => StopReason::Cancelled,
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
    fn new_session_request_selects_model() {
        let params: NewSessionParams = serde_json::from_value(json!({
            "cwd": "/tmp",
            "mcpServers": [],
            "_meta": {
                "yolop.dev/acp": {
                    "selectedModel": {
                        "provider": "openai",
                        "model": "gpt-5.2",
                        "reasoningEffort": "high"
                    }
                }
            }
        }))
        .expect("valid ACP new-session request");

        let selected = selected_model_from_new_session(&params)
            .expect("valid selection")
            .expect("selection present");

        assert_eq!(selected.provider_name(), "openai");
        assert_eq!(selected.model_id(), "gpt-5.2");
        assert_eq!(selected.reasoning_effort(), Some("high"));
    }

    #[test]
    fn rejects_selected_model_without_provider() {
        let meta = json!({
            "yolop.dev/acp": {
                "selectedModel": { "model": "gpt-5.2" }
            }
        });

        let error = selected_model_from_meta(Some(&meta))
            .expect_err("provider-less selection must be rejected");

        assert_eq!(error.code, agent_client_protocol::ErrorCode::InvalidParams);
    }

    #[test]
    fn rejects_unknown_selected_model_provider() {
        let meta = json!({
            "yolop.dev/acp": {
                "model": { "provider": "unknown-provider", "model": "x" }
            }
        });

        let error =
            selected_model_from_meta(Some(&meta)).expect_err("unknown provider must be rejected");

        assert_eq!(error.code, agent_client_protocol::ErrorCode::InvalidParams);
    }
}

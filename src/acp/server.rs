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
use everruns_core::command::{CommandDescriptor, CommandSource, ExecuteCommandRequest};
use everruns_core::typed_id::SessionId as RuntimeSessionId;
use futures::{AsyncBufReadExt, AsyncWriteExt, StreamExt};
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::oneshot;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use crate::runtime::{BuiltRuntime, ModelState, RuntimeHandles};

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
    ) -> Result<BuiltRuntime>;
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
    commands: StdMutex<Vec<CommandDescriptor>>,
    cancel: StdMutex<Option<oneshot::Sender<()>>>,
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
    let server = Arc::new(Server {
        factory,
        sessions: StdMutex::new(HashMap::new()),
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
                    match handle_new_session(&server, params).await {
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
                    .image(false)
                    .audio(false)
                    .embedded_context(true),
            )
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
    params: NewSessionParams,
) -> std::result::Result<NewSessionResult, agent_client_protocol::Error> {
    let cwd = params.cwd;

    let built = server
        .factory
        .build(cwd, None)
        .await
        .map_err(|e| internal_error(format!("build runtime: {e}")))?;

    let acp_id = register_session(server, built);

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
                .build(params.cwd, Some(resume_session_id))
                .await
                .map_err(|e| internal_error(format!("load runtime: {e}")))?;
            let acp_id = register_session(server, built);
            server
                .session(&acp_id)
                .ok_or_else(|| internal_error("loaded session was not registered"))?
        }
    };

    replay_session_history(peer, &session).await?;
    Ok((LoadSessionResult::new(), session.acp_id.clone()))
}

fn register_session<F: RuntimeFactory>(server: &Arc<Server<F>>, built: BuiltRuntime) -> String {
    let acp_id = built.handles.session_id.to_string();
    let commands = built.startup.capability_commands.clone();
    let session = Arc::new(Session {
        acp_id: acp_id.clone(),
        handles: built.handles,
        model: built.model,
        commands: StdMutex::new(commands.clone()),
        cancel: StdMutex::new(None),
    });
    server
        .sessions
        .lock()
        .unwrap()
        .insert(acp_id.clone(), session);

    acp_id
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

    let stop_reason = match parse_slash_command(&prompt) {
        Some((name, args)) => run_slash_command(peer, session, name, args).await,
        None => run_prompt(peer, session, prompt).await,
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
        .map(|arg| {
            if arg.description.trim().is_empty() {
                arg.name.as_str()
            } else {
                arg.description.as_str()
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
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

fn parse_slash_command(prompt: &str) -> Option<(String, String)> {
    let trimmed = prompt.trim();
    let rest = trimmed.strip_prefix('/')?.trim_start();
    let mut parts = rest.splitn(2, char::is_whitespace);
    let name = parts.next()?.trim();
    if name.is_empty() {
        return None;
    }
    let args = parts.next().unwrap_or_default().trim();
    Some((name.to_string(), args.to_string()))
}

async fn run_slash_command(
    peer: Arc<Peer>,
    session: Arc<Session>,
    name: String,
    args: String,
) -> StopReason {
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
                    ToolCall::new(tool_call_id.clone(), command_title(&descriptor.name, &args))
                        .kind(ToolKind::Other)
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
            run_prompt(peer, session, text).await
        }
    }
}

fn command_title(name: &str, args: &str) -> String {
    if args.is_empty() {
        format!("/{name}")
    } else {
        format!("/{name} {args}")
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

/// Drive one prompt turn: stream the runtime's events to the client as
/// `session/update`s and resolve a stop reason. Honours `session/cancel`.
async fn run_prompt(peer: Arc<Peer>, session: Arc<Session>, prompt: String) -> StopReason {
    let handles = session.handles.clone();
    let session_id = handles.session_id;
    let acp_id = session.acp_id.clone();

    // Subscribe before launching the turn so no early events are missed; the
    // broadcast only delivers events emitted after `subscribe`.
    let mut live = handles.events.subscribe();
    let events_before = handles.runtime.events().await.map(|e| e.len()).unwrap_or(0);

    let input = session.model.input_message(prompt);
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

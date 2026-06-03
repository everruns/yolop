//! ACP server: the JSON-RPC peer and request dispatch.
//!
//! yolop acts as an ACP *agent*: it reads newline-delimited JSON-RPC 2.0
//! messages from a client (an editor such as Zed) and drives the everruns
//! runtime in response. [`serve`] owns the read loop and is generic over the
//! byte streams and a [`RuntimeFactory`], so the production binary wires it to
//! real stdin/stdout while tests drive it over in-memory pipes with a scripted
//! runtime.
//!
//! Concurrency model:
//!   * A single writer task serialises every outbound line (responses,
//!     notifications, and agent→client requests) so writes never interleave.
//!   * The read loop never blocks on slow work — `session/prompt` runs in its
//!     own task — so `session/cancel` and permission responses keep flowing
//!     while a turn is in progress.
//!   * Agent→client requests (`session/request_permission`) are correlated by
//!     id through a pending map the read loop resolves when the response
//!     arrives.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use everruns_core::command::{CommandDescriptor, CommandSource, ExecuteCommandRequest};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};

use crate::approval::{ApprovalGate, ApprovalRequest};
use crate::runtime::{BuiltRuntime, ModelState, RuntimeHandles};

use super::bridge::Translator;
use super::protocol::{
    self, AgentCapabilities, AvailableCommand, AvailableCommandInput, InitializeParams,
    InitializeResult, NewSessionParams, NewSessionResult, PermissionOption, PermissionOptionKind,
    PermissionOutcome, PromptCapabilities, PromptParams, PromptResult, RequestPermissionParams,
    RequestPermissionResult, SessionNotification, SessionUpdate, StopReason, ToolCallContent,
    ToolCallStatus, ToolKind,
};

/// JSON-RPC error codes used in agent responses.
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const INTERNAL_ERROR: i64 = -32603;

/// How often the prompt loop wakes to check whether the turn task finished,
/// in case the final event was already drained from the broadcast.
const TURN_POLL_INTERVAL: Duration = Duration::from_millis(150);

/// Builds a runtime for a freshly opened ACP session. Abstracted so tests can
/// substitute a scripted llmsim runtime for the real provider wiring.
#[async_trait]
pub trait RuntimeFactory: Send + Sync + 'static {
    async fn build(&self, cwd: PathBuf, gate: Arc<ApprovalGate>) -> Result<BuiltRuntime>;
}

struct RpcError {
    code: i64,
    message: String,
}

impl RpcError {
    fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

/// The JSON-RPC peer: a serialised outbound channel plus a pending-request
/// table for agent→client calls.
struct Peer {
    out: mpsc::UnboundedSender<String>,
    next_id: AtomicI64,
    pending: StdMutex<HashMap<i64, oneshot::Sender<std::result::Result<Value, RpcError>>>>,
}

impl Peer {
    fn send_value(&self, value: Value) {
        // Compact serialization keeps each message on a single line, as the
        // ndjson transport requires.
        let _ = self.out.send(value.to_string());
    }

    fn respond_ok(&self, id: Value, result: Value) {
        self.send_value(json!({ "jsonrpc": "2.0", "id": id, "result": result }));
    }

    fn respond_err(&self, id: Value, code: i64, message: &str) {
        self.send_value(json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": code, "message": message },
        }));
    }

    fn notify(&self, method: &str, params: Value) {
        self.send_value(json!({ "jsonrpc": "2.0", "method": method, "params": params }));
    }

    fn session_update(&self, session_id: &str, update: SessionUpdate) {
        let params = serde_json::to_value(SessionNotification {
            session_id: session_id.to_string(),
            update,
        })
        .expect("session notification serializes");
        self.notify("session/update", params);
    }

    /// Issue an agent→client request and await its response. Fails fast (rather
    /// than awaiting forever) if the outbound channel is already closed — the
    /// writer task has exited, so no response can ever arrive.
    async fn request(&self, method: &str, params: Value) -> std::result::Result<Value, RpcError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        let line = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        })
        .to_string();
        if self.out.send(line).is_err() {
            // Connection gone: don't leak the pending entry or block on a
            // response that will never come.
            self.pending.lock().unwrap().remove(&id);
            return Err(RpcError::new(INTERNAL_ERROR, "connection closed"));
        }
        rx.await
            .unwrap_or_else(|_| Err(RpcError::new(INTERNAL_ERROR, "connection closed")))
    }

    /// Resolve a pending agent→client request from an inbound response.
    fn route_response(&self, id: i64, result: std::result::Result<Value, RpcError>) {
        if let Some(tx) = self.pending.lock().unwrap().remove(&id) {
            let _ = tx.send(result);
        }
    }

    /// Fail every in-flight agent→client request. Called once the connection
    /// ends so awaiting tasks (e.g. a forwarded `session/request_permission`)
    /// unwind instead of deadlocking and holding the server alive.
    fn fail_all_pending(&self) {
        let drained: Vec<_> = self.pending.lock().unwrap().drain().collect();
        for (_, tx) in drained {
            let _ = tx.send(Err(RpcError::new(INTERNAL_ERROR, "connection closed")));
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
    peer: Arc<Peer>,
    factory: Arc<F>,
    sessions: StdMutex<HashMap<String, Arc<Session>>>,
}

impl<F: RuntimeFactory> Server<F> {
    fn session(&self, id: &str) -> Option<Arc<Session>> {
        self.sessions.lock().unwrap().get(id).cloned()
    }
}

/// Run the ACP agent over the given byte streams until the client closes its
/// end (EOF on `reader`). Returns once the read loop ends.
pub async fn serve<R, W, F>(reader: R, writer: W, factory: Arc<F>) -> Result<()>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
    F: RuntimeFactory,
{
    // Single writer task: every outbound line funnels through here so
    // notifications, responses, and requests never interleave on the wire.
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<String>();
    let writer_task = tokio::spawn(async move {
        let mut writer = writer;
        while let Some(line) = out_rx.recv().await {
            if writer.write_all(line.as_bytes()).await.is_err()
                || writer.write_all(b"\n").await.is_err()
                || writer.flush().await.is_err()
            {
                break;
            }
        }
    });

    let server = Arc::new(Server {
        peer: Arc::new(Peer {
            out: out_tx,
            next_id: AtomicI64::new(1),
            pending: StdMutex::new(HashMap::new()),
        }),
        factory,
        sessions: StdMutex::new(HashMap::new()),
    });

    let mut lines = BufReader::new(reader).lines();
    let read_result = loop {
        match lines.next_line().await {
            Ok(Some(line)) => {
                if line.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str::<Value>(&line) {
                    Ok(message) => dispatch(server.clone(), message),
                    Err(err) => tracing::warn!(%err, "acp: dropping unparseable line"),
                }
            }
            Ok(None) => break Ok(()),
            Err(err) => break Err(err),
        }
    };

    // Connection ended: fail any in-flight agent→client requests so awaiting
    // tasks (a forwarded permission prompt, a streaming turn) unwind instead of
    // deadlocking and holding the writer task — and thus `serve` — open.
    server.peer.fail_all_pending();
    drop(server);
    let _ = writer_task.await;
    read_result.map_err(Into::into)
}

/// Classify an inbound message and route it. Requests are handled in spawned
/// tasks so the read loop keeps draining (essential for cancel + permission
/// flows during a long prompt). Responses resolve pending agent→client calls.
fn dispatch<F: RuntimeFactory>(server: Arc<Server<F>>, message: Value) {
    let has_method = message.get("method").and_then(Value::as_str).is_some();
    if has_method {
        let id = message.get("id").cloned();
        match id {
            Some(id) if !id.is_null() => {
                tokio::spawn(handle_request(server, id, message));
            }
            _ => handle_notification(&server, &message),
        }
        return;
    }
    // Otherwise it is a response to one of our outbound requests.
    if let Some(id) = message.get("id").and_then(Value::as_i64) {
        if let Some(error) = message.get("error") {
            let code = error
                .get("code")
                .and_then(Value::as_i64)
                .unwrap_or(INTERNAL_ERROR);
            let msg = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("request failed")
                .to_string();
            server
                .peer
                .route_response(id, Err(RpcError::new(code, msg)));
        } else {
            let result = message.get("result").cloned().unwrap_or(Value::Null);
            server.peer.route_response(id, Ok(result));
        }
    }
}

async fn handle_request<F: RuntimeFactory>(server: Arc<Server<F>>, id: Value, message: Value) {
    let method = message
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let params = message.get("params").cloned().unwrap_or(Value::Null);

    if method == "session/new" {
        match handle_new_session(&server, params).await {
            Ok(result) => {
                let session_id = result
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                server.peer.respond_ok(id, result);
                if let Some(session_id) = session_id
                    && let Some(session) = server.session(&session_id)
                {
                    let commands = session.commands.lock().unwrap().clone();
                    notify_available_commands(&server.peer, &session_id, &commands);
                }
            }
            Err(err) => server.peer.respond_err(id, err.code, &err.message),
        }
        return;
    }

    let outcome = match method.as_str() {
        "initialize" => handle_initialize(params),
        "authenticate" => Ok(json!({})),
        "session/prompt" => handle_prompt(&server, params).await,
        other => Err(RpcError::new(
            METHOD_NOT_FOUND,
            format!("unknown method: {other}"),
        )),
    };

    match outcome {
        Ok(result) => server.peer.respond_ok(id, result),
        Err(err) => server.peer.respond_err(id, err.code, &err.message),
    }
}

fn handle_notification<F: RuntimeFactory>(server: &Arc<Server<F>>, message: &Value) {
    let method = message
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if method == "session/cancel" {
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        if let Ok(parsed) = serde_json::from_value::<protocol::CancelParams>(params)
            && let Some(session) = server.session(&parsed.session_id)
        {
            session.trigger_cancel();
        }
    }
}

fn handle_initialize(params: Value) -> std::result::Result<Value, RpcError> {
    let params: InitializeParams = serde_json::from_value(params)
        .map_err(|e| RpcError::new(INVALID_PARAMS, format!("initialize: {e}")))?;
    // Echo a supported version: honour the client's request when it is one we
    // speak, otherwise advertise our own.
    let version = match params.protocol_version {
        Some(v) if v == protocol::PROTOCOL_VERSION => v,
        _ => protocol::PROTOCOL_VERSION,
    };
    let result = InitializeResult {
        protocol_version: version,
        agent_capabilities: AgentCapabilities {
            // yolop builds a fresh runtime per session and does not yet
            // rehydrate prior ACP sessions, so loadSession stays false.
            load_session: false,
            prompt_capabilities: PromptCapabilities {
                image: false,
                audio: false,
                embedded_context: true,
            },
            meta: Some(json!({
                "yolop.dev/acp": {
                    "commandMetadata": true,
                    "commandArgSuggestions": true,
                    "commandToolLifecycle": true
                }
            })),
        },
        // No auth handshake: credentials come from the environment/settings
        // the agent process already inherits.
        auth_methods: vec![],
    };
    Ok(serde_json::to_value(result).expect("initialize result serializes"))
}

async fn handle_new_session<F: RuntimeFactory>(
    server: &Arc<Server<F>>,
    params: Value,
) -> std::result::Result<Value, RpcError> {
    let params: NewSessionParams = serde_json::from_value(params)
        .map_err(|e| RpcError::new(INVALID_PARAMS, format!("session/new: {e}")))?;
    let cwd = PathBuf::from(&params.cwd);

    // Delegate destructive-operation approval to the client via
    // `session/request_permission`. The editor owns the human, so this is the
    // idiomatic ACP behaviour.
    let (gate_tx, mut approval_rx) =
        mpsc::unbounded_channel::<(ApprovalRequest, oneshot::Sender<bool>)>();
    let gate = ApprovalGate::channel(gate_tx);

    let built = server
        .factory
        .build(cwd, gate)
        .await
        .map_err(|e| RpcError::new(INTERNAL_ERROR, format!("build runtime: {e}")))?;

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

    // Forward every approval request to the client as a permission prompt for
    // the lifetime of the session.
    let peer = server.peer.clone();
    let permission_session = acp_id.clone();
    tokio::spawn(async move {
        while let Some((request, responder)) = approval_rx.recv().await {
            let allowed = request_permission(&peer, &permission_session, &request).await;
            let _ = responder.send(allowed);
        }
    });

    let result = NewSessionResult { session_id: acp_id };
    Ok(serde_json::to_value(result).expect("new session result serializes"))
}

async fn handle_prompt<F: RuntimeFactory>(
    server: &Arc<Server<F>>,
    params: Value,
) -> std::result::Result<Value, RpcError> {
    let params: PromptParams = serde_json::from_value(params)
        .map_err(|e| RpcError::new(INVALID_PARAMS, format!("session/prompt: {e}")))?;
    let session = server
        .session(&params.session_id)
        .ok_or_else(|| RpcError::new(INVALID_PARAMS, "unknown session id"))?;
    let prompt = protocol::prompt_text(&params.prompt);

    let stop_reason = match parse_slash_command(&prompt) {
        Some((name, args)) => run_slash_command(server.peer.clone(), session, name, args).await,
        None => run_prompt(server.peer.clone(), session, prompt).await,
    };
    let result = PromptResult { stop_reason };
    Ok(serde_json::to_value(result).expect("prompt result serializes"))
}

fn available_commands(commands: &[CommandDescriptor]) -> Vec<AvailableCommand> {
    commands
        .iter()
        .map(|command| AvailableCommand {
            name: command.name.clone(),
            description: command.description.clone(),
            input: command_input(command),
            meta: command_meta(command),
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
    Some(AvailableCommandInput { hint })
}

fn notify_available_commands(peer: &Arc<Peer>, session_id: &str, commands: &[CommandDescriptor]) {
    peer.session_update(
        session_id,
        SessionUpdate::AvailableCommandsUpdate {
            available_commands: available_commands(commands),
            meta: Some(json!({
                "yolop.dev/acp": {
                    "argSuggestions": true
                }
            })),
        },
    );
}

fn command_meta(command: &CommandDescriptor) -> Option<Value> {
    if command.args.is_empty() {
        return None;
    }
    let source = match command.source {
        CommandSource::System => "system",
        CommandSource::Skill => "skill",
    };
    Some(json!({
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
            SessionUpdate::AgentMessageChunk {
                content: protocol::ContentBlock::text(format!("unknown command: /{name}")),
            },
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
            SessionUpdate::AgentMessageChunk {
                content: protocol::ContentBlock::text(format!("/{name} requires: {needed}")),
            },
        );
        return StopReason::EndTurn;
    }

    match descriptor.source {
        CommandSource::System => {
            let tool_call_id = format!("command_{}", peer.next_id.fetch_add(1, Ordering::Relaxed));
            peer.session_update(
                &session.acp_id,
                SessionUpdate::ToolCall {
                    tool_call_id: tool_call_id.clone(),
                    title: command_title(&descriptor.name, &args),
                    kind: ToolKind::Other,
                    status: ToolCallStatus::InProgress,
                    raw_input: Some(json!({
                        "command": descriptor.name,
                        "arguments": if args.is_empty() { Value::Null } else { Value::String(args.clone()) },
                        "source": "system",
                    })),
                    content: Vec::new(),
                },
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
                SessionUpdate::ToolCallUpdate {
                    tool_call_id,
                    status: Some(if success {
                        ToolCallStatus::Completed
                    } else {
                        ToolCallStatus::Failed
                    }),
                    content: vec![ToolCallContent::Content {
                        content: protocol::ContentBlock::text(message),
                    }],
                    raw_output: Some(raw_output),
                },
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
                    SessionUpdate::AgentMessageChunk {
                        content: protocol::ContentBlock::text(format!("turn error: {error}")),
                    },
                );
            }
            StopReason::EndTurn
        }
        Ok(Err(err)) => {
            peer.session_update(
                &acp_id,
                SessionUpdate::AgentMessageChunk {
                    content: protocol::ContentBlock::text(format!("turn failed: {err}")),
                },
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

/// Ask the client to approve a destructive operation. Maps the client's
/// selection back to a boolean. Any error or cancellation denies the
/// operation, matching the channel gate's fail-closed default.
async fn request_permission(peer: &Arc<Peer>, session_id: &str, request: &ApprovalRequest) -> bool {
    const ALLOW: &str = "allow";
    const REJECT: &str = "reject";

    let params = RequestPermissionParams {
        session_id: session_id.to_string(),
        tool_call: json!({ "toolCallId": "pending", "title": request.headline() }),
        options: vec![
            PermissionOption {
                option_id: ALLOW.to_string(),
                name: "Allow".to_string(),
                kind: PermissionOptionKind::AllowOnce,
            },
            PermissionOption {
                option_id: REJECT.to_string(),
                name: "Reject".to_string(),
                kind: PermissionOptionKind::RejectOnce,
            },
        ],
    };
    let params = serde_json::to_value(params).expect("permission params serialize");

    match peer.request("session/request_permission", params).await {
        Ok(value) => match serde_json::from_value::<RequestPermissionResult>(value) {
            Ok(result) => match result.outcome {
                PermissionOutcome::Selected { option_id } => option_id == ALLOW,
                PermissionOutcome::Cancelled => false,
            },
            Err(err) => {
                tracing::warn!(%err, "acp: malformed permission response; denying");
                false
            }
        },
        Err(err) => {
            tracing::warn!(code = err.code, message = %err.message, "acp: permission request failed; denying");
            false
        }
    }
}

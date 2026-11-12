//! ACP protocol v2 front end.
//!
//! Protocol v2 (draft) is what Zed's Delta requires of external agents. It
//! changes the lifecycle more than the payloads: `session/prompt` answers with
//! a `messageId` as soon as the message is inserted, and the turn's progress
//! and end travel as `state_update` notifications (`running`, `requires_action`,
//! `idle` with a stop reason), so an agent can work before, between and after
//! prompts. Messages carry ids, tool calls are upserts, plans have ids, modes
//! became a `mode` config option, and the client no longer offers files or
//! terminals.
//!
//! Decision: v2 is a translation layer over the v1 session core in
//! [`super`], not a second turn loop. Handlers here map v2 requests onto the
//! shared helpers (`open_session`, `reopen_session`, `run_client_turn`,
//! `set_config_value`), and [`Link`] translates every v1 `session/update` the
//! core emits into its v2 shape. The SDK's protocol router picks this
//! implementation for clients that request v2 or newer; v1 clients keep the
//! v1 implementation untouched.
//!
//! See `knowledge/specs/acp.md`, "Protocol v2".

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::v1::schema as v1s;
use agent_client_protocol::v2::schema as s;
use agent_client_protocol::v2::{self as acp2, ConnectionTo};
use agent_client_protocol::{Agent, Client, ConnectTo};
use everruns_core::{McpServerTransportType, ScopedMcpServer, ScopedMcpServers};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use super::{
    Peer, PeerLink, RuntimeFactory, Server, StopReason, detect_client_identity,
    emit_config_options, internal_error, invalid_params, notify_available_commands,
    offer_setup_page, open_session, prompt_input, reopen_session, replay_session_history,
    run_client_turn, session_config_options, set_config_value,
};

/// `-32002`: the session is unknown or no longer active on this connection.
const SESSION_NOT_FOUND: i32 = -32002;
/// The id of the one item plan yolop keeps per session (its todo list).
const PLAN_ID: &str = "todos";

/// The v2 side of a connection: the SDK handle plus the per-session state v2
/// adds on top of v1 (message ids and the foreground-work count behind
/// `state_update`).
pub(in crate::editor::acp) struct Link {
    cx: ConnectionTo<Client>,
    sessions: StdMutex<HashMap<String, LinkSession>>,
    /// Ids only need to be unique per session and entity type; a process
    /// epoch keeps them unique across reconnects to the same saved session.
    epoch: u64,
    next_id: AtomicU64,
}

#[derive(Default)]
struct LinkSession {
    /// Id of the message the next chunk of each kind appends to. Any other
    /// update closes the open message, so text before and after a tool call
    /// stays two messages in the client, in order.
    agent_message: Option<String>,
    agent_thought: Option<String>,
    user_message: Option<String>,
    /// Foreground work started and not yet finished: queued prompts plus a
    /// running prompt or wake turn. `idle` is reported when it returns to 0.
    active: usize,
    /// Permission requests in flight, so `requires_action` lasts until the
    /// last one is answered.
    waiting: usize,
}

impl Link {
    fn new(cx: ConnectionTo<Client>) -> Self {
        let epoch = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_millis() as u64)
            .unwrap_or_default();
        Self {
            cx,
            sessions: StdMutex::new(HashMap::new()),
            epoch,
            next_id: AtomicU64::new(1),
        }
    }

    fn new_id(&self, prefix: &str) -> String {
        let n = self.next_id.fetch_add(1, Ordering::Relaxed);
        format!("{prefix}_{:x}_{n}", self.epoch)
    }

    fn send(&self, session_id: &str, update: s::SessionUpdate) {
        let notification = s::UpdateSessionNotification::new(session_id.to_string(), update);
        if let Err(err) = self.cx.send_notification(notification) {
            tracing::warn!(%err, "acp v2: failed to send session update");
        }
    }

    pub(super) fn session_update(&self, session_id: &str, update: v1s::SessionUpdate) {
        let translated = {
            let mut sessions = self.sessions.lock().unwrap();
            let state = sessions.entry(session_id.to_string()).or_default();
            translate_update(state, update, || self.new_id("msg"))
        };
        match translated {
            Ok(Some(update)) => self.send(session_id, update),
            Ok(None) => {}
            Err(err) => tracing::warn!(%err, "acp v2: could not translate session update"),
        }
    }

    /// Send a complete user message, as the echo of an inserted prompt.
    fn user_message(&self, session_id: &str, message_id: &str, content: Vec<s::ContentBlock>) {
        self.close_messages(session_id);
        self.send(
            session_id,
            s::SessionUpdate::UserMessage(
                s::UserMessage::new(message_id.to_string()).content(content),
            ),
        );
    }

    fn close_messages(&self, session_id: &str) {
        let mut sessions = self.sessions.lock().unwrap();
        let state = sessions.entry(session_id.to_string()).or_default();
        state.close_messages();
    }

    pub(super) fn work_started(&self, session_id: &str) {
        let first = {
            let mut sessions = self.sessions.lock().unwrap();
            let state = sessions.entry(session_id.to_string()).or_default();
            state.active += 1;
            state.active == 1
        };
        if first {
            self.send(
                session_id,
                s::SessionUpdate::StateUpdate(
                    s::StateUpdate::Running(s::RunningStateUpdate::new()),
                ),
            );
        }
    }

    pub(super) fn work_finished(&self, session_id: &str, stop: StopReason) {
        let last = {
            let mut sessions = self.sessions.lock().unwrap();
            let state = sessions.entry(session_id.to_string()).or_default();
            state.active = state.active.saturating_sub(1);
            if state.active == 0 {
                state.close_messages();
            }
            state.active == 0
        };
        if last {
            self.send(session_id, idle(Some(stop)));
        }
    }

    /// The current state, sent before a `session/resume` response.
    fn state_snapshot(&self, session_id: &str) {
        let state = {
            let sessions = self.sessions.lock().unwrap();
            sessions
                .get(session_id)
                .map(|state| (state.active, state.waiting))
                .unwrap_or_default()
        };
        let update = match state {
            (0, _) => idle(None),
            (_, 0) => {
                s::SessionUpdate::StateUpdate(s::StateUpdate::Running(s::RunningStateUpdate::new()))
            }
            _ => s::SessionUpdate::StateUpdate(s::StateUpdate::RequiresAction(
                s::RequiresActionStateUpdate::new(),
            )),
        };
        self.send(session_id, update);
    }

    pub(super) async fn request_permission(
        &self,
        request: v1s::RequestPermissionRequest,
    ) -> Result<Option<String>, agent_client_protocol::Error> {
        let session_id = request.session_id.to_string();
        let request = permission_request(request)?;
        self.set_waiting(&session_id, true);
        let response = self.cx.send_request(request).block_task().await;
        self.set_waiting(&session_id, false);
        Ok(match response?.outcome {
            s::RequestPermissionOutcome::Selected(selected) => Some(selected.option_id.to_string()),
            s::RequestPermissionOutcome::Cancelled(_) => None,
            // Never read an unknown outcome as approval.
            _ => Some(String::new()),
        })
    }

    fn set_waiting(&self, session_id: &str, waiting: bool) {
        let transition = {
            let mut sessions = self.sessions.lock().unwrap();
            let state = sessions.entry(session_id.to_string()).or_default();
            if waiting {
                state.waiting += 1;
                (state.waiting == 1).then_some(true)
            } else {
                state.waiting = state.waiting.saturating_sub(1);
                (state.waiting == 0 && state.active > 0).then_some(false)
            }
        };
        match transition {
            Some(true) => self.send(
                session_id,
                s::SessionUpdate::StateUpdate(s::StateUpdate::RequiresAction(
                    s::RequiresActionStateUpdate::new(),
                )),
            ),
            Some(false) => self.send(
                session_id,
                s::SessionUpdate::StateUpdate(
                    s::StateUpdate::Running(s::RunningStateUpdate::new()),
                ),
            ),
            None => {}
        }
    }

    /// Form elicitation for `ask_user`. The v1 and v2 form payloads share
    /// their wire shape, so the request and its answer convert through JSON.
    pub(in crate::editor::acp) async fn create_elicitation(
        &self,
        request: v1s::CreateElicitationRequest,
    ) -> Result<v1s::ElicitationAction, agent_client_protocol::Error> {
        let session_id = match &request.mode {
            v1s::ElicitationMode::Form(form) => match &form.scope {
                v1s::ElicitationScope::Session(scope) => Some(scope.session_id.to_string()),
                _ => None,
            },
            _ => None,
        };
        let request: s::CreateElicitationRequest = convert(&request)?;
        if let Some(session_id) = &session_id {
            self.set_waiting(session_id, true);
        }
        let response = self.cx.send_request(request).block_task().await;
        if let Some(session_id) = &session_id {
            self.set_waiting(session_id, false);
        }
        convert(&response?.action)
    }

    fn forget(&self, session_id: &str) {
        self.sessions.lock().unwrap().remove(session_id);
    }
}

impl LinkSession {
    fn close_messages(&mut self) {
        self.agent_message = None;
        self.agent_thought = None;
        self.user_message = None;
    }
}

fn idle(stop: Option<StopReason>) -> s::SessionUpdate {
    let mut update = s::IdleStateUpdate::new();
    if let Some(stop) = stop {
        update = update.stop_reason(stop_reason(stop));
    }
    s::SessionUpdate::StateUpdate(s::StateUpdate::Idle(update))
}

fn stop_reason(stop: StopReason) -> s::StopReason {
    match stop {
        StopReason::EndTurn => s::StopReason::EndTurn,
        StopReason::MaxTokens => s::StopReason::MaxTokens,
        StopReason::MaxTurnRequests => s::StopReason::MaxTurnRequests,
        StopReason::Refusal => s::StopReason::Refusal,
        StopReason::Cancelled => s::StopReason::Cancelled,
        _ => s::StopReason::EndTurn,
    }
}

/// Serialize one protocol's value and read it back as the other's. Used only
/// where the two versions share a wire shape; a mismatch is an internal error,
/// and the translation tests pin each shape yolop sends.
fn convert<A: Serialize, B: DeserializeOwned>(
    value: &A,
) -> Result<B, agent_client_protocol::Error> {
    let value = serde_json::to_value(value).map_err(|e| internal_error(e.to_string()))?;
    serde_json::from_value(value).map_err(|e| internal_error(e.to_string()))
}

/// Translate one v1 `session/update` into its v2 form, or `None` for updates
/// v2 dropped (`current_mode_update`: the `mode` config option replaces it).
fn translate_update(
    state: &mut LinkSession,
    update: v1s::SessionUpdate,
    mut new_id: impl FnMut() -> String,
) -> Result<Option<s::SessionUpdate>, agent_client_protocol::Error> {
    let mut value = serde_json::to_value(&update).map_err(|e| internal_error(e.to_string()))?;
    let kind = value
        .get("sessionUpdate")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let Some(object) = value.as_object_mut() else {
        return Ok(None);
    };
    match kind.as_str() {
        "agent_message_chunk" | "agent_thought_chunk" | "user_message_chunk" => {
            let open = match kind.as_str() {
                "agent_message_chunk" => &mut state.agent_message,
                "agent_thought_chunk" => &mut state.agent_thought,
                _ => &mut state.user_message,
            };
            let id = open.get_or_insert_with(&mut new_id).clone();
            let keep = kind.clone();
            // A different kind of chunk starts a new message of its own.
            if keep != "agent_message_chunk" {
                state.agent_message = None;
            }
            if keep != "agent_thought_chunk" {
                state.agent_thought = None;
            }
            if keep != "user_message_chunk" {
                state.user_message = None;
            }
            // v1 may carry its own optional id; v2 requires the link's.
            object.insert("messageId".into(), Value::String(id));
        }
        "tool_call" | "tool_call_update" => {
            state.close_messages();
            // v2 has no separate create: the first upsert for an id creates it.
            object.insert("sessionUpdate".into(), json!("tool_call_update"));
        }
        "plan" => {
            state.close_messages();
            let entries = object.remove("entries").unwrap_or_else(|| json!([]));
            let meta = object.remove("_meta");
            object.clear();
            object.insert("sessionUpdate".into(), json!("plan_update"));
            object.insert(
                "plan".into(),
                json!({ "type": "items", "planId": PLAN_ID, "entries": entries }),
            );
            if let Some(meta) = meta {
                object.insert("_meta".into(), meta);
            }
        }
        "available_commands_update" => {
            if let Some(Value::Array(commands)) = object.get_mut("availableCommands") {
                for command in commands {
                    v2_command(command);
                }
            }
        }
        "config_option_update" => {
            if let Some(Value::Array(options)) = object.get_mut("configOptions") {
                for option in options {
                    v2_config_option(option);
                }
            }
        }
        "session_info_update" | "usage_update" => {}
        // v2 removed modes; the `mode` config option carries the level.
        "current_mode_update" => return Ok(None),
        other => {
            tracing::debug!(kind = other, "acp v2: dropping update with no v2 form");
            return Ok(None);
        }
    }
    serde_json::from_value(value)
        .map(Some)
        .map_err(|e| internal_error(format!("{kind}: {e}")))
}

/// v1 command → v2: commands and their input gain a `type` discriminator.
fn v2_command(command: &mut Value) {
    let Some(command) = command.as_object_mut() else {
        return;
    };
    command.insert("type".into(), json!("command"));
    if let Some(Value::Object(input)) = command.get_mut("input")
        && !input.contains_key("type")
    {
        input.insert("type".into(), json!("text"));
    }
}

/// v1 config option → v2: `id` is `configId` and a group's `group` is `groupId`.
fn v2_config_option(option: &mut Value) {
    let Some(option) = option.as_object_mut() else {
        return;
    };
    if let Some(id) = option.remove("id") {
        option.insert("configId".into(), id);
    }
    if let Some(Value::Array(entries)) = option.get_mut("options") {
        for entry in entries {
            if let Some(entry) = entry.as_object_mut()
                && let Some(group) = entry.remove("group")
            {
                entry.insert("groupId".into(), group);
            }
        }
    }
}

fn config_options(options: Vec<v1s::SessionConfigOption>) -> Vec<s::SessionConfigOption> {
    options
        .into_iter()
        .filter_map(|option| {
            let mut value = serde_json::to_value(option).ok()?;
            v2_config_option(&mut value);
            serde_json::from_value(value)
                .inspect_err(|err| tracing::warn!(%err, "acp v2: config option did not convert"))
                .ok()
        })
        .collect()
}

fn available_commands(commands: Vec<v1s::AvailableCommand>) -> Vec<s::AvailableCommand> {
    commands
        .into_iter()
        .filter_map(|command| {
            let mut value = serde_json::to_value(command).ok()?;
            v2_command(&mut value);
            serde_json::from_value(value)
                .inspect_err(|err| tracing::warn!(%err, "acp v2: command did not convert"))
                .ok()
        })
        .collect()
}

/// v1 `session/request_permission` → v2: the gated tool call becomes the
/// request's `subject`, and its name the required `title`.
fn permission_request(
    request: v1s::RequestPermissionRequest,
) -> Result<s::RequestPermissionRequest, agent_client_protocol::Error> {
    let title = request
        .tool_call
        .fields
        .title
        .clone()
        .unwrap_or_else(|| "Run tool".to_string());
    let tool_call =
        serde_json::to_value(&request.tool_call).map_err(|e| internal_error(e.to_string()))?;
    let options =
        serde_json::to_value(&request.options).map_err(|e| internal_error(e.to_string()))?;
    serde_json::from_value(json!({
        "sessionId": request.session_id.to_string(),
        "title": format!("Allow {title}?"),
        "subject": { "type": "tool_call", "toolCall": tool_call },
        "options": options,
    }))
    .map_err(|e| internal_error(e.to_string()))
}

/// v2 prompt content → the v1 blocks the core reads. Same wire shape.
fn prompt_blocks(
    blocks: &[s::ContentBlock],
) -> Result<Vec<v1s::ContentBlock>, agent_client_protocol::Error> {
    blocks
        .iter()
        .map(|block| convert(block).map_err(|e| invalid_params(e.message.clone())))
        .collect()
}

fn scoped_mcp_servers(
    servers: &[s::McpServer],
) -> Result<ScopedMcpServers, agent_client_protocol::Error> {
    let mut scoped = ScopedMcpServers::new();
    let mut names = HashSet::new();
    for server in servers {
        let (name, entry) = match server {
            s::McpServer::Http(http) => (
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
            s::McpServer::Stdio(stdio) => (
                stdio.name.clone(),
                ScopedMcpServer {
                    transport_type: McpServerTransportType::Stdio,
                    command: Some(stdio.command.0.to_string_lossy().into_owned()),
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
        if !names.insert(name.clone()) {
            return Err(invalid_params(format!(
                "duplicate mcp server name `{name}`"
            )));
        }
        scoped.insert(name, entry);
    }
    Ok(scoped)
}

fn session_not_found(session_id: &str) -> agent_client_protocol::Error {
    agent_client_protocol::Error::new(SESSION_NOT_FOUND, "Session not found")
        .data(json!({ "sessionId": session_id }))
}

fn initialize_response(auth_methods: Vec<v1s::AuthMethod>) -> s::InitializeResponse {
    let auth_methods = auth_methods
        .iter()
        .filter_map(|method| {
            let v1s::AuthMethod::Agent(agent) = method else {
                return None;
            };
            let mut value = serde_json::to_value(agent).ok()?;
            let object = value.as_object_mut()?;
            if let Some(id) = object.remove("id") {
                object.insert("methodId".into(), id);
            }
            object.insert("type".into(), json!("agent"));
            serde_json::from_value(value).ok()
        })
        .collect();
    s::InitializeResponse::new(
        ProtocolVersion::V2,
        s::Implementation::new("yolop", env!("CARGO_PKG_VERSION")).title("Yolop"),
    )
    .auth_methods(auth_methods)
    .capabilities(
        s::AgentCapabilities::new().session(
            s::SessionCapabilities::new()
                .prompt(
                    s::PromptCapabilities::new()
                        .image(s::PromptImageCapabilities::new())
                        .embedded_context(s::PromptEmbeddedContextCapabilities::new()),
                )
                .mcp(
                    s::McpCapabilities::new()
                        .stdio(s::McpStdioCapabilities::new())
                        .http(s::McpHttpCapabilities::new()),
                ),
        ),
    )
}

/// The protocol v2 agent. Handlers capture the same [`Server`] the v1 agent
/// does; the router runs only one of them on a connection.
pub(super) fn agent<F: RuntimeFactory>(
    server: Arc<Server<F>>,
    next_tool_id: Arc<AtomicI64>,
) -> impl ConnectTo<Client> {
    // The SDK hands every handler the same connection, but only once one is
    // running; the link is built from the first handler that needs it.
    let link: Arc<StdMutex<Option<Arc<Link>>>> = Arc::new(StdMutex::new(None));
    let peer_for = {
        let link = link.clone();
        move |cx: &ConnectionTo<Client>| -> (Arc<Link>, Arc<Peer>) {
            let link = link
                .lock()
                .unwrap()
                .get_or_insert_with(|| Arc::new(Link::new(cx.clone())))
                .clone();
            let peer = Arc::new(Peer {
                link: PeerLink::V2(link.clone()),
                next_id: next_tool_id.clone(),
            });
            (link, peer)
        }
    };

    Agent
        .v2()
        .name("yolop")
        .on_receive_request(
            {
                let server = server.clone();
                async move |params: s::InitializeRequest, responder: acp2::Responder<_>, _cx| {
                    let identity = detect_client_identity(
                        Some(&v1s::Implementation::new(
                            params.info.name.clone(),
                            params.info.version.clone(),
                        )),
                        params.meta.as_ref(),
                    );
                    server.factory.note_client_identity(identity);
                    let forms = params
                        .capabilities
                        .elicitation
                        .as_ref()
                        .is_some_and(|elicitation| elicitation.form.is_some());
                    server.elicitation_forms.store(forms, Ordering::Release);
                    responder.respond(initialize_response(server.factory.auth_methods()))
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let server = server.clone();
                let peer_for = peer_for.clone();
                async move |params: s::LoginAuthRequest, responder: acp2::Responder<_>, cx| {
                    let method_id = params.method_id.to_string();
                    if !server
                        .factory
                        .auth_methods()
                        .iter()
                        .any(|method| method.id().to_string() == method_id)
                    {
                        return responder.respond_with_error(invalid_params(format!(
                            "unknown authentication method `{method_id}`"
                        )));
                    }
                    match server.factory.authenticate(&method_id).await {
                        Ok(()) => {
                            responder.respond(s::LoginAuthResponse::new())?;
                            let (_, peer) = peer_for(&cx);
                            for session in server.sessions() {
                                emit_config_options(&peer, &session).await;
                            }
                            Ok(())
                        }
                        Err(err) => responder
                            .respond_with_error(internal_error(format!("auth/login: {err}"))),
                    }
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let server = server.clone();
                let peer_for = peer_for.clone();
                async move |_params: s::LogoutAuthRequest, responder: acp2::Responder<_>, cx| {
                    match server.factory.logout().await {
                        Ok(()) => {
                            responder.respond(s::LogoutAuthResponse::new())?;
                            let (_, peer) = peer_for(&cx);
                            for session in server.sessions() {
                                emit_config_options(&peer, &session).await;
                            }
                            Ok(())
                        }
                        Err(err) => responder
                            .respond_with_error(internal_error(format!("auth/logout: {err}"))),
                    }
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let server = server.clone();
                let peer_for = peer_for.clone();
                async move |params: s::NewSessionRequest, responder: acp2::Responder<_>, cx| {
                    let (_, peer) = peer_for(&cx);
                    let mcp = match scoped_mcp_servers(&params.mcp_servers) {
                        Ok(mcp) => mcp,
                        Err(err) => return responder.respond_with_error(err),
                    };
                    let session = match open_session(&server, &peer, params.cwd.0, mcp).await {
                        Ok(session) => session,
                        Err(err) => return responder.respond_with_error(err),
                    };
                    let options = session_config_options(&session).await;
                    *session.last_config_options.lock().unwrap() = Some(options.clone());
                    let commands = session.commands.lock().unwrap().clone();
                    responder.respond(
                        s::NewSessionResponse::new(session.acp_id.clone())
                            .config_options(config_options(options))
                            .available_commands(available_commands(super::available_commands(
                                &commands,
                            ))),
                    )?;
                    offer_setup_page(&server, &peer, &session).await;
                    super::maybe_spawn_settings_watcher(&server, &peer);
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let server = server.clone();
                let peer_for = peer_for.clone();
                async move |params: s::ResumeSessionRequest, responder: acp2::Responder<_>, cx| {
                    let (link, peer) = peer_for(&cx);
                    let session_id = params.session_id.to_string();
                    let replay = match &params.replay_from {
                        None => false,
                        Some(s::ReplayFrom::Start(_)) => true,
                        Some(_) => {
                            return responder.respond_with_error(invalid_params(
                                "unsupported replayFrom cursor; only `start` is supported",
                            ));
                        }
                    };
                    let mcp = match scoped_mcp_servers(&params.mcp_servers) {
                        Ok(mcp) => mcp,
                        Err(err) => return responder.respond_with_error(err),
                    };
                    let session = match reopen_session(
                        &server,
                        &peer,
                        &session_id,
                        params.cwd.0,
                        mcp,
                    )
                    .await
                    {
                        Ok(session) => session,
                        Err(err)
                            if err.code == agent_client_protocol::ErrorCode::INVALID_PARAMS
                                && err.data.as_ref().is_some_and(|data| {
                                    data.as_str()
                                        .is_some_and(|text| text.starts_with("unknown session id"))
                                }) =>
                        {
                            return responder.respond_with_error(session_not_found(&session_id));
                        }
                        Err(err) => return responder.respond_with_error(err),
                    };
                    session.closed.store(false, Ordering::Release);
                    if replay {
                        if let Err(err) = replay_session_history(&peer, &session).await {
                            return responder.respond_with_error(err);
                        }
                        link.close_messages(&session_id);
                    }
                    // Current state, never history: the snapshot follows the
                    // replay and precedes the response.
                    link.state_snapshot(&session_id);
                    emit_config_options(&peer, &session).await;
                    let commands = session.commands.lock().unwrap().clone();
                    notify_available_commands(&peer, &session_id, &commands);
                    responder.respond(s::ResumeSessionResponse::new())?;
                    super::maybe_spawn_settings_watcher(&server, &peer);
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let server = server.clone();
                async move |params: s::ListSessionsRequest, responder: acp2::Responder<_>, _cx| {
                    let cwd = params.cwd.as_ref().map(|cwd| cwd.0.clone());
                    let sessions = server
                        .factory
                        .list_sessions(cwd.as_deref())
                        .await
                        .into_iter()
                        .map(session_info)
                        .collect();
                    responder.respond(s::ListSessionsResponse::new(sessions))
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let server = server.clone();
                let peer_for = peer_for.clone();
                async move |params: s::CloseSessionRequest, responder: acp2::Responder<_>, cx| {
                    let (link, _) = peer_for(&cx);
                    let session_id = params.session_id.to_string();
                    let Some(session) = server.session(&session_id) else {
                        return responder.respond_with_error(session_not_found(&session_id));
                    };
                    // Stop queued and running work, then wait for the running
                    // turn to finish its final updates (and `idle: cancelled`)
                    // before answering.
                    session.closed.store(true, Ordering::Release);
                    session.trigger_cancel();
                    let _turn = session.turn_lock.lock().await;
                    server.sessions.lock().unwrap().remove(&session_id);
                    session.handles.flush_trace_exporters().await;
                    link.forget(&session_id);
                    responder.respond(s::CloseSessionResponse::new())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let server = server.clone();
                let peer_for = peer_for.clone();
                async move |params: s::PromptRequest, responder: acp2::Responder<_>, cx| {
                    let (link, peer) = peer_for(&cx);
                    let session_id = params.session_id.to_string();
                    let Some(session) = server.session(&session_id) else {
                        return responder.respond_with_error(session_not_found(&session_id));
                    };
                    // Everything that can reject the prompt happens before the
                    // response: an error means the message was not inserted.
                    let blocks = match prompt_blocks(&params.prompt) {
                        Ok(blocks) => blocks,
                        Err(err) => return responder.respond_with_error(err),
                    };
                    let input = match prompt_input(&session.model, &blocks) {
                        Ok(input) => input,
                        Err(err) => {
                            return responder.respond_with_error(invalid_params(format!(
                                "invalid prompt image: {err}"
                            )));
                        }
                    };
                    let message_id = link.new_id("user");
                    // Count the work before answering, so a `session/close`
                    // racing this prompt still sees it.
                    peer.work_started(&session_id);
                    if let Err(err) = responder.respond(s::PromptResponse::new(message_id.clone()))
                    {
                        peer.work_finished(&session_id, StopReason::Cancelled);
                        return Err(err);
                    }
                    link.user_message(&session_id, &message_id, params.prompt);
                    // The turn runs off the dispatch loop so `session/cancel`,
                    // permission answers and further prompts keep flowing.
                    // Prompts for a busy session queue on its turn lock.
                    tokio::spawn({
                        let server = server.clone();
                        async move {
                            let stop =
                                run_client_turn(&server, peer.clone(), session, &blocks, input)
                                    .await;
                            peer.work_finished(&session_id, stop);
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
                async move |params: s::SetSessionConfigOptionRequest,
                            responder: acp2::Responder<_>,
                            _cx| {
                    let session_id = params.session_id.to_string();
                    let Some(session) = server.session(&session_id) else {
                        return responder.respond_with_error(session_not_found(&session_id));
                    };
                    let s::SessionConfigOptionValue::Id { value } = &params.value else {
                        return responder.respond_with_error(invalid_params(
                            "session config options take a value id",
                        ));
                    };
                    if let Err(err) = set_config_value(
                        &session,
                        &params.config_id.to_string(),
                        &value.to_string(),
                    )
                    .await
                    {
                        return responder.respond_with_error(err);
                    }
                    let options = session_config_options(&session).await;
                    *session.last_config_options.lock().unwrap() = Some(options.clone());
                    responder.respond(s::SetSessionConfigOptionResponse::new(config_options(
                        options,
                    )))
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            {
                let server = server.clone();
                async move |params: s::CancelSessionNotification, _cx| {
                    if let Some(session) = server.session(&params.session_id.to_string()) {
                        session.trigger_cancel();
                    }
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_notification!(),
        )
}

/// A saved session, as `session/list` reports it.
pub(crate) struct SavedSession {
    pub session_id: String,
    pub cwd: PathBuf,
    pub updated_at: Option<std::time::SystemTime>,
}

fn session_info(saved: SavedSession) -> s::SessionInfo {
    let mut info = s::SessionInfo::new(saved.session_id, saved.cwd);
    if let Some(updated) = saved.updated_at {
        let updated: chrono::DateTime<chrono::Utc> = updated.into();
        info = info.updated_at(updated.to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
    }
    info
}

/// Saved sessions under `sessions_dir`, newest first, optionally only those
/// whose workspace is `cwd`. Sessions with no recorded workspace are skipped:
/// v2 requires an absolute `cwd` for each.
pub(in crate::editor::acp) fn list_saved_sessions(
    sessions_dir: &Path,
    cwd: Option<&Path>,
) -> Vec<SavedSession> {
    let Ok(entries) = std::fs::read_dir(sessions_dir) else {
        return Vec::new();
    };
    let mut sessions = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().to_str()?.to_string();
            let id = name
                .parse::<everruns_contracts::typed_id::SessionId>()
                .ok()?;
            let dir = entry.path();
            let workspace = crate::runtime::session_log::read_session_workspace(&dir)
                .ok()
                .flatten()?;
            if !workspace.is_absolute() || cwd.is_some_and(|cwd| cwd != workspace) {
                return None;
            }
            let modified = crate::runtime::saved_sessions::modified(&dir, id);
            Some(SavedSession {
                session_id: name,
                cwd: workspace,
                updated_at: Some(modified),
            })
        })
        .collect::<Vec<_>>();
    sessions.sort_by_key(|session| std::cmp::Reverse(session.updated_at));
    sessions
}

#[cfg(test)]
mod tests {
    use super::*;

    fn translate(state: &mut LinkSession, update: v1s::SessionUpdate) -> Value {
        // Ids come from the link's counter; a fresh one per call is enough
        // to tell an open message (same id) from a new one.
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let update = translate_update(state, update, || {
            format!("id{}", NEXT.fetch_add(1, Ordering::Relaxed))
        })
        .expect("translates")
        .expect("has a v2 form");
        serde_json::to_value(update).unwrap()
    }

    fn text(text: &str) -> v1s::ContentChunk {
        v1s::ContentChunk::new(v1s::ContentBlock::Text(v1s::TextContent::new(text)))
    }

    #[test]
    fn chunks_share_a_message_id_until_another_update_closes_it() {
        let mut state = LinkSession::default();
        let first = translate(&mut state, v1s::SessionUpdate::AgentMessageChunk(text("a")));
        let second = translate(&mut state, v1s::SessionUpdate::AgentMessageChunk(text("b")));
        assert_eq!(first["messageId"], second["messageId"]);
        assert_eq!(first["sessionUpdate"], "agent_message_chunk");

        let tool = translate(
            &mut state,
            v1s::SessionUpdate::ToolCall(v1s::ToolCall::new("call_1", "Read file")),
        );
        assert_eq!(tool["sessionUpdate"], "tool_call_update");
        assert_eq!(tool["toolCallId"], "call_1");
        assert_eq!(tool["title"], "Read file");

        let after = translate(&mut state, v1s::SessionUpdate::AgentMessageChunk(text("c")));
        assert_ne!(
            after["messageId"], first["messageId"],
            "the tool call closed it"
        );

        let thought = translate(&mut state, v1s::SessionUpdate::AgentThoughtChunk(text("t")));
        let resumed = translate(&mut state, v1s::SessionUpdate::AgentMessageChunk(text("d")));
        assert_ne!(thought["messageId"], after["messageId"]);
        assert_ne!(
            resumed["messageId"], after["messageId"],
            "a thought closes it too"
        );
    }

    #[test]
    fn plan_becomes_an_items_plan_update() {
        let mut state = LinkSession::default();
        let plan = translate(
            &mut state,
            v1s::SessionUpdate::Plan(v1s::Plan::new(vec![v1s::PlanEntry::new(
                "write tests",
                v1s::PlanEntryPriority::Medium,
                v1s::PlanEntryStatus::InProgress,
            )])),
        );
        assert_eq!(plan["sessionUpdate"], "plan_update");
        assert_eq!(plan["plan"]["type"], "items");
        assert_eq!(plan["plan"]["planId"], PLAN_ID);
        assert_eq!(plan["plan"]["entries"][0]["content"], "write tests");
    }

    #[test]
    fn commands_and_config_options_gain_v2_discriminators() {
        let mut state = LinkSession::default();
        let commands = translate(
            &mut state,
            v1s::SessionUpdate::AvailableCommandsUpdate(v1s::AvailableCommandsUpdate::new(vec![
                v1s::AvailableCommand::new("setup", "Connect a provider").input(
                    v1s::AvailableCommandInput::Unstructured(v1s::UnstructuredCommandInput::new(
                        "provider",
                    )),
                ),
            ])),
        );
        let command = &commands["availableCommands"][0];
        assert_eq!(command["type"], "command");
        assert_eq!(command["input"]["type"], "text");
        assert_eq!(command["input"]["hint"], "provider");

        let options = translate(
            &mut state,
            v1s::SessionUpdate::ConfigOptionUpdate(v1s::ConfigOptionUpdate::new(vec![
                v1s::SessionConfigOption::select(
                    "mode",
                    "Approval",
                    "normal",
                    vec![v1s::SessionConfigSelectOption::new("normal", "Normal")],
                )
                .category(v1s::SessionConfigOptionCategory::Mode),
            ])),
        );
        let option = &options["configOptions"][0];
        assert_eq!(option["configId"], "mode");
        assert_eq!(option["category"], "mode");
        assert_eq!(option["currentValue"], "normal");
    }

    #[test]
    fn mode_updates_have_no_v2_form() {
        let mut state = LinkSession::default();
        let update = translate_update(
            &mut state,
            v1s::SessionUpdate::CurrentModeUpdate(v1s::CurrentModeUpdate::new("off")),
            || "id".into(),
        )
        .unwrap();
        assert!(update.is_none());
    }

    #[test]
    fn permission_requests_name_the_tool_call_as_their_subject() {
        let request = permission_request(v1s::RequestPermissionRequest::new(
            "session",
            v1s::ToolCallUpdate::new(
                "call_1",
                v1s::ToolCallUpdateFields::new().title("bash".to_string()),
            ),
            vec![v1s::PermissionOption::new(
                "allow_once",
                "Allow",
                v1s::PermissionOptionKind::AllowOnce,
            )],
        ))
        .unwrap();
        let value = serde_json::to_value(request).unwrap();
        assert_eq!(value["title"], "Allow bash?");
        assert_eq!(value["subject"]["type"], "tool_call");
        assert_eq!(value["subject"]["toolCall"]["toolCallId"], "call_1");
        assert_eq!(value["options"][0]["optionId"], "allow_once");
    }

    #[test]
    fn duplicate_mcp_server_names_are_invalid_params() {
        let server = s::McpServer::Http(s::McpServerHttp::new("docs", "https://example.com/mcp"));
        let err = scoped_mcp_servers(&[server.clone(), server]).unwrap_err();
        assert_eq!(err.code, agent_client_protocol::ErrorCode::INVALID_PARAMS);
    }
}

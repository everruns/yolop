//! Agent Client Protocol (ACP) support.
//!
//! ACP lets editors such as Zed drive yolop as an external agent over stdio
//! using newline-delimited JSON-RPC 2.0. Run it with `yolop --acp`; the editor
//! spawns that process, performs the `initialize` handshake, opens sessions
//! with `session/new`, and sends turns with `session/prompt`. yolop streams the
//! turn back as `session/update` notifications.
//!
//! See `knowledge/specs/acp.md` for the full surface and `README.md` for editor setup.
//!
//! Module layout:
//!   * [`protocol`] — SDK schema re-exports plus small yolop helpers.
//!   * [`bridge`] — pure translation of runtime events into `session/update`s.
//!   * [`server`] — SDK-backed transport/dispatch plus turn streaming.

mod bridge;
mod modes;
mod protocol;
mod server;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;

use crate::capabilities::ClientUiContext;
use crate::config::{SandboxMode, SettingsStore};
use crate::runtime::session_log::{legacy_session_log_path, session_dir_path, session_log_path};
use crate::runtime::{BuildOptions, BuiltRuntime, ProviderChoice, build_with_options};
use agent_client_protocol::schema::v1::{AuthMethod, AuthMethodAgent};
use everruns_core::ScopedMcpServers;
use everruns_core::typed_id::SessionId as RuntimeSessionId;

pub use server::{RuntimeFactory, serve};

pub(crate) fn advertised_auth_methods() -> Vec<AuthMethod> {
    vec![
        AuthMethod::Agent(
            AuthMethodAgent::new("codex_browser", "Sign in with ChatGPT")
                .description("Open a browser to connect the Codex provider."),
        ),
        AuthMethod::Agent(
            AuthMethodAgent::new("openrouter_browser", "Sign in with OpenRouter")
                .description("Open a browser to create an OpenRouter API key."),
        ),
    ]
}

/// Production [`RuntimeFactory`]: builds a real provider-backed runtime rooted
/// at the client-supplied `cwd` for each `session/new`. The provider, settings,
/// and session-log directory come from the CLI invocation and are shared
/// across every session the client opens.
struct ConfigRuntimeFactory {
    provider: ProviderChoice,
    settings: Arc<SettingsStore>,
    sessions_dir: PathBuf,
    sandbox_mode_override: Option<SandboxMode>,
}

#[async_trait]
impl RuntimeFactory for ConfigRuntimeFactory {
    fn session_exists(&self, session_id: RuntimeSessionId) -> bool {
        let session_dir = session_dir_path(&self.sessions_dir, session_id);
        session_log_path(&session_dir).exists()
            || legacy_session_log_path(&self.sessions_dir, session_id).exists()
    }

    fn auth_methods(&self) -> Vec<AuthMethod> {
        advertised_auth_methods()
    }

    async fn authenticate(&self, method_id: &str) -> Result<()> {
        match method_id {
            "codex_browser" => {
                let auth = crate::auth::codex::login_with_browser().await?;
                self.settings.set_codex_auth(auth)
            }
            "openrouter_browser" => {
                let key = crate::auth::openrouter::login_with_browser().await?;
                self.settings.set_token("openrouter".to_string(), key)
            }
            _ => anyhow::bail!("unknown authentication method `{method_id}`"),
        }
    }

    async fn build(
        &self,
        cwd: PathBuf,
        resume_session_id: Option<RuntimeSessionId>,
        client_mcp_servers: ScopedMcpServers,
        tool_approver: Option<Arc<dyn crate::capabilities::ToolApprover>>,
    ) -> Result<BuiltRuntime> {
        build_with_options(
            cwd,
            self.provider.clone(),
            resume_session_id,
            self.sessions_dir.clone(),
            self.settings.clone(),
            BuildOptions {
                client_ui: ClientUiContext::Acp,
                client_mcp_servers,
                tool_approver,
                sandbox_mode_override: self.sandbox_mode_override,
                ..BuildOptions::default()
            },
        )
        .await
    }
}

/// Serve the ACP agent over this process's stdin/stdout until the client
/// disconnects. Tracing still writes to stderr, keeping stdout clean for the
/// protocol.
pub async fn run_stdio(
    provider: ProviderChoice,
    settings: Arc<SettingsStore>,
    sessions_dir: PathBuf,
    sandbox_mode_override: Option<SandboxMode>,
) -> Result<()> {
    let factory = Arc::new(ConfigRuntimeFactory {
        provider,
        settings,
        sessions_dir,
        sandbox_mode_override,
    });
    serve(tokio::io::stdin(), tokio::io::stdout(), factory).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::{
        InitializeRequest, InitializeResponse, NewSessionRequest, PromptRequest, SessionId,
        SessionModeId, SessionNotification, SessionUpdate, SetSessionModeRequest, StopReason,
    };
    use agent_client_protocol::{
        Agent, ByteStreams, Client, ConnectionTo, JsonRpcRequest, SessionMessage,
    };
    use everruns_core::llmsim_driver::{LlmSimConfig, SimToolCall, SimTurn};
    use futures::Future;
    use serde::{Deserialize, Serialize};
    use serde_json::{Value, json};
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, Lines};
    use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

    /// Scripted [`RuntimeFactory`] for tests: each session gets its own
    /// offline llmsim runtime rooted at the supplied `cwd`. The session-log
    /// directory is a kept tempdir (OS cleans `/tmp`) so it outlives the
    /// runtime, which canonicalizes and retains its paths.
    struct ScriptedFactory {
        config: LlmSimConfig,
        sessions_dir: PathBuf,
        settings: Arc<SettingsStore>,
    }

    #[async_trait]
    impl RuntimeFactory for ScriptedFactory {
        fn session_exists(&self, session_id: RuntimeSessionId) -> bool {
            let session_dir = session_dir_path(&self.sessions_dir, session_id);
            session_log_path(&session_dir).exists()
                || legacy_session_log_path(&self.sessions_dir, session_id).exists()
        }

        fn auth_methods(&self) -> Vec<AuthMethod> {
            vec![AuthMethod::Agent(AuthMethodAgent::new(
                "test_connect_openai",
                "Connect OpenAI for tests",
            ))]
        }

        async fn authenticate(&self, method_id: &str) -> Result<()> {
            anyhow::ensure!(
                method_id == "test_connect_openai",
                "unknown test auth method"
            );
            self.settings
                .set_token("openai".to_string(), "test-token".to_string())
        }

        async fn build(
            &self,
            cwd: PathBuf,
            resume_session_id: Option<RuntimeSessionId>,
            client_mcp_servers: ScopedMcpServers,
            tool_approver: Option<Arc<dyn crate::capabilities::ToolApprover>>,
        ) -> Result<BuiltRuntime> {
            build_with_options(
                cwd,
                ProviderChoice::Sim,
                resume_session_id,
                self.sessions_dir.clone(),
                self.settings.clone(),
                BuildOptions {
                    llmsim_override: Some(self.config.clone().with_model("llmsim-yolop")),
                    client_ui: ClientUiContext::Acp,
                    client_mcp_servers,
                    tool_approver,
                    ..BuildOptions::default()
                },
            )
            .await
        }
    }

    /// Like [`ScriptedFactory`], but after each build it records the MCP server
    /// names actually wired into the session, so a wire-level `session/new` test
    /// can assert the client's `mcpServers` reached the runtime end to end.
    struct RecordingFactory {
        config: LlmSimConfig,
        sessions_dir: PathBuf,
        settings: Arc<SettingsStore>,
        seen_servers: Arc<std::sync::Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl RuntimeFactory for RecordingFactory {
        fn session_exists(&self, session_id: RuntimeSessionId) -> bool {
            let session_dir = session_dir_path(&self.sessions_dir, session_id);
            session_log_path(&session_dir).exists()
                || legacy_session_log_path(&self.sessions_dir, session_id).exists()
        }

        async fn build(
            &self,
            cwd: PathBuf,
            resume_session_id: Option<RuntimeSessionId>,
            client_mcp_servers: ScopedMcpServers,
            tool_approver: Option<Arc<dyn crate::capabilities::ToolApprover>>,
        ) -> Result<BuiltRuntime> {
            let built = build_with_options(
                cwd,
                ProviderChoice::Sim,
                resume_session_id,
                self.sessions_dir.clone(),
                self.settings.clone(),
                BuildOptions {
                    llmsim_override: Some(self.config.clone().with_model("llmsim-yolop")),
                    client_ui: ClientUiContext::Acp,
                    client_mcp_servers,
                    tool_approver,
                    ..BuildOptions::default()
                },
            )
            .await?;
            // Read the servers resolved onto the live session — what the next
            // turn would use — rather than the raw request, proving the whole
            // session/new → build → session chain.
            use everruns_core::SessionStore;
            if let Ok(Some(session)) = built
                .handles
                .session_store
                .get_session(built.handles.session_id)
                .await
            {
                let mut names: Vec<String> = session.mcp_servers.keys().cloned().collect();
                names.sort();
                *self.seen_servers.lock().unwrap() = names;
            }
            Ok(built)
        }
    }

    struct SdkClient {
        cx: ConnectionTo<Agent>,
        init: InitializeResponse,
    }

    impl SdkClient {
        async fn new_session(
            &self,
        ) -> agent_client_protocol::Result<agent_client_protocol::ActiveSession<'static, Agent>>
        {
            let cwd = tempfile::tempdir().expect("cwd tempdir").keep();
            self.cx
                .build_session_from(NewSessionRequest::new(cwd))
                .block_task()
                .start_session()
                .await
        }

        async fn prompt(
            session: &mut agent_client_protocol::ActiveSession<'static, Agent>,
            prompt: &str,
        ) -> agent_client_protocol::Result<PromptRun> {
            session.send_prompt(prompt)?;
            collect_prompt_run(session).await
        }
    }

    struct PromptRun {
        stop_reason: StopReason,
        updates: Vec<Value>,
    }

    impl PromptRun {
        fn updates_of_kind(&self, kind: &str) -> Vec<&Value> {
            self.updates
                .iter()
                .filter(|u| u.get("sessionUpdate").and_then(Value::as_str) == Some(kind))
                .collect()
        }

        fn assistant_text(&self) -> String {
            self.updates_of_kind("agent_message_chunk")
                .iter()
                .filter_map(|u| {
                    u.get("content")
                        .and_then(|c| c.get("text"))
                        .and_then(Value::as_str)
                })
                .collect::<Vec<_>>()
                .join("")
        }
    }

    async fn with_sdk_client<T, F, Fut>(config: LlmSimConfig, op: F) -> T
    where
        F: FnOnce(SdkClient) -> Fut + Send + 'static,
        Fut: Future<Output = agent_client_protocol::Result<T>> + Send + 'static,
        T: Send + 'static,
    {
        with_sdk_client_completion(config, false, op).await
    }

    async fn with_sdk_client_completion<T, F, Fut>(
        config: LlmSimConfig,
        completion_tracking: bool,
        op: F,
    ) -> T
    where
        F: FnOnce(SdkClient) -> Fut + Send + 'static,
        Fut: Future<Output = agent_client_protocol::Result<T>> + Send + 'static,
        T: Send + 'static,
    {
        let (client_w, agent_r) = tokio::io::duplex(64 * 1024);
        let (agent_w, client_r) = tokio::io::duplex(64 * 1024);
        let sessions = tempfile::tempdir().expect("sessions tempdir").keep();
        let settings = Arc::new(SettingsStore::open(sessions.join("settings.toml")));
        if completion_tracking {
            settings
                .set_capability_enabled(
                    crate::session_state::user_ask::USER_ASK_CAPABILITY_ID,
                    true,
                )
                .expect("enable completion tracking for ACP fixture");
        }
        let factory = Arc::new(ScriptedFactory {
            config,
            sessions_dir: sessions,
            settings,
        });
        tokio::spawn(async move {
            let _ = serve(agent_r, agent_w, factory).await;
        });
        let transport = ByteStreams::new(client_w.compat_write(), client_r.compat());

        Client
            .builder()
            .name("test-client")
            .connect_with(transport, async move |cx| {
                let init = cx
                    .send_request(InitializeRequest::new(protocol::PROTOCOL_VERSION))
                    .block_task()
                    .await?;
                op(SdkClient { cx, init }).await
            })
            .await
            .expect("SDK ACP client run")
    }

    async fn collect_prompt_run(
        session: &mut agent_client_protocol::ActiveSession<'static, Agent>,
    ) -> agent_client_protocol::Result<PromptRun> {
        let mut updates = Vec::new();
        loop {
            match tokio::time::timeout(Duration::from_secs(15), session.read_update()).await {
                Ok(Ok(SessionMessage::SessionMessage(dispatch))) => {
                    let message = dispatch.to_untyped_message()?;
                    if message.method() == "session/update" {
                        let notification: SessionNotification =
                            serde_json::from_value(message.params().clone())?;
                        updates.push(serde_json::to_value(notification.update)?);
                    }
                }
                Ok(Ok(SessionMessage::StopReason(stop_reason))) => {
                    return Ok(PromptRun {
                        stop_reason,
                        updates,
                    });
                }
                Ok(Ok(_)) => {}
                Ok(Err(err)) => return Err(err),
                Err(_) => {
                    return Err(agent_client_protocol::Error::internal_error()
                        .data("timed out waiting for prompt update"));
                }
            }
        }
    }

    async fn collect_available_commands(
        session: &mut agent_client_protocol::ActiveSession<'static, Agent>,
    ) -> agent_client_protocol::Result<Vec<Value>> {
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                let update = match session.read_update().await {
                    Ok(SessionMessage::SessionMessage(dispatch)) => {
                        let message = dispatch.to_untyped_message()?;
                        if message.method() != "session/update" {
                            continue;
                        }
                        let notification: SessionNotification =
                            serde_json::from_value(message.params().clone())?;
                        notification.update
                    }
                    Ok(SessionMessage::StopReason(_)) => continue,
                    Ok(_) => continue,
                    Err(err) => return Err(err),
                };
                if matches!(update, SessionUpdate::AvailableCommandsUpdate(_)) {
                    return Ok(vec![serde_json::to_value(update)?]);
                }
            }
        })
        .await
        .map_err(|_| {
            agent_client_protocol::Error::internal_error()
                .data("timed out waiting for available_commands_update")
        })?
    }

    async fn send_json(w: &mut DuplexStream, value: Value) {
        let line = value.to_string();
        w.write_all(line.as_bytes()).await.unwrap();
        w.write_all(b"\n").await.unwrap();
        w.flush().await.unwrap();
    }

    async fn next_json(reader: &mut Lines<BufReader<DuplexStream>>) -> Value {
        let line = tokio::time::timeout(Duration::from_secs(30), reader.next_line())
            .await
            .expect("timed out")
            .expect("read line")
            .expect("stream open");
        serde_json::from_str(&line).expect("valid json")
    }

    async fn collect_until_response_id(
        reader: &mut Lines<BufReader<DuplexStream>>,
        id: i64,
    ) -> (Value, Vec<Value>) {
        let mut updates = Vec::new();
        loop {
            let msg = next_json(reader).await;
            if msg.get("method").and_then(Value::as_str) == Some("session/update") {
                updates.push(msg);
                continue;
            }
            if msg.get("id").and_then(Value::as_i64) == Some(id)
                && (msg.get("result").is_some() || msg.get("error").is_some())
            {
                return (msg, updates);
            }
        }
    }

    fn start_raw_server(
        config: LlmSimConfig,
        sessions_dir: PathBuf,
    ) -> (
        DuplexStream,
        Lines<BufReader<DuplexStream>>,
        tokio::task::JoinHandle<Result<()>>,
    ) {
        let settings = Arc::new(SettingsStore::open(sessions_dir.join("settings.toml")));
        start_raw_server_with_settings(config, sessions_dir, settings)
    }

    fn start_raw_server_with_settings(
        config: LlmSimConfig,
        sessions_dir: PathBuf,
        settings: Arc<SettingsStore>,
    ) -> (
        DuplexStream,
        Lines<BufReader<DuplexStream>>,
        tokio::task::JoinHandle<Result<()>>,
    ) {
        let (client_w, agent_r) = tokio::io::duplex(64 * 1024);
        let (agent_w, client_r) = tokio::io::duplex(64 * 1024);
        let factory = Arc::new(ScriptedFactory {
            config,
            sessions_dir,
            settings,
        });
        let server = tokio::spawn(async move { serve(agent_r, agent_w, factory).await });
        (client_w, BufReader::new(client_r).lines(), server)
    }

    fn update_texts(updates: &[Value], kind: &str) -> Vec<String> {
        updates
            .iter()
            .filter_map(|msg| msg.get("params")?.get("update")?.as_object())
            .filter(|update| update.get("sessionUpdate").and_then(Value::as_str) == Some(kind))
            .filter_map(|update| update.get("content")?.get("text").and_then(Value::as_str))
            .map(str::to_string)
            .collect()
    }

    #[derive(Debug, Clone, Serialize, Deserialize, JsonRpcRequest)]
    #[request(method = "does/not/exist", response = Value)]
    struct UnknownRequest {}

    fn fixed(text: &str) -> LlmSimConfig {
        LlmSimConfig::fixed(text)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn initialize_advertises_protocol_version_and_capabilities() {
        let init = with_sdk_client(fixed("hi"), |client| async move { Ok(client.init) }).await;
        assert_eq!(init.protocol_version, protocol::PROTOCOL_VERSION);
        assert!(init.agent_capabilities.load_session);
        assert!(init.agent_capabilities.prompt_capabilities.embedded_context);
        // Client-provided MCP servers over `http` are accepted (#1).
        assert!(init.agent_capabilities.mcp_capabilities.http);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn session_new_wires_client_mcp_servers_into_session() {
        // End to end over the wire: a `session/new` carrying `mcpServers`
        // (one http, one stdio) must land both servers on the built session.
        let sessions = tempfile::tempdir().expect("sessions tempdir").keep();
        let settings = Arc::new(SettingsStore::open(sessions.join("settings.toml")));
        let seen_servers = Arc::new(std::sync::Mutex::new(Vec::new()));
        let factory = Arc::new(RecordingFactory {
            config: fixed("hi"),
            sessions_dir: sessions,
            settings,
            seen_servers: seen_servers.clone(),
        });

        let (client_w, agent_r) = tokio::io::duplex(64 * 1024);
        let (agent_w, client_r) = tokio::io::duplex(64 * 1024);
        let _server = tokio::spawn(async move {
            let _ = serve(agent_r, agent_w, factory).await;
        });
        let mut w = client_w;
        let mut reader = BufReader::new(client_r).lines();

        send_json(
            &mut w,
            json!({ "jsonrpc": "2.0", "id": 0, "method": "initialize", "params": { "protocolVersion": 1 } }),
        )
        .await;
        collect_until_response_id(&mut reader, 0).await;

        let cwd = tempfile::tempdir().expect("cwd tempdir").keep();
        send_json(
            &mut w,
            json!({
                "jsonrpc": "2.0", "id": 1, "method": "session/new",
                "params": {
                    "cwd": cwd,
                    "mcpServers": [
                        { "type": "http", "name": "docs", "url": "https://example.com/mcp", "headers": [] },
                        { "name": "fs", "command": "/usr/bin/mcp-fs", "args": [], "env": [] }
                    ]
                }
            }),
        )
        .await;
        let (resp, _) = collect_until_response_id(&mut reader, 1).await;
        assert!(resp.get("result").is_some(), "session/new failed: {resp}");

        let names = seen_servers.lock().unwrap().clone();
        assert!(
            names.contains(&"docs".to_string()) && names.contains(&"fs".to_string()),
            "client mcpServers must reach the session; wired: {names:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn client_mcp_server_is_discovered_and_its_tool_executes() {
        // Live round-trip: a real stdio MCP server (the Python echo fixture)
        // supplied via `session/new` `mcpServers` must be discovered and its
        // tool actually executed. The server writes a marker file on each call,
        // so the filesystem proves execution — nothing is mocked but the LLM.
        let test = "client_mcp_server_is_discovered_and_its_tool_executes";
        let Some(python) = crate::testing::mcp_e2e::require_python3(test) else {
            return;
        };
        let marker = tempfile::tempdir().expect("marker tempdir").keep();
        let tool = crate::testing::mcp_e2e::mcp_tool("echo", "echo");
        let sessions = tempfile::tempdir().expect("sessions tempdir").keep();
        // The scripted model calls the client-supplied server's tool on the turn.
        let (mut w, mut reader, _server) = start_raw_server(
            crate::testing::mcp_e2e::script(&tool, "hello-acp-mcp"),
            sessions,
        );

        send_json(
            &mut w,
            json!({ "jsonrpc": "2.0", "id": 0, "method": "initialize", "params": { "protocolVersion": 1 } }),
        )
        .await;
        collect_until_response_id(&mut reader, 0).await;

        let cwd = tempfile::tempdir().expect("cwd tempdir").keep();
        let fixture = crate::testing::mcp_e2e::fixture_server();
        send_json(
            &mut w,
            json!({
                "jsonrpc": "2.0", "id": 1, "method": "session/new",
                "params": {
                    "cwd": cwd,
                    "mcpServers": [{
                        "name": "echo",
                        "command": python.to_str().unwrap(),
                        "args": [ fixture.to_str().unwrap(), marker.to_str().unwrap() ],
                        "env": []
                    }]
                }
            }),
        )
        .await;
        let (new_resp, _) = collect_until_response_id(&mut reader, 1).await;
        let session_id = new_resp["result"]["sessionId"]
            .as_str()
            .unwrap()
            .to_string();

        // Disable the approval gate so this test isolates MCP discovery and
        // execution (the permission gate has its own tests).
        send_json(
            &mut w,
            json!({ "jsonrpc": "2.0", "id": 2, "method": "session/set_mode",
                    "params": { "sessionId": session_id, "modeId": "off" } }),
        )
        .await;
        collect_until_response_id(&mut reader, 2).await;

        send_json(
            &mut w,
            json!({
                "jsonrpc": "2.0", "id": 3, "method": "session/prompt",
                "params": { "sessionId": session_id, "prompt": [{ "type": "text", "text": "use the echo tool" }] }
            }),
        )
        .await;
        collect_until_response_id(&mut reader, 3).await;

        let called = marker.join("echo.called");
        assert!(
            called.exists(),
            "the client-supplied MCP server must be discovered and its tool executed (marker missing)"
        );
        let body = std::fs::read_to_string(&called).unwrap();
        assert!(
            body.contains("hello-acp-mcp"),
            "the server should receive the call arguments: {body}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn advertises_modes_and_set_mode_switches_level() {
        with_sdk_client(fixed("hi"), |client| async move {
            // A new session advertises the three approval levels, defaulting to
            // `normal`.
            let cwd1 = tempfile::tempdir().expect("cwd tempdir").keep();
            let s1 = client
                .cx
                .send_request(NewSessionRequest::new(cwd1))
                .block_task()
                .await?;
            let modes1 = s1.modes.clone().expect("modes advertised");
            assert_eq!(modes1.current_mode_id.to_string(), "normal");
            let ids: Vec<String> = modes1
                .available_modes
                .iter()
                .map(|m| m.id.to_string())
                .collect();
            assert_eq!(ids, vec!["protective", "normal", "off"]);

            // Switching the mode persists the approval level.
            client
                .cx
                .send_request(SetSessionModeRequest::new(
                    s1.session_id.clone(),
                    SessionModeId::new("protective"),
                ))
                .block_task()
                .await?;

            // A fresh session (the level is a shared setting) now reports the
            // switched level as current.
            let cwd2 = tempfile::tempdir().expect("cwd tempdir").keep();
            let s2 = client
                .cx
                .send_request(NewSessionRequest::new(cwd2))
                .block_task()
                .await?;
            assert_eq!(
                s2.modes
                    .expect("modes advertised")
                    .current_mode_id
                    .to_string(),
                "protective"
            );
            Ok(())
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn set_mode_with_unknown_id_is_invalid_params() {
        let err = with_sdk_client(fixed("hi"), |client| async move {
            let cwd = tempfile::tempdir().expect("cwd tempdir").keep();
            let session = client
                .cx
                .send_request(NewSessionRequest::new(cwd))
                .block_task()
                .await?;
            let result = client
                .cx
                .send_request(SetSessionModeRequest::new(
                    session.session_id.clone(),
                    SessionModeId::new("whenever"),
                ))
                .block_task()
                .await;
            Ok(result.expect_err("unknown mode must be rejected"))
        })
        .await;
        assert_eq!(err.code, agent_client_protocol::ErrorCode::InvalidParams);
    }

    /// Collect the tool-call statuses (from `tool_call` and `tool_call_update`)
    /// seen for a raw prompt whose gated tool triggers `session/request_permission`,
    /// answering each permission request with `option_id`.
    async fn statuses_for_gated_prompt(
        w: &mut DuplexStream,
        reader: &mut Lines<BufReader<DuplexStream>>,
        session_id: &str,
        prompt_id: i64,
        option_id: &str,
    ) -> (bool, Vec<String>) {
        send_json(
            w,
            json!({
                "jsonrpc": "2.0", "id": prompt_id, "method": "session/prompt",
                "params": { "sessionId": session_id, "prompt": [{ "type": "text", "text": "go" }] }
            }),
        )
        .await;

        let mut asked = false;
        let mut statuses = Vec::new();
        loop {
            let msg = next_json(reader).await;
            match msg.get("method").and_then(Value::as_str) {
                Some("session/request_permission") => {
                    asked = true;
                    // The prompt names the gated tool.
                    assert_eq!(msg["params"]["toolCall"]["toolCallId"], "call_1");
                    assert_eq!(msg["params"]["options"].as_array().unwrap().len(), 4);
                    send_json(
                        w,
                        json!({
                            "jsonrpc": "2.0", "id": msg["id"],
                            "result": { "outcome": { "outcome": "selected", "optionId": option_id } }
                        }),
                    )
                    .await;
                }
                Some("session/update") => {
                    if let Some(status) = msg["params"]["update"]
                        .get("status")
                        .and_then(Value::as_str)
                    {
                        statuses.push(status.to_string());
                    }
                }
                _ => {
                    if msg.get("id").and_then(Value::as_i64) == Some(prompt_id)
                        && (msg.get("result").is_some() || msg.get("error").is_some())
                    {
                        return (asked, statuses);
                    }
                }
            }
        }
    }

    /// Drive initialize → new session → set protective, returning the raw
    /// channel plus the session id, ready for a gated prompt.
    async fn protective_session(
        config: LlmSimConfig,
        sessions_dir: PathBuf,
    ) -> (
        DuplexStream,
        Lines<BufReader<DuplexStream>>,
        tokio::task::JoinHandle<Result<()>>,
        String,
    ) {
        let (mut w, mut reader, server) = start_raw_server(config, sessions_dir);
        send_json(
            &mut w,
            json!({ "jsonrpc": "2.0", "id": 0, "method": "initialize", "params": { "protocolVersion": 1 } }),
        )
        .await;
        collect_until_response_id(&mut reader, 0).await;

        let cwd = tempfile::tempdir().expect("cwd tempdir").keep();
        send_json(
            &mut w,
            json!({ "jsonrpc": "2.0", "id": 1, "method": "session/new", "params": { "cwd": cwd, "mcpServers": [] } }),
        )
        .await;
        let (new_resp, _) = collect_until_response_id(&mut reader, 1).await;
        let session_id = new_resp["result"]["sessionId"]
            .as_str()
            .unwrap()
            .to_string();

        send_json(
            &mut w,
            json!({ "jsonrpc": "2.0", "id": 2, "method": "session/set_mode",
                    "params": { "sessionId": session_id, "modeId": "protective" } }),
        )
        .await;
        collect_until_response_id(&mut reader, 2).await;

        (w, reader, server, session_id)
    }

    fn one_bash_call() -> LlmSimConfig {
        LlmSimConfig::scripted(vec![
            SimTurn::ToolCalls(vec![SimToolCall {
                name: "bash".to_string(),
                arguments: json!({ "command": "printf gated" }),
                id: Some("call_1".to_string()),
            }]),
            SimTurn::Assistant("done".to_string()),
        ])
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn standard_config_options_select_the_live_session_model() {
        let sessions = tempfile::tempdir().expect("sessions tempdir");
        SettingsStore::open(sessions.path().join("settings.toml"))
            .set_token("ollama".to_string(), "local".to_string())
            .expect("mark Ollama configured");
        let (mut w, mut reader, _server) =
            start_raw_server(fixed("unused"), sessions.path().to_path_buf());
        send_json(
            &mut w,
            json!({ "jsonrpc": "2.0", "id": 0, "method": "initialize", "params": { "protocolVersion": 1 } }),
        )
        .await;
        collect_until_response_id(&mut reader, 0).await;

        let cwd = tempfile::tempdir().expect("cwd tempdir").keep();
        send_json(
            &mut w,
            json!({ "jsonrpc": "2.0", "id": 1, "method": "session/new", "params": { "cwd": cwd, "mcpServers": [] } }),
        )
        .await;
        let (new_session, _) = collect_until_response_id(&mut reader, 1).await;
        let session_id = new_session["result"]["sessionId"]
            .as_str()
            .expect("sessionId");
        let options = new_session["result"]["configOptions"]
            .as_array()
            .expect("standard configOptions");
        let model = options
            .iter()
            .find(|option| option["id"] == "model")
            .expect("model config option");
        assert_eq!(model["category"], "model");
        assert_eq!(model["currentValue"], "llmsim:llmsim-yolop");
        let selected_model = model["options"]
            .as_array()
            .expect("model options")
            .iter()
            .find_map(|option| {
                option["value"]
                    .as_str()
                    .filter(|value| value.starts_with("ollama:"))
            })
            .expect("Ollama model option")
            .to_owned();

        send_json(
            &mut w,
            json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "session/set_config_option",
                "params": {
                    "sessionId": session_id,
                    "configId": "model",
                    "value": selected_model
                }
            }),
        )
        .await;
        let (changed, _) = collect_until_response_id(&mut reader, 2).await;
        assert!(
            changed.get("error").is_none(),
            "selection failed: {changed}"
        );
        let changed_model = changed["result"]["configOptions"]
            .as_array()
            .expect("updated configOptions")
            .iter()
            .find(|option| option["id"] == "model")
            .expect("updated model config option");
        assert_eq!(changed_model["currentValue"], selected_model);

        send_json(
            &mut w,
            json!({
                "jsonrpc": "2.0",
                "id": 3,
                "method": "session/set_config_option",
                "params": {
                    "sessionId": session_id,
                    "configId": "model",
                    "value": "unknown:model"
                }
            }),
        )
        .await;
        let (invalid, _) = collect_until_response_id(&mut reader, 3).await;
        assert_eq!(invalid["error"]["code"], -32602);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn setup_credential_change_pushes_refreshed_model_options() {
        let sessions = tempfile::tempdir().expect("sessions tempdir");
        let settings = Arc::new(SettingsStore::open(sessions.path().join("settings.toml")));
        let (mut w, mut reader, _server) = start_raw_server_with_settings(
            fixed("unused"),
            sessions.path().to_path_buf(),
            settings.clone(),
        );
        send_json(
            &mut w,
            json!({ "jsonrpc": "2.0", "id": 0, "method": "initialize", "params": { "protocolVersion": 1 } }),
        )
        .await;
        collect_until_response_id(&mut reader, 0).await;

        let cwd = tempfile::tempdir().expect("cwd tempdir").keep();
        send_json(
            &mut w,
            json!({ "jsonrpc": "2.0", "id": 1, "method": "session/new", "params": { "cwd": cwd, "mcpServers": [] } }),
        )
        .await;
        let (new_session, _) = collect_until_response_id(&mut reader, 1).await;
        let session_id = new_session["result"]["sessionId"]
            .as_str()
            .expect("sessionId");
        let initial_options = new_session["result"]["configOptions"][0]["options"]
            .as_array()
            .expect("initial model options");
        assert!(
            !initial_options.iter().any(|option| option["value"]
                .as_str()
                .is_some_and(|value| value.starts_with("openai:"))),
            "disconnected OpenAI must not be advertised"
        );

        settings
            .set_token("openai".to_string(), "test-token".to_string())
            .expect("connect OpenAI outside the ACP prompt channel");
        send_json(
            &mut w,
            json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "session/prompt",
                "params": {
                    "sessionId": session_id,
                    "prompt": [{ "type": "text", "text": "/setup status" }]
                }
            }),
        )
        .await;
        let (_, updates) = collect_until_response_id(&mut reader, 2).await;
        let refreshed = updates
            .iter()
            .filter_map(|message| message.get("params")?.get("update"))
            .find(|update| update["sessionUpdate"] == "config_option_update")
            .expect("config_option_update after credential change");
        let model = refreshed["configOptions"]
            .as_array()
            .expect("refreshed options")
            .iter()
            .find(|option| option["id"] == "model")
            .expect("model config option");
        assert!(
            model["options"]
                .as_array()
                .expect("model choices")
                .iter()
                .any(|option| option["value"]
                    .as_str()
                    .is_some_and(|value| value.starts_with("openai:"))),
            "connected OpenAI must be advertised after the settings change: {model}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn setup_token_is_rejected_without_echoing_the_secret() {
        let sessions = tempfile::tempdir().expect("sessions tempdir");
        let (mut w, mut reader, _server) =
            start_raw_server(fixed("unused"), sessions.path().to_path_buf());
        send_json(
            &mut w,
            json!({ "jsonrpc": "2.0", "id": 0, "method": "initialize", "params": { "protocolVersion": 1 } }),
        )
        .await;
        collect_until_response_id(&mut reader, 0).await;
        let cwd = tempfile::tempdir().expect("cwd tempdir").keep();
        send_json(
            &mut w,
            json!({ "jsonrpc": "2.0", "id": 1, "method": "session/new", "params": { "cwd": cwd, "mcpServers": [] } }),
        )
        .await;
        let (new_session, _) = collect_until_response_id(&mut reader, 1).await;
        let session_id = new_session["result"]["sessionId"]
            .as_str()
            .expect("sessionId");

        send_json(
            &mut w,
            json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "session/prompt",
                "params": {
                    "sessionId": session_id,
                    "prompt": [{ "type": "text", "text": "/setup token openai top-secret" }]
                }
            }),
        )
        .await;
        let (_, updates) = collect_until_response_id(&mut reader, 2).await;
        let serialized = serde_json::to_string(&updates).expect("updates serialize");
        assert!(serialized.contains("cannot be entered over ACP"));
        assert!(!serialized.contains("top-secret"));
        let settings_file =
            std::fs::read_to_string(sessions.path().join("settings.toml")).unwrap_or_default();
        assert!(!settings_file.contains("top-secret"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn authenticate_refreshes_model_options_for_open_sessions() {
        let sessions = tempfile::tempdir().expect("sessions tempdir");
        let (mut w, mut reader, _server) =
            start_raw_server(fixed("unused"), sessions.path().to_path_buf());
        send_json(
            &mut w,
            json!({ "jsonrpc": "2.0", "id": 0, "method": "initialize", "params": { "protocolVersion": 1 } }),
        )
        .await;
        let (initialize, _) = collect_until_response_id(&mut reader, 0).await;
        assert_eq!(
            initialize["result"]["authMethods"][0]["id"],
            "test_connect_openai"
        );

        let cwd = tempfile::tempdir().expect("cwd tempdir").keep();
        send_json(
            &mut w,
            json!({ "jsonrpc": "2.0", "id": 1, "method": "session/new", "params": { "cwd": cwd, "mcpServers": [] } }),
        )
        .await;
        let (new_session, _) = collect_until_response_id(&mut reader, 1).await;
        let session_id = new_session["result"]["sessionId"]
            .as_str()
            .expect("sessionId");

        send_json(
            &mut w,
            json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "authenticate",
                "params": { "methodId": "unknown" }
            }),
        )
        .await;
        let (unknown, _) = collect_until_response_id(&mut reader, 2).await;
        assert_eq!(unknown["error"]["code"], -32602);

        send_json(
            &mut w,
            json!({
                "jsonrpc": "2.0",
                "id": 3,
                "method": "authenticate",
                "params": { "methodId": "test_connect_openai" }
            }),
        )
        .await;
        collect_until_response_id(&mut reader, 3).await;
        let update = next_json(&mut reader).await;
        let refreshed = update
            .get("params")
            .filter(|params| {
                params["sessionId"] == session_id
                    && params["update"]["sessionUpdate"] == "config_option_update"
            })
            .expect("open session receives config option refresh after authentication");
        assert!(
            refreshed["update"]["configOptions"][0]["options"]
                .as_array()
                .expect("model choices")
                .iter()
                .any(|option| option["value"]
                    .as_str()
                    .is_some_and(|value| value.starts_with("openai:")))
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn protective_mode_blocks_tool_when_permission_rejected() {
        let sessions = tempfile::tempdir().expect("sessions tempdir");
        let (mut w, mut reader, _server, session_id) =
            protective_session(one_bash_call(), sessions.path().to_path_buf()).await;

        let (asked, statuses) =
            statuses_for_gated_prompt(&mut w, &mut reader, &session_id, 3, "reject_once").await;

        assert!(asked, "protective mode must request permission before bash");
        assert!(
            statuses.iter().any(|s| s == "failed"),
            "a rejected tool must surface as failed; saw {statuses:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn protective_mode_runs_tool_when_permission_allowed() {
        let sessions = tempfile::tempdir().expect("sessions tempdir");
        let (mut w, mut reader, _server, session_id) =
            protective_session(one_bash_call(), sessions.path().to_path_buf()).await;

        let (asked, statuses) =
            statuses_for_gated_prompt(&mut w, &mut reader, &session_id, 3, "allow_once").await;

        assert!(asked, "protective mode must request permission before bash");
        assert!(
            statuses.iter().any(|s| s == "completed"),
            "an allowed tool must run to completion; saw {statuses:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn load_session_replays_history_and_continues_turns() {
        let sessions = tempfile::tempdir().expect("sessions tempdir");
        let sessions_dir = sessions.path().to_path_buf();
        let cwd = tempfile::tempdir().expect("cwd tempdir").keep();
        let first_config = LlmSimConfig::scripted(vec![
            SimTurn::ToolCalls(vec![SimToolCall {
                name: "bash".to_string(),
                arguments: json!({ "command": "printf replayed-tool" }),
                id: Some("call_replayed".to_string()),
            }]),
            SimTurn::Assistant("first answer".to_string()),
        ]);

        let (mut first_w, mut first_reader, first_server) =
            start_raw_server(first_config, sessions_dir.clone());
        send_json(
            &mut first_w,
            json!({ "jsonrpc": "2.0", "id": 0, "method": "initialize", "params": { "protocolVersion": 1 } }),
        )
        .await;
        let (init, _) = collect_until_response_id(&mut first_reader, 0).await;
        assert_eq!(init["result"]["agentCapabilities"]["loadSession"], true);

        send_json(
            &mut first_w,
            json!({ "jsonrpc": "2.0", "id": 1, "method": "session/new", "params": { "cwd": cwd.to_str().unwrap(), "mcpServers": [] } }),
        )
        .await;
        let (new_session, _) = collect_until_response_id(&mut first_reader, 1).await;
        let session_id = new_session["result"]["sessionId"]
            .as_str()
            .expect("sessionId")
            .to_string();

        send_json(
            &mut first_w,
            json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "session/prompt",
                "params": { "sessionId": session_id, "prompt": [{ "type": "text", "text": "first prompt" }] },
            }),
        )
        .await;
        let (prompt_response, first_updates) =
            collect_until_response_id(&mut first_reader, 2).await;
        assert!(prompt_response.get("result").is_some());
        assert!(
            update_texts(&first_updates, "agent_message_chunk")
                .iter()
                .any(|text| text.contains("first answer")),
            "expected first prompt response in updates: {first_updates:?}"
        );

        drop(first_w);
        drop(first_reader);
        tokio::time::timeout(Duration::from_secs(10), first_server)
            .await
            .expect("first server must stop")
            .expect("first server joins")
            .expect("first server returns Ok");

        let (mut second_w, mut second_reader, second_server) =
            start_raw_server(fixed("second answer"), sessions_dir);
        send_json(
            &mut second_w,
            json!({ "jsonrpc": "2.0", "id": 10, "method": "initialize", "params": { "protocolVersion": 1 } }),
        )
        .await;
        let _ = collect_until_response_id(&mut second_reader, 10).await;

        send_json(
            &mut second_w,
            json!({
                "jsonrpc": "2.0",
                "id": 11,
                "method": "session/load",
                "params": { "sessionId": session_id, "cwd": cwd.to_str().unwrap(), "mcpServers": [] },
            }),
        )
        .await;
        let (load_response, replay_updates) =
            collect_until_response_id(&mut second_reader, 11).await;
        assert!(load_response.get("result").is_some());
        assert!(
            update_texts(&replay_updates, "user_message_chunk")
                .iter()
                .any(|text| text.contains("first prompt")),
            "expected replayed user message, got: {replay_updates:?}"
        );
        assert!(
            update_texts(&replay_updates, "agent_message_chunk")
                .iter()
                .any(|text| text.contains("first answer")),
            "expected replayed agent message, got: {replay_updates:?}"
        );
        let replayed_tool_updates = |kind: &str| {
            replay_updates
                .iter()
                .filter_map(|message| message.get("params")?.get("update")?.as_object())
                .filter(|update| update.get("sessionUpdate").and_then(Value::as_str) == Some(kind))
                .collect::<Vec<_>>()
        };
        assert!(
            replayed_tool_updates("tool_call").iter().any(|update| {
                update.get("toolCallId").and_then(Value::as_str) == Some("call_replayed")
                    && update["rawInput"]["command"] == "printf replayed-tool"
            }),
            "expected reconstructed tool call before completion: {replay_updates:?}"
        );
        assert!(
            replayed_tool_updates("tool_call_update")
                .iter()
                .any(|update| {
                    update.get("toolCallId").and_then(Value::as_str) == Some("call_replayed")
                        && update["status"] == "completed"
                }),
            "expected replayed tool completion: {replay_updates:?}"
        );

        send_json(
            &mut second_w,
            json!({
                "jsonrpc": "2.0",
                "id": 12,
                "method": "session/prompt",
                "params": { "sessionId": session_id, "prompt": [{ "type": "text", "text": "second prompt" }] },
            }),
        )
        .await;
        let (second_prompt, second_updates) =
            collect_until_response_id(&mut second_reader, 12).await;
        assert!(second_prompt.get("result").is_some());
        assert!(
            update_texts(&second_updates, "agent_message_chunk")
                .iter()
                .any(|text| text.contains("second answer")),
            "expected loaded session to accept a new prompt, got: {second_updates:?}"
        );

        drop(second_w);
        drop(second_reader);
        tokio::time::timeout(Duration::from_secs(10), second_server)
            .await
            .expect("second server must stop")
            .expect("second server joins")
            .expect("second server returns Ok");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn completion_tracking_pivot_survives_acp_session_resume() {
        let sessions = tempfile::tempdir().expect("sessions tempdir");
        let sessions_dir = sessions.path().to_path_buf();
        let cwd = tempfile::tempdir().expect("cwd tempdir").keep();
        let settings = Arc::new(SettingsStore::open(sessions_dir.join("settings.toml")));
        settings
            .set_capability_enabled(crate::session_state::user_ask::USER_ASK_CAPABILITY_ID, true)
            .expect("enable completion tracking");
        let (mut first_w, mut first_reader, first_server) =
            start_raw_server_with_settings(fixed(""), sessions_dir.clone(), settings);
        send_json(
            &mut first_w,
            json!({ "jsonrpc": "2.0", "id": 0, "method": "initialize", "params": { "protocolVersion": 1 } }),
        )
        .await;
        collect_until_response_id(&mut first_reader, 0).await;
        send_json(
            &mut first_w,
            json!({ "jsonrpc": "2.0", "id": 1, "method": "session/new", "params": { "cwd": cwd.to_str().unwrap(), "mcpServers": [] } }),
        )
        .await;
        let (new_session, _) = collect_until_response_id(&mut first_reader, 1).await;
        let session_id = new_session["result"]["sessionId"]
            .as_str()
            .expect("sessionId")
            .to_string();

        for (id, prompt) in [
            (2, "original unfinished ask"),
            (3, "pivoted unfinished ask"),
        ] {
            send_json(
                &mut first_w,
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "method": "session/prompt",
                    "params": { "sessionId": session_id, "prompt": [{ "type": "text", "text": prompt }] },
                }),
            )
            .await;
            let (_, updates) = collect_until_response_id(&mut first_reader, id).await;
            assert!(
                update_texts(&updates, "agent_message_chunk")
                    .iter()
                    .any(|text| text.contains("budget exhausted")),
                "unfinished ask should stop at the host budget: {updates:?}"
            );
        }

        drop(first_w);
        drop(first_reader);
        tokio::time::timeout(Duration::from_secs(10), first_server)
            .await
            .expect("first server must stop")
            .expect("first server joins")
            .expect("first server returns Ok");

        let (mut second_w, mut second_reader, second_server) =
            start_raw_server(fixed("unused"), sessions_dir);
        send_json(
            &mut second_w,
            json!({ "jsonrpc": "2.0", "id": 10, "method": "initialize", "params": { "protocolVersion": 1 } }),
        )
        .await;
        collect_until_response_id(&mut second_reader, 10).await;
        send_json(
            &mut second_w,
            json!({
                "jsonrpc": "2.0",
                "id": 11,
                "method": "session/load",
                "params": { "sessionId": session_id, "cwd": cwd.to_str().unwrap(), "mcpServers": [] },
            }),
        )
        .await;
        collect_until_response_id(&mut second_reader, 11).await;
        send_json(
            &mut second_w,
            json!({
                "jsonrpc": "2.0",
                "id": 12,
                "method": "session/prompt",
                "params": { "sessionId": session_id, "prompt": [{ "type": "text", "text": "/ask" }] },
            }),
        )
        .await;
        let (_, ask_updates) = collect_until_response_id(&mut second_reader, 12).await;
        let ask_text = serde_json::to_string(&ask_updates).expect("serialize ask updates");
        assert!(
            ask_text.contains("pivoted unfinished ask"),
            "resumed tracker must retain the actual pivoted ask: {ask_updates:?}"
        );
        assert!(
            !ask_text.contains("ask: original unfinished ask"),
            "superseded ask must not resume as current: {ask_updates:?}"
        );

        drop(second_w);
        drop(second_reader);
        tokio::time::timeout(Duration::from_secs(10), second_server)
            .await
            .expect("second server must stop")
            .expect("second server joins")
            .expect("second server returns Ok");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn full_handshake_then_prompt_streams_text_and_ends_turn() {
        let run = with_sdk_client(fixed("hello from acp"), |client| async move {
            let mut session = client.new_session().await?;
            SdkClient::prompt(&mut session, "say hi").await
        })
        .await;

        assert_eq!(run.stop_reason, StopReason::EndTurn);
        assert!(
            run.assistant_text().contains("hello from acp"),
            "expected streamed assistant text, got updates: {:?}",
            run.updates
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn new_session_advertises_available_commands() {
        let command_updates = with_sdk_client(fixed("hi"), |client| async move {
            let mut session = client.new_session().await?;
            collect_available_commands(&mut session).await
        })
        .await;
        assert!(
            !command_updates.is_empty(),
            "expected available_commands_update"
        );
        let commands = command_updates[0]["availableCommands"]
            .as_array()
            .expect("availableCommands array");
        assert!(
            commands.iter().any(|c| c["name"] == "setup"),
            "expected /setup to be advertised, got: {commands:?}"
        );
        assert!(
            commands.iter().any(|c| c["name"] == "shell"),
            "expected /shell to be advertised, got: {commands:?}"
        );
        for name in ["rewind", "undo", "redo"] {
            assert!(
                commands.iter().any(|command| command["name"] == name),
                "expected /{name} to be advertised, got: {commands:?}"
            );
        }
        let setup = commands
            .iter()
            .find(|c| c["name"] == "setup")
            .expect("setup command");
        assert_eq!(
            setup["description"],
            "Configure provider, API key, and model."
        );
        assert_eq!(setup["input"]["hint"], "<action>");
        let suggestions = setup["_meta"]["yolop.dev/command"]["args"][0]["suggestions"]
            .as_array()
            .expect("setup suggestions");
        assert!(
            suggestions.iter().any(|s| s == "login")
                && suggestions.iter().any(|s| s == "status")
                && suggestions.iter().any(|s| s == "provider openai"),
            "expected setup choices in command metadata, got: {setup:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn slash_system_command_executes_without_model_turn() {
        let run = with_sdk_client(fixed("model should not run"), |client| async move {
            let mut session = client.new_session().await?;
            let _ = collect_available_commands(&mut session).await?;
            SdkClient::prompt(&mut session, "/setup status").await
        })
        .await;

        assert_eq!(run.stop_reason, StopReason::EndTurn);
        let tool_calls = run.updates_of_kind("tool_call");
        assert!(
            tool_calls
                .iter()
                .any(|u| u["title"] == "/setup status" && u["rawInput"]["command"] == "setup"),
            "expected command tool_call, got updates: {:?}",
            run.updates
        );
        let tool_updates = run.updates_of_kind("tool_call_update");
        let completed = tool_updates
            .iter()
            .find(|u| u["status"] == "completed")
            .expect("completed command tool update");
        assert!(
            completed["content"][0]["content"]["text"]
                .as_str()
                .is_some_and(|text| text.contains("setup: provider=")),
            "expected setup status in command output, got: {completed:?}"
        );
        assert_eq!(completed["rawOutput"]["success"], true);
        assert!(
            !run.assistant_text().contains("model should not run"),
            "slash command should not invoke the model"
        );
        assert!(!run.updates_of_kind("available_commands_update").is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn plain_setup_runs_advertised_acp_authentication() {
        let run = with_sdk_client(fixed("model should not run"), |client| async move {
            let mut session = client.new_session().await?;
            let _ = collect_available_commands(&mut session).await?;
            SdkClient::prompt(&mut session, "/setup").await
        })
        .await;

        assert_eq!(run.stop_reason, StopReason::EndTurn);
        let completed = run
            .updates_of_kind("tool_call_update")
            .into_iter()
            .find(|update| update["status"] == "completed")
            .expect("completed setup login");
        assert_eq!(completed["rawOutput"]["success"], true);
        assert!(
            completed["content"][0]["content"]["text"]
                .as_str()
                .is_some_and(|text| text.contains("Authentication refreshed")),
            "expected authentication recovery output, got: {completed:?}"
        );
        let config_update = run
            .updates_of_kind("config_option_update")
            .into_iter()
            .next()
            .expect("model options refreshed after setup login");
        assert!(
            config_update["configOptions"][0]["options"]
                .as_array()
                .expect("model options")
                .iter()
                .any(|option| option["value"]
                    .as_str()
                    .is_some_and(|value| value.starts_with("openai:"))),
            "authenticated provider must become selectable: {config_update:?}"
        );
        assert!(!run.assistant_text().contains("model should not run"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn provider_authentication_failure_has_actionable_recovery() {
        use everruns_core::llmsim_driver::SimError;

        let config = LlmSimConfig::scripted(vec![SimTurn::Error(SimError::Authentication)]);
        let run = with_sdk_client(config, |client| async move {
            let mut session = client.new_session().await?;
            let _ = collect_available_commands(&mut session).await?;
            SdkClient::prompt(&mut session, "continue the task").await
        })
        .await;

        let text = run.assistant_text();
        assert!(
            text.contains("authentication failed")
                && text.contains("cannot be entered securely over ACP"),
            "expected actionable authentication recovery, got: {text}"
        );
        assert!(!text.contains("contact support"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bang_shell_command_executes_without_model_turn() {
        let run = with_sdk_client(fixed("model should not run"), |client| async move {
            let mut session = client.new_session().await?;
            let _ = collect_available_commands(&mut session).await?;
            SdkClient::prompt(&mut session, "!printf acp-shell").await
        })
        .await;

        assert_eq!(run.stop_reason, StopReason::EndTurn);
        let tool_calls = run.updates_of_kind("tool_call");
        assert!(
            tool_calls
                .iter()
                .any(|u| u["title"] == "!shell printf acp-shell"
                    && u["rawInput"]["command"] == "shell"),
            "expected shell command tool_call, got updates: {:?}",
            run.updates
        );
        let tool_updates = run.updates_of_kind("tool_call_update");
        let completed = tool_updates
            .iter()
            .find(|u| u["status"] == "completed")
            .expect("completed command tool update");
        assert!(
            completed["content"][0]["content"]["text"]
                .as_str()
                .is_some_and(|text| text.contains("acp-shell")),
            "expected shell output in command content, got: {completed:?}"
        );
        assert_eq!(completed["rawOutput"]["success"], true);
        assert!(
            !run.assistant_text().contains("model should not run"),
            "bang shell command should not invoke the model"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unknown_method_returns_method_not_found() {
        let err = with_sdk_client(fixed("hi"), |client| async move {
            match client.cx.send_request(UnknownRequest {}).block_task().await {
                Ok(_) => panic!("unknown method unexpectedly succeeded"),
                Err(err) => Ok(err),
            }
        })
        .await;
        assert_eq!(err.code, agent_client_protocol::ErrorCode::MethodNotFound);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn prompt_to_unknown_session_is_invalid_params() {
        let err = with_sdk_client(fixed("hi"), |client| async move {
            let request = PromptRequest::new(
                SessionId::new("session_does_not_exist"),
                vec!["hello".to_string().into()],
            );
            match client.cx.send_request(request).block_task().await {
                Ok(_) => panic!("unknown session unexpectedly succeeded"),
                Err(err) => Ok(err),
            }
        })
        .await;
        assert_eq!(err.code, agent_client_protocol::ErrorCode::InvalidParams);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn load_unknown_session_is_invalid_params() {
        let sessions = tempfile::tempdir().expect("sessions tempdir");
        let cwd = tempfile::tempdir().expect("cwd tempdir").keep();
        let (mut client_w, mut reader, server) =
            start_raw_server(fixed("hi"), sessions.path().to_path_buf());

        send_json(
            &mut client_w,
            json!({ "jsonrpc": "2.0", "id": 0, "method": "initialize", "params": { "protocolVersion": 1 } }),
        )
        .await;
        let _ = collect_until_response_id(&mut reader, 0).await;

        send_json(
            &mut client_w,
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "session/load",
                "params": { "sessionId": "session_does_not_exist", "cwd": cwd.to_str().unwrap(), "mcpServers": [] },
            }),
        )
        .await;
        let (response, updates) = collect_until_response_id(&mut reader, 1).await;
        assert!(
            updates.is_empty(),
            "unknown load should not replay: {updates:?}"
        );
        assert_eq!(response["error"]["code"], -32602);

        drop(client_w);
        drop(reader);
        tokio::time::timeout(Duration::from_secs(10), server)
            .await
            .expect("server must stop")
            .expect("server joins")
            .expect("server returns Ok");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn scripted_tool_call_streams_tool_updates() {
        // First scripted turn writes a marker file via bash; second closes
        // the loop with plain text. The bash write runs autonomously.
        let marker = "acp_tool_ran.marker";
        let config = LlmSimConfig::scripted(vec![
            SimTurn::ToolCalls(vec![SimToolCall {
                name: "bash".to_string(),
                arguments: json!({ "command": format!("touch {marker}") }),
                id: None,
            }]),
            SimTurn::Assistant("tool done".to_string()),
        ]);
        let run = with_sdk_client(config, |client| async move {
            let mut session = client.new_session().await?;
            let _ = collect_available_commands(&mut session).await?;
            SdkClient::prompt(&mut session, "run the tool").await
        })
        .await;

        assert_eq!(run.stop_reason, StopReason::EndTurn);
        let tool_calls = run.updates_of_kind("tool_call");
        assert!(
            !tool_calls.is_empty(),
            "expected a tool_call update, got: {:?}",
            run.updates
        );
        assert_eq!(tool_calls[0]["kind"], "execute");
        let updates = run.updates_of_kind("tool_call_update");
        assert!(
            updates.iter().any(|u| u["status"] == "completed"),
            "expected a completed tool_call_update, got: {:?}",
            run.updates
        );
        assert!(run.assistant_text().contains("tool done"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn opt_in_completion_gate_continues_tool_only_stop_to_one_final() {
        use everruns_core::llmsim_driver::OnExhausted;

        let config = LlmSimConfig::scripted(vec![
            SimTurn::ToolCalls(vec![SimToolCall {
                name: "bash".to_string(),
                arguments: json!({ "command": "true" }),
                id: None,
            }]),
            SimTurn::Assistant(String::new()),
            SimTurn::Assistant("verified final".to_string()),
        ])
        .with_on_exhausted(OnExhausted::Error);
        let run = with_sdk_client_completion(config, true, |client| async move {
            let mut session = client.new_session().await?;
            let _ = collect_available_commands(&mut session).await?;
            SdkClient::prompt(&mut session, "run the check and report the result").await
        })
        .await;

        assert_eq!(run.stop_reason, StopReason::EndTurn);
        assert!(run.assistant_text().contains("verified final"));
        assert_eq!(run.assistant_text().matches("verified final").count(), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn trivial_exact_reply_uses_one_generation_and_no_evaluator() {
        use everruns_core::llmsim_driver::OnExhausted;

        let config = LlmSimConfig::scripted(vec![SimTurn::Assistant("exact".to_string())])
            .with_on_exhausted(OnExhausted::Error);
        let run = with_sdk_client_completion(config, true, |client| async move {
            let mut session = client.new_session().await?;
            let _ = collect_available_commands(&mut session).await?;
            SdkClient::prompt(&mut session, "reply exactly: exact").await
        })
        .await;

        assert_eq!(run.assistant_text(), "exact");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tool_using_final_pays_one_bounded_semantic_evaluator_call() {
        use everruns_core::llmsim_driver::OnExhausted;

        // Baseline trivial fixture above consumes one scripted generation.
        // This mutation fixture consumes two agent-loop generations plus
        // exactly one evaluator generation; exhaustion makes any regression
        // beyond that explicit three-call budget fail.
        let config = LlmSimConfig::scripted(vec![
            SimTurn::ToolCalls(vec![SimToolCall {
                name: "bash".to_string(),
                arguments: json!({ "command": "true" }),
                id: None,
            }]),
            SimTurn::Assistant("candidate final".to_string()),
            SimTurn::Assistant(r#"{"outcome":"achieved","reason":"validated"}"#.to_string()),
        ])
        .with_on_exhausted(OnExhausted::Error);
        let run = with_sdk_client_completion(config, true, |client| async move {
            let mut session = client.new_session().await?;
            let _ = collect_available_commands(&mut session).await?;
            SdkClient::prompt(&mut session, "make and validate the change").await
        })
        .await;

        assert_eq!(run.assistant_text(), "candidate final");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn permanent_provider_failure_is_failed_without_continuation() {
        use everruns_core::llmsim_driver::{OnExhausted, SimError};

        let config = LlmSimConfig::scripted(vec![SimTurn::Error(SimError::Other(
            "permanent provider failure".to_string(),
        ))])
        .with_on_exhausted(OnExhausted::Error);
        let run = with_sdk_client_completion(config, true, |client| async move {
            let mut session = client.new_session().await?;
            let _ = collect_available_commands(&mut session).await?;
            SdkClient::prompt(&mut session, "do the work").await
        })
        .await;

        assert!(run.assistant_text().contains("task failed"));
        assert!(!run.assistant_text().contains("budget exhausted"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn clarification_is_blocked_and_not_auto_continued() {
        use everruns_core::llmsim_driver::OnExhausted;

        let config = LlmSimConfig::scripted(vec![SimTurn::Assistant(
            "I need the target path. Which file should I edit?".to_string(),
        )])
        .with_on_exhausted(OnExhausted::Error);
        let run = with_sdk_client_completion(config, true, |client| async move {
            let mut session = client.new_session().await?;
            let _ = collect_available_commands(&mut session).await?;
            SdkClient::prompt(&mut session, "edit the file").await
        })
        .await;

        assert_eq!(
            run.assistant_text(),
            "I need the target path. Which file should I edit?",
            "ACP must preserve the clarification without appending host status"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn empty_reply_loop_stops_at_host_budget() {
        let run = with_sdk_client_completion(LlmSimConfig::fixed(""), true, |client| async move {
            let mut session = client.new_session().await?;
            let _ = collect_available_commands(&mut session).await?;
            SdkClient::prompt(&mut session, "finish this").await
        })
        .await;

        assert!(run.assistant_text().contains("budget exhausted"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn write_todos_tool_call_streams_plan_update() {
        let config = LlmSimConfig::scripted(vec![
            SimTurn::ToolCalls(vec![SimToolCall {
                name: "write_todos".to_string(),
                arguments: json!({
                    "todos": [
                        { "content": "step one", "status": "in_progress", "activeForm": "doing one" },
                        { "content": "step two", "status": "pending", "activeForm": "doing two" },
                    ]
                }),
                id: None,
            }]),
            SimTurn::Assistant("planned".to_string()),
        ]);
        let run = with_sdk_client(config, |client| async move {
            let mut session = client.new_session().await?;
            let _ = collect_available_commands(&mut session).await?;
            SdkClient::prompt(&mut session, "make a plan").await
        })
        .await;

        let plans = run.updates_of_kind("plan");
        assert!(
            !plans.is_empty(),
            "expected a plan update, got: {:?}",
            run.updates
        );
        let entries = plans[0]["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0]["content"], "step one");
        assert_eq!(entries[0]["status"], "in_progress");
    }

    /// Regression: if the client disconnects mid-turn, `serve` must still
    /// return rather than deadlock. The EOF signal winds the agent process
    /// down even while a turn task is in flight.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn disconnect_mid_turn_lets_serve_return() {
        // First turn issues a bash tool call; we disconnect as soon as the
        // agent starts streaming the turn back.
        let config = LlmSimConfig::scripted(vec![
            SimTurn::ToolCalls(vec![SimToolCall {
                name: "bash".to_string(),
                arguments: json!({ "command": "true" }),
                id: None,
            }]),
            SimTurn::Assistant("after".to_string()),
        ]);

        let (mut client_w, agent_r) = tokio::io::duplex(64 * 1024);
        let (agent_w, client_r) = tokio::io::duplex(64 * 1024);
        let sessions = tempfile::tempdir().expect("sessions tempdir").keep();
        let settings = Arc::new(SettingsStore::open(sessions.join("settings.toml")));
        let factory = Arc::new(ScriptedFactory {
            config,
            sessions_dir: sessions,
            settings,
        });
        let server = tokio::spawn(async move { serve(agent_r, agent_w, factory).await });
        let mut reader = BufReader::new(client_r).lines();

        async fn send(w: &mut DuplexStream, value: Value) {
            let line = value.to_string();
            w.write_all(line.as_bytes()).await.unwrap();
            w.write_all(b"\n").await.unwrap();
            w.flush().await.unwrap();
        }
        async fn next(reader: &mut Lines<BufReader<DuplexStream>>) -> Value {
            let line = tokio::time::timeout(Duration::from_secs(15), reader.next_line())
                .await
                .expect("timed out")
                .expect("read line")
                .expect("stream open");
            serde_json::from_str(&line).expect("valid json")
        }
        async fn await_id(reader: &mut Lines<BufReader<DuplexStream>>, id: i64) -> Value {
            loop {
                let msg = next(reader).await;
                if msg.get("id").and_then(Value::as_i64) == Some(id)
                    && (msg.get("result").is_some() || msg.get("error").is_some())
                {
                    return msg;
                }
            }
        }

        send(
            &mut client_w,
            json!({ "jsonrpc": "2.0", "id": 0, "method": "initialize", "params": { "protocolVersion": 1 } }),
        )
        .await;
        await_id(&mut reader, 0).await;

        let cwd = tempfile::tempdir().expect("cwd tempdir").keep();
        send(
            &mut client_w,
            json!({ "jsonrpc": "2.0", "id": 1, "method": "session/new", "params": { "cwd": cwd.to_str().unwrap(), "mcpServers": [] } }),
        )
        .await;
        let session_id = await_id(&mut reader, 1).await["result"]["sessionId"]
            .as_str()
            .expect("sessionId")
            .to_string();

        // Send a prompt but never read its response: we want to disconnect
        // mid-turn, while the turn task is still running.
        send(
            &mut client_w,
            json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "session/prompt",
                "params": { "sessionId": session_id, "prompt": [{ "type": "text", "text": "go" }] },
            }),
        )
        .await;

        // Wait until the agent starts streaming the turn back, then drop the
        // client's write half to simulate a disconnect mid-turn.
        loop {
            let msg = next(&mut reader).await;
            if msg.get("method").and_then(Value::as_str) == Some("session/update") {
                break;
            }
        }
        drop(client_w);
        drop(reader);

        // The server must wind down: the turn finishes and `serve` returns.
        tokio::time::timeout(Duration::from_secs(10), server)
            .await
            .expect("serve must return after disconnect, not hang")
            .expect("serve task joins")
            .expect("serve returns Ok");
    }

    /// The everruns-native path: a `spawn_background` run that finishes while the
    /// ACP session is idle must wake the agent through the platform-store wake
    /// seam (`background_wake`), with no client prompt. Proves the seam delivers
    /// everruns background completions, not just the legacy yolop registry.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn spawn_background_completion_wakes_agent() {
        // Turn 1: spawn_background wrapping bash `true`. Turn 2 closes the user
        // turn. Turn 3 is what the seam-driven wake turn asks for once the run
        // completes and signals the session.
        let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
        let config = LlmSimConfig::scripted(vec![
            SimTurn::Assistant("recorded long parent context".to_string()),
            SimTurn::ToolCalls(vec![SimToolCall {
                name: "spawn_background".to_string(),
                arguments: json!({
                    "tool": "bash",
                    "args": { "command": "printf decisive-validation" },
                    "title": "validate compact wake",
                    "signal_on_completion": true,
                }),
                id: None,
            }]),
            SimTurn::Assistant("spawned background bash".to_string()),
            SimTurn::Assistant("reviewed spawn_background result".to_string()),
        ])
        .with_message_capture(captured.clone());

        let sessions = tempfile::tempdir().expect("sessions tempdir").keep();
        let (mut client_w, mut reader, _server) = start_raw_server(config, sessions.clone());

        send_json(
            &mut client_w,
            json!({ "jsonrpc": "2.0", "id": 0, "method": "initialize", "params": { "protocolVersion": 1 } }),
        )
        .await;
        collect_until_response_id(&mut reader, 0).await;

        let cwd = tempfile::tempdir().expect("cwd tempdir").keep();
        send_json(
            &mut client_w,
            json!({ "jsonrpc": "2.0", "id": 1, "method": "session/new", "params": { "cwd": cwd.to_str().unwrap(), "mcpServers": [] } }),
        )
        .await;
        let (new_session, _) = collect_until_response_id(&mut reader, 1).await;
        let session_id = new_session["result"]["sessionId"]
            .as_str()
            .expect("sessionId")
            .to_string();

        let long_parent = format!(
            "Primary ask: prove compact background continuation. {}",
            "LONG_PARENT_HISTORY_MARKER".repeat(32_000)
        );
        send_json(
            &mut client_w,
            json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "session/prompt",
                "params": { "sessionId": session_id, "prompt": [{ "type": "text", "text": long_parent.clone() }] },
            }),
        )
        .await;
        collect_until_response_id(&mut reader, 2).await;

        send_json(
            &mut client_w,
            json!({
                "jsonrpc": "2.0",
                "id": 3,
                "method": "session/prompt",
                "params": { "sessionId": session_id, "prompt": [{ "type": "text", "text": "run the requested validation in the background, then finish the primary ask" }] },
            }),
        )
        .await;
        let (_prompt_response, prompt_updates) = collect_until_response_id(&mut reader, 3).await;
        assert!(
            update_texts(&prompt_updates, "agent_message_chunk")
                .iter()
                .any(|t| t.contains("spawned background bash")),
            "expected the user turn to run spawn_background, got: {prompt_updates:?}"
        );

        let woke = tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let msg = next_json(&mut reader).await;
                if msg.get("method").and_then(Value::as_str) != Some("session/update") {
                    continue;
                }
                if let Some(text) = msg
                    .get("params")
                    .and_then(|p| p.get("update"))
                    .and_then(|u| u.get("content"))
                    .and_then(|c| c.get("text"))
                    .and_then(Value::as_str)
                    && text.contains("reviewed spawn_background result")
                {
                    return true;
                }
            }
        })
        .await;
        assert!(
            woke.unwrap_or(false),
            "expected a proactive wake turn after spawn_background finished (via the wake seam)"
        );
        assert!(
            sessions.join(&session_id).join(".background").is_dir(),
            "background artifacts should live in the session directory"
        );
        assert!(
            !cwd.join(".background").exists(),
            "background execution must not create .background in the workspace"
        );
        let session_dir = sessions.join(&session_id);
        let run_dir = std::fs::read_dir(session_dir.join(".background"))
            .expect("list durable background artifacts")
            .next()
            .expect("one background run")
            .expect("read background run entry")
            .path();
        assert!(
            std::fs::read_to_string(run_dir.join("output.log"))
                .expect("retrieve full raw background log on demand")
                .contains("decisive-validation")
        );
        assert!(run_dir.join("result.json").is_file());

        let calls = captured.lock().expect("captured provider messages");
        let candidate = calls.last().expect("automatic wake provider call");
        let candidate_bytes: usize = candidate
            .iter()
            .map(|message| message.content_as_text().len())
            .sum();
        // Explicit old-path baseline: the automatic turn received the stored
        // parent conversation, whose dominant fixture message is long_parent.
        // The candidate is the exact provider-visible wake request capture.
        let baseline_bytes = long_parent.len();
        let candidate_text = candidate
            .iter()
            .map(|message| message.content_as_text())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            candidate_bytes * 4 < baseline_bytes,
            "compact wake should reduce provider-visible bytes by at least 75%: {baseline_bytes} -> {candidate_bytes}"
        );
        assert!(candidate_text.contains("<background_handoff"));
        assert!(candidate_text.contains("validate compact wake"));
        assert!(candidate_text.contains("succeeded"));
        assert!(candidate_text.contains("result.json"));
        assert!(
            candidate_text.contains("run the requested validation"),
            "compact wake lost the tracked ask: {candidate_text}"
        );
        assert!(!candidate_text.contains("LONG_PARENT_HISTORY_MARKER"));

        let durable_events = std::fs::read_to_string(session_dir.join("events.jsonl"))
            .expect("read durable parent history");
        assert!(
            durable_events.contains("LONG_PARENT_HISTORY_MARKER"),
            "compaction must not rewrite durable parent history"
        );
        assert!(
            durable_events.contains("yolop.background_handoff"),
            "authenticated handoff provenance must survive session replay"
        );
    }

    /// A persisted local schedule must wake an idle ACP session at its due time,
    /// let that turn start the requested background command, then deliver the
    /// command's ordinary completion wake through the same serialized channel.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn scheduled_background_run_wakes_agent_through_completion() {
        let scheduled_at = (chrono::Utc::now() + chrono::Duration::seconds(3)).to_rfc3339();
        let config = LlmSimConfig::scripted(vec![
            SimTurn::ToolCalls(vec![SimToolCall {
                name: "spawn_background".to_string(),
                arguments: json!({
                    "tool": "bash",
                    "args": { "command": "true" },
                    "title": "scheduled ACP wake regression",
                    "schedule": { "scheduled_at": scheduled_at },
                    "signal_on_completion": true,
                }),
                id: None,
            }]),
            SimTurn::Assistant("scheduled background bash".to_string()),
            SimTurn::ToolCalls(vec![SimToolCall {
                name: "spawn_background".to_string(),
                arguments: json!({
                    "tool": "bash",
                    "args": { "command": "true" },
                    "title": "scheduled ACP wake regression",
                    "signal_on_completion": true,
                }),
                id: None,
            }]),
            SimTurn::Assistant("scheduled monitor fired and started run".to_string()),
            SimTurn::Assistant("scheduled run completed".to_string()),
        ]);

        let sessions = tempfile::tempdir().expect("sessions tempdir").keep();
        let (mut client_w, mut reader, _server) = start_raw_server(config, sessions);

        send_json(
            &mut client_w,
            json!({ "jsonrpc": "2.0", "id": 0, "method": "initialize", "params": { "protocolVersion": 1 } }),
        )
        .await;
        collect_until_response_id(&mut reader, 0).await;

        let cwd = tempfile::tempdir().expect("cwd tempdir").keep();
        send_json(
            &mut client_w,
            json!({ "jsonrpc": "2.0", "id": 1, "method": "session/new", "params": { "cwd": cwd.to_str().unwrap(), "mcpServers": [] } }),
        )
        .await;
        let (new_session, _) = collect_until_response_id(&mut reader, 1).await;
        let session_id = new_session["result"]["sessionId"]
            .as_str()
            .expect("sessionId")
            .to_string();

        send_json(
            &mut client_w,
            json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "session/prompt",
                "params": { "sessionId": session_id, "prompt": [{ "type": "text", "text": "check CI in a moment" }] },
            }),
        )
        .await;
        let (_prompt_response, prompt_updates) = collect_until_response_id(&mut reader, 2).await;
        assert!(
            update_texts(&prompt_updates, "agent_message_chunk")
                .iter()
                .any(|text| text.contains("scheduled background bash")),
            "expected the foreground turn to create the schedule, got: {prompt_updates:?}"
        );

        let messages = tokio::time::timeout(Duration::from_secs(25), async {
            let mut messages = Vec::new();
            while !messages
                .iter()
                .any(|text: &String| text.contains("scheduled run completed"))
            {
                let msg = next_json(&mut reader).await;
                if msg.get("method").and_then(Value::as_str) != Some("session/update") {
                    continue;
                }
                if let Some(text) = msg
                    .get("params")
                    .and_then(|params| params.get("update"))
                    .and_then(|update| update.get("content"))
                    .and_then(|content| content.get("text"))
                    .and_then(Value::as_str)
                {
                    messages.push(text.to_string());
                }
            }
            messages
        })
        .await
        .expect("scheduled run and its completion wake must arrive");

        assert!(
            messages
                .iter()
                .any(|text| text.contains("scheduled monitor fired and started run")),
            "expected the due schedule to start a wake turn, got: {messages:?}"
        );
        assert!(
            messages
                .iter()
                .any(|text| text.contains("scheduled run completed")),
            "expected the scheduled run completion to wake the session, got: {messages:?}"
        );
    }
}

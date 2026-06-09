//! Agent Client Protocol (ACP) support.
//!
//! ACP lets editors such as Zed drive yolop as an external agent over stdio
//! using newline-delimited JSON-RPC 2.0. Run it with `yolop --acp`; the editor
//! spawns that process, performs the `initialize` handshake, opens sessions
//! with `session/new`, and sends turns with `session/prompt`. yolop streams the
//! turn back as `session/update` notifications.
//!
//! See `specs/acp.md` for the full surface and `README.md` for editor setup.
//!
//! Module layout:
//!   * [`protocol`] — SDK schema re-exports plus small yolop helpers.
//!   * [`bridge`] — pure translation of runtime events into `session/update`s.
//!   * [`server`] — SDK-backed transport/dispatch plus turn streaming.

mod bridge;
mod protocol;
mod server;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;

use crate::runtime::{self, BuiltRuntime, ProviderChoice};
use crate::settings::SettingsStore;

pub use server::{RuntimeFactory, serve};

/// Production [`RuntimeFactory`]: builds a real provider-backed runtime rooted
/// at the client-supplied `cwd` for each `session/new`. The provider, settings,
/// and session-log directory come from the CLI invocation and are shared
/// across every session the client opens.
struct ConfigRuntimeFactory {
    provider: ProviderChoice,
    settings: Arc<SettingsStore>,
    sessions_dir: PathBuf,
}

#[async_trait]
impl RuntimeFactory for ConfigRuntimeFactory {
    async fn build(&self, cwd: PathBuf) -> Result<BuiltRuntime> {
        runtime::build(
            cwd,
            self.provider.clone(),
            None,
            self.sessions_dir.clone(),
            self.settings.clone(),
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
) -> Result<()> {
    let factory = Arc::new(ConfigRuntimeFactory {
        provider,
        settings,
        sessions_dir,
    });
    serve(tokio::io::stdin(), tokio::io::stdout(), factory).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{BuildOptions, build_with_options};
    use agent_client_protocol::schema::{
        InitializeRequest, InitializeResponse, NewSessionRequest, PromptRequest, SessionId,
        SessionUpdate,
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
    }

    #[async_trait]
    impl RuntimeFactory for ScriptedFactory {
        async fn build(&self, cwd: PathBuf) -> Result<BuiltRuntime> {
            let sessions = tempfile::tempdir().expect("sessions tempdir").keep();
            let settings = Arc::new(SettingsStore::open(sessions.join("settings.toml")));
            build_with_options(
                cwd,
                ProviderChoice::Sim,
                None,
                sessions,
                settings,
                BuildOptions {
                    llmsim_override: Some(self.config.clone().with_model("llmsim-yolop")),
                    ..BuildOptions::default()
                },
            )
            .await
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
        stop_reason: agent_client_protocol::schema::StopReason,
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
        let (client_w, agent_r) = tokio::io::duplex(64 * 1024);
        let (agent_w, client_r) = tokio::io::duplex(64 * 1024);
        let factory = Arc::new(ScriptedFactory { config });
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
                        let notification: agent_client_protocol::schema::SessionNotification =
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
                        let notification: agent_client_protocol::schema::SessionNotification =
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
        assert!(!init.agent_capabilities.load_session);
        assert!(init.agent_capabilities.prompt_capabilities.embedded_context);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn full_handshake_then_prompt_streams_text_and_ends_turn() {
        let run = with_sdk_client(fixed("hello from acp"), |client| async move {
            let mut session = client.new_session().await?;
            SdkClient::prompt(&mut session, "say hi").await
        })
        .await;

        assert_eq!(
            run.stop_reason,
            agent_client_protocol::schema::StopReason::EndTurn
        );
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
        let setup = commands
            .iter()
            .find(|c| c["name"] == "setup")
            .expect("setup command");
        let suggestions = setup["_meta"]["yolop.dev/command"]["args"][0]["suggestions"]
            .as_array()
            .expect("setup suggestions");
        assert!(
            suggestions.iter().any(|s| s == "status")
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

        assert_eq!(
            run.stop_reason,
            agent_client_protocol::schema::StopReason::EndTurn
        );
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

        assert_eq!(
            run.stop_reason,
            agent_client_protocol::schema::StopReason::EndTurn
        );
        let tool_calls = run.updates_of_kind("tool_call");
        assert!(
            !tool_calls.is_empty(),
            "expected a tool_call update, got: {:?}",
            run.updates
        );
        assert!(
            tool_calls[0].get("kind").is_none(),
            "autonomous tools should not advertise approval-looking ACP kinds: {:?}",
            tool_calls[0]
        );
        let updates = run.updates_of_kind("tool_call_update");
        assert!(
            updates.iter().any(|u| u["status"] == "completed"),
            "expected a completed tool_call_update, got: {:?}",
            run.updates
        );
        assert!(run.assistant_text().contains("tool done"));
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
        let factory = Arc::new(ScriptedFactory { config });
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
}

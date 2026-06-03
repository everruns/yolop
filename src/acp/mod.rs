//! Agent Client Protocol (ACP) support.
//!
//! ACP lets editors such as Zed drive yolop as an external agent over stdio
//! using newline-delimited JSON-RPC 2.0. Run it with `yolop --acp`; the editor
//! spawns that process, performs the `initialize` handshake, opens sessions
//! with `session/new`, and sends turns with `session/prompt`. yolop streams the
//! turn back as `session/update` notifications and delegates destructive-action
//! approval to the editor via `session/request_permission`.
//!
//! See `specs/acp.md` for the full surface and `README.md` for editor setup.
//!
//! Module layout:
//!   * [`protocol`] — serde types for the ACP wire format.
//!   * [`bridge`] — pure translation of runtime events into `session/update`s.
//!   * [`server`] — the JSON-RPC peer, dispatch, and turn streaming.

mod bridge;
mod protocol;
mod server;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;

use crate::approval::ApprovalGate;
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
    async fn build(&self, cwd: PathBuf, gate: Arc<ApprovalGate>) -> Result<BuiltRuntime> {
        runtime::build(
            cwd,
            self.provider.clone(),
            gate,
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
    use everruns_core::llmsim_driver::{LlmSimConfig, SimToolCall, SimTurn};
    use serde_json::{Value, json};
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, Lines};

    /// Scripted [`RuntimeFactory`] for tests: each session gets its own
    /// offline llmsim runtime rooted at the supplied `cwd`. The session-log
    /// directory is a kept tempdir (OS cleans `/tmp`) so it outlives the
    /// runtime, which canonicalizes and retains its paths.
    struct ScriptedFactory {
        config: LlmSimConfig,
    }

    #[async_trait]
    impl RuntimeFactory for ScriptedFactory {
        async fn build(&self, cwd: PathBuf, gate: Arc<ApprovalGate>) -> Result<BuiltRuntime> {
            let sessions = tempfile::tempdir().expect("sessions tempdir").keep();
            let settings = Arc::new(SettingsStore::open(sessions.join("settings.toml")));
            build_with_options(
                cwd,
                ProviderChoice::Sim,
                gate,
                None,
                sessions,
                settings,
                BuildOptions {
                    llmsim_override: Some(self.config.clone().with_model("llmsim-yolop")),
                },
            )
            .await
        }
    }

    /// In-memory ACP client driving the agent over a pair of duplex pipes.
    struct TestClient {
        writer: DuplexStream,
        reader: Lines<BufReader<DuplexStream>>,
        next_id: i64,
        /// Notifications collected while waiting for responses.
        notifications: Vec<Value>,
        /// How the client answers `session/request_permission` requests.
        permission_allow: bool,
    }

    impl TestClient {
        /// Spawn `serve` against this scripted factory and return a connected
        /// client. The server task runs until the client's write half drops.
        fn spawn(config: LlmSimConfig, permission_allow: bool) -> Self {
            let (client_w, agent_r) = tokio::io::duplex(64 * 1024);
            let (agent_w, client_r) = tokio::io::duplex(64 * 1024);
            let factory = Arc::new(ScriptedFactory { config });
            tokio::spawn(async move {
                let _ = serve(agent_r, agent_w, factory).await;
            });
            Self {
                writer: client_w,
                reader: BufReader::new(client_r).lines(),
                next_id: 0,
                notifications: Vec::new(),
                permission_allow,
            }
        }

        fn alloc_id(&mut self) -> i64 {
            let id = self.next_id;
            self.next_id += 1;
            id
        }

        async fn send(&mut self, value: Value) {
            let line = value.to_string();
            self.writer.write_all(line.as_bytes()).await.unwrap();
            self.writer.write_all(b"\n").await.unwrap();
            self.writer.flush().await.unwrap();
        }

        async fn next_message(&mut self) -> Value {
            let line = tokio::time::timeout(Duration::from_secs(15), self.reader.next_line())
                .await
                .expect("timed out waiting for agent message")
                .expect("read agent line")
                .expect("agent closed stream");
            serde_json::from_str(&line).expect("agent line is valid json")
        }

        /// Send a request and pump messages until its response arrives.
        /// Notifications are buffered; permission requests are auto-answered
        /// per `permission_allow`.
        async fn request(&mut self, method: &str, params: Value) -> Value {
            let id = self.alloc_id();
            self.send(json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": method,
                "params": params,
            }))
            .await;
            loop {
                let message = self.next_message().await;
                if message.get("id").and_then(Value::as_i64) == Some(id)
                    && (message.get("result").is_some() || message.get("error").is_some())
                {
                    return message;
                }
                self.handle_incoming(message).await;
            }
        }

        async fn handle_incoming(&mut self, message: Value) {
            let method = message.get("method").and_then(Value::as_str);
            match method {
                Some("session/request_permission") => {
                    let id = message.get("id").cloned().unwrap_or(Value::Null);
                    let option_id = if self.permission_allow {
                        "allow"
                    } else {
                        "reject"
                    };
                    self.send(json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": { "outcome": { "outcome": "selected", "optionId": option_id } },
                    }))
                    .await;
                }
                Some("session/update") => {
                    self.notifications.push(message);
                }
                _ => {}
            }
        }

        /// Collect every `session/update` whose `sessionUpdate` matches.
        fn updates_of_kind(&self, kind: &str) -> Vec<Value> {
            self.notifications
                .iter()
                .filter_map(|n| n.get("params"))
                .filter(|p| {
                    p.get("update")
                        .and_then(|u| u.get("sessionUpdate"))
                        .and_then(Value::as_str)
                        == Some(kind)
                })
                .cloned()
                .collect()
        }

        /// All assistant text streamed during the session, concatenated.
        fn assistant_text(&self) -> String {
            self.updates_of_kind("agent_message_chunk")
                .iter()
                .filter_map(|p| {
                    p.get("update")
                        .and_then(|u| u.get("content"))
                        .and_then(|c| c.get("text"))
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .collect::<Vec<_>>()
                .join("")
        }

        async fn initialize(&mut self) -> Value {
            self.request(
                "initialize",
                json!({
                    "protocolVersion": 1,
                    "clientCapabilities": { "fs": { "readTextFile": true, "writeTextFile": true } },
                }),
            )
            .await
        }

        async fn new_session(&mut self) -> String {
            let cwd = tempfile::tempdir().expect("cwd tempdir").keep();
            let response = self
                .request(
                    "session/new",
                    json!({ "cwd": cwd.to_str().unwrap(), "mcpServers": [] }),
                )
                .await;
            response["result"]["sessionId"]
                .as_str()
                .expect("sessionId in response")
                .to_string()
        }
    }

    fn fixed(text: &str) -> LlmSimConfig {
        LlmSimConfig::fixed(text)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn initialize_advertises_protocol_version_and_capabilities() {
        let mut client = TestClient::spawn(fixed("hi"), true);
        let response = client.initialize().await;
        assert_eq!(response["result"]["protocolVersion"], 1);
        assert_eq!(
            response["result"]["agentCapabilities"]["loadSession"],
            false
        );
        assert_eq!(
            response["result"]["agentCapabilities"]["promptCapabilities"]["embeddedContext"],
            true
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn full_handshake_then_prompt_streams_text_and_ends_turn() {
        let mut client = TestClient::spawn(fixed("hello from acp"), true);
        client.initialize().await;
        let session_id = client.new_session().await;

        let response = client
            .request(
                "session/prompt",
                json!({
                    "sessionId": session_id,
                    "prompt": [{ "type": "text", "text": "say hi" }],
                }),
            )
            .await;

        assert_eq!(response["result"]["stopReason"], "end_turn");
        assert!(
            client.assistant_text().contains("hello from acp"),
            "expected streamed assistant text, got notifications: {:?}",
            client.notifications
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn new_session_advertises_available_commands() {
        let mut client = TestClient::spawn(fixed("hi"), true);
        client.initialize().await;
        client.new_session().await;

        let command_updates = client.updates_of_kind("available_commands_update");
        assert!(
            !command_updates.is_empty(),
            "expected available_commands_update, got: {:?}",
            client.notifications
        );
        let commands = command_updates[0]["update"]["availableCommands"]
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
        let mut client = TestClient::spawn(fixed("model should not run"), true);
        client.initialize().await;
        let session_id = client.new_session().await;

        let response = client
            .request(
                "session/prompt",
                json!({
                    "sessionId": session_id,
                    "prompt": [{ "type": "text", "text": "/setup status" }],
                }),
            )
            .await;

        assert_eq!(response["result"]["stopReason"], "end_turn");
        let tool_calls = client.updates_of_kind("tool_call");
        assert!(
            tool_calls
                .iter()
                .any(|u| u["update"]["title"] == "/setup status"
                    && u["update"]["rawInput"]["command"] == "setup"),
            "expected command tool_call, got notifications: {:?}",
            client.notifications
        );
        let tool_updates = client.updates_of_kind("tool_call_update");
        let completed = tool_updates
            .iter()
            .find(|u| u["update"]["status"] == "completed")
            .expect("completed command tool update");
        assert!(
            completed["update"]["content"][0]["content"]["text"]
                .as_str()
                .is_some_and(|text| text.contains("setup: provider=")),
            "expected setup status in command output, got: {completed:?}"
        );
        assert_eq!(completed["update"]["rawOutput"]["success"], true);
        assert!(
            !client.assistant_text().contains("model should not run"),
            "slash command should not invoke the model"
        );
        assert!(
            client.updates_of_kind("available_commands_update").len() >= 2,
            "expected command refresh after execution, got: {:?}",
            client.notifications
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unknown_method_returns_method_not_found() {
        let mut client = TestClient::spawn(fixed("hi"), true);
        client.initialize().await;
        let response = client.request("does/not/exist", json!({})).await;
        assert_eq!(response["error"]["code"], -32601);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn prompt_to_unknown_session_is_invalid_params() {
        let mut client = TestClient::spawn(fixed("hi"), true);
        client.initialize().await;
        let response = client
            .request(
                "session/prompt",
                json!({ "sessionId": "session_does_not_exist", "prompt": [] }),
            )
            .await;
        assert_eq!(response["error"]["code"], -32602);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn scripted_tool_call_streams_tool_updates_when_permission_granted() {
        // First scripted turn writes a marker file via bash; second closes
        // the loop with plain text. The bash write is gated through the
        // approval channel, which the client grants.
        let marker = "acp_tool_ran.marker";
        let config = LlmSimConfig::scripted(vec![
            SimTurn::ToolCalls(vec![SimToolCall {
                name: "bash".to_string(),
                arguments: json!({ "command": format!("touch {marker}") }),
                id: None,
            }]),
            SimTurn::Assistant("tool done".to_string()),
        ]);
        let mut client = TestClient::spawn(config, true);
        client.initialize().await;
        let session_id = client.new_session().await;

        let response = client
            .request(
                "session/prompt",
                json!({
                    "sessionId": session_id,
                    "prompt": [{ "type": "text", "text": "run the tool" }],
                }),
            )
            .await;

        assert_eq!(response["result"]["stopReason"], "end_turn");
        let tool_calls = client.updates_of_kind("tool_call");
        assert!(
            !tool_calls.is_empty(),
            "expected a tool_call update, got: {:?}",
            client.notifications
        );
        assert_eq!(
            tool_calls[0]["update"]["kind"], "execute",
            "bash should map to execute kind"
        );
        let updates = client.updates_of_kind("tool_call_update");
        assert!(
            updates.iter().any(|u| u["update"]["status"] == "completed"),
            "expected a completed tool_call_update, got: {:?}",
            client.notifications
        );
        assert!(client.assistant_text().contains("tool done"));
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
        let mut client = TestClient::spawn(config, true);
        client.initialize().await;
        let session_id = client.new_session().await;

        client
            .request(
                "session/prompt",
                json!({
                    "sessionId": session_id,
                    "prompt": [{ "type": "text", "text": "make a plan" }],
                }),
            )
            .await;

        let plans = client.updates_of_kind("plan");
        assert!(
            !plans.is_empty(),
            "expected a plan update, got: {:?}",
            client.notifications
        );
        let entries = plans[0]["update"]["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0]["content"], "step one");
        assert_eq!(entries[0]["status"], "in_progress");
    }

    /// Regression: if the client disconnects while an agent→client request is
    /// in flight (a forwarded `session/request_permission` that never gets an
    /// answer), `serve` must still return rather than deadlock on the awaiting
    /// permission task. Without `fail_all_pending` + the fail-fast send in
    /// `Peer::request`, this hangs forever.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn disconnect_during_permission_lets_serve_return() {
        // First turn issues a bash tool call, which is gated and forwarded to
        // the client as a permission request the client deliberately ignores.
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
            json!({ "jsonrpc": "2.0", "id": 1, "method": "session/new", "params": { "cwd": cwd.to_str().unwrap() } }),
        )
        .await;
        let session_id = await_id(&mut reader, 1).await["result"]["sessionId"]
            .as_str()
            .expect("sessionId")
            .to_string();

        // Send a prompt but never read its response: we want to disconnect
        // mid-turn, while the permission request is outstanding.
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

        // Wait until the agent forwards the permission request, then drop the
        // client's write half to simulate a disconnect without answering it.
        loop {
            let msg = next(&mut reader).await;
            if msg.get("method").and_then(Value::as_str) == Some("session/request_permission") {
                break;
            }
        }
        drop(client_w);
        drop(reader);

        // The server must wind down: the pending permission fails (deny), the
        // tool errors, the turn finishes, and `serve` returns.
        tokio::time::timeout(Duration::from_secs(10), server)
            .await
            .expect("serve must return after disconnect, not hang")
            .expect("serve task joins")
            .expect("serve returns Ok");
    }
}

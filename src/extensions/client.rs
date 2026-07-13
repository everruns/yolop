//! Transport-generic YEP connection: ndjson JSON-RPC over any
//! `AsyncRead`/`AsyncWrite` pair, so tests drive the full stack over
//! `tokio::io::duplex` and only `manager.rs` knows about processes
//! (the same seam split as `capabilities/lsp/client.rs`).

use super::protocol::{
    ErrorObject, Incoming, InitializeParams, InitializeResult, ToolUpdateParams, classify_line,
    notification_line, request_line, response_error_line, version_compatible,
};
use anyhow::{Result, anyhow};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};

/// Pending host-originated requests, keyed by id. Host and server id spaces
/// are independent per direction; only `method`-less lines land here.
type PendingMap = Arc<Mutex<Option<HashMap<u64, oneshot::Sender<Result<Value, ErrorObject>>>>>>;

pub struct YepConnection {
    writer_tx: mpsc::UnboundedSender<String>,
    pending: PendingMap,
    next_id: AtomicU64,
    request_timeout: Duration,
    /// Extension name, for log targets only.
    name: String,
}

impl YepConnection {
    /// Perform the `initialize`/`initialized` handshake over the given
    /// transport and return the live connection plus the server's handshake.
    pub async fn connect<R, W>(
        reader: R,
        writer: W,
        name: &str,
        init: InitializeParams,
        request_timeout: Duration,
    ) -> Result<(Arc<Self>, InitializeResult)>
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let (writer_tx, mut writer_rx) = mpsc::unbounded_channel::<String>();
        tokio::spawn(async move {
            let mut writer = writer;
            while let Some(line) = writer_rx.recv().await {
                if writer.write_all(line.as_bytes()).await.is_err()
                    || writer.write_all(b"\n").await.is_err()
                    || writer.flush().await.is_err()
                {
                    break;
                }
            }
        });

        let pending: PendingMap = Arc::new(Mutex::new(Some(HashMap::new())));
        let connection = Arc::new(Self {
            writer_tx,
            pending: pending.clone(),
            next_id: AtomicU64::new(1),
            request_timeout,
            name: name.to_string(),
        });

        let read_conn = connection.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(reader).lines();
            // Exits on EOF and on read errors alike.
            while let Ok(Some(line)) = lines.next_line().await {
                read_conn.dispatch_line(&line);
            }
            // EOF or read error: fail every pending call promptly instead of
            // letting them hang until their individual timeouts.
            let senders = read_conn
                .pending
                .lock()
                .expect("yep pending lock")
                .take()
                .unwrap_or_default();
            for (_, sender) in senders {
                let _ = sender.send(Err(ErrorObject::message(
                    "extension server closed the connection",
                )));
            }
        });

        let init_params = serde_json::to_value(&init)?;
        let handshake_raw = connection.request("initialize", init_params).await?;
        let handshake: InitializeResult = serde_json::from_value(handshake_raw)
            .map_err(|e| anyhow!("malformed initialize result: {e}"))?;
        if !version_compatible(&handshake.protocol_version) {
            return Err(anyhow!(
                "extension `{name}` speaks YEP {} which is incompatible with this yolop ({})",
                handshake.protocol_version,
                super::protocol::PROTOCOL_VERSION
            ));
        }
        connection.notify("initialized", Value::Null);
        Ok((connection, handshake))
    }

    fn dispatch_line(&self, line: &str) {
        if line.trim().is_empty() {
            return;
        }
        let Some(incoming) = classify_line(line) else {
            tracing::warn!(target: "yolop::ext", ext = %self.name, "skipping malformed wire line");
            return;
        };
        match incoming {
            Incoming::Response { id, result } => {
                let sender = self
                    .pending
                    .lock()
                    .expect("yep pending lock")
                    .as_mut()
                    .and_then(|map| map.remove(&id));
                match sender {
                    Some(sender) => {
                        let _ = sender.send(result);
                    }
                    None => tracing::debug!(
                        target: "yolop::ext", ext = %self.name,
                        "response for unknown or already-completed request id {id}"
                    ),
                }
            }
            Incoming::Notification { method, params } => self.handle_notification(&method, params),
            Incoming::Request { id, method, params } => {
                // Server→host requests (ui/ask, …) arrive in a later phase.
                // Refuse them cleanly instead of corrupting our own routing.
                tracing::debug!(
                    target: "yolop::ext", ext = %self.name,
                    has_params = !params.is_null(),
                    "refusing unsupported server request `{method}`"
                );
                let _ = self.writer_tx.send(response_error_line(
                    id,
                    &ErrorObject::method_not_found(&method),
                ));
            }
        }
    }

    fn handle_notification(&self, method: &str, params: Value) {
        match method {
            "tool/update" => {
                let update: ToolUpdateParams =
                    serde_json::from_value(params).unwrap_or(ToolUpdateParams {
                        request_id: 0,
                        output: String::new(),
                    });
                // TUI live streaming is a follow-up; surface progress through
                // the tracing layer so `RUST_LOG` shows it today.
                tracing::info!(
                    target: "yolop::ext", ext = %self.name,
                    request_id = update.request_id, "{}", update.output
                );
            }
            "log" => {
                let message = params
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                tracing::info!(target: "yolop::ext", ext = %self.name, "{message}");
            }
            "status/changed" => {
                tracing::warn!(target: "yolop::ext", ext = %self.name, status = %params, "status changed");
            }
            // Unknown notification kinds are carried through the log, never a
            // failure — open vocabulary per the forward-compat rules.
            other => {
                tracing::debug!(target: "yolop::ext", ext = %self.name, "notification `{other}` ignored");
            }
        }
    }

    /// Send one request and await its correlated response.
    pub async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.pending.lock().expect("yep pending lock");
            match pending.as_mut() {
                Some(map) => {
                    map.insert(id, tx);
                }
                None => return Err(anyhow!("extension server connection is closed")),
            }
        }
        if self
            .writer_tx
            .send(request_line(id, method, &params))
            .is_err()
        {
            self.pending
                .lock()
                .expect("yep pending lock")
                .as_mut()
                .map(|map| map.remove(&id));
            return Err(anyhow!("extension server connection is closed"));
        }
        match tokio::time::timeout(self.request_timeout, rx).await {
            Ok(Ok(Ok(result))) => Ok(result),
            Ok(Ok(Err(error))) => Err(anyhow!("{}", error.message)),
            Ok(Err(_)) => Err(anyhow!("extension server dropped the request")),
            Err(_) => {
                self.pending
                    .lock()
                    .expect("yep pending lock")
                    .as_mut()
                    .map(|map| map.remove(&id));
                Err(anyhow!(
                    "extension request `{method}` timed out after {:?}",
                    self.request_timeout
                ))
            }
        }
    }

    pub fn notify(&self, method: &str, params: Value) {
        let _ = self.writer_tx.send(notification_line(method, &params));
    }

    /// Whether the read loop has torn the connection down.
    pub fn is_closed(&self) -> bool {
        self.pending.lock().expect("yep pending lock").is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extensions::protocol::PROTOCOL_VERSION;
    use serde_json::json;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream};

    fn init_params() -> InitializeParams {
        InitializeParams {
            protocol_version: PROTOCOL_VERSION.to_string(),
            session_id: "test".into(),
            workspace_root: "/w".into(),
            config: Value::Null,
            capabilities: vec!["cancel".into()],
        }
    }

    /// A scripted fake server over duplex pipes: answers initialize, then
    /// runs `handler` for each subsequent request line.
    async fn fake_server<F>(server_io: DuplexStream, handshake: Value, mut handler: F)
    where
        F: FnMut(u64, &str, &Value) -> Vec<String> + Send,
    {
        let (read_half, mut write_half) = tokio::io::split(server_io);
        let mut lines = BufReader::new(read_half).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let value: Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let method = value["method"].as_str().unwrap_or_default().to_string();
            if method == "initialized" {
                continue;
            }
            let id = value["id"].as_u64().unwrap_or_default();
            let out = if method == "initialize" {
                vec![json!({"id": id, "result": handshake}).to_string()]
            } else {
                handler(id, &method, &value["params"])
            };
            for line in out {
                let _ = write_half.write_all(line.as_bytes()).await;
                let _ = write_half.write_all(b"\n").await;
            }
        }
    }

    fn handshake_json() -> Value {
        json!({
            "protocol_version": "1.0",
            "name": "fake",
            "capabilities": ["tools"],
            "capability_params": { "tools": [{"name": "echo"}] }
        })
    }

    #[tokio::test]
    async fn handshake_tool_call_and_streaming_update() {
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        tokio::spawn(fake_server(
            server_io,
            handshake_json(),
            |id, method, params| {
                assert_eq!(method, "tool/call");
                vec![
                // Streamed update correlated by request_id in the payload.
                json!({"method": "tool/update", "params": {"request_id": id, "output": "working"}})
                    .to_string(),
                json!({"id": id, "result": {"echoed": params["args"]["text"]}}).to_string(),
            ]
            },
        ));
        let (read_half, write_half) = tokio::io::split(client_io);
        let (conn, handshake) = YepConnection::connect(
            read_half,
            write_half,
            "fake",
            init_params(),
            Duration::from_secs(5),
        )
        .await
        .expect("handshake");
        assert_eq!(handshake.name, "fake");
        assert_eq!(handshake.capability_params.tools[0].name, "echo");

        let result = conn
            .request(
                "tool/call",
                json!({"tool_call_id": "t1", "name": "echo", "args": {"text": "hi"}}),
            )
            .await
            .expect("tool call");
        assert_eq!(result["echoed"], "hi");
    }

    #[tokio::test]
    async fn error_response_maps_to_message() {
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        tokio::spawn(fake_server(server_io, handshake_json(), |id, _, _| {
            vec![json!({"id": id, "error": {"message": "no such tool"}}).to_string()]
        }));
        let (read_half, write_half) = tokio::io::split(client_io);
        let (conn, _) = YepConnection::connect(
            read_half,
            write_half,
            "fake",
            init_params(),
            Duration::from_secs(5),
        )
        .await
        .expect("handshake");
        let err = conn
            .request("tool/call", json!({"name": "missing"}))
            .await
            .expect_err("should error");
        assert!(err.to_string().contains("no such tool"), "{err}");
    }

    #[tokio::test]
    async fn unanswered_request_times_out() {
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        tokio::spawn(fake_server(server_io, handshake_json(), |_, _, _| {
            // Answer nothing: the per-request timeout is the backstop.
            Vec::new()
        }));
        let (read_half, write_half) = tokio::io::split(client_io);
        let (conn, _) = YepConnection::connect(
            read_half,
            write_half,
            "fake",
            init_params(),
            Duration::from_millis(200),
        )
        .await
        .expect("handshake");
        // No response arrives: the per-request timeout is the backstop.
        let err = conn
            .request("tool/call", json!({"name": "slow"}))
            .await
            .expect_err("should time out");
        assert!(err.to_string().contains("timed out"), "{err}");
    }

    #[tokio::test]
    async fn incompatible_major_is_refused() {
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        tokio::spawn(fake_server(
            server_io,
            json!({"protocol_version": "2.0", "name": "future"}),
            |_, _, _| Vec::new(),
        ));
        let (read_half, write_half) = tokio::io::split(client_io);
        let err = match YepConnection::connect(
            read_half,
            write_half,
            "future",
            init_params(),
            Duration::from_secs(5),
        )
        .await
        {
            Err(err) => err,
            Ok(_) => panic!("must refuse an incompatible major"),
        };
        assert!(err.to_string().contains("incompatible"), "{err}");
    }

    #[tokio::test]
    async fn server_request_gets_method_not_found() {
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        // Server that, after initialize, sends a reverse request and expects
        // an error response back; it then answers the pending tool call with
        // what it received, so the test can assert on it.
        tokio::spawn(async move {
            let (read_half, mut write_half) = tokio::io::split(server_io);
            let mut lines = BufReader::new(read_half).lines();
            let mut tool_call_id = None;
            while let Ok(Some(line)) = lines.next_line().await {
                let value: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
                match value["method"].as_str() {
                    Some("initialize") => {
                        let reply =
                            json!({"id": value["id"], "result": handshake_json()}).to_string();
                        let _ = write_half.write_all(reply.as_bytes()).await;
                        let _ = write_half.write_all(b"\n").await;
                    }
                    Some("initialized") => {
                        // Reverse request on the server's own id space.
                        let ask = json!({"id": 1, "method": "ui/ask", "params": {}}).to_string();
                        let _ = write_half.write_all(ask.as_bytes()).await;
                        let _ = write_half.write_all(b"\n").await;
                    }
                    Some("tool/call") => tool_call_id = value["id"].as_u64(),
                    None if value.get("error").is_some() => {
                        // Host refused the reverse request; resolve the tool call.
                        if let Some(id) = tool_call_id {
                            let reply = json!({"id": id, "result": {
                                "refused_code": value["error"]["code"]
                            }})
                            .to_string();
                            let _ = write_half.write_all(reply.as_bytes()).await;
                            let _ = write_half.write_all(b"\n").await;
                        }
                    }
                    _ => {}
                }
            }
        });
        let (read_half, write_half) = tokio::io::split(client_io);
        let (conn, _) = YepConnection::connect(
            read_half,
            write_half,
            "fake",
            init_params(),
            Duration::from_secs(5),
        )
        .await
        .expect("handshake");
        let result = conn
            .request("tool/call", json!({"name": "echo"}))
            .await
            .expect("tool call resolves after reverse-request refusal");
        assert_eq!(result["refused_code"], -32601);
    }
}

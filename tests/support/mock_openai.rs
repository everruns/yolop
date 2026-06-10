//! Minimal OpenAI-compatible Chat Completions mock for integration tests.
//!
//! Binds an ephemeral localhost port and answers `POST */chat/completions`
//! with a fixed assistant reply, echoing every parsed request body to the
//! test through a channel so assertions can inspect what the driver actually
//! sent (model id, messages, stream flag, …). Handles both streaming (SSE)
//! and non-streaming requests, so it keeps working whichever mode the
//! Chat Completions driver picks.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::Duration;

pub struct MockOpenAiServer {
    /// Base URL ending in `/v1`, ready for `CUSTOM_BASE_URL` / settings.
    pub base_url: String,
    requests: Receiver<serde_json::Value>,
}

impl MockOpenAiServer {
    pub fn spawn(reply_text: &str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock server");
        let addr = listener.local_addr().expect("mock server addr");
        let (tx, requests) = mpsc::channel();
        let reply_text = reply_text.to_string();
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                // Serial handling is fine: the driver sends one request at a
                // time and each response closes the connection.
                handle_connection(stream, &tx, &reply_text);
            }
        });
        Self {
            base_url: format!("http://{addr}/v1"),
            requests,
        }
    }

    /// Next request body the server received, or panic after `timeout`.
    pub fn next_request(&self, timeout: Duration) -> serde_json::Value {
        self.requests
            .recv_timeout(timeout)
            .expect("mock server should receive a chat completions request")
    }
}

fn handle_connection(mut stream: TcpStream, tx: &Sender<serde_json::Value>, reply_text: &str) {
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("set read timeout");
    let Some(body) = read_request_body(&mut stream) else {
        return;
    };
    let Ok(request) = serde_json::from_slice::<serde_json::Value>(&body) else {
        return;
    };
    let model = request["model"].as_str().unwrap_or("m").to_string();
    let streaming = request["stream"].as_bool().unwrap_or(false);
    let _ = tx.send(request);

    if streaming {
        let chunk = serde_json::json!({
            "id": "c1", "object": "chat.completion.chunk", "created": 0, "model": model,
            "choices": [{ "index": 0,
                "delta": { "role": "assistant", "content": reply_text },
                "finish_reason": null }],
        });
        let done = serde_json::json!({
            "id": "c1", "object": "chat.completion.chunk", "created": 0, "model": model,
            "choices": [{ "index": 0, "delta": {}, "finish_reason": "stop" }],
            "usage": { "prompt_tokens": 1, "completion_tokens": 5, "total_tokens": 6 },
        });
        let body = format!("data: {chunk}\n\ndata: {done}\n\ndata: [DONE]\n\n");
        let _ = write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
    } else {
        let response = serde_json::json!({
            "id": "c1", "object": "chat.completion", "created": 0, "model": model,
            "choices": [{ "index": 0,
                "message": { "role": "assistant", "content": reply_text },
                "finish_reason": "stop" }],
            "usage": { "prompt_tokens": 1, "completion_tokens": 5, "total_tokens": 6 },
        });
        let body = response.to_string();
        let _ = write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
    }
    let _ = stream.flush();
}

/// Read one HTTP request and return its body (requires Content-Length, which
/// reqwest always sends for JSON bodies).
fn read_request_body(stream: &mut TcpStream) -> Option<Vec<u8>> {
    let mut buf = Vec::new();
    let mut chunk = [0_u8; 4096];
    let header_end = loop {
        if let Some(pos) = find_header_end(&buf) {
            break pos;
        }
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let headers = String::from_utf8_lossy(&buf[..header_end]);
    let content_length: usize = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse().ok())?
        })
        .unwrap_or(0);
    let body_start = header_end + 4;
    while buf.len() < body_start + content_length {
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    Some(buf[body_start..body_start + content_length].to_vec())
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

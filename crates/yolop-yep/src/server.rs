//! Server SDK: a builder + stdio serve loop for authoring YEP extension
//! capability servers. Mirrors the mira SDK's `serve()` design — the author
//! registers handlers; the loop owns framing, the `initialize`/`initialized`
//! handshake, dispatch, and error shaping.

use crate::protocol::{
    ErrorObject, HookFireParams, PROTOCOL_VERSION, ToolCallParams, classify_line,
    response_error_line, version_compatible,
};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::{BufRead, Write};

/// What a tool handler returns.
pub enum ToolResponse {
    Ok(Value),
    Err(String),
}

impl ToolResponse {
    pub fn ok(value: Value) -> Self {
        ToolResponse::Ok(value)
    }
    pub fn err(message: impl Into<String>) -> Self {
        ToolResponse::Err(message.into())
    }
}

/// What a hook handler returns (pre_tool_use may block).
#[derive(Default)]
pub struct HookResponse {
    pub block: bool,
    pub reason: String,
}

impl HookResponse {
    /// Allow the tool call (the default).
    pub fn allow() -> Self {
        Self::default()
    }
    /// Block the tool call with a reason (pre_tool_use only).
    pub fn block(reason: impl Into<String>) -> Self {
        Self {
            block: true,
            reason: reason.into(),
        }
    }
}

type ToolHandler = Box<dyn Fn(&Value) -> ToolResponse + Send>;
type HookHandler = Box<dyn Fn(&HookFireParams) -> HookResponse + Send>;
type PromptHandler = Box<dyn Fn() -> String + Send>;

/// Builder for an extension capability server.
pub struct Server {
    name: String,
    instructions: Option<String>,
    tools: BTreeMap<String, ToolHandler>,
    hook: Option<HookHandler>,
    dynamic_prompt: Option<PromptHandler>,
}

impl Server {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            instructions: None,
            tools: BTreeMap::new(),
            hook: None,
            dynamic_prompt: None,
        }
    }

    /// Static system-prompt contribution, sent in the handshake `instructions`
    /// field (also readable by non-yolop MCP-ish hosts).
    pub fn instructions(mut self, text: impl Into<String>) -> Self {
        self.instructions = Some(text.into());
        self
    }

    /// Register a tool by name. The handler receives the call's `args`.
    pub fn tool<F>(mut self, name: impl Into<String>, handler: F) -> Self
    where
        F: Fn(&Value) -> ToolResponse + Send + 'static,
    {
        self.tools.insert(name.into(), Box::new(handler));
        self
    }

    /// Register a single `hook/fire` handler (matches whatever the manifest
    /// subscribes; the handler inspects `params.event`/`params.tool_name`).
    pub fn on_hook<F>(mut self, handler: F) -> Self
    where
        F: Fn(&HookFireParams) -> HookResponse + Send + 'static,
    {
        self.hook = Some(Box::new(handler));
        self
    }

    /// Register a dynamic system-prompt handler (`prompt/contribution`),
    /// recomputed per turn.
    pub fn dynamic_prompt<F>(mut self, handler: F) -> Self
    where
        F: Fn() -> String + Send + 'static,
    {
        self.dynamic_prompt = Some(Box::new(handler));
        self
    }

    /// Run the stdio serve loop until EOF. Blocks the calling thread.
    pub fn serve(self) -> std::io::Result<()> {
        let stdin = std::io::stdin();
        let mut stdout = std::io::stdout();
        for line in stdin.lock().lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            if let Some(out) = self.dispatch(&line) {
                stdout.write_all(out.as_bytes())?;
                stdout.write_all(b"\n")?;
                stdout.flush()?;
            }
        }
        Ok(())
    }

    /// Handle one wire line, returning the response line to write (if any).
    fn dispatch(&self, line: &str) -> Option<String> {
        use crate::protocol::Incoming;
        let incoming = classify_line(line)?;
        match incoming {
            Incoming::Request { id, method, params } => {
                Some(self.handle_request(id, &method, params))
            }
            // Servers don't act on host notifications in this SDK; ignore.
            Incoming::Notification { .. } | Incoming::Response { .. } => None,
        }
    }

    fn handle_request(&self, id: u64, method: &str, params: Value) -> String {
        match method {
            "initialize" => self.initialize_result(id, &params),
            "tool/call" => self.tool_call_result(id, params),
            "hook/fire" => {
                let params: HookFireParams = serde_json::from_value(params).unwrap_or_default();
                let decision = self.hook.as_ref().map(|h| h(&params)).unwrap_or_default();
                json!({ "id": id, "result": {
                    "block": decision.block, "reason": decision.reason } })
                .to_string()
            }
            "prompt/contribution" => {
                let text = self
                    .dynamic_prompt
                    .as_ref()
                    .map(|p| p())
                    .unwrap_or_default();
                json!({ "id": id, "result": { "text": text } }).to_string()
            }
            "shutdown" => json!({ "id": id, "result": {} }).to_string(),
            other => error_line(id, ErrorObject::method_not_found(other)),
        }
    }

    fn initialize_result(&self, id: u64, params: &Value) -> String {
        // Refuse a host whose major we can't speak — but never crash.
        if let Some(version) = params.get("protocol_version").and_then(Value::as_str)
            && !version.is_empty()
            && !version_compatible(version)
        {
            return error_line(
                id,
                ErrorObject::message(format!(
                    "host speaks YEP {version}, incompatible with this server ({PROTOCOL_VERSION})"
                )),
            );
        }
        let mut capabilities = vec![Value::from("tools")];
        let mut capability_params = serde_json::Map::new();
        capability_params.insert(
            "tools".into(),
            Value::Array(
                self.tools
                    .keys()
                    .map(|name| json!({ "name": name }))
                    .collect(),
            ),
        );
        if let Some(text) = &self.instructions {
            capabilities.push(Value::from("prompt"));
            capability_params.insert("prompt".into(), json!({ "static": text }));
        }
        if self.dynamic_prompt.is_some() {
            capabilities.push(Value::from("dynamic_prompt"));
        }
        json!({ "id": id, "result": {
            "protocol_version": PROTOCOL_VERSION,
            "name": self.name,
            "capabilities": capabilities,
            "capability_params": capability_params,
        } })
        .to_string()
    }

    fn tool_call_result(&self, id: u64, params: Value) -> String {
        let call: ToolCallParams = serde_json::from_value(params).unwrap_or_default();
        match self.tools.get(&call.name) {
            Some(handler) => match handler(&call.args) {
                ToolResponse::Ok(value) => json!({ "id": id, "result": value }).to_string(),
                ToolResponse::Err(message) => error_line(id, ErrorObject::message(message)),
            },
            None => error_line(
                id,
                ErrorObject::message(format!("no such tool: {}", call.name)),
            ),
        }
    }
}

fn error_line(id: u64, error: ErrorObject) -> String {
    response_error_line(id, &error)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server() -> Server {
        Server::new("echo")
            .instructions("Use echo.")
            .tool("echo", |args| {
                ToolResponse::ok(json!({ "echoed": args.get("text").cloned().unwrap_or_default() }))
            })
            .on_hook(|p| {
                if p.args.get("forbidden") == Some(&Value::Bool(true)) {
                    HookResponse::block("nope")
                } else {
                    HookResponse::allow()
                }
            })
            .dynamic_prompt(|| "fresh".to_string())
    }

    #[test]
    fn initialize_advertises_capabilities() {
        let out = server().dispatch(
            &json!({ "id": 1, "method": "initialize",
                     "params": { "protocol_version": "1.0" } })
            .to_string(),
        );
        let v: Value = serde_json::from_str(&out.unwrap()).unwrap();
        assert_eq!(v["result"]["name"], "echo");
        let caps = v["result"]["capabilities"].as_array().unwrap();
        assert!(caps.contains(&Value::from("tools")));
        assert!(caps.contains(&Value::from("prompt")));
        assert!(caps.contains(&Value::from("dynamic_prompt")));
        assert_eq!(v["result"]["capability_params"]["tools"][0]["name"], "echo");
    }

    #[test]
    fn incompatible_major_refused() {
        let out = server().dispatch(
            &json!({ "id": 1, "method": "initialize",
                     "params": { "protocol_version": "2.0" } })
            .to_string(),
        );
        let v: Value = serde_json::from_str(&out.unwrap()).unwrap();
        assert!(
            v["error"]["message"]
                .as_str()
                .unwrap()
                .contains("incompatible")
        );
    }

    #[test]
    fn tool_call_dispatches_and_reports_unknown() {
        let ok = server().dispatch(
            &json!({ "id": 2, "method": "tool/call",
                     "params": { "name": "echo", "args": { "text": "hi" } } })
            .to_string(),
        );
        let v: Value = serde_json::from_str(&ok.unwrap()).unwrap();
        assert_eq!(v["result"]["echoed"], "hi");

        let missing = server().dispatch(
            &json!({ "id": 3, "method": "tool/call", "params": { "name": "ghost" } }).to_string(),
        );
        let v: Value = serde_json::from_str(&missing.unwrap()).unwrap();
        assert!(
            v["error"]["message"]
                .as_str()
                .unwrap()
                .contains("no such tool")
        );
    }

    #[test]
    fn hook_blocks_and_prompt_serves() {
        let block = server().dispatch(
            &json!({ "id": 4, "method": "hook/fire",
                     "params": { "event": "pre_tool_use", "tool_name": "bash",
                                 "args": { "forbidden": true } } })
            .to_string(),
        );
        let v: Value = serde_json::from_str(&block.unwrap()).unwrap();
        assert_eq!(v["result"]["block"], true);

        let prompt =
            server().dispatch(&json!({ "id": 5, "method": "prompt/contribution" }).to_string());
        let v: Value = serde_json::from_str(&prompt.unwrap()).unwrap();
        assert_eq!(v["result"]["text"], "fresh");
    }
}

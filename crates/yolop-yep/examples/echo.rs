//! A minimal reference extension server built on `yolop-yep`. yolop's own
//! client drives this exact binary in an integration test, so it doubles as
//! the SDK's conformance fixture (the Rust counterpart of the Python
//! `yep_echo_server.py`). Package it with a `plugin.json` whose
//! `capabilityServer.command` points at the built binary to use it for real.

use serde_json::json;
use yolop_yep::{HookResponse, Server, ToolResponse};

fn main() -> std::io::Result<()> {
    Server::new("echo")
        .instructions("Use the echo tool to repeat text back.")
        .tool("echo", |args| {
            let text = args.get("text").and_then(|v| v.as_str()).unwrap_or("");
            ToolResponse::ok(json!({ "echoed": text }))
        })
        .on_hook(|params| {
            // Block any tool call whose args carry {"forbidden": true}.
            if params.args.get("forbidden") == Some(&json!(true)) {
                HookResponse::block("blocked by echo example server")
            } else {
                HookResponse::allow()
            }
        })
        .dynamic_prompt(|| "dynamic echo prompt".to_string())
        .serve()
}

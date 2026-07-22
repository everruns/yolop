//! yolop extension capability server: Apple's official signed macOS Computer
//! Use, exposed as direct YEP tools with no nested model call.
//!
//! Each `computer_use_*` tool spawns one ephemeral, zero-turn `codex app-server`
//! (from the installed ChatGPT.app), dispatches the single official MCP tool
//! call, and tears the process tree down — see `broker.rs`. This is a native
//! Rust port of `codex-computer-use-mcp`'s MCP façade onto the yolop extension
//! protocol; it needs no Node runtime, only the signed ChatGPT.app bundle.

mod broker;
mod tools;

use serde_json::{Value, json};
use std::time::Duration;
use yolop_yep::{Server, ToolResponse};

const TOOL_PREFIX: &str = "computer_use_";
const STATUS_TOOL: &str = "computer_use_status";
const CALL_TIMEOUT: Duration = Duration::from_secs(120);

const INSTRUCTIONS: &str = "\
`computer_use_*` tools drive the *host macOS desktop* via Apple's official \
signed Computer Use helper (through the installed ChatGPT.app). They are \
macOS-only and control real apps.\n\
- Call `computer_use_get_app_state` once per turn before interacting with an \
app; it returns a screenshot and an accessibility tree whose element indices \
the interaction tools reference.\n\
- Use `computer_use_list_apps` to discover targetable apps, and \
`computer_use_status` to check that the signed broker is available.\n\
- Every call is a fresh, model-free dispatch: there is no persistent session \
and no Codex model turn.";

fn main() -> std::io::Result<()> {
    let mut server = Server::new("computer-use").instructions(INSTRUCTIONS);
    for &method in tools::OFFICIAL_METHODS {
        let tool_name = format!("{TOOL_PREFIX}{method}");
        server = server.tool(tool_name, move |args| run_method(method, args));
    }
    server = server.tool(STATUS_TOOL, |_args| status_response());
    server.serve()
}

/// Dispatch one official method through the broker and shape the MCP result
/// into the tool response the host records.
fn run_method(method: &str, args: &Value) -> ToolResponse {
    let mut call_args = args.clone();
    // `press_key` accepts human aliases (cmd+c); normalize to xdotool syntax.
    if method == "press_key"
        && let Some(key) = call_args.get("key").and_then(Value::as_str)
    {
        let normalized = tools::normalize_key_expression(key);
        if let Some(map) = call_args.as_object_mut() {
            map.insert("key".into(), Value::String(normalized));
        }
    }

    match broker::call_official_direct_tool(method, &call_args, CALL_TIMEOUT) {
        Ok(result) => ToolResponse::ok(json!({
            "content": result.content,
            "isError": result.is_error,
            "details": {
                "method": method,
                "architecture": "official-codex-app-server-direct-mcp-tool-call",
                "modelTurns": 0,
                "ephemeralRuntimeContext": true,
                "elicitationRequests": result.elicitation_requests,
                "brokerVersion": result.broker_version,
                "clientBuild": result.client_build,
                "durationMs": result.duration_ms,
            },
        })),
        Err(message) => ToolResponse::err(message),
    }
}

/// The `computer_use_status` tool: the durable policy and whether the signed
/// broker verifies on this machine. Readable on any platform (off macOS it
/// simply reports `brokerVerified: false`).
fn status_response() -> ToolResponse {
    let (broker_verified, broker_version, client_build) =
        match broker::verify_official_direct_broker() {
            Ok((version, build)) => (true, Some(version), Some(build)),
            Err(_) => (false, None, None),
        };
    let methods: Vec<String> = tools::OFFICIAL_METHODS
        .iter()
        .map(|m| format!("{TOOL_PREFIX}{m}"))
        .collect();
    ToolResponse::ok(json!({
        "architecture": "official-codex-app-server-direct-mcp-tool-call",
        "platform": "macos-only",
        "brokerVerified": broker_verified,
        "brokerVersion": broker_version,
        "clientBuild": client_build,
        "codexPath": broker::CODEX_PATH,
        "nestedModel": false,
        "modelUsage": false,
        "permissionMode": "no-permissions",
        "wrapperPermissionPrompts": false,
        "availableTools": methods,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The manifest is the approval boundary; its tool schemas must not drift
    /// from what the server actually dispatches (and drift-checks against the
    /// live app-server inventory). This test keeps plugin.json honest.
    #[test]
    fn manifest_matches_the_served_tools() {
        let manifest: Value =
            serde_json::from_str(include_str!("../plugin.json")).expect("plugin.json parses");
        let declared = manifest["yolop"]["tools"].as_array().expect("tools array");

        let names: Vec<&str> = declared
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        // 10 official tools + the status tool.
        assert_eq!(names.len(), tools::OFFICIAL_METHODS.len() + 1);
        assert!(names.contains(&STATUS_TOOL));

        for &method in tools::OFFICIAL_METHODS {
            let tool_name = format!("{TOOL_PREFIX}{method}");
            let entry = declared
                .iter()
                .find(|t| t["name"].as_str() == Some(&tool_name))
                .unwrap_or_else(|| panic!("{tool_name} declared"));
            assert_eq!(
                entry["schema"],
                tools::input_schema(method),
                "{tool_name} schema drift"
            );
            assert_eq!(
                entry["description"],
                tools::description(method),
                "{tool_name} description drift"
            );
        }

        // The inspection tools (and status) are the always-loaded set.
        for &method in tools::INSPECTION_METHODS {
            let tool_name = format!("{TOOL_PREFIX}{method}");
            let entry = declared
                .iter()
                .find(|t| t["name"].as_str() == Some(&tool_name))
                .unwrap();
            assert_eq!(
                entry["never_defer"],
                Value::Bool(true),
                "{tool_name} should be never_defer"
            );
        }
    }
}

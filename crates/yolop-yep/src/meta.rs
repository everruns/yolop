//! Protocol metadata: the method and capability-token vocabularies, as data.
//!
//! This is the machine-readable index emitted to `schema/yep/v1/meta.json`
//! (the mira pattern) so a non-Rust extension author can discover the wire
//! surface without reading these structs. It is generated from the constants
//! here by the `schema-gen` binary, and a drift test keeps the committed file
//! in lockstep — the vocabulary can't change without the artifact changing.

use crate::protocol::PROTOCOL_VERSION;
use serde::Serialize;

/// Every method name in the protocol, tagged by direction.
pub const METHODS: &[Method] = &[
    Method::host(
        "initialize",
        "Handshake: negotiate version, pass config; server replies with contributions.",
    ),
    Method::host("initialized", "Notification: handshake complete."),
    Method::host("tool/call", "Invoke a tool the server declared."),
    Method::server(
        "tool/update",
        "Notification: streamed progress for an in-flight tool/call (correlated by request_id).",
    ),
    Method::server(
        "status/changed",
        "Notification: a short capability status the host surfaces in its status bar \
         (empty status clears it).",
    ),
    Method::host(
        "hook/fire",
        "Fire a subscribed lifecycle hook (pre_tool_use/post_tool_use).",
    ),
    Method::host(
        "prompt/contribution",
        "Request a fresh dynamic system-prompt contribution.",
    ),
    Method::host(
        "command/execute",
        "Run a manifest-declared slash command; the result message is shown to the user.",
    ),
    Method::server(
        "ui/ask",
        "Ask the user a question and wait for a typed answer (TUI hosts only).",
    ),
    Method::host("config/changed", "Notify the server its config changed."),
    Method::host("shutdown", "Request a graceful stop."),
];

/// Capability tokens a server may advertise in the `initialize` result.
pub const CAPABILITY_TOKENS: &[&str] = &[
    "tools",
    "streaming",
    "prompt",
    "dynamic_prompt",
    "hooks",
    "mcp_servers",
    "status",
    "commands",
    "ui_ask",
];

/// Direction a method flows.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    /// host → server
    Host,
    /// server → host
    Server,
}

#[derive(Debug, Clone, Serialize)]
pub struct Method {
    pub name: &'static str,
    pub direction: Direction,
    pub description: &'static str,
}

impl Method {
    const fn host(name: &'static str, description: &'static str) -> Self {
        Self {
            name,
            direction: Direction::Host,
            description,
        }
    }
    const fn server(name: &'static str, description: &'static str) -> Self {
        Self {
            name,
            direction: Direction::Server,
            description,
        }
    }
}

/// The full protocol metadata index.
#[derive(Debug, Clone, Serialize)]
pub struct Meta {
    pub version: &'static str,
    /// Minimum protocol version this implementation interoperates with.
    pub min_version: &'static str,
    pub methods: &'static [Method],
    pub capabilities: &'static [&'static str],
}

/// Build the metadata index for the current protocol version.
pub fn meta() -> Meta {
    Meta {
        version: PROTOCOL_VERSION,
        // Same major = compatible; `1.0` is the baseline.
        min_version: "1.0",
        methods: METHODS,
        capabilities: CAPABILITY_TOKENS,
    }
}

/// The canonical pretty-printed JSON for `schema/yep/v1/meta.json`, with a
/// trailing newline. The generator writes this; the drift test compares.
pub fn meta_json() -> String {
    let mut json = serde_json::to_string_pretty(&meta()).expect("meta serializes");
    json.push('\n');
    json
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meta_covers_the_protocol() {
        let meta = meta();
        assert_eq!(meta.version, "1.0");
        assert!(meta.methods.iter().any(|m| m.name == "initialize"));
        assert!(
            meta.methods
                .iter()
                .any(|m| m.name == "tool/update" && m.direction == Direction::Server)
        );
        assert!(meta.capabilities.contains(&"tools"));
        // Serializes to valid JSON.
        assert!(meta_json().starts_with('{'));
    }

    /// Drift guard: the committed `schema/yep/v1/meta.json` must match what the
    /// generator produces. This makes `cargo test` (i.e. CI) the enforcement —
    /// a vocabulary change can't merge without regenerating the artifact.
    /// Run `cargo run -p yolop-yep --bin schema-gen` to fix a failure here.
    ///
    /// The schema file lives at the repo root, outside the crate, so it is
    /// absent when the published crate is built/tested standalone (e.g. a
    /// downstream `cargo test`). In that case the guard is a no-op — it only
    /// enforces in-repo, which is where drift can be introduced.
    #[test]
    fn committed_meta_json_is_up_to_date() {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../schema/yep/v1/meta.json");
        let Ok(committed) = std::fs::read_to_string(&path) else {
            return; // not in the repo tree; nothing to guard
        };
        assert_eq!(
            committed,
            meta_json(),
            "schema/yep/v1/meta.json is stale — run `cargo run -p yolop-yep --bin schema-gen`"
        );
    }
}

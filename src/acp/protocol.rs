//! Wire types for the Agent Client Protocol (ACP).
//!
//! ACP is the JSON-RPC 2.0 protocol spoken between a code editor (the
//! *client*, e.g. Zed) and a coding agent (yolop, the *agent*) over stdio,
//! one JSON object per line (newline-delimited). These structs map 1:1 to
//! the schema published at <https://agentclientprotocol.com>. Only the
//! subset yolop implements is modelled here; unknown fields on inbound
//! messages are ignored so newer clients keep working.
//!
//! Field casing follows the spec exactly: object keys are camelCase and the
//! `sessionUpdate` / `type` / `kind` / `status` discriminators are
//! snake_case. The matching `#[serde(rename_all = ...)]` attributes encode
//! that, so the Rust side stays idiomatic snake_case.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Protocol version yolop implements. ACP versions are integers; v1 is the
/// current stable major. We echo back the client's requested version when it
/// is one we support, and otherwise advertise this.
pub const PROTOCOL_VERSION: i64 = 1;

// ---------- initialize ----------

/// Inbound `initialize` params. yolop only needs the requested protocol
/// version; the client's capabilities (`clientCapabilities`, `clientInfo`)
/// are accepted and ignored — serde drops unknown fields by default — because
/// yolop's runtime touches the host disk directly rather than routing file
/// ops back through the client.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct InitializeParams {
    #[serde(default)]
    pub protocol_version: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResult {
    pub protocol_version: i64,
    pub agent_capabilities: AgentCapabilities,
    pub auth_methods: Vec<AuthMethod>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentCapabilities {
    pub load_session: bool,
    pub prompt_capabilities: PromptCapabilities,
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<Value>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptCapabilities {
    pub image: bool,
    pub audio: bool,
    pub embedded_context: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthMethod {
    pub id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

// ---------- session/new ----------

/// Inbound `session/new` params. yolop uses `cwd` to root the new runtime;
/// `mcpServers` and `additionalDirectories` are accepted and ignored (no MCP
/// pass-through yet — serde drops unmodelled fields).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewSessionParams {
    pub cwd: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NewSessionResult {
    pub session_id: String,
}

// ---------- session/prompt ----------

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptParams {
    pub session_id: String,
    #[serde(default)]
    pub prompt: Vec<Value>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptResult {
    pub stop_reason: StopReason,
}

/// Why a prompt turn ended. Mirrors ACP's `StopReason`. yolop currently only
/// resolves `EndTurn` and `Cancelled`; the token-limit and refusal variants
/// exist to model the wire enum completely (the runtime doesn't surface those
/// outcomes distinctly yet).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
#[allow(dead_code)]
pub enum StopReason {
    EndTurn,
    MaxTokens,
    MaxTurnRequests,
    Refusal,
    Cancelled,
}

// ---------- session/cancel ----------

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CancelParams {
    pub session_id: String,
}

// ---------- session/update notification ----------

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionNotification {
    pub session_id: String,
    pub update: SessionUpdate,
}

/// A streaming update emitted mid-turn. The `sessionUpdate` discriminator
/// selects the variant.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "sessionUpdate", rename_all = "snake_case")]
pub enum SessionUpdate {
    AgentMessageChunk {
        content: ContentBlock,
    },
    AgentThoughtChunk {
        content: ContentBlock,
    },
    ToolCall {
        #[serde(rename = "toolCallId")]
        tool_call_id: String,
        title: String,
        kind: ToolKind,
        status: ToolCallStatus,
        #[serde(rename = "rawInput", skip_serializing_if = "Option::is_none")]
        raw_input: Option<Value>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        content: Vec<ToolCallContent>,
    },
    ToolCallUpdate {
        #[serde(rename = "toolCallId")]
        tool_call_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        status: Option<ToolCallStatus>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        content: Vec<ToolCallContent>,
        #[serde(rename = "rawOutput", skip_serializing_if = "Option::is_none")]
        raw_output: Option<Value>,
    },
    Plan {
        entries: Vec<PlanEntry>,
    },
    AvailableCommandsUpdate {
        #[serde(rename = "availableCommands")]
        available_commands: Vec<AvailableCommand>,
        #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
        meta: Option<Value>,
    },
}

/// A content block. yolop only ever emits text blocks; the spec also defines
/// image/audio/resource blocks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text { text: String },
}

impl ContentBlock {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }
}

/// Categorises a tool call so editors can pick an icon/affordance. The full
/// ACP vocabulary is modelled; yolop's tool-name mapping doesn't currently
/// emit every variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
#[allow(dead_code)]
pub enum ToolKind {
    Read,
    Edit,
    Delete,
    Move,
    Search,
    Execute,
    Think,
    Fetch,
    Other,
}

/// Lifecycle status of a tool call. yolop emits `InProgress` then
/// `Completed`/`Failed`; `Pending` rounds out the wire enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
#[allow(dead_code)]
pub enum ToolCallStatus {
    Pending,
    InProgress,
    Completed,
    Failed,
}

/// Content attached to a tool call. yolop emits the `content` variant
/// (wrapping a text block); the `diff` variant is modelled for completeness
/// and future structured-diff support (the spec also defines `terminal`).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[allow(dead_code)]
pub enum ToolCallContent {
    Content { content: ContentBlock },
    Diff(ToolCallDiff),
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallDiff {
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_text: Option<String>,
    pub new_text: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanEntry {
    pub content: String,
    pub priority: PlanPriority,
    pub status: PlanStatus,
}

/// Plan-entry priority. yolop's todo tool has no priority concept, so it
/// always emits `Medium`; the other variants exist to model the wire enum
/// completely.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
#[allow(dead_code)]
pub enum PlanPriority {
    High,
    Medium,
    Low,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanStatus {
    Pending,
    InProgress,
    Completed,
}

// ---------- available_commands_update ----------

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AvailableCommand {
    pub name: String,
    pub description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<AvailableCommandInput>,
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AvailableCommandInput {
    pub hint: String,
}

// ---------- session/request_permission ----------

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestPermissionParams {
    pub session_id: String,
    pub tool_call: Value,
    pub options: Vec<PermissionOption>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionOption {
    pub option_id: String,
    pub name: String,
    pub kind: PermissionOptionKind,
}

/// Kinds of permission option an agent can offer. yolop offers a single
/// allow/reject pair (`AllowOnce`/`RejectOnce`); the "always" variants exist
/// to model the wire enum completely.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
#[allow(dead_code)]
pub enum PermissionOptionKind {
    AllowOnce,
    AllowAlways,
    RejectOnce,
    RejectAlways,
}

/// Client's answer to a permission request. The `outcome` discriminator is
/// either `selected` (with the chosen `optionId`) or `cancelled`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestPermissionResult {
    pub outcome: PermissionOutcome,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum PermissionOutcome {
    Selected {
        #[serde(rename = "optionId")]
        option_id: String,
    },
    Cancelled,
}

/// Extract the concatenated plain text from an inbound prompt's content
/// blocks, ignoring non-text blocks (images, resources). Newline-joined so a
/// multi-block prompt reads naturally.
pub fn prompt_text(blocks: &[Value]) -> String {
    let mut parts = Vec::new();
    for block in blocks {
        if block.get("type").and_then(Value::as_str) == Some("text")
            && let Some(text) = block.get("text").and_then(Value::as_str)
        {
            parts.push(text.to_string());
        }
    }
    parts.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn initialize_result_serializes_camel_case() {
        let result = InitializeResult {
            protocol_version: PROTOCOL_VERSION,
            agent_capabilities: AgentCapabilities {
                load_session: false,
                prompt_capabilities: PromptCapabilities {
                    image: false,
                    audio: false,
                    embedded_context: true,
                },
                meta: None,
            },
            auth_methods: vec![],
        };
        let v = serde_json::to_value(&result).unwrap();
        assert_eq!(v["protocolVersion"], 1);
        assert_eq!(v["agentCapabilities"]["loadSession"], false);
        assert_eq!(
            v["agentCapabilities"]["promptCapabilities"]["embeddedContext"],
            true
        );
        assert!(v["authMethods"].as_array().unwrap().is_empty());
    }

    #[test]
    fn agent_message_chunk_uses_snake_case_discriminator() {
        let update = SessionUpdate::AgentMessageChunk {
            content: ContentBlock::text("hi"),
        };
        let v = serde_json::to_value(&update).unwrap();
        assert_eq!(v["sessionUpdate"], "agent_message_chunk");
        assert_eq!(v["content"]["type"], "text");
        assert_eq!(v["content"]["text"], "hi");
    }

    #[test]
    fn tool_call_serializes_camel_case_ids_and_snake_case_enums() {
        let update = SessionUpdate::ToolCall {
            tool_call_id: "call_1".into(),
            title: "Run bash".into(),
            kind: ToolKind::Execute,
            status: ToolCallStatus::InProgress,
            raw_input: Some(json!({ "command": "ls" })),
            content: vec![],
        };
        let v = serde_json::to_value(&update).unwrap();
        assert_eq!(v["sessionUpdate"], "tool_call");
        assert_eq!(v["toolCallId"], "call_1");
        assert_eq!(v["kind"], "execute");
        assert_eq!(v["status"], "in_progress");
        assert_eq!(v["rawInput"]["command"], "ls");
        // Empty content is omitted from the wire form.
        assert!(v.get("content").is_none());
    }

    #[test]
    fn tool_call_update_omits_empty_optionals() {
        let update = SessionUpdate::ToolCallUpdate {
            tool_call_id: "call_1".into(),
            status: Some(ToolCallStatus::Completed),
            content: vec![ToolCallContent::Content {
                content: ContentBlock::text("done"),
            }],
            raw_output: None,
        };
        let v = serde_json::to_value(&update).unwrap();
        assert_eq!(v["sessionUpdate"], "tool_call_update");
        assert_eq!(v["status"], "completed");
        assert_eq!(v["content"][0]["type"], "content");
        assert_eq!(v["content"][0]["content"]["text"], "done");
        assert!(v.get("rawOutput").is_none());
    }

    #[test]
    fn diff_content_serializes_camel_case_text_fields() {
        let content = ToolCallContent::Diff(ToolCallDiff {
            path: "src/x.rs".into(),
            old_text: Some("a".into()),
            new_text: "b".into(),
        });
        let v = serde_json::to_value(&content).unwrap();
        assert_eq!(v["type"], "diff");
        assert_eq!(v["path"], "src/x.rs");
        assert_eq!(v["oldText"], "a");
        assert_eq!(v["newText"], "b");
    }

    #[test]
    fn plan_entry_enums_are_snake_case() {
        let entry = PlanEntry {
            content: "do it".into(),
            priority: PlanPriority::Medium,
            status: PlanStatus::InProgress,
        };
        let v = serde_json::to_value(&entry).unwrap();
        assert_eq!(v["priority"], "medium");
        assert_eq!(v["status"], "in_progress");
    }

    #[test]
    fn available_commands_update_serializes_command_list() {
        let update = SessionUpdate::AvailableCommandsUpdate {
            available_commands: vec![AvailableCommand {
                name: "setup".into(),
                description: "Configure provider.".into(),
                input: Some(AvailableCommandInput {
                    hint: "provider, token, model".into(),
                }),
                meta: Some(json!({
                    "yolop.dev/command": {
                        "args": [
                            {
                                "name": "action",
                                "suggestions": ["status", "provider openai"]
                            }
                        ]
                    }
                })),
            }],
            meta: Some(json!({ "yolop.dev/acp": { "argSuggestions": true } })),
        };
        let v = serde_json::to_value(&update).unwrap();
        assert_eq!(v["sessionUpdate"], "available_commands_update");
        assert_eq!(v["availableCommands"][0]["name"], "setup");
        assert_eq!(
            v["availableCommands"][0]["input"]["hint"],
            "provider, token, model"
        );
        assert_eq!(
            v["availableCommands"][0]["_meta"]["yolop.dev/command"]["args"][0]["suggestions"][0],
            "status"
        );
        assert_eq!(v["_meta"]["yolop.dev/acp"]["argSuggestions"], true);
    }

    #[test]
    fn stop_reason_serializes_snake_case() {
        assert_eq!(
            serde_json::to_value(StopReason::EndTurn).unwrap(),
            json!("end_turn")
        );
        assert_eq!(
            serde_json::to_value(StopReason::MaxTurnRequests).unwrap(),
            json!("max_turn_requests")
        );
    }

    #[test]
    fn initialize_params_tolerate_missing_and_unknown_fields() {
        // Empty params: protocol version absent.
        let empty: InitializeParams = serde_json::from_value(json!({})).unwrap();
        assert_eq!(empty.protocol_version, None);
        // Client capabilities and clientInfo are accepted and ignored.
        let full: InitializeParams = serde_json::from_value(json!({
            "protocolVersion": 1,
            "clientCapabilities": { "fs": { "readTextFile": true, "writeTextFile": true } },
            "clientInfo": { "name": "zed" }
        }))
        .unwrap();
        assert_eq!(full.protocol_version, Some(1));
    }

    #[test]
    fn prompt_text_concatenates_text_blocks_only() {
        let blocks = vec![
            json!({ "type": "text", "text": "hello" }),
            json!({ "type": "image", "data": "..." }),
            json!({ "type": "text", "text": "world" }),
        ];
        assert_eq!(prompt_text(&blocks), "hello\nworld");
    }

    #[test]
    fn permission_outcome_parses_selected_and_cancelled() {
        let selected: RequestPermissionResult = serde_json::from_value(
            json!({ "outcome": { "outcome": "selected", "optionId": "ok" } }),
        )
        .unwrap();
        match selected.outcome {
            PermissionOutcome::Selected { option_id } => assert_eq!(option_id, "ok"),
            _ => panic!("expected selected"),
        }
        let cancelled: RequestPermissionResult =
            serde_json::from_value(json!({ "outcome": { "outcome": "cancelled" } })).unwrap();
        assert!(matches!(cancelled.outcome, PermissionOutcome::Cancelled));
    }
}

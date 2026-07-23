//! Extension package discovery and manifest parsing.
//!
//! One scope, global: `<config_dir>/yolop/extensions/<name>/plugin.json`
//! (`YOLOP_EXTENSIONS_DIR` overrides, for tests and dev). There is
//! deliberately no workspace scope — repos never carry yolop extensions
//! (see specs/extensions.md, "Packaging, install, trust").
//!
//! The manifest is the approval boundary (D4): it carries the full tool
//! definitions (name, description, parameters schema, policy) so every
//! contribution is inspectable — and clamps the handshake — without
//! executing the server binary.

use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const MANIFEST_FILE: &str = "plugin.json";

/// `plugin.json`, upstream everruns-plugin dialect plus the `yolop` facet.
/// Lenient: unknown fields ignored everywhere.
#[derive(Debug, Clone, Deserialize)]
struct RawManifest {
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    version: Option<String>,
    yolop: RawFacet,
}

#[derive(Debug, Clone, Deserialize)]
struct RawFacet {
    #[serde(default)]
    protocol_version: Option<String>,
    #[serde(rename = "capabilityServer")]
    capability_server: ServerSpec,
    #[serde(default)]
    config_schema: Option<Value>,
    #[serde(default)]
    tools: Vec<ToolDefinition>,
    /// Permits a handshake system-prompt contribution.
    #[serde(default)]
    prompt: bool,
    /// When true, the server serves a fresh `prompt/contribution` each turn
    /// (in addition to / instead of the static handshake prompt).
    #[serde(default)]
    dynamic_prompt: bool,
    /// MCP servers the extension contributes; consumed by yolop's own MCP
    /// client exactly as if listed in `.mcp.json` (D1: MCP is a
    /// contribution, not the base wire). Keyed by logical server name.
    #[serde(rename = "mcpServers", default)]
    mcp_servers: BTreeMap<String, ContributedMcpServer>,
    /// Lifecycle hook subscriptions the extension serves over `hook/fire`.
    #[serde(default)]
    hooks: Vec<HookSubscription>,
    /// Slash commands the extension contributes (dispatched to the server over
    /// `command/execute`).
    #[serde(default)]
    commands: Vec<CommandDef>,
    /// Permits the server to push `status/changed` notifications into the host
    /// status bar (D4: the approval boundary — a non-declaring extension can
    /// never write the status bar).
    #[serde(default)]
    status: bool,
    /// Contributes the package's `skills/` directory as read-only skills (D4:
    /// only a declaring, enabled extension's skills load). Static markdown, so
    /// no server is involved.
    #[serde(default)]
    skills: bool,
    /// Permits the server to send `ui/ask` requests (prompt the user for a typed
    /// answer). D4 opt-in — a non-declaring extension's `ui/ask` is refused.
    #[serde(default)]
    ui_ask: bool,
    /// Subscribes the extension to the session's agentic event stream: the host
    /// forwards each lifecycle event (`turn.*`, `reason.*`, `act.*`,
    /// `tool.started/completed`, `llm.generation`) as a fire-and-forget
    /// `trace/event` notification, for export to a tracing backend. D4 opt-in —
    /// the event stream is only forwarded to a declaring, enabled extension, and
    /// the facet triggers an eager server spawn so no early events are missed.
    #[serde(default)]
    trace: bool,
}

/// A manifest-declared hook subscription. Static (the approved upper bound):
/// which event, which tools it fires for, how long it may take, and what
/// happens if the server errors. A match-all glob must be spelled `"*"` and
/// is surfaced at approval time.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookSubscription {
    /// `pre_tool_use` (may block/observe before a tool runs) or
    /// `post_tool_use` (observe after).
    pub event: HookEvent,
    /// Glob over tool names this hook fires for (`"*"` = all). Matched with
    /// simple `*` wildcards.
    #[serde(default = "match_all_glob")]
    pub tool_name_glob: String,
    #[serde(default = "default_hook_timeout_ms")]
    pub timeout_ms: u64,
    #[serde(default)]
    pub on_error: HookOnError,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookEvent {
    PreToolUse,
    PostToolUse,
}

/// What to do when the extension server errors or times out on a hook.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HookOnError {
    /// Log and allow the tool call (availability over integrity).
    #[default]
    Warn,
    /// Block the tool call (integrity over availability).
    Block,
}

fn match_all_glob() -> String {
    "*".to_string()
}

fn default_hook_timeout_ms() -> u64 {
    5_000
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServerSpec {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
}

/// A manifest-declared MCP server contribution. Manifest-declared (not
/// handshake-negotiated) so it is inspectable at install without executing
/// the binary, and clamped by construction — the approved transport shape
/// (stdio command vs http url) is exactly what runs. `${VAR}` expansion in
/// string fields is handled downstream by the runtime, as for `.mcp.json`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContributedMcpServer {
    /// `stdio` (default) or `http`.
    #[serde(rename = "type", default)]
    pub transport: McpTransport,
    // stdio
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: std::collections::HashMap<String, String>,
    // http
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub headers: std::collections::HashMap<String, String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum McpTransport {
    #[default]
    Stdio,
    Http,
}

/// A manifest-declared tool: the full definition the LLM sees, plus policy.
#[derive(Debug, Clone, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// JSON Schema for the tool's parameters.
    #[serde(default = "default_schema")]
    pub schema: Value,
    #[serde(default)]
    pub never_defer: bool,
}

fn default_schema() -> Value {
    serde_json::json!({ "type": "object" })
}

/// A manifest-declared slash command. Registered (namespaced `<ext>:<name>`) in
/// the host command palette; invoking it sends `command/execute` to the server.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandDef {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub args: Vec<CommandArgDef>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandArgDef {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub required: bool,
}

#[derive(Debug, Clone)]
pub struct ExtensionManifest {
    pub name: String,
    pub description: String,
    pub version: Option<String>,
    pub capability_server: ServerSpec,
    pub config_schema: Option<Value>,
    pub tools: Vec<ToolDefinition>,
    pub prompt: bool,
    pub dynamic_prompt: bool,
    pub mcp_servers: BTreeMap<String, ContributedMcpServer>,
    pub hooks: Vec<HookSubscription>,
    pub commands: Vec<CommandDef>,
    pub status: bool,
    pub skills: bool,
    pub ui_ask: bool,
    pub trace: bool,
}

#[derive(Debug, Clone)]
pub struct ExtensionPackage {
    pub dir: PathBuf,
    pub manifest: ExtensionManifest,
}

/// One field declared in an extension's `config_schema`, with yolop's setup
/// vocabulary folded in: `secret` (the value is a credential — stored in the
/// secret store, redacted, never shown to the agent) and `env` (inject the
/// value into the server's environment under this name). `required` comes from
/// the schema's top-level `required` array and drives setup prompting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigField {
    pub name: String,
    pub secret: bool,
    pub env: Option<String>,
    pub required: bool,
    pub description: String,
}

impl ExtensionManifest {
    /// The setup fields declared in `config_schema.properties`. Empty when the
    /// extension declares no schema. Lenient — unknown keywords are ignored.
    pub fn config_fields(&self) -> Vec<ConfigField> {
        let Some(schema) = &self.config_schema else {
            return Vec::new();
        };
        let required: std::collections::HashSet<&str> = schema
            .get("required")
            .and_then(Value::as_array)
            .map(|arr| arr.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let Some(props) = schema.get("properties").and_then(Value::as_object) else {
            return Vec::new();
        };
        props
            .iter()
            .map(|(name, spec)| ConfigField {
                name: name.clone(),
                secret: spec.get("secret").and_then(Value::as_bool).unwrap_or(false),
                env: spec.get("env").and_then(Value::as_str).map(str::to_string),
                required: required.contains(name.as_str()),
                description: spec
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            })
            .collect()
    }

    /// The subset of `config_fields` marked `secret` — credentials that live in
    /// the secret store, never in `settings.toml` config.
    pub fn secret_fields(&self) -> Vec<ConfigField> {
        self.config_fields()
            .into_iter()
            .filter(|f| f.secret)
            .collect()
    }
}

/// Capability ref for an extension: `ext:<name>`.
pub fn extension_capability_id(name: &str) -> String {
    format!("ext:{name}")
}

/// `(name, skills_dir)` for every enabled extension that declares `skills` and
/// actually ships a `skills/` directory. Read-only skill scopes are built from
/// this (see `capabilities::skills`). Skills load only for enabled extensions,
/// matching the MCP-contribution rule.
pub fn extension_skill_scopes(
    packages: &[ExtensionPackage],
    is_enabled: impl Fn(&str) -> bool,
) -> Vec<(String, PathBuf)> {
    packages
        .iter()
        .filter_map(|pkg| {
            if !pkg.manifest.skills || !is_enabled(&pkg.manifest.name) {
                return None;
            }
            let dir = pkg.dir.join("skills");
            dir.is_dir().then(|| (pkg.manifest.name.clone(), dir))
        })
        .collect()
}

pub fn parse_manifest(raw: &str) -> Result<ExtensionManifest, String> {
    let raw: RawManifest =
        serde_json::from_str(raw).map_err(|e| format!("invalid plugin.json: {e}"))?;
    let name = raw.name.trim().to_string();
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(format!(
            "invalid extension name `{name}`: use ascii letters, digits, `-`, `_`"
        ));
    }
    if let Some(version) = &raw.yolop.protocol_version
        && !super::protocol::version_compatible(version)
    {
        return Err(format!(
            "extension targets YEP {version}, incompatible with this yolop ({})",
            super::protocol::PROTOCOL_VERSION
        ));
    }
    if raw.yolop.capability_server.command.trim().is_empty() {
        return Err("yolop.capabilityServer.command must not be empty".into());
    }
    if raw.yolop.tools.is_empty()
        && !raw.yolop.prompt
        && !raw.yolop.dynamic_prompt
        && raw.yolop.mcp_servers.is_empty()
        && raw.yolop.hooks.is_empty()
        && raw.yolop.commands.is_empty()
        && !raw.yolop.status
        && !raw.yolop.skills
        && !raw.yolop.trace
    {
        return Err("extension declares no contributions; nothing to contribute".into());
    }
    for command in &raw.yolop.commands {
        if command.name.trim().is_empty()
            || !command
                .name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Err(format!(
                "invalid command name `{}`: use ascii letters, digits, `-`, `_`",
                command.name
            ));
        }
    }
    for (server_name, server) in &raw.yolop.mcp_servers {
        match server.transport {
            McpTransport::Stdio if server.command.as_deref().unwrap_or("").trim().is_empty() => {
                return Err(format!(
                    "mcpServers.{server_name}: stdio transport requires a non-empty `command`"
                ));
            }
            McpTransport::Http if server.url.as_deref().unwrap_or("").trim().is_empty() => {
                return Err(format!(
                    "mcpServers.{server_name}: http transport requires a non-empty `url`"
                ));
            }
            _ => {}
        }
    }
    Ok(ExtensionManifest {
        name,
        description: raw.description,
        version: raw.version,
        capability_server: raw.yolop.capability_server,
        config_schema: raw.yolop.config_schema,
        tools: raw.yolop.tools,
        prompt: raw.yolop.prompt,
        dynamic_prompt: raw.yolop.dynamic_prompt,
        mcp_servers: raw.yolop.mcp_servers,
        hooks: raw.yolop.hooks,
        commands: raw.yolop.commands,
        status: raw.yolop.status,
        skills: raw.yolop.skills,
        ui_ask: raw.yolop.ui_ask,
        trace: raw.yolop.trace,
    })
}

/// The global extensions directory. `None` when the platform has no config
/// dir and no override is set.
pub fn extensions_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("YOLOP_EXTENSIONS_DIR") {
        return Some(PathBuf::from(dir));
    }
    dirs::config_dir().map(|p| p.join("yolop").join("extensions"))
}

/// Discover installed packages. Malformed packages warn and are skipped;
/// a missing directory is silent. Never sinks startup.
pub fn discover_extensions(dir: &Path) -> Vec<ExtensionPackage> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut packages = Vec::new();
    for entry in entries.flatten() {
        let package_dir = entry.path();
        let manifest_path = package_dir.join(MANIFEST_FILE);
        if !manifest_path.is_file() {
            continue;
        }
        let raw = match std::fs::read_to_string(&manifest_path) {
            Ok(raw) => raw,
            Err(err) => {
                tracing::warn!(target: "yolop::ext", "skipping {}: {err}", manifest_path.display());
                continue;
            }
        };
        match parse_manifest(&raw) {
            Ok(manifest) => {
                // The directory name is the identity users enable by; a
                // mismatched manifest name is a packaging bug worth refusing.
                let dir_name = entry.file_name().to_string_lossy().to_string();
                if dir_name != manifest.name {
                    tracing::warn!(
                        target: "yolop::ext",
                        "skipping {}: directory `{dir_name}` != manifest name `{}`",
                        manifest_path.display(),
                        manifest.name
                    );
                    continue;
                }
                tracing::debug!(
                    target: "yolop::ext",
                    "discovered extension `{}` {}",
                    manifest.name,
                    manifest.version.as_deref().unwrap_or("(unversioned)")
                );
                packages.push(ExtensionPackage {
                    dir: package_dir,
                    manifest,
                });
            }
            Err(err) => {
                tracing::warn!(target: "yolop::ext", "skipping {}: {err}", manifest_path.display());
            }
        }
    }
    packages.sort_by(|a, b| a.manifest.name.cmp(&b.manifest.name));
    packages
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn manifest_json() -> Value {
        json!({
            "name": "echo",
            "description": "Echo test extension.",
            "version": "0.1.0",
            "future_top_level": true,
            "yolop": {
                "protocol_version": "1.0",
                "capabilityServer": { "command": "yolop-extension-echo" },
                "tools": [
                    { "name": "echo", "description": "Echo text back.",
                      "schema": { "type": "object", "properties": { "text": { "type": "string" } } },
                      "never_defer": true }
                ],
                "prompt": true
            }
        })
    }

    #[test]
    fn parses_manifest_and_ignores_unknown_fields() {
        let manifest = parse_manifest(&manifest_json().to_string()).expect("parse");
        assert_eq!(manifest.name, "echo");
        assert_eq!(manifest.tools.len(), 1);
        assert!(manifest.tools[0].never_defer);
        assert!(manifest.prompt);
        assert_eq!(extension_capability_id(&manifest.name), "ext:echo");
    }

    #[test]
    fn rejects_incompatible_protocol_and_empty_contribution() {
        let mut incompatible = manifest_json();
        incompatible["yolop"]["protocol_version"] = json!("2.0");
        assert!(
            parse_manifest(&incompatible.to_string())
                .unwrap_err()
                .contains("incompatible")
        );

        let mut empty = manifest_json();
        empty["yolop"]["tools"] = json!([]);
        empty["yolop"]["prompt"] = json!(false);
        assert!(
            parse_manifest(&empty.to_string())
                .unwrap_err()
                .contains("nothing to contribute")
        );
    }

    #[test]
    fn parses_contributed_mcp_servers() {
        let mut m = manifest_json();
        m["yolop"]["mcpServers"] = json!({
            "docs": { "type": "http", "url": "https://example.com/mcp",
                      "headers": { "Authorization": "Bearer ${DOCS_TOKEN}" } },
            "fs": { "command": "mcp-fs", "args": ["/w"], "env": { "RUST_LOG": "info" } }
        });
        let manifest = parse_manifest(&m.to_string()).expect("parse");
        assert_eq!(manifest.mcp_servers.len(), 2);
        assert_eq!(manifest.mcp_servers["fs"].transport, McpTransport::Stdio);
        assert_eq!(manifest.mcp_servers["docs"].transport, McpTransport::Http);
        assert_eq!(
            manifest.mcp_servers["docs"].url.as_deref(),
            Some("https://example.com/mcp")
        );
    }

    #[test]
    fn mcp_only_extension_is_valid_and_transports_are_checked() {
        // No tools, no prompt, but an MCP server — a valid contribution.
        let mcp_only = json!({
            "name": "wrap", "description": "Wrap an MCP server.",
            "yolop": {
                "protocol_version": "1.0",
                "capabilityServer": { "command": "x" },
                "mcpServers": { "svc": { "command": "svc-bin" } }
            }
        });
        assert!(parse_manifest(&mcp_only.to_string()).is_ok());

        // stdio without a command is rejected.
        let mut bad = mcp_only.clone();
        bad["yolop"]["mcpServers"]["svc"] = json!({ "type": "stdio" });
        assert!(
            parse_manifest(&bad.to_string())
                .unwrap_err()
                .contains("requires a non-empty `command`")
        );

        // http without a url is rejected.
        let mut bad_http = mcp_only;
        bad_http["yolop"]["mcpServers"]["svc"] = json!({ "type": "http" });
        assert!(
            parse_manifest(&bad_http.to_string())
                .unwrap_err()
                .contains("requires a non-empty `url`")
        );
    }

    #[test]
    fn discovery_skips_malformed_and_mismatched_dirs() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let good = tmp.path().join("echo");
        std::fs::create_dir_all(&good).unwrap();
        std::fs::write(good.join(MANIFEST_FILE), manifest_json().to_string()).unwrap();

        let malformed = tmp.path().join("broken");
        std::fs::create_dir_all(&malformed).unwrap();
        std::fs::write(malformed.join(MANIFEST_FILE), "{not json").unwrap();

        // Manifest says `echo` but lives under `wrong-name`.
        let mismatched = tmp.path().join("wrong-name");
        std::fs::create_dir_all(&mismatched).unwrap();
        std::fs::write(mismatched.join(MANIFEST_FILE), manifest_json().to_string()).unwrap();

        let found = discover_extensions(tmp.path());
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].manifest.name, "echo");
    }

    #[test]
    fn ui_ask_parses_but_is_not_a_standalone_contribution() {
        // ui_ask alongside a tool: opt-in is recorded.
        let with_tool = json!({
            "name": "asker", "description": "t",
            "yolop": { "protocol_version": "1.0",
                "capabilityServer": { "command": "x" },
                "tools": [{ "name": "t" }], "ui_ask": true }
        });
        assert!(parse_manifest(&with_tool.to_string()).unwrap().ui_ask);

        // ui_ask alone can't be triggered, so it's not a contribution by itself.
        let alone = json!({
            "name": "asker", "description": "t",
            "yolop": { "protocol_version": "1.0",
                "capabilityServer": { "command": "x" }, "ui_ask": true }
        });
        assert!(
            parse_manifest(&alone.to_string())
                .unwrap_err()
                .contains("nothing to contribute")
        );
    }

    #[test]
    fn parses_commands_and_rejects_bad_names() {
        let ok = json!({
            "name": "greeter", "description": "t",
            "yolop": { "protocol_version": "1.0",
                "capabilityServer": { "command": "x" },
                "commands": [{ "name": "hello", "description": "Say hi",
                    "args": [{ "name": "who", "required": true }] }] }
        });
        let manifest = parse_manifest(&ok.to_string()).expect("commands parse");
        assert_eq!(manifest.commands.len(), 1);
        assert_eq!(manifest.commands[0].name, "hello");
        assert_eq!(manifest.commands[0].args[0].name, "who");
        assert!(manifest.commands[0].args[0].required);

        // A command name with a space/slash is rejected.
        let mut bad = ok;
        bad["yolop"]["commands"][0]["name"] = json!("bad name");
        assert!(
            parse_manifest(&bad.to_string())
                .unwrap_err()
                .contains("invalid command name")
        );
    }

    #[test]
    fn extension_skill_scopes_needs_flag_enabled_and_a_skills_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let mk = |name: &str, declares: bool, has_dir: bool| -> ExtensionPackage {
            let dir = tmp.path().join(name);
            std::fs::create_dir_all(&dir).unwrap();
            if has_dir {
                std::fs::create_dir_all(dir.join("skills")).unwrap();
            }
            let manifest = parse_manifest(
                &json!({
                    "name": name, "description": "t",
                    "yolop": { "protocol_version": "1.0",
                        "capabilityServer": { "command": "x" },
                        "tools": [{ "name": "t" }], "skills": declares }
                })
                .to_string(),
            )
            .unwrap();
            ExtensionPackage { dir, manifest }
        };
        let packages = vec![
            mk("has-skills", true, true), // declares + dir + enabled → included
            mk("no-flag", false, true),   // no `skills` flag → excluded
            mk("no-dir", true, false),    // flag but no skills/ dir → excluded
            mk("disabled", true, true),   // flag + dir but disabled → excluded
        ];
        let scopes = extension_skill_scopes(&packages, |name| name != "disabled");
        assert_eq!(scopes.len(), 1, "{scopes:?}");
        assert_eq!(scopes[0].0, "has-skills");
        assert!(scopes[0].1.ends_with("skills"));
    }
}

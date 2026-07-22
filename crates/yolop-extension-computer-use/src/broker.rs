//! The direct broker: one ephemeral, zero-turn `codex app-server` process per
//! tool call, driven over newline-delimited JSON-RPC on stdio.
//!
//! A faithful Rust port of `codex-computer-use-mcp`'s `src/direct-broker.ts`.
//! The load-bearing pieces are reproduced exactly because they are what makes
//! the *signed* helper accept the call (the plain path is rejected with
//! `Sender process is not authenticated`):
//!
//!   1. signature + OpenAI-team-id verification of the bundled binaries;
//!   2. the exact `app-server --stdio` config (`buildDirectAppServerArgs`) that
//!      disables every model/provider/plugin path and registers only the
//!      bundled `computer-use` MCP server;
//!   3. the `initialize` → `initialized` → `thread/start` (ephemeral, zero-turn,
//!      Full access) → `mcpServerStatus/list` (schema drift check) →
//!      `mcpServer/tool/call` sequence;
//!   4. failing closed if any `turn/*` or `item/*` model activity appears.
//!
//! Deliberately *not* ported in this first cut (macOS trust extras layered on
//! top of the functional core in the upstream service; tracked as follow-ups —
//! see README): the JSONL audit log, the per-app `lockf` lease, the frontmost
//! focus telemetry / background-preservation enforcement, and osascript app
//! identity canonicalization. The signed-binary verification and the
//! zero-model-turn guarantee — the two properties the security story actually
//! rests on — are ported.

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

pub const CODEX_PATH: &str = "/Applications/ChatGPT.app/Contents/Resources/codex";
pub const COMPUTER_USE_PLUGIN_ROOT: &str =
    "/Applications/ChatGPT.app/Contents/Resources/plugins/openai-bundled/plugins/computer-use";
const OPENAI_TEAM_ID: &str = "2DC432GLL2";

fn computer_use_client_path() -> String {
    format!(
        "{COMPUTER_USE_PLUGIN_ROOT}/Codex Computer Use.app/Contents/SharedSupport/SkyComputerUseClient.app/Contents/MacOS/SkyComputerUseClient"
    )
}

/// Dev/test override: point at a stub app-server executable. When set, signature
/// verification is skipped (there is no signed bundle to verify). Mirrors the
/// upstream `appServerCommand` + `skipSignatureVerification` test seams.
const APP_SERVER_OVERRIDE_ENV: &str = "YOLOP_COMPUTER_USE_APP_SERVER";

pub struct BrokerResult {
    pub content: Vec<Value>,
    pub is_error: bool,
    pub broker_version: String,
    pub client_build: String,
    pub elicitation_requests: u32,
    pub duration_ms: u128,
}

// ---- verification --------------------------------------------------------

fn verify_signed_binary(binary: &str) -> Result<(), String> {
    let name = Path::new(binary)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| binary.to_string());
    let verify = Command::new("/usr/bin/codesign")
        .args(["--verify", "--strict", binary])
        .output()
        .map_err(|e| format!("codesign unavailable: {e}"))?;
    if !verify.status.success() {
        return Err(format!("Signature verification failed for {name}"));
    }
    let details = Command::new("/usr/bin/codesign")
        .args(["-dv", "--verbose=2", binary])
        .output()
        .map_err(|e| format!("codesign unavailable: {e}"))?;
    let combined = format!(
        "{}\n{}",
        String::from_utf8_lossy(&details.stdout),
        String::from_utf8_lossy(&details.stderr)
    );
    if !details.status.success() || !combined.contains(&format!("TeamIdentifier={OPENAI_TEAM_ID}"))
    {
        return Err(format!("{name} is not signed by the expected OpenAI team"));
    }
    Ok(())
}

fn client_build() -> Result<String, String> {
    let plist = format!("{COMPUTER_USE_PLUGIN_ROOT}/Codex Computer Use.app/Contents/Info.plist");
    let out = Command::new("/usr/bin/plutil")
        .args(["-extract", "CFBundleVersion", "raw", &plist])
        .output()
        .map_err(|e| format!("plutil unavailable: {e}"))?;
    let version = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if !out.status.success() || version.is_empty() {
        return Err("Could not verify the official Computer Use client build".into());
    }
    Ok(version)
}

/// Verify the signed binaries and read their versions. Returns
/// `(broker_version, client_build)`.
pub fn verify_official_direct_broker() -> Result<(String, String), String> {
    verify_signed_binary(CODEX_PATH)?;
    verify_signed_binary(&computer_use_client_path())?;
    let out = Command::new(CODEX_PATH)
        .arg("--version")
        .output()
        .map_err(|e| format!("app-server unavailable: {e}"))?;
    let version = String::from_utf8_lossy(&out.stdout).trim().to_string();
    // Matches `^codex-cli\s+\d+\.`.
    let looks_like_codex = version
        .strip_prefix("codex-cli")
        .map(|rest| {
            let rest = rest.trim_start();
            let mut chars = rest.chars();
            matches!(chars.next(), Some(c) if c.is_ascii_digit()) && rest.split('.').count() >= 2
        })
        .unwrap_or(false);
    if !out.status.success() || !looks_like_codex {
        return Err("Could not verify the app-bundled Codex app-server version".into());
    }
    Ok((version, client_build()?))
}

// ---- app-server launch ---------------------------------------------------

fn broker_env(codex_home: &str, temp_root: &str) -> Vec<(String, String)> {
    let mut env = vec![
        ("HOME".to_string(), temp_root.to_string()),
        ("CODEX_HOME".to_string(), codex_home.to_string()),
        (
            "PATH".to_string(),
            "/usr/bin:/bin:/usr/sbin:/sbin".to_string(),
        ),
        ("TMPDIR".to_string(), temp_root.to_string()),
        ("NO_COLOR".to_string(), "1".to_string()),
        ("CLICOLOR".to_string(), "0".to_string()),
    ];
    for key in [
        "USER", "LOGNAME", "LANG", "LC_ALL", "LC_CTYPE", "SHELL", "TERM",
    ] {
        if let Ok(value) = std::env::var(key) {
            env.push((key.to_string(), value));
        }
    }
    env
}

/// The exact `app-server --stdio` arguments (`buildDirectAppServerArgs`). Every
/// `-c` override disables a model/provider/plugin path so the process can do
/// nothing but proxy the one bundled `computer-use` MCP server.
pub fn build_app_server_args(mcp_cwd: &str) -> Vec<String> {
    let client = computer_use_client_path();
    let mcp_table = format!(
        "{{\"computer-use\" = {{ command = {}, args = [\"mcp\"], cwd = {}, enabled = true, startup_timeout_sec = 30, tool_timeout_sec = 120 }}}}",
        json_string(&client),
        json_string(mcp_cwd),
    );
    let disabled_provider = "{ name = \"Direct dispatch disabled provider\", base_url = \"http://127.0.0.1:9/v1\", wire_api = \"responses\", request_max_retries = 0, stream_max_retries = 0, supports_websockets = false, requires_openai_auth = false }";
    [
        "-c",
        "model_provider=\"direct_disabled\"",
        "-c",
        "model=\"direct-disabled\"",
        "-c",
        &format!("model_providers.direct_disabled={disabled_provider}"),
        "-c",
        "features.shell_tool=false",
        "-c",
        "features.unified_exec=false",
        "-c",
        "features.multi_agent=false",
        "-c",
        "features.memories=false",
        "-c",
        "memories.use_memories=false",
        "-c",
        "memories.generate_memories=false",
        "-c",
        "features.remote_plugin=false",
        "-c",
        "features.plugins=false",
        "-c",
        "features.remote_control=false",
        "-c",
        "features.hooks=false",
        "-c",
        "analytics.enabled=false",
        "-c",
        "otel.exporter=\"none\"",
        "-c",
        "web_search=\"disabled\"",
        "-c",
        "history.persistence=\"none\"",
        "-c",
        &format!("mcp_servers={mcp_table}"),
        "-c",
        "plugins={}",
        "app-server",
        "--stdio",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// A JSON string literal (quoted, escaped) — the TOML config values above use
/// JSON string syntax, matching `JSON.stringify` in the upstream args builder.
fn json_string(value: &str) -> String {
    Value::String(value.to_string()).to_string()
}

// ---- the call ------------------------------------------------------------

pub fn call_official_direct_tool(
    method: &str,
    args: &Value,
    timeout: Duration,
) -> Result<BrokerResult, String> {
    let started = Instant::now();
    let override_cmd = std::env::var(APP_SERVER_OVERRIDE_ENV)
        .ok()
        .filter(|s| !s.is_empty());

    let (broker_version, build) = match &override_cmd {
        Some(_) => ("test-app-server".to_string(), "test-client".to_string()),
        None => verify_official_direct_broker()?,
    };

    let temp_root = make_temp_root()?;
    let codex_home = temp_root.join("codex-home");
    let work_dir = temp_root.join("work");
    std::fs::create_dir_all(&codex_home).map_err(|e| format!("temp setup failed: {e}"))?;
    std::fs::create_dir_all(&work_dir).map_err(|e| format!("temp setup failed: {e}"))?;
    let _ = std::fs::write(codex_home.join("config.toml"), "");
    harden_dir(&temp_root);
    harden_dir(&codex_home);
    harden_dir(&work_dir);

    let work_dir_str = work_dir.to_string_lossy().to_string();
    let command = override_cmd
        .clone()
        .unwrap_or_else(|| CODEX_PATH.to_string());
    let args_vec = build_app_server_args(&work_dir_str);
    let env = broker_env(&codex_home.to_string_lossy(), &temp_root.to_string_lossy());

    let mut child = spawn_app_server(&command, &args_vec, &work_dir, &env)?;
    let pid = child.id();

    // Reader thread → channel of parsed protocol lines (Err = malformed JSONL).
    let stdout = child.stdout.take().expect("piped stdout");
    let (tx, rx) = channel::<Result<Value, String>>();
    std::thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines() {
            let Ok(line) = line else { break };
            if line.trim().is_empty() {
                continue;
            }
            let parsed = serde_json::from_str::<Value>(&line)
                .map_err(|_| "Official app-server emitted malformed JSONL".to_string());
            if tx.send(parsed).is_err() {
                break;
            }
        }
    });

    // Drain stderr (bounded) for authentication-failure detection.
    let stderr_buf = Arc::new(Mutex::new(String::new()));
    if let Some(stderr) = child.stderr.take() {
        let sink = Arc::clone(&stderr_buf);
        std::thread::spawn(move || {
            let reader = BufReader::new(stderr);
            for line in reader.lines().map_while(Result::ok) {
                let mut guard = sink.lock().unwrap();
                if guard.len() >= 16_384 {
                    break;
                }
                guard.push_str(&line);
                guard.push('\n');
            }
        });
    }

    let mut stdin = child.stdin.take().expect("piped stdin");
    let mut session = Session {
        rx: &rx,
        stdin: &mut stdin,
        next_id: 1,
        elicitation_requests: 0,
    };

    let outcome = run_sequence(&mut session, method, args, &work_dir_str, timeout);

    // Teardown always: kill the whole process group, reap, remove temp tree.
    let elicitation_requests = session.elicitation_requests;
    drop(stdin); // EOF on the child's stdin
    teardown(&mut child, pid);
    let _ = std::fs::remove_dir_all(&temp_root);

    match outcome {
        Ok((content, is_error)) => Ok(BrokerResult {
            content,
            is_error,
            broker_version,
            client_build: build,
            elicitation_requests,
            duration_ms: started.elapsed().as_millis(),
        }),
        Err(err) => {
            let stderr = stderr_buf.lock().unwrap().clone();
            Err(map_auth_failure(&err, &stderr))
        }
    }
}

struct Session<'a> {
    rx: &'a Receiver<Result<Value, String>>,
    stdin: &'a mut std::process::ChildStdin,
    next_id: u64,
    elicitation_requests: u32,
}

impl Session<'_> {
    fn send(&mut self, message: &Value) -> Result<(), String> {
        let line = message.to_string();
        self.stdin
            .write_all(line.as_bytes())
            .and_then(|_| self.stdin.write_all(b"\n"))
            .and_then(|_| self.stdin.flush())
            .map_err(|e| format!("Official app-server stdin is unavailable: {e}"))
    }

    /// Issue one request and pump incoming messages until its response arrives,
    /// handling interleaved server→client requests (elicitation) and failing
    /// closed on any model-turn activity.
    fn request(&mut self, method: &str, params: Value, timeout: Duration) -> Result<Value, String> {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&json!({ "method": method, "id": id.to_string(), "params": params }))?;

        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .ok_or_else(|| format!("Official app-server request timed out: {method}"))?;
            let message = match self.rx.recv_timeout(remaining) {
                Ok(Ok(message)) => message,
                Ok(Err(malformed)) => return Err(malformed),
                Err(RecvTimeoutError::Timeout) => {
                    return Err(format!("Official app-server request timed out: {method}"));
                }
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(format!(
                        "Official app-server exited before completing the request: {method}"
                    ));
                }
            };
            if let Some(resolved) = self.route(&message, &id.to_string())? {
                return Ok(resolved);
            }
        }
    }

    /// Route one incoming message. `Some(result)` resolves the awaited request;
    /// `None` means it was handled (server request / notification) — keep
    /// waiting. `Err` is fatal.
    fn route(&mut self, message: &Value, awaited_id: &str) -> Result<Option<Value>, String> {
        // Fail closed on any model-turn activity — the zero-turn guarantee.
        if let Some(m) = message.get("method").and_then(Value::as_str)
            && (m.starts_with("turn/") || m.starts_with("item/"))
        {
            return Err(
                "Official app-server unexpectedly emitted model-turn activity during direct dispatch".into(),
            );
        }
        let has_id = message.get("id").is_some_and(|v| !v.is_null());
        let is_response =
            has_id && (message.get("result").is_some() || message.get("error").is_some());
        if is_response {
            if id_string(&message["id"]).as_deref() != Some(awaited_id) {
                return Ok(None); // not ours (shouldn't happen: strictly sequential)
            }
            if let Some(error) = message.get("error") {
                let msg = error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                return Err(format!("Official app-server error: {msg}"));
            }
            return Ok(Some(message.get("result").cloned().unwrap_or(Value::Null)));
        }
        // Server→client request.
        if let (true, Some(server_method)) = (has_id, message.get("method").and_then(Value::as_str))
        {
            let reply_id = message["id"].clone();
            if server_method == "mcpServer/elicitation/request" {
                // The yep SDK has no client-side elicitation forwarding; under
                // Codex Full access these are auto-resolved and should not
                // appear. If one does, decline safely — never fabricate accept.
                self.elicitation_requests += 1;
                self.send(&json!({ "id": reply_id, "result": { "action": "cancel" } }))?;
            } else {
                self.send(&json!({ "id": reply_id, "error": { "code": -32601, "message": "Unsupported server request" } }))?;
            }
            return Ok(None);
        }
        // Notification we don't act on.
        Ok(None)
    }
}

fn run_sequence(
    session: &mut Session,
    method: &str,
    args: &Value,
    work_dir: &str,
    timeout: Duration,
) -> Result<(Vec<Value>, bool), String> {
    session.request(
        "initialize",
        json!({
            "clientInfo": { "name": "yolop_computer_use", "title": "yolop Computer Use", "version": env!("CARGO_PKG_VERSION") },
            "capabilities": { "mcpServerOpenaiFormElicitation": false },
        }),
        Duration::from_secs(15),
    )?;
    session.send(&json!({ "method": "initialized" }))?;

    let started = session.request(
        "thread/start",
        json!({
            "cwd": work_dir,
            "approvalPolicy": "never",
            "sandbox": "danger-full-access",
            "ephemeral": true,
            "serviceName": "yolop_computer_use",
        }),
        Duration::from_secs(30),
    )?;
    let thread = started.get("thread").cloned().unwrap_or(Value::Null);
    let thread_id = thread.get("id").and_then(Value::as_str);
    let attests_ephemeral = thread_id.is_some()
        && thread.get("ephemeral") == Some(&Value::Bool(true))
        && thread.get("path") == Some(&Value::Null)
        && thread
            .get("turns")
            .and_then(Value::as_array)
            .map(Vec::is_empty)
            == Some(true);
    if !attests_ephemeral {
        return Err("App-server did not attest an empty pathless ephemeral runtime context".into());
    }
    let thread_id = thread_id.unwrap().to_string();

    let inventory = session.request(
        "mcpServerStatus/list",
        json!({ "threadId": thread_id, "detail": "toolsAndAuthOnly" }),
        Duration::from_secs(45),
    )?;
    validate_inventory(&inventory)?;

    let raw = session.request(
        "mcpServer/tool/call",
        json!({ "threadId": thread_id, "server": "computer-use", "tool": method, "arguments": args }),
        timeout,
    )?;
    validate_result(&raw)
}

// ---- validation ----------------------------------------------------------

fn validate_inventory(result: &Value) -> Result<(), String> {
    let data = result
        .get("data")
        .and_then(Value::as_array)
        .ok_or("App-server returned an invalid MCP inventory")?;
    let server = data
        .iter()
        .find(|entry| entry.get("name") == Some(&Value::String("computer-use".into())))
        .ok_or("Official computer-use MCP server was not available")?;
    let tools = server
        .get("tools")
        .and_then(Value::as_object)
        .ok_or("Official computer-use MCP server was not available")?;
    let mut names: Vec<&str> = tools.keys().map(String::as_str).collect();
    names.sort_unstable();
    let mut expected: Vec<&str> = crate::tools::OFFICIAL_METHODS.to_vec();
    expected.sort_unstable();
    if names != expected {
        return Err(
            "Official Computer Use tool inventory drifted; refusing direct dispatch".into(),
        );
    }
    for &method in crate::tools::OFFICIAL_METHODS {
        let upstream = &tools[method];
        let schema = upstream
            .get("inputSchema")
            .or_else(|| upstream.get("input_schema"))
            .cloned()
            .unwrap_or(Value::Null);
        if normalize_schema(&schema) != normalize_schema(&crate::tools::input_schema(method)) {
            return Err(format!(
                "Official Computer Use schema drifted for {method}; refusing direct dispatch"
            ));
        }
    }
    Ok(())
}

/// Order-insensitive schema comparison (`normalizeSchema`): sort arrays by their
/// serialized form. Object key order is already irrelevant to `Value` equality.
fn normalize_schema(value: &Value) -> Value {
    match value {
        Value::Array(items) => {
            let mut normalized: Vec<Value> = items.iter().map(normalize_schema).collect();
            normalized.sort_by_key(|v| v.to_string());
            Value::Array(normalized)
        }
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), normalize_schema(v)))
                .collect(),
        ),
        other => other.clone(),
    }
}

const MAX_RESULT_BYTES: usize = 25 * 1024 * 1024;

fn validate_result(value: &Value) -> Result<(Vec<Value>, bool), String> {
    let record = value
        .as_object()
        .ok_or("Official Computer Use returned an invalid result")?;
    let content = record
        .get("content")
        .and_then(Value::as_array)
        .ok_or("Official Computer Use returned invalid content blocks")?;
    if content.len() > 100 {
        return Err("Official Computer Use returned invalid content blocks".into());
    }
    for item in content {
        let obj = item
            .as_object()
            .ok_or("Official Computer Use returned an unsupported content block")?;
        match obj.get("type").and_then(Value::as_str) {
            Some("text") => {
                if !obj.get("text").is_some_and(Value::is_string) {
                    return Err("Official Computer Use returned malformed text content".into());
                }
            }
            Some("image") => {
                if !obj.get("data").is_some_and(Value::is_string)
                    || !obj.get("mimeType").is_some_and(Value::is_string)
                {
                    return Err("Official Computer Use returned malformed image content".into());
                }
            }
            _ => return Err("Official Computer Use returned an unsupported content block".into()),
        }
    }
    let encoded =
        json!({ "content": content, "structuredContent": record.get("structuredContent") });
    if encoded.to_string().len() > MAX_RESULT_BYTES {
        return Err("Official Computer Use result exceeded the 25MB safety bound".into());
    }
    let is_error = record.get("isError") == Some(&Value::Bool(true));
    Ok((content.clone(), is_error))
}

// ---- process plumbing ----------------------------------------------------

fn make_temp_root() -> Result<std::path::PathBuf, String> {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!("yolop-cu.{}.{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).map_err(|e| format!("temp setup failed: {e}"))?;
    Ok(dir)
}

#[cfg(unix)]
fn harden_dir(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
}

#[cfg(not(unix))]
fn harden_dir(_dir: &Path) {}

#[cfg(unix)]
fn spawn_app_server(
    command: &str,
    args: &[String],
    work_dir: &Path,
    env: &[(String, String)],
) -> Result<Child, String> {
    use std::os::unix::process::CommandExt;
    Command::new(command)
        .args(args)
        .current_dir(work_dir)
        .env_clear()
        .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Own process group so the whole helper tree can be signalled at once.
        .process_group(0)
        .spawn()
        .map_err(|e| format!("Could not start the official app-server: {e}"))
}

#[cfg(not(unix))]
fn spawn_app_server(
    command: &str,
    args: &[String],
    work_dir: &Path,
    env: &[(String, String)],
) -> Result<Child, String> {
    Command::new(command)
        .args(args)
        .current_dir(work_dir)
        .env_clear()
        .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Could not start the official app-server: {e}"))
}

/// Terminate the app-server and its descendants. The child leads its own
/// process group (see `spawn_app_server`), so signalling `-pid` reaches the
/// whole tree. Simpler than the upstream freeze/enumerate cleanup, which
/// guarded against process-tree races; a hardened teardown is a follow-up.
fn teardown(child: &mut Child, pid: u32) {
    #[cfg(unix)]
    {
        let group = format!("-{pid}");
        let _ = Command::new("/bin/kill").args(["-TERM", &group]).status();
        std::thread::sleep(Duration::from_millis(50));
        let _ = Command::new("/bin/kill").args(["-KILL", &group]).status();
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn id_string(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn map_auth_failure(message: &str, stderr: &str) -> String {
    let haystack = format!("{message}\n{stderr}").to_ascii_lowercase();
    // "authenticat" catches both "not authenticated" (the signed-helper
    // rejection) and "authentication".
    if ["authenticat", "bearer", "token"]
        .iter()
        .any(|needle| haystack.contains(needle))
    {
        return "Official direct Computer Use broker reported an authentication failure".into();
    }
    message.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_server_args_register_only_computer_use() {
        let args = build_app_server_args("/tmp/work");
        assert!(args.iter().any(|a| a.contains("mcp_servers=")));
        assert!(args.iter().any(|a| a == "features.plugins=false"));
        assert_eq!(args[args.len() - 2], "app-server");
        assert_eq!(args[args.len() - 1], "--stdio");
        let table = args.iter().find(|a| a.starts_with("mcp_servers=")).unwrap();
        assert!(table.contains("\"computer-use\""));
        assert!(table.contains("SkyComputerUseClient"));
    }

    #[test]
    fn schema_normalization_is_order_insensitive() {
        let a = json!({ "required": ["a", "b"], "enum": ["x", "y"] });
        let b = json!({ "enum": ["y", "x"], "required": ["b", "a"] });
        assert_eq!(normalize_schema(&a), normalize_schema(&b));
    }

    #[test]
    fn validate_result_accepts_text_and_image_rejects_junk() {
        let ok = json!({ "content": [
            { "type": "text", "text": "hi" },
            { "type": "image", "data": "AAAA", "mimeType": "image/png" }
        ]});
        let (content, is_error) = validate_result(&ok).expect("valid");
        assert_eq!(content.len(), 2);
        assert!(!is_error);

        let bad = json!({ "content": [{ "type": "video", "url": "x" }] });
        assert!(validate_result(&bad).is_err());

        let flagged = json!({ "content": [{ "type": "text", "text": "boom" }], "isError": true });
        assert!(validate_result(&flagged).unwrap().1);
    }

    #[test]
    fn inventory_drift_is_rejected() {
        // A well-formed inventory that matches our baseline schemas passes.
        let mut tools = serde_json::Map::new();
        for &m in crate::tools::OFFICIAL_METHODS {
            tools.insert(
                m.to_string(),
                json!({ "inputSchema": crate::tools::input_schema(m) }),
            );
        }
        let good = json!({ "data": [{ "name": "computer-use", "tools": tools }] });
        assert!(validate_inventory(&good).is_ok());

        // Drop a method → drift.
        let mut fewer = good.clone();
        fewer["data"][0]["tools"]
            .as_object_mut()
            .unwrap()
            .remove("click");
        assert!(validate_inventory(&fewer).is_err());
    }

    #[test]
    fn auth_failures_are_normalized() {
        assert_eq!(
            map_auth_failure("Official app-server error: Sender not authenticated", ""),
            "Official direct Computer Use broker reported an authentication failure"
        );
        assert_eq!(map_auth_failure("some other error", ""), "some other error");
    }

    /// End-to-end broker drive against a shell stub that speaks the app-server
    /// JSON-RPC dance. Runs on Linux CI (no macOS / ChatGPT.app needed): it
    /// exercises everything except the signed binaries — the handshake,
    /// zero-turn ephemeral thread attestation, inventory drift check, tool
    /// dispatch, and teardown.
    #[cfg(unix)]
    #[test]
    fn drives_a_stub_app_server_through_the_full_sequence() {
        use std::os::unix::fs::PermissionsExt;

        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("yolop-cu-test.{}.{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        // Inventory (response id "3") whose schemas match our baseline exactly.
        let mut tools = serde_json::Map::new();
        for &m in crate::tools::OFFICIAL_METHODS {
            tools.insert(
                m.to_string(),
                json!({ "inputSchema": crate::tools::input_schema(m) }),
            );
        }
        let inventory = json!({ "id": "3", "result": { "data": [{ "name": "computer-use", "tools": tools }] } });
        let inv_path = dir.join("inventory.json");
        // Trailing newline so the reader delimits the line (the stub `cat`s it).
        std::fs::write(&inv_path, format!("{inventory}\n")).unwrap();

        let stub_path = dir.join("stub-app-server");
        let stub = format!(
            "#!/bin/sh\nwhile IFS= read -r line; do\n  case \"$line\" in\n    *tool/call*) printf '{{\"id\":\"4\",\"result\":{{\"content\":[{{\"type\":\"text\",\"text\":\"stub ok\"}}]}}}}\\n' ;;\n    *Status/list*) cat '{inv}' ;;\n    *thread/start*) printf '{{\"id\":\"2\",\"result\":{{\"thread\":{{\"id\":\"t1\",\"ephemeral\":true,\"path\":null,\"turns\":[]}}}}}}\\n' ;;\n    *initialized*) : ;;\n    *initialize*) printf '{{\"id\":\"1\",\"result\":{{}}}}\\n' ;;\n  esac\ndone\n",
            inv = inv_path.display()
        );
        std::fs::write(&stub_path, stub).unwrap();
        std::fs::set_permissions(&stub_path, std::fs::Permissions::from_mode(0o755)).unwrap();

        // SAFETY: single-threaded test with no other reader of this env var.
        unsafe { std::env::set_var(APP_SERVER_OVERRIDE_ENV, &stub_path) };
        let result = call_official_direct_tool("list_apps", &json!({}), Duration::from_secs(20));
        unsafe { std::env::remove_var(APP_SERVER_OVERRIDE_ENV) };
        let _ = std::fs::remove_dir_all(&dir);

        let result = result.expect("stub dispatch succeeds");
        assert!(!result.is_error);
        assert_eq!(result.content.len(), 1);
        assert_eq!(result.content[0]["text"], "stub ok");
        assert_eq!(result.broker_version, "test-app-server");
    }
}

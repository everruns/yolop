//! Extension scaffolding: generate a ready-to-edit YEP extension package so
//! the agent (or a human) can author an extension end-to-end without
//! remembering the wire protocol or the package layout.
//!
//! The generated package is **correct by construction** — authoring collapses
//! to "fill in the handler bodies." The server is resolved by prepending
//! `<package>/bin` to `PATH` (see `manager.rs`); the exec bit survives install
//! because `store::copy_dir` uses `std::fs::copy`.
//!
//! Three language templates, all emitting the same raw newline-delimited
//! JSON-RPC server:
//! - **Python** / **Node.js** (`typescript`): single-file, dependency-free,
//!   zero-build — the executable server lives directly in `bin/`, so the
//!   package installs and passes `doctor_extension` immediately.
//! - **Rust**: a `serde_json`-only crate. Compiled, so it carries a `build`
//!   step ([`Scaffolded::build`]) that produces the binary into `bin/` before
//!   install — the one difference from the zero-build path.

use serde_json::{Value, json};
use std::path::{Path, PathBuf};

/// A tool contribution to scaffold: the definition the model will see.
pub struct ToolSpec {
    pub name: String,
    pub description: String,
}

/// A hook subscription to scaffold.
pub struct HookSpec {
    /// `pre_tool_use` or `post_tool_use`.
    pub event: String,
    /// Glob over tool names the hook fires for (default `*`).
    pub tool_name_glob: String,
}

/// Which language template to emit. Both current templates are single-file,
/// dependency-free, and need no build step — the fastest path for a
/// self-writing loop. (A compiled Rust/`yolop-yep` template is a follow-up; it
/// has a distinct flow because the binary must be built before it can run.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    /// A `#!/usr/bin/env python3` server.
    Python,
    /// A dependency-free Node.js (`#!/usr/bin/env node`) server — the
    /// toolchain-free path for the TypeScript/JavaScript ecosystem.
    Node,
    /// A `serde_json`-only Rust crate. Compiled: the binary must be built into
    /// `bin/` before install (see [`Scaffolded::build`]).
    Rust,
}

impl Language {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "python" | "py" => Ok(Self::Python),
            "typescript" | "ts" | "javascript" | "js" | "node" => Ok(Self::Node),
            "rust" | "rs" => Ok(Self::Rust),
            other => Err(format!(
                "unknown language `{other}`; use `python`, `typescript`, or `rust`"
            )),
        }
    }

    /// The runtime/toolchain the template needs — used to skip tests when it
    /// isn't installed (`python3`/`node` to run, `cargo` to build).
    #[cfg(test)]
    pub fn interpreter(self) -> &'static str {
        match self {
            Self::Python => "python3",
            Self::Node => "node",
            Self::Rust => "cargo",
        }
    }
}

/// A fully specified scaffold request.
pub struct ScaffoldRequest {
    pub name: String,
    pub description: String,
    pub language: Language,
    pub tools: Vec<ToolSpec>,
    pub hooks: Vec<HookSpec>,
    /// Slash-command names the extension contributes (dispatched to the server).
    pub commands: Vec<String>,
    pub prompt: Option<String>,
    /// Contribute a status-bar field (the server may push `status/changed`).
    pub status: bool,
    /// Contribute a `skills/` directory (a starter `SKILL.md` is generated).
    pub skills: bool,
    /// Absolute path of the package directory to create.
    pub dir: PathBuf,
}

/// Outcome of a scaffold: what was written and where.
#[derive(Debug)]
pub struct Scaffolded {
    pub dir: PathBuf,
    /// The file the author edits to add logic — the server script for the
    /// zero-build templates, or `src/main.rs` for Rust.
    pub edit: PathBuf,
    pub files: Vec<String>,
    /// A shell command that must run before install to produce the server
    /// binary (Rust). `None` for the zero-build templates.
    pub build: Option<String>,
}

/// Validate the request the way `parse_manifest` will, so a scaffold never
/// produces a package the installer would reject.
fn validate(req: &ScaffoldRequest) -> Result<(), String> {
    let name = req.name.trim();
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(format!(
            "invalid extension name `{name}`: use ascii letters, digits, `-`, `_`"
        ));
    }
    if req.tools.is_empty()
        && req.hooks.is_empty()
        && req.commands.is_empty()
        && req.prompt.is_none()
        && !req.status
        && !req.skills
    {
        return Err(
            "an extension must contribute something: pass at least one of `tools`, `hooks`, \
             `commands`, `prompt`, `status`, or `skills`"
                .into(),
        );
    }
    for hook in &req.hooks {
        if hook.event != "pre_tool_use" && hook.event != "post_tool_use" {
            return Err(format!(
                "invalid hook event `{}`: use `pre_tool_use` or `post_tool_use`",
                hook.event
            ));
        }
    }
    Ok(())
}

/// Generate the package on disk. Refuses to clobber a non-empty directory so
/// in-progress edits are never lost.
pub fn scaffold(req: &ScaffoldRequest) -> Result<Scaffolded, String> {
    validate(req)?;
    if req.dir.is_dir()
        && std::fs::read_dir(&req.dir)
            .map(|mut d| d.next().is_some())
            .unwrap_or(false)
    {
        return Err(format!(
            "{} already exists and is not empty; pick a fresh `dir` or remove it",
            req.dir.display()
        ));
    }
    let bin_dir = req.dir.join("bin");
    std::fs::create_dir_all(&bin_dir)
        .map_err(|e| format!("creating {}: {e}", bin_dir.display()))?;

    let manifest = manifest_json(req);
    let manifest_path = req.dir.join("plugin.json");
    write(&manifest_path, &format!("{manifest:#}\n"))?;

    let server_name = format!("{}-server", req.name);
    let mut files = vec!["plugin.json".to_string(), "README.md".to_string()];

    let (edit, build) = match req.language {
        Language::Python | Language::Node => {
            // Zero-build: the executable server itself lives in bin/.
            let server_path = bin_dir.join(&server_name);
            let source = match req.language {
                Language::Python => python_server(req),
                Language::Node => node_server(req),
                Language::Rust => unreachable!(),
            };
            write(&server_path, &source)?;
            make_executable(&server_path)?;
            files.push(format!("bin/{server_name}"));
            (server_path, None)
        }
        Language::Rust => {
            // Compiled: emit a Cargo project. The binary is produced by a build
            // step and dropped into bin/ before install (see `build`).
            let src_dir = req.dir.join("src");
            std::fs::create_dir_all(&src_dir)
                .map_err(|e| format!("creating {}: {e}", src_dir.display()))?;
            let main_rs = src_dir.join("main.rs");
            write(&req.dir.join("Cargo.toml"), &rust_cargo_toml(req))?;
            write(&main_rs, &rust_main(req))?;
            write(&req.dir.join(".gitignore"), "/target\n")?;
            files.push("Cargo.toml".into());
            files.push("src/main.rs".into());
            files.push(".gitignore".into());
            let build = format!(
                "cargo build --release --manifest-path {cargo} && cp \
                 {dir}/target/release/{server_name} {dir}/bin/{server_name}",
                cargo = req.dir.join("Cargo.toml").display(),
                dir = req.dir.display(),
            );
            (main_rs, Some(build))
        }
    };

    write(
        &req.dir.join("README.md"),
        &readme(req, &server_name, &build),
    )?;

    if req.skills {
        // A starter skill under skills/<name>/SKILL.md. The host mounts the
        // whole skills/ dir read-only for an enabled, declaring extension.
        let skill_dir = req.dir.join("skills").join(&req.name);
        std::fs::create_dir_all(&skill_dir)
            .map_err(|e| format!("creating {}: {e}", skill_dir.display()))?;
        write(&skill_dir.join("SKILL.md"), &starter_skill(req))?;
        files.push(format!("skills/{}/SKILL.md", req.name));
    }

    Ok(Scaffolded {
        dir: req.dir.clone(),
        edit,
        files,
        build,
    })
}

fn starter_skill(req: &ScaffoldRequest) -> String {
    let desc = if req.description.is_empty() {
        "What this skill helps with (one line, used for matching)."
    } else {
        &req.description
    };
    format!(
        "---\nname: {name}\ndescription: {desc}\n---\n\n# {name}\n\n\
         TODO: write the instructions the agent should follow when this skill is \
         active.\n",
        name = req.name,
        desc = desc,
    )
}

fn manifest_json(req: &ScaffoldRequest) -> Value {
    let tools: Vec<Value> = req
        .tools
        .iter()
        .map(|t| {
            json!({
                "name": t.name,
                "description": t.description,
                "schema": { "type": "object" },
            })
        })
        .collect();
    let hooks: Vec<Value> = req
        .hooks
        .iter()
        .map(|h| json!({ "event": h.event, "tool_name_glob": h.tool_name_glob }))
        .collect();
    let mut yolop = json!({
        "protocol_version": "1.0",
        "capabilityServer": { "command": format!("{}-server", req.name) },
    });
    let obj = yolop.as_object_mut().expect("object");
    if !tools.is_empty() {
        obj.insert("tools".into(), Value::Array(tools));
    }
    if !hooks.is_empty() {
        obj.insert("hooks".into(), Value::Array(hooks));
    }
    if !req.commands.is_empty() {
        let commands: Vec<Value> = req
            .commands
            .iter()
            .map(|name| json!({ "name": name, "description": format!("The {name} command.") }))
            .collect();
        obj.insert("commands".into(), Value::Array(commands));
    }
    if req.prompt.is_some() {
        obj.insert("prompt".into(), Value::Bool(true));
    }
    if req.status {
        obj.insert("status".into(), Value::Bool(true));
    }
    if req.skills {
        obj.insert("skills".into(), Value::Bool(true));
    }
    json!({
        "name": req.name,
        "description": req.description,
        "version": "0.1.0",
        "yolop": yolop,
    })
}

/// The generated Python server: a data-driven YEP server whose only
/// author-editable seams are the three `handle_*` bodies. Everything else is
/// protocol plumbing that must not change.
fn python_server(req: &ScaffoldRequest) -> String {
    // A JSON array/string literal is also a valid Python literal, so serialize
    // through serde_json rather than Rust's `{:?}` (whose `\u{..}` escapes are
    // not valid Python) — this stays correct for arbitrary tool names / prompt.
    let names: Vec<&String> = req.tools.iter().map(|t| &t.name).collect();
    let tools_list = serde_json::to_string(&names).unwrap_or_else(|_| "[]".into());
    let commands_list = serde_json::to_string(&req.commands).unwrap_or_else(|_| "[]".into());
    let name_literal = serde_json::to_string(&req.name).unwrap_or_else(|_| "\"\"".into());
    let prompt_literal = match &req.prompt {
        Some(text) => serde_json::to_string(text).unwrap_or_else(|_| "\"\"".into()),
        None => "None".into(),
    };
    let hooks_cap = if req.hooks.is_empty() {
        ""
    } else {
        "\n            caps.append(\"hooks\")"
    };
    let status_cap = if req.status {
        "\n            caps.append(\"status\")"
    } else {
        ""
    };
    let commands_cap = if req.commands.is_empty() {
        ""
    } else {
        "\n            caps.append(\"commands\")"
    };
    format!(
        r##"#!/usr/bin/env python3
"""{name} — a yolop extension (YEP capability server).

Speaks the yolop extension protocol: newline-delimited JSON-RPC over stdio.
stdout carries ONLY protocol JSON — write any logs to stderr via log(). This
file has no third-party dependencies.

To author: edit the handle_* bodies below, then from yolop:
  install_extension source=<this package directory>
  doctor_extension  name={name}
  enable_extension  name={name}    # takes effect on the next session
"""

import json
import sys

NAME = {name_literal}
# Tool names this server serves. MUST match plugin.json's yolop.tools.
TOOLS = {tools_list}
# Slash-command names this server serves. MUST match plugin.json's yolop.commands.
COMMANDS = {commands_list}
# Static system-prompt contribution, or None.
PROMPT = {prompt_literal}


def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()


def log(msg):
    sys.stderr.write(str(msg) + "\n")
    sys.stderr.flush()


def emit_status(text):
    # Push a status-bar update to the host (requires "status" in plugin.json).
    # An empty string clears this extension's field.
    send({{"method": "status/changed", "params": {{"status": text}}}})


# --- author-editable handlers -------------------------------------------------

def handle_tool(name, args):
    """Return the tool-result dict for a `tool/call`.

    TODO: implement each tool in TOOLS. `args` is the model-supplied object.
    """
    return {{"ok": True, "tool": name, "args": args}}


def handle_hook(event, tool_name, args):
    """Decide a subscribed lifecycle event. Return {{}} to allow (unchanged),
    or {{"block": True, "reason": "..."}} to deny (pre_tool_use only).

    Example — block the shell tool when the command runs git:
        if event == "pre_tool_use" and tool_name in ("bash", "shell"):
            command = (args or {{}}).get("command", "")
            if "git" in command.split():
                return {{"block": True, "reason": "git is disabled by {name}"}}
    """
    return {{}}


def handle_prompt():
    """Dynamic system-prompt contribution (only if the manifest opts in)."""
    return {{"text": PROMPT or ""}}


def handle_command(name, arguments):
    """Run a slash command. Return {{"message": "..."}} to show the user (and
    optionally "success": false). `arguments` is the raw text after the command.

    TODO: implement each command in COMMANDS.
    """
    return {{"message": f"{{NAME}}:{{name}} ran with args: {{arguments!r}}"}}


# --- protocol plumbing (do not edit) -----------------------------------------

def main():
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            msg = json.loads(line)
        except json.JSONDecodeError:
            continue
        method = msg.get("method")
        msg_id = msg.get("id")
        if method == "initialize":
            caps = ["tools", "streaming"]
            params = {{"tools": [{{"name": t}} for t in TOOLS]}}
            if PROMPT is not None:
                caps.append("prompt")
                params["prompt"] = {{"static": PROMPT}}{hooks_cap}{status_cap}{commands_cap}
            send({{"id": msg_id, "result": {{
                "protocol_version": "1.0",
                "name": NAME,
                "capabilities": caps,
                "capability_params": params,
            }}}})
        elif method == "initialized":
            continue
        elif method == "tool/call":
            p = msg.get("params") or {{}}
            name = p.get("name")
            if name not in TOOLS:
                send({{"id": msg_id, "error": {{"message": f"no such tool: {{name}}"}}}})
                continue
            try:
                send({{"id": msg_id, "result": handle_tool(name, p.get("args") or {{}})}})
            except Exception as exc:  # a tool error must not crash the server
                send({{"id": msg_id, "error": {{"message": str(exc)}}}})
        elif method == "hook/fire":
            p = msg.get("params") or {{}}
            try:
                decision = handle_hook(
                    p.get("event", ""), p.get("tool_name", ""), p.get("args") or {{}})
                send({{"id": msg_id, "result": decision}})
            except Exception as exc:
                log(f"hook error: {{exc}}")
                send({{"id": msg_id, "result": {{}}}})  # fail open
        elif method == "prompt/contribution":
            send({{"id": msg_id, "result": handle_prompt()}})
        elif method == "command/execute":
            p = msg.get("params") or {{}}
            try:
                result = handle_command(p.get("name", ""), p.get("arguments", ""))
                send({{"id": msg_id, "result": result}})
            except Exception as exc:
                send({{"id": msg_id, "result": {{"success": False, "message": str(exc)}}}})
        elif method == "shutdown":
            send({{"id": msg_id, "result": {{}}}})
            return
        elif msg_id is not None:
            send({{"id": msg_id, "error": {{
                "code": -32601, "message": f"method not found: {{method}}"}}}})


if __name__ == "__main__":
    main()
"##,
        name = req.name,
        name_literal = name_literal,
        tools_list = tools_list,
        prompt_literal = prompt_literal,
        hooks_cap = hooks_cap,
        status_cap = status_cap,
    )
}

/// The generated Node.js server: the JavaScript twin of `python_server`.
/// Dependency-free (only the `readline` builtin) and runs on any Node with no
/// build step. Author-editable seams are the three `handle*` bodies.
fn node_server(req: &ScaffoldRequest) -> String {
    // JSON literals are valid JavaScript literals, so serde_json output drops
    // straight into the source.
    let names: Vec<&String> = req.tools.iter().map(|t| &t.name).collect();
    let tools_list = serde_json::to_string(&names).unwrap_or_else(|_| "[]".into());
    let commands_list = serde_json::to_string(&req.commands).unwrap_or_else(|_| "[]".into());
    let name_literal = serde_json::to_string(&req.name).unwrap_or_else(|_| "\"\"".into());
    let prompt_literal = match &req.prompt {
        Some(text) => serde_json::to_string(text).unwrap_or_else(|_| "null".into()),
        None => "null".into(),
    };
    let hooks_cap = if req.hooks.is_empty() {
        ""
    } else {
        "\n    caps.push(\"hooks\");"
    };
    let status_cap = if req.status {
        "\n    caps.push(\"status\");"
    } else {
        ""
    };
    let commands_cap = if req.commands.is_empty() {
        ""
    } else {
        "\n    caps.push(\"commands\");"
    };
    format!(
        r##"#!/usr/bin/env node
// {name} — a yolop extension (YEP capability server).
//
// Speaks the yolop extension protocol: newline-delimited JSON-RPC over stdio.
// stdout carries ONLY protocol JSON — write any logs to stderr via log(). No
// third-party dependencies.
//
// To author: edit the handle* bodies below, then from yolop:
//   install_extension source=<this package directory>
//   doctor_extension  name={name}
//   enable_extension  name={name}    // takes effect on the next session

const readline = require("readline");

const NAME = {name_literal};
// Tool names this server serves. MUST match plugin.json's yolop.tools.
const TOOLS = {tools_list};
// Slash-command names this server serves. MUST match plugin.json's yolop.commands.
const COMMANDS = {commands_list};
// Static system-prompt contribution, or null.
const PROMPT = {prompt_literal};

function send(obj) {{
  process.stdout.write(JSON.stringify(obj) + "\n");
}}

function log(msg) {{
  process.stderr.write(String(msg) + "\n");
}}

function emitStatus(text) {{
  // Push a status-bar update to the host (requires "status" in plugin.json).
  // An empty string clears this extension's field.
  send({{ method: "status/changed", params: {{ status: text }} }});
}}

// --- author-editable handlers ------------------------------------------------

function handleTool(name, args) {{
  // TODO: implement each tool in TOOLS. `args` is the model-supplied object.
  return {{ ok: true, tool: name, args: args }};
}}

function handleHook(event, toolName, args) {{
  // Return {{}} to allow (unchanged), or {{ block: true, reason: "..." }} to deny
  // (pre_tool_use only). The server sees every subscribed tool call.
  //
  // Example — block the shell tool when the command runs git:
  //   if (event === "pre_tool_use" && (toolName === "bash" || toolName === "shell")) {{
  //     const command = (args || {{}}).command || "";
  //     if (command.split(/\s+/).includes("git")) {{
  //       return {{ block: true, reason: "git is disabled by {name}" }};
  //     }}
  //   }}
  return {{}};
}}

function handlePrompt() {{
  return {{ text: PROMPT || "" }};
}}

function handleCommand(name, args) {{
  // Run a slash command. Return {{ message: "..." }} to show the user (and
  // optionally success: false). `args` is the raw text after the command.
  return {{ message: NAME + ":" + name + " ran with args: " + JSON.stringify(args) }};
}}

// --- protocol plumbing (do not edit) -----------------------------------------

const rl = readline.createInterface({{ input: process.stdin }});
rl.on("line", (raw) => {{
  const line = raw.trim();
  if (!line) return;
  let msg;
  try {{
    msg = JSON.parse(line);
  }} catch (_e) {{
    return;
  }}
  const method = msg.method;
  const id = msg.id;
  if (method === "initialize") {{
    const caps = ["tools", "streaming"];
    const params = {{ tools: TOOLS.map((t) => ({{ name: t }})) }};
    if (PROMPT !== null) {{
      caps.push("prompt");
      params.prompt = {{ static: PROMPT }};
    }}{hooks_cap}{status_cap}{commands_cap}
    send({{ id: id, result: {{
      protocol_version: "1.0",
      name: NAME,
      capabilities: caps,
      capability_params: params,
    }} }});
  }} else if (method === "initialized") {{
    // no-op
  }} else if (method === "tool/call") {{
    const p = msg.params || {{}};
    if (!TOOLS.includes(p.name)) {{
      send({{ id: id, error: {{ message: "no such tool: " + p.name }} }});
      return;
    }}
    try {{
      send({{ id: id, result: handleTool(p.name, p.args || {{}}) }});
    }} catch (e) {{
      send({{ id: id, error: {{ message: String(e) }} }});
    }}
  }} else if (method === "hook/fire") {{
    const p = msg.params || {{}};
    try {{
      send({{ id: id, result: handleHook(p.event || "", p.tool_name || "", p.args || {{}}) }});
    }} catch (e) {{
      log("hook error: " + e);
      send({{ id: id, result: {{}} }});  // fail open
    }}
  }} else if (method === "prompt/contribution") {{
    send({{ id: id, result: handlePrompt() }});
  }} else if (method === "command/execute") {{
    const p = msg.params || {{}};
    try {{
      send({{ id: id, result: handleCommand(p.name || "", p.arguments || "") }});
    }} catch (e) {{
      send({{ id: id, result: {{ success: false, message: String(e) }} }});
    }}
  }} else if (method === "shutdown") {{
    send({{ id: id, result: {{}} }});
    rl.close();
    process.exit(0);
  }} else if (id !== undefined && id !== null) {{
    send({{ id: id, error: {{ code: -32601, message: "method not found: " + method }} }});
  }}
}});
"##,
        name = req.name,
        name_literal = name_literal,
        tools_list = tools_list,
        commands_list = commands_list,
        prompt_literal = prompt_literal,
        hooks_cap = hooks_cap,
        status_cap = status_cap,
        commands_cap = commands_cap,
    )
}

fn readme(req: &ScaffoldRequest, server_name: &str, build: &Option<String>) -> String {
    let (layout, edit, build_block) = match build {
        // Zero-build templates: the executable server is bin/<name>-server.
        None => (
            format!(
                "- `bin/{server_name}` — the capability server (stdio JSON-RPC). yolop puts \
                 `bin/` on `PATH`, so `capabilityServer.command` resolves here."
            ),
            format!("Edit the handler bodies in `bin/{server_name}`"),
            String::new(),
        ),
        // Rust: build the binary into bin/ before install.
        Some(cmd) => (
            "- `Cargo.toml` / `src/main.rs` — the capability server source.\n\
             - `bin/<name>-server` — the built binary (produced by the build step below)."
                .to_string(),
            "Edit the `handle_*` bodies in `src/main.rs`".to_string(),
            format!("Build the server binary into `bin/` first:\n\n```\n{cmd}\n```\n\n"),
        ),
    };
    format!(
        "# {name}\n\n\
         {desc}\n\n\
         A [yolop](https://crates.io/crates/yolop) extension (YEP capability server).\n\n\
         ## Layout\n\n\
         - `plugin.json` — the manifest: the contributions yolop approves at install.\n\
         {layout}\n\n\
         ## Author\n\n\
         {edit}, then from yolop:\n\n\
         {build_block}\
         ```\n\
         install_extension source=<this directory>\n\
         doctor_extension  name={name}\n\
         enable_extension  name={name}\n\
         ```\n\n\
         Enabling takes effect on the next session.\n",
        name = req.name,
        desc = req.description,
    )
}

/// The generated Cargo manifest: a standalone crate whose only dependency is
/// `serde_json`, with the binary named `<name>-server` so it lands in `bin/`.
fn rust_cargo_toml(req: &ScaffoldRequest) -> String {
    format!(
        "[package]\n\
         name = {pkg:?}\n\
         version = \"0.1.0\"\n\
         edition = \"2021\"\n\n\
         [[bin]]\n\
         name = \"{name}-server\"\n\
         path = \"src/main.rs\"\n\n\
         [dependencies]\n\
         serde_json = \"1\"\n",
        pkg = req.name,
        name = req.name,
    )
}

/// The generated Rust server: a raw newline-delimited JSON-RPC server over
/// stdio with only `serde_json` as a dependency — the compiled twin of the
/// Python/Node templates. Author-editable seams are the three `handle_*`
/// functions.
fn rust_main(req: &ScaffoldRequest) -> String {
    let names: Vec<&String> = req.tools.iter().map(|t| &t.name).collect();
    let tools_list = names
        .iter()
        .map(|n| format!("{n:?}"))
        .collect::<Vec<_>>()
        .join(", ");
    let commands_list = req
        .commands
        .iter()
        .map(|n| format!("{n:?}"))
        .collect::<Vec<_>>()
        .join(", ");
    let name_literal = format!("{:?}", req.name);
    let prompt_literal = match &req.prompt {
        Some(text) => format!("Some({text:?})"),
        None => "None".into(),
    };
    let hooks_cap = if req.hooks.is_empty() {
        ""
    } else {
        "\n            caps.push(json!(\"hooks\"));"
    };
    let commands_cap = if req.commands.is_empty() {
        ""
    } else {
        "\n            caps.push(json!(\"commands\"));"
    };
    format!(
        r##"//! {name} — a yolop extension (YEP capability server).
//!
//! Speaks the yolop extension protocol: newline-delimited JSON-RPC over stdio.
//! stdout carries ONLY protocol JSON — write any logs to stderr. Depends only
//! on `serde_json`.
//!
//! To author: edit the handle_* bodies below, then from yolop:
//!   cargo build --release   # then copy target/release/{name}-server to bin/
//!   install_extension source=<this package directory>
//!   doctor_extension  name={name}
//!   enable_extension  name={name}    // takes effect on the next session

use serde_json::{{Value, json}};
use std::io::{{BufRead, Write}};

const NAME: &str = {name_literal};
// Tool names this server serves. MUST match plugin.json's yolop.tools.
const TOOLS: &[&str] = &[{tools_list}];
// Slash-command names this server serves. MUST match plugin.json's yolop.commands.
const COMMANDS: &[&str] = &[{commands_list}];
// Static system-prompt contribution, or None.
const PROMPT: Option<&str> = {prompt_literal};

fn send(obj: &Value) {{
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{{obj}}");
    let _ = out.flush();
}}

// --- author-editable handlers -----------------------------------------------

fn handle_tool(name: &str, args: &Value) -> Value {{
    // TODO: implement each tool in TOOLS. `args` is the model-supplied object.
    json!({{ "ok": true, "tool": name, "args": args }})
}}

fn handle_hook(event: &str, tool_name: &str, args: &Value) -> Value {{
    // Return {{}} to allow (unchanged), or {{"block": true, "reason": "..."}} to
    // deny (pre_tool_use only). The server sees every subscribed tool call.
    //
    // Example — block the shell tool when the command runs git:
    //   if event == "pre_tool_use" && (tool_name == "bash" || tool_name == "shell") {{
    //       let command = args.get("command").and_then(Value::as_str).unwrap_or("");
    //       if command.split_whitespace().any(|w| w == "git") {{
    //           return json!({{"block": true, "reason": "git is disabled by {name}"}});
    //       }}
    //   }}
    let _ = (event, tool_name, args);
    json!({{}})
}}

fn handle_prompt() -> Value {{
    json!({{ "text": PROMPT.unwrap_or("") }})
}}

fn handle_command(name: &str, arguments: &str) -> Value {{
    // Run a slash command. Return {{"message": "..."}} to show the user (and
    // optionally "success": false). `arguments` is the raw text after the command.
    let _ = COMMANDS;
    json!({{ "message": format!("{{NAME}}:{{name}} ran with args: {{arguments:?}}") }})
}}

// --- protocol plumbing (do not edit) ----------------------------------------

fn main() {{
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {{
        let Ok(line) = line else {{ break }};
        let line = line.trim();
        if line.is_empty() {{
            continue;
        }}
        let Ok(msg) = serde_json::from_str::<Value>(line) else {{
            continue;
        }};
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
        let id = msg.get("id").cloned().unwrap_or(Value::Null);
        match method {{
            "initialize" => {{
                let mut caps = vec![json!("tools"), json!("streaming")];
                let tools: Vec<Value> = TOOLS.iter().map(|t| json!({{ "name": t }})).collect();
                let mut params = json!({{ "tools": tools }});
                if let Some(p) = PROMPT {{
                    caps.push(json!("prompt"));
                    params["prompt"] = json!({{ "static": p }});
                }}{hooks_cap}{commands_cap}
                send(&json!({{ "id": id, "result": {{
                    "protocol_version": "1.0",
                    "name": NAME,
                    "capabilities": caps,
                    "capability_params": params,
                }} }}));
            }}
            "initialized" => {{}}
            "tool/call" => {{
                let p = msg.get("params").cloned().unwrap_or_else(|| json!({{}}));
                let tool = p.get("name").and_then(Value::as_str).unwrap_or("");
                if !TOOLS.contains(&tool) {{
                    send(&json!({{ "id": id, "error": {{ "message": format!("no such tool: {{tool}}") }} }}));
                    continue;
                }}
                let args = p.get("args").cloned().unwrap_or_else(|| json!({{}}));
                send(&json!({{ "id": id, "result": handle_tool(tool, &args) }}));
            }}
            "hook/fire" => {{
                let p = msg.get("params").cloned().unwrap_or_else(|| json!({{}}));
                let event = p.get("event").and_then(Value::as_str).unwrap_or("");
                let tool_name = p.get("tool_name").and_then(Value::as_str).unwrap_or("");
                let args = p.get("args").cloned().unwrap_or_else(|| json!({{}}));
                send(&json!({{ "id": id, "result": handle_hook(event, tool_name, &args) }}));
            }}
            "prompt/contribution" => send(&json!({{ "id": id, "result": handle_prompt() }})),
            "command/execute" => {{
                let p = msg.get("params").cloned().unwrap_or_else(|| json!({{}}));
                let name = p.get("name").and_then(Value::as_str).unwrap_or("");
                let arguments = p.get("arguments").and_then(Value::as_str).unwrap_or("");
                send(&json!({{ "id": id, "result": handle_command(name, arguments) }}));
            }}
            "shutdown" => {{
                send(&json!({{ "id": id, "result": {{}} }}));
                return;
            }}
            _ => {{
                if !id.is_null() {{
                    send(&json!({{ "id": id, "error": {{
                        "code": -32601, "message": format!("method not found: {{method}}") }} }}));
                }}
            }}
        }}
    }}
}}
"##,
        name = req.name,
        name_literal = name_literal,
        tools_list = tools_list,
        commands_list = commands_list,
        prompt_literal = prompt_literal,
        hooks_cap = hooks_cap,
        commands_cap = commands_cap,
    )
}

fn write(path: &Path, contents: &str) -> Result<(), String> {
    std::fs::write(path, contents).map_err(|e| format!("writing {}: {e}", path.display()))
}

#[cfg(unix)]
fn make_executable(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path)
        .map_err(|e| format!("stat {}: {e}", path.display()))?
        .permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms).map_err(|e| format!("chmod {}: {e}", path.display()))
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> Result<(), String> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extensions::package::parse_manifest;

    fn req(dir: PathBuf) -> ScaffoldRequest {
        ScaffoldRequest {
            name: "git-guard".into(),
            description: "Blocks git.".into(),
            language: Language::Python,
            tools: vec![ToolSpec {
                name: "note".into(),
                description: "Record a note.".into(),
            }],
            hooks: vec![HookSpec {
                event: "pre_tool_use".into(),
                tool_name_glob: "*".into(),
            }],
            commands: Vec::new(),
            prompt: Some("Guarding git.".into()),
            status: false,
            skills: false,
            dir,
        }
    }

    #[test]
    fn generates_a_package_the_installer_accepts() {
        let tmp = tempfile::tempdir().unwrap();
        let out = scaffold(&req(tmp.path().join("git-guard"))).unwrap();

        // Manifest parses under the real installer path, with the declared
        // contributions clamped in.
        let manifest_src = std::fs::read_to_string(out.dir.join("plugin.json")).unwrap();
        let manifest = parse_manifest(&manifest_src).expect("manifest parses");
        assert_eq!(manifest.name, "git-guard");
        assert_eq!(manifest.capability_server.command, "git-guard-server");
        assert_eq!(manifest.tools.len(), 1);
        assert_eq!(manifest.hooks.len(), 1);
        assert!(manifest.prompt);

        // Server exists, carries the shebang, and serves exactly the tool.
        let server = std::fs::read_to_string(&out.edit).unwrap();
        assert!(server.starts_with("#!/usr/bin/env python3"));
        assert!(server.contains(r#"TOOLS = ["note"]"#));
        assert!(out.build.is_none());
        assert_eq!(out.files.len(), 3);
    }

    #[cfg(unix)]
    #[test]
    fn server_is_executable() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let out = scaffold(&req(tmp.path().join("git-guard"))).unwrap();
        let mode = std::fs::metadata(&out.edit).unwrap().permissions().mode();
        assert_eq!(mode & 0o111, 0o111, "server must be executable");
    }

    #[test]
    fn generates_a_rust_crate_the_installer_accepts() {
        let tmp = tempfile::tempdir().unwrap();
        let mut r = req(tmp.path().join("git-guard"));
        r.language = Language::Rust;
        let out = scaffold(&r).unwrap();

        // Manifest still parses; the command names the built binary.
        let manifest_src = std::fs::read_to_string(out.dir.join("plugin.json")).unwrap();
        let manifest = parse_manifest(&manifest_src).expect("manifest parses");
        assert_eq!(manifest.capability_server.command, "git-guard-server");

        // A Cargo project is emitted with a build step; the edit target is the
        // source, not a bin/ script.
        assert!(out.edit.ends_with("src/main.rs"));
        assert!(
            out.build
                .as_ref()
                .unwrap()
                .contains("cargo build --release")
        );
        let cargo = std::fs::read_to_string(out.dir.join("Cargo.toml")).unwrap();
        assert!(cargo.contains(r#"name = "git-guard-server""#));
        assert!(cargo.contains("serde_json"));
        let main = std::fs::read_to_string(&out.edit).unwrap();
        assert!(main.contains("fn handle_hook"));
        assert!(main.contains(r#"const TOOLS: &[&str] = &["note"];"#));
    }

    #[test]
    fn refuses_empty_contributions() {
        let tmp = tempfile::tempdir().unwrap();
        let mut r = req(tmp.path().join("x"));
        r.tools.clear();
        r.hooks.clear();
        r.prompt = None;
        assert!(scaffold(&r).unwrap_err().contains("contribute"));
    }

    #[test]
    fn status_facet_scaffolds_manifest_flag_and_helper() {
        let tmp = tempfile::tempdir().unwrap();
        let mut r = req(tmp.path().join("counter"));
        r.status = true;
        let out = scaffold(&r).unwrap();

        let manifest =
            parse_manifest(&std::fs::read_to_string(out.dir.join("plugin.json")).unwrap()).unwrap();
        assert!(manifest.status, "manifest declares the status facet");

        let server = std::fs::read_to_string(&out.edit).unwrap();
        assert!(
            server.contains("def emit_status"),
            "emit_status helper present"
        );
        assert!(server.contains(r#"caps.append("status")"#));
    }

    #[test]
    fn status_alone_is_a_valid_contribution() {
        let tmp = tempfile::tempdir().unwrap();
        let mut r = req(tmp.path().join("s"));
        r.tools.clear();
        r.hooks.clear();
        r.prompt = None;
        r.status = true;
        assert!(scaffold(&r).is_ok());
    }

    #[test]
    fn skills_facet_scaffolds_manifest_flag_and_starter_skill() {
        let tmp = tempfile::tempdir().unwrap();
        let mut r = req(tmp.path().join("git-guard"));
        r.skills = true;
        let out = scaffold(&r).unwrap();

        let manifest =
            parse_manifest(&std::fs::read_to_string(out.dir.join("plugin.json")).unwrap()).unwrap();
        assert!(manifest.skills, "manifest declares the skills facet");

        let skill = std::fs::read_to_string(out.dir.join("skills/git-guard/SKILL.md")).unwrap();
        assert!(skill.contains("name: git-guard"));
        assert!(out.files.iter().any(|f| f.contains("SKILL.md")));
    }

    #[test]
    fn skills_alone_is_a_valid_contribution() {
        let tmp = tempfile::tempdir().unwrap();
        let mut r = req(tmp.path().join("packof"));
        r.tools.clear();
        r.hooks.clear();
        r.prompt = None;
        r.skills = true;
        assert!(scaffold(&r).is_ok());
    }

    #[test]
    fn commands_facet_scaffolds_manifest_and_dispatch() {
        for (lang, needle) in [
            (Language::Python, "def handle_command"),
            (Language::Node, "function handleCommand"),
            (Language::Rust, "fn handle_command"),
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let mut r = req(tmp.path().join("git-guard"));
            r.language = lang;
            r.commands = vec!["review".into()];
            let out = scaffold(&r).unwrap();

            let manifest =
                parse_manifest(&std::fs::read_to_string(out.dir.join("plugin.json")).unwrap())
                    .unwrap();
            assert_eq!(manifest.commands.len(), 1, "{lang:?}");
            assert_eq!(manifest.commands[0].name, "review");

            let server = std::fs::read_to_string(&out.edit).unwrap();
            assert!(
                server.contains(needle),
                "{lang:?} missing handler: {needle}"
            );
            assert!(
                server.contains("command/execute"),
                "{lang:?} missing dispatch"
            );
        }
    }

    #[test]
    fn refuses_bad_name() {
        let tmp = tempfile::tempdir().unwrap();
        let mut r = req(tmp.path().join("x"));
        r.name = "bad name!".into();
        assert!(scaffold(&r).unwrap_err().contains("invalid extension name"));
    }

    #[test]
    fn wont_clobber_a_nonempty_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("git-guard");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("keep.txt"), "mine").unwrap();
        assert!(scaffold(&req(dir)).unwrap_err().contains("not empty"));
    }

    #[test]
    fn language_parsing() {
        assert_eq!(Language::parse("python").unwrap(), Language::Python);
        assert_eq!(Language::parse("py").unwrap(), Language::Python);
        assert_eq!(Language::parse("typescript").unwrap(), Language::Node);
        assert_eq!(Language::parse("ts").unwrap(), Language::Node);
        assert_eq!(Language::parse("node").unwrap(), Language::Node);
        assert_eq!(Language::parse("rust").unwrap(), Language::Rust);
        assert_eq!(Language::parse("rs").unwrap(), Language::Rust);
        assert!(
            Language::parse("cobol")
                .unwrap_err()
                .contains("unknown language")
        );
    }

    #[test]
    fn generates_a_node_package_the_installer_accepts() {
        let tmp = tempfile::tempdir().unwrap();
        let mut r = req(tmp.path().join("git-guard"));
        r.language = Language::Node;
        let out = scaffold(&r).unwrap();

        let manifest_src = std::fs::read_to_string(out.dir.join("plugin.json")).unwrap();
        parse_manifest(&manifest_src).expect("manifest parses");

        let server = std::fs::read_to_string(&out.edit).unwrap();
        assert!(server.starts_with("#!/usr/bin/env node"));
        assert!(server.contains(r#"const TOOLS = ["note"];"#));
        assert!(server.contains("function handleHook"));
    }
}

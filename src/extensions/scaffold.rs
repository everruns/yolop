//! Extension scaffolding: generate a ready-to-edit YEP extension package so
//! the agent (or a human) can author an extension end-to-end without
//! remembering the wire protocol or the package layout.
//!
//! The generated package is **correct by construction** — it installs via
//! `install_extension source=<dir>` and passes `doctor_extension` out of the
//! box — so authoring collapses to "fill in the handler bodies." The server is
//! a single self-contained executable under `bin/`, which the runtime resolves
//! by prepending `<package>/bin` to `PATH` (see `manager.rs`); the exec bit
//! survives install because `store::copy_dir` uses `std::fs::copy`.
//!
//! Language templates are pluggable. Python lands first because it is
//! toolchain-free (no build step), the fastest and most reliable path for a
//! self-writing loop; Rust (`yolop-yep`) and TypeScript follow.

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
}

impl Language {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "python" | "py" => Ok(Self::Python),
            "typescript" | "ts" | "javascript" | "js" | "node" => Ok(Self::Node),
            "rust" | "rs" => Err(format!(
                "`{s}` needs a compiled template (build before run); use `python` or \
                 `typescript` for the toolchain-free path for now"
            )),
            other => Err(format!(
                "unknown language `{other}`; use `python` or `typescript`"
            )),
        }
    }

    /// The interpreter the generated launcher shebang invokes — used to skip
    /// tests when the runtime isn't installed.
    #[cfg(test)]
    pub fn interpreter(self) -> &'static str {
        match self {
            Self::Python => "python3",
            Self::Node => "node",
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
    pub prompt: Option<String>,
    /// Absolute path of the package directory to create.
    pub dir: PathBuf,
}

/// Outcome of a scaffold: what was written and where.
#[derive(Debug)]
pub struct Scaffolded {
    pub dir: PathBuf,
    pub server: PathBuf,
    pub files: Vec<String>,
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
    if req.tools.is_empty() && req.hooks.is_empty() && req.prompt.is_none() {
        return Err(
            "an extension must contribute something: pass at least one of `tools`, `hooks`, \
             or `prompt`"
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
    let server_path = bin_dir.join(&server_name);
    match req.language {
        Language::Python => write(&server_path, &python_server(req))?,
        Language::Node => write(&server_path, &node_server(req))?,
    }
    make_executable(&server_path)?;

    let readme_path = req.dir.join("README.md");
    write(&readme_path, &readme(req, &server_name))?;

    Ok(Scaffolded {
        dir: req.dir.clone(),
        server: server_path,
        files: vec![
            "plugin.json".into(),
            format!("bin/{server_name}"),
            "README.md".into(),
        ],
    })
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
    if req.prompt.is_some() {
        obj.insert("prompt".into(), Value::Bool(true));
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
    let name_literal = serde_json::to_string(&req.name).unwrap_or_else(|_| "\"\"".into());
    let prompt_literal = match &req.prompt {
        Some(text) => serde_json::to_string(text).unwrap_or_else(|_| "\"\"".into()),
        None => "None".into(),
    };
    let has_hooks = !req.hooks.is_empty();
    let hooks_cap = if has_hooks {
        "\n            caps.append(\"hooks\")"
    } else {
        ""
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
# Static system-prompt contribution, or None.
PROMPT = {prompt_literal}


def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()


def log(msg):
    sys.stderr.write(str(msg) + "\n")
    sys.stderr.flush()


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
                params["prompt"] = {{"static": PROMPT}}{hooks_cap}
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
// Static system-prompt contribution, or null.
const PROMPT = {prompt_literal};

function send(obj) {{
  process.stdout.write(JSON.stringify(obj) + "\n");
}}

function log(msg) {{
  process.stderr.write(String(msg) + "\n");
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
    }}{hooks_cap}
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
        prompt_literal = prompt_literal,
        hooks_cap = hooks_cap,
    )
}

fn readme(req: &ScaffoldRequest, server_name: &str) -> String {
    format!(
        "# {name}\n\n\
         {desc}\n\n\
         A [yolop](https://crates.io/crates/yolop) extension (YEP capability server).\n\n\
         ## Layout\n\n\
         - `plugin.json` — the manifest: the contributions yolop approves at install.\n\
         - `bin/{server}` — the capability server (stdio JSON-RPC). yolop puts `bin/` on\n  \
         `PATH`, so `capabilityServer.command` resolves here.\n\n\
         ## Author\n\n\
         Edit the `handle_*` bodies in `bin/{server}`, then from yolop:\n\n\
         ```\n\
         install_extension source=<this directory>\n\
         doctor_extension  name={name}\n\
         enable_extension  name={name}\n\
         ```\n\n\
         Enabling takes effect on the next session.\n",
        name = req.name,
        desc = req.description,
        server = server_name,
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
            prompt: Some("Guarding git.".into()),
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
        let server = std::fs::read_to_string(&out.server).unwrap();
        assert!(server.starts_with("#!/usr/bin/env python3"));
        assert!(server.contains(r#"TOOLS = ["note"]"#));
        assert_eq!(out.files.len(), 3);
    }

    #[cfg(unix)]
    #[test]
    fn server_is_executable() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let out = scaffold(&req(tmp.path().join("git-guard"))).unwrap();
        let mode = std::fs::metadata(&out.server).unwrap().permissions().mode();
        assert_eq!(mode & 0o111, 0o111, "server must be executable");
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
        assert!(
            Language::parse("rust")
                .unwrap_err()
                .contains("compiled template")
        );
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

        let server = std::fs::read_to_string(&out.server).unwrap();
        assert!(server.starts_with("#!/usr/bin/env node"));
        assert!(server.contains(r#"const TOOLS = ["note"];"#));
        assert!(server.contains("function handleHook"));
    }
}

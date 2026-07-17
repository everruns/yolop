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

/// Which language template to emit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    Python,
}

impl Language {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "python" | "py" => Ok(Self::Python),
            "rust" | "rs" | "typescript" | "ts" | "javascript" | "js" | "node" => Err(format!(
                "`{s}` templates are not available yet; use `python` (toolchain-free) for now"
            )),
            other => Err(format!("unknown language `{other}`; use `python`")),
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
    let tools_list = req
        .tools
        .iter()
        .map(|t| format!("{:?}", t.name))
        .collect::<Vec<_>>()
        .join(", ");
    let prompt_literal = match &req.prompt {
        Some(text) => format!("{text:?}"),
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

NAME = {name:?}
# Tool names this server serves. MUST match plugin.json's yolop.tools.
TOOLS = [{tools_list}]
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
    fn unbuilt_languages_report_clearly() {
        assert!(
            Language::parse("rust")
                .unwrap_err()
                .contains("not available yet")
        );
        assert!(
            Language::parse("typescript")
                .unwrap_err()
                .contains("not available yet")
        );
        assert!(
            Language::parse("cobol")
                .unwrap_err()
                .contains("unknown language")
        );
    }
}

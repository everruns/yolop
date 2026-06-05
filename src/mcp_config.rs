//! Load MCP servers from `.mcp.json` (workspace + global, merged).
//!
//! The file shape matches the `mcpServers` object every MCP client
//! understands, so a project's `.mcp.json` works across tools:
//!
//! ```json
//! {
//!   "mcpServers": {
//!     "docs": { "type": "http", "url": "https://example.com/mcp",
//!               "headers": { "Authorization": "Bearer ${DOCS_TOKEN}" } },
//!     "fs":   { "type": "stdio", "command": "mcp-server-filesystem",
//!               "args": ["${WORKSPACE}"] }
//!   }
//! }
//! ```
//!
//! Two scopes are read and merged (global first, workspace second, so a
//! project can override a global server of the same name):
//!
//! - **global**: `<config_dir>/yolop/mcp.json` (e.g. `~/.config/yolop/mcp.json`)
//! - **workspace**: `<workspace_root>/.mcp.json`
//!
//! Each scope is best-effort: a missing file contributes nothing, and a
//! malformed file is warned about and skipped so it cannot mask the other
//! scope. String values support `${VAR}` environment expansion (matching how
//! every other MCP client lets you keep secrets out of the file). Both remote
//! HTTP and local stdio servers are supported; stdio requires the runtime's
//! `mcp-stdio` feature, which yolop enables.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use everruns_core::{ScopedMcpServer, ScopedMcpServers, merge_scoped_mcp_servers};
use serde::Deserialize;

/// File name read from the workspace root.
pub const MCP_CONFIG_FILE: &str = ".mcp.json";

#[derive(Debug, Deserialize)]
struct McpConfigFile {
    #[serde(default, rename = "mcpServers")]
    mcp_servers: ScopedMcpServers,
}

/// Path to the global MCP config (`<config_dir>/yolop/mcp.json`), if a config
/// directory can be resolved on this platform.
pub fn global_mcp_config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|p| p.join("yolop").join("mcp.json"))
}

/// Load the effective scoped MCP servers for a workspace: the global config
/// overlaid by the workspace `.mcp.json`, with `${VAR}` placeholders expanded
/// from the environment. Each scope is best-effort (a malformed or unreadable
/// file is warned about and skipped), so one bad file never masks the other.
pub fn load_mcp_servers(workspace_root: &Path) -> ScopedMcpServers {
    load_merged(global_mcp_config_path().as_deref(), workspace_root)
}

/// Merge an explicit global path (or none) with the workspace `.mcp.json`.
/// Split out from [`load_mcp_servers`] so tests can pin both scopes and stay
/// independent of the host's real `<config_dir>/yolop/mcp.json`.
fn load_merged(global_path: Option<&Path>, workspace_root: &Path) -> ScopedMcpServers {
    let global = global_path.map(read_scope).unwrap_or_default();
    let workspace = read_scope(&workspace_root.join(MCP_CONFIG_FILE));
    let mut merged = merge_scoped_mcp_servers(&global, &workspace);
    for server in merged.values_mut() {
        expand_server_env(server);
    }
    merged
}

/// Read one scope, downgrading any read/parse failure to a warning + no
/// servers so a malformed file in one scope cannot sink the other.
fn read_scope(path: &Path) -> ScopedMcpServers {
    match read_config(path) {
        Ok(servers) => servers,
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "ignoring malformed MCP config");
            ScopedMcpServers::default()
        }
    }
}

/// Read one `.mcp.json`-shaped file. Absent file → no servers; malformed file
/// → error (callers decide whether to skip it).
fn read_config(path: &Path) -> Result<ScopedMcpServers> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ScopedMcpServers::default());
        }
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    };
    let config: McpConfigFile =
        serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
    Ok(config.mcp_servers)
}

/// Expand `${VAR}` references in every string field of a scoped server, so
/// secrets and paths can come from the environment instead of the file.
fn expand_server_env(server: &mut ScopedMcpServer) {
    server.url = expand_env(&server.url);
    server.command = server.command.as_deref().map(expand_env);
    for arg in &mut server.args {
        *arg = expand_env(arg);
    }
    replace_values(&mut server.headers);
    replace_values(&mut server.env);
}

fn replace_values(map: &mut HashMap<String, String>) {
    for value in map.values_mut() {
        *value = expand_env(value);
    }
}

/// Replace `${VAR}` occurrences with the value of `VAR` from the environment.
/// Unset variables are left untouched (so a stray placeholder is visible in
/// `/mcp` output and discovery errors rather than silently blanked).
fn expand_env(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        match after.find('}') {
            Some(end) => {
                let name = &after[..end];
                match std::env::var(name) {
                    Ok(value) => out.push_str(&value),
                    Err(_) => {
                        // Leave the original `${VAR}` so the gap is debuggable.
                        out.push_str(&rest[start..start + 2 + end + 1]);
                    }
                }
                rest = &after[end + 1..];
            }
            None => {
                // No closing brace: emit the rest verbatim.
                out.push_str(&rest[start..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use everruns_core::McpServerTransportType;

    fn write(dir: &Path, contents: &str) {
        std::fs::write(dir.join(MCP_CONFIG_FILE), contents).unwrap();
    }

    /// Workspace-only load with no global scope — deterministic regardless of
    /// the host's real `<config_dir>/yolop/mcp.json`.
    fn load_workspace(dir: &Path) -> ScopedMcpServers {
        load_merged(None, dir)
    }

    #[test]
    fn missing_file_yields_no_servers() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_workspace(dir.path()).is_empty());
    }

    #[test]
    fn parses_http_server_with_headers() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            r#"{ "mcpServers": {
                "docs": {
                    "type": "http",
                    "url": "https://example.com/mcp",
                    "headers": { "Authorization": "Bearer t0ken" }
                }
            }}"#,
        );

        let servers = load_workspace(dir.path());
        let docs = servers.get("docs").expect("docs server");
        assert_eq!(docs.transport_type, McpServerTransportType::Http);
        assert_eq!(docs.url, "https://example.com/mcp");
        assert_eq!(
            docs.headers.get("Authorization").map(String::as_str),
            Some("Bearer t0ken")
        );
        assert!(docs.tool_discovery, "tool discovery defaults on");
    }

    #[test]
    fn type_defaults_to_http() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            r#"{ "mcpServers": { "b": { "url": "https://b.example.com/mcp" } } }"#,
        );
        let servers = load_workspace(dir.path());
        assert_eq!(servers["b"].transport_type, McpServerTransportType::Http);
    }

    #[test]
    fn empty_object_is_ok() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "{}");
        assert!(load_workspace(dir.path()).is_empty());
    }

    #[test]
    fn malformed_json_is_an_error_at_the_scope_level() {
        // `read_config` surfaces the parse error...
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "{ not json");
        assert!(read_config(&dir.path().join(MCP_CONFIG_FILE)).is_err());
    }

    #[test]
    fn malformed_scope_is_skipped_not_fatal() {
        // ...but a malformed file in one scope must not mask the other. A bad
        // global config still lets a valid workspace `.mcp.json` load.
        let global = tempfile::tempdir().unwrap();
        let global_path = global.path().join("mcp.json");
        std::fs::write(&global_path, "{ not json").unwrap();

        let workspace = tempfile::tempdir().unwrap();
        write(
            workspace.path(),
            r#"{ "mcpServers": { "ws": { "url": "https://ws.example.com/mcp" } } }"#,
        );

        let servers = load_merged(Some(&global_path), workspace.path());
        assert!(
            servers.contains_key("ws"),
            "valid workspace server survives a malformed global config: {servers:?}"
        );
    }

    #[test]
    fn parses_stdio_server() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            r#"{ "mcpServers": {
                "fs": {
                    "type": "stdio",
                    "command": "mcp-server-filesystem",
                    "args": ["/work"],
                    "env": { "RUST_LOG": "info" }
                }
            }}"#,
        );

        let servers = load_workspace(dir.path());
        let fs = servers.get("fs").expect("fs server");
        assert_eq!(fs.transport_type, McpServerTransportType::Stdio);
        assert_eq!(fs.command.as_deref(), Some("mcp-server-filesystem"));
        assert_eq!(fs.args, vec!["/work".to_string()]);
        assert_eq!(fs.env.get("RUST_LOG").map(String::as_str), Some("info"));
    }

    #[test]
    fn expands_env_placeholders() {
        // Serialize against every other env-mutating test in this binary.
        let _guard = crate::test_env::lock();
        unsafe {
            std::env::set_var("YOLOP_TEST_MCP_TOKEN", "s3cret");
            std::env::set_var("YOLOP_TEST_MCP_ROOT", "/srv/work");
        }
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            r#"{ "mcpServers": {
                "docs": {
                    "type": "http",
                    "url": "https://example.com/mcp",
                    "headers": { "Authorization": "Bearer ${YOLOP_TEST_MCP_TOKEN}" }
                },
                "fs": {
                    "type": "stdio",
                    "command": "mcp-server-filesystem",
                    "args": ["${YOLOP_TEST_MCP_ROOT}"]
                }
            }}"#,
        );

        let servers = load_workspace(dir.path());
        assert_eq!(
            servers["docs"]
                .headers
                .get("Authorization")
                .map(String::as_str),
            Some("Bearer s3cret")
        );
        assert_eq!(servers["fs"].args, vec!["/srv/work".to_string()]);
        unsafe {
            std::env::remove_var("YOLOP_TEST_MCP_TOKEN");
            std::env::remove_var("YOLOP_TEST_MCP_ROOT");
        }
    }

    #[test]
    fn unset_env_placeholder_is_left_intact() {
        let _guard = crate::test_env::lock();
        unsafe {
            std::env::remove_var("YOLOP_UNSET_VAR_XYZ");
        }
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            r#"{ "mcpServers": {
                "docs": { "type": "http", "url": "https://example.com/${YOLOP_UNSET_VAR_XYZ}" }
            }}"#,
        );
        let servers = load_workspace(dir.path());
        assert_eq!(
            servers["docs"].url,
            "https://example.com/${YOLOP_UNSET_VAR_XYZ}"
        );
    }

    #[test]
    fn expand_env_handles_multiple_and_adjacent() {
        let _guard = crate::test_env::lock();
        unsafe {
            std::env::set_var("YOLOP_TEST_A", "a");
            std::env::set_var("YOLOP_TEST_B", "b");
        }
        assert_eq!(expand_env("${YOLOP_TEST_A}-${YOLOP_TEST_B}"), "a-b");
        assert_eq!(expand_env("x${YOLOP_TEST_A}${YOLOP_TEST_B}y"), "xaby");
        assert_eq!(expand_env("no placeholders"), "no placeholders");
        assert_eq!(expand_env("${unterminated"), "${unterminated");
        unsafe {
            std::env::remove_var("YOLOP_TEST_A");
            std::env::remove_var("YOLOP_TEST_B");
        }
    }
}

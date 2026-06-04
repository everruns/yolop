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
//! String values support `${VAR}` environment expansion (matching how every
//! other MCP client lets you keep secrets out of the file). Both remote HTTP
//! and local stdio servers are supported; stdio requires the runtime's
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
/// overlaid by the workspace `.mcp.json`. Missing files contribute nothing;
/// `${VAR}` placeholders in string fields are expanded from the environment.
pub fn load_mcp_servers(workspace_root: &Path) -> Result<ScopedMcpServers> {
    let global = match global_mcp_config_path() {
        Some(path) => read_config(&path)?,
        None => ScopedMcpServers::default(),
    };
    let workspace = read_config(&workspace_root.join(MCP_CONFIG_FILE))?;
    let mut merged = merge_scoped_mcp_servers(&global, &workspace);
    for server in merged.values_mut() {
        expand_server_env(server);
    }
    Ok(merged)
}

/// Read one `.mcp.json`-shaped file. Absent file → no servers.
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

    #[test]
    fn missing_file_yields_no_servers() {
        let dir = tempfile::tempdir().unwrap();
        let servers = load_mcp_servers(dir.path()).unwrap();
        assert!(servers.is_empty());
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

        let servers = load_mcp_servers(dir.path()).unwrap();
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
        let servers = load_mcp_servers(dir.path()).unwrap();
        assert_eq!(servers["b"].transport_type, McpServerTransportType::Http);
    }

    #[test]
    fn empty_object_is_ok() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "{}");
        assert!(load_mcp_servers(dir.path()).unwrap().is_empty());
    }

    #[test]
    fn malformed_json_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "{ not json");
        assert!(load_mcp_servers(dir.path()).is_err());
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

        let servers = load_mcp_servers(dir.path()).unwrap();
        let fs = servers.get("fs").expect("fs server");
        assert_eq!(fs.transport_type, McpServerTransportType::Stdio);
        assert_eq!(fs.command.as_deref(), Some("mcp-server-filesystem"));
        assert_eq!(fs.args, vec!["/work".to_string()]);
        assert_eq!(fs.env.get("RUST_LOG").map(String::as_str), Some("info"));
    }

    #[test]
    fn expands_env_placeholders() {
        // SAFETY: single-threaded test; restores nothing it doesn't own.
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

        let servers = load_mcp_servers(dir.path()).unwrap();
        assert_eq!(
            servers["docs"]
                .headers
                .get("Authorization")
                .map(String::as_str),
            Some("Bearer s3cret")
        );
        assert_eq!(servers["fs"].args, vec!["/srv/work".to_string()]);
    }

    #[test]
    fn unset_env_placeholder_is_left_intact() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            r#"{ "mcpServers": {
                "docs": { "type": "http", "url": "https://example.com/${YOLOP_UNSET_VAR_XYZ}" }
            }}"#,
        );
        let servers = load_mcp_servers(dir.path()).unwrap();
        assert_eq!(
            servers["docs"].url,
            "https://example.com/${YOLOP_UNSET_VAR_XYZ}"
        );
    }

    #[test]
    fn expand_env_handles_multiple_and_adjacent() {
        unsafe {
            std::env::set_var("YOLOP_TEST_A", "a");
            std::env::set_var("YOLOP_TEST_B", "b");
        }
        assert_eq!(expand_env("${YOLOP_TEST_A}-${YOLOP_TEST_B}"), "a-b");
        assert_eq!(expand_env("x${YOLOP_TEST_A}${YOLOP_TEST_B}y"), "xaby");
        assert_eq!(expand_env("no placeholders"), "no placeholders");
        assert_eq!(expand_env("${unterminated"), "${unterminated");
    }
}

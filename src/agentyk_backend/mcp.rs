//! yolop's configured MCP servers, attached to the agentyk agent.
//!
//! The servers come from the same place the shipping backend reads them —
//! global settings plus the workspace's `.mcp.json`, resolved by
//! [`McpConfigStore`], so `yolop mcp add` configures both backends at once.
//! Only servers the store reports as *effective* are attached; a disabled or
//! globally-overridden entry is not the agentyk backend's business to
//! second-guess.
//!
//! Credentials follow yolop's existing env-var convention rather than being
//! read out of the config file, because a token in a config file is a token
//! in a backup. agentyk asks the provider per request, so a token that
//! changes between turns is picked up without reconnecting.

use agentyk::{Capability, McpAuthProvider, McpCapability, McpServer, Result as AgentykResult};
use async_trait::async_trait;
use everruns_core::{McpServerTransportType, ScopedMcpServer};

use crate::config::mcp::McpConfigStore;

/// Build one capability per effective MCP server, plus any warnings worth
/// showing the operator.
///
/// A server that cannot be expressed is reported and skipped rather than
/// failing the run: losing one MCP server should not cost you the session.
pub fn capabilities(workspace: &std::path::Path) -> (Vec<Box<dyn Capability>>, Vec<String>) {
    let store = McpConfigStore::default_for_workspace(workspace);
    let effective = match store.effective() {
        Ok(config) => config,
        Err(error) => return (Vec::new(), vec![format!("mcp config ignored: {error}")]),
    };

    let mut capabilities: Vec<Box<dyn Capability>> = Vec::new();
    let mut warnings = Vec::new();
    for summary in effective.servers.into_iter().filter(|s| s.effective) {
        match server(&summary.name, &summary.server.server) {
            Ok(server) => {
                let auth = EnvBearer {
                    server: summary.name.clone(),
                    oauth_provider_id: summary.server.server.oauth_provider_id.clone(),
                };
                capabilities.push(Box::new(McpCapability::new(server).auth(auth)));
            }
            Err(reason) => {
                warnings.push(format!("mcp server `{}` skipped: {reason}", summary.name))
            }
        }
    }
    (capabilities, warnings)
}

/// Map one of yolop's configured servers onto agentyk's.
fn server(name: &str, configured: &ScopedMcpServer) -> Result<McpServer, String> {
    match &configured.transport_type {
        McpServerTransportType::Stdio => {
            let command = configured
                .command
                .clone()
                .ok_or("stdio transport needs a command")?;
            let mut server = McpServer::stdio(name, command).args(configured.args.clone());
            for (key, value) in &configured.env {
                server = server.env(key, value);
            }
            Ok(server)
        }
        McpServerTransportType::Http => {
            if configured.url.trim().is_empty() {
                return Err("http transport needs a url".into());
            }
            let mut server = McpServer::http(name, configured.url.clone());
            for (key, value) in &configured.headers {
                server = server.header(key, value);
            }
            Ok(server)
        }
    }
}

/// Reads a bearer token from the environment, using the same names the
/// shipping backend's MCP auth provider does.
struct EnvBearer {
    server: String,
    oauth_provider_id: Option<String>,
}

#[async_trait]
impl McpAuthProvider for EnvBearer {
    async fn authorization(&self, _server: &str) -> AgentykResult<Option<String>> {
        let token = token_names(&self.server, self.oauth_provider_id.as_deref())
            .into_iter()
            .find_map(|name| std::env::var(name).ok())
            .map(|token| token.trim().to_string())
            .filter(|token| !token.is_empty());
        // No token is not an error: plenty of servers need none, and a 401
        // says far more than a refusal to try.
        Ok(token.map(|token| format!("Bearer {token}")))
    }
}

/// The env vars checked, in order — an OAuth provider's names first, then the
/// server's own. Mirrors `EnvMcpAuthProvider` on the everruns backend.
fn token_names(server: &str, oauth_provider_id: Option<&str>) -> Vec<String> {
    let mut names = Vec::new();
    if let Some(provider) = oauth_provider_id {
        let prefix = env_key(provider);
        names.push(format!("{prefix}_ACCESS_TOKEN"));
        names.push(format!("{prefix}_API_KEY"));
        names.push(format!("{prefix}_TOKEN"));
    }
    let prefix = env_key(server);
    names.push(format!("MCP_{prefix}_ACCESS_TOKEN"));
    names.push(format!("MCP_{prefix}_API_KEY"));
    names.push(format!("MCP_{prefix}_TOKEN"));
    names
}

fn env_key(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn stdio() -> ScopedMcpServer {
        ScopedMcpServer {
            transport_type: McpServerTransportType::Stdio,
            command: Some("github-mcp-server".into()),
            args: vec!["stdio".into()],
            env: HashMap::from([("GITHUB_TOKEN".to_string(), "x".to_string())]),
            ..ScopedMcpServer::default()
        }
    }

    #[test]
    fn a_stdio_server_keeps_its_command_args_and_environment() {
        let mapped = server("github", &stdio()).expect("stdio maps");
        match mapped.transport {
            agentyk::McpTransport::Stdio { command, args, env } => {
                assert_eq!(command, "github-mcp-server");
                assert_eq!(args, vec!["stdio".to_string()]);
                assert_eq!(env, vec![("GITHUB_TOKEN".to_string(), "x".to_string())]);
            }
            other => panic!("expected stdio, got {other:?}"),
        }
    }

    #[test]
    fn an_http_server_keeps_its_url_and_headers() {
        let configured = ScopedMcpServer {
            transport_type: McpServerTransportType::Http,
            url: "https://example.invalid/mcp".into(),
            headers: HashMap::from([("X-Tenant".to_string(), "acme".to_string())]),
            ..ScopedMcpServer::default()
        };
        let mapped = server("hosted", &configured).expect("http maps");
        match mapped.transport {
            agentyk::McpTransport::Http { url, headers } => {
                assert_eq!(url, "https://example.invalid/mcp");
                assert_eq!(headers, vec![("X-Tenant".to_string(), "acme".to_string())]);
            }
            other => panic!("expected http, got {other:?}"),
        }
    }

    #[test]
    fn an_unusable_entry_is_reported_rather_than_silently_dropped() {
        let no_command = ScopedMcpServer {
            transport_type: McpServerTransportType::Stdio,
            ..ScopedMcpServer::default()
        };
        assert!(server("broken", &no_command).is_err());

        let no_url = ScopedMcpServer {
            transport_type: McpServerTransportType::Http,
            ..ScopedMcpServer::default()
        };
        assert!(server("broken", &no_url).is_err());
    }

    #[test]
    fn token_names_prefer_the_oauth_provider_then_the_server() {
        assert_eq!(
            token_names("github-remote", Some("github")),
            [
                "GITHUB_ACCESS_TOKEN",
                "GITHUB_API_KEY",
                "GITHUB_TOKEN",
                "MCP_GITHUB_REMOTE_ACCESS_TOKEN",
                "MCP_GITHUB_REMOTE_API_KEY",
                "MCP_GITHUB_REMOTE_TOKEN",
            ]
        );
    }

    #[tokio::test]
    async fn a_missing_token_is_absence_not_failure() {
        let provider = EnvBearer {
            server: "nothing-set-for-this-one".into(),
            oauth_provider_id: None,
        };
        assert_eq!(provider.authorization("x").await.unwrap(), None);
    }
}

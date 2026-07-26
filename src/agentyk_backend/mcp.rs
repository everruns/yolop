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

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use agentyk::capability::{CommandContext, CommandDescriptor, SystemPromptContext};
use agentyk::{
    Capability, McpAuthProvider, McpCapability, McpServer, Result as AgentykResult, Tool,
    ToolOutput,
};
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
                capabilities.push(Box::new(BestEffort::new(
                    summary.name.clone(),
                    McpCapability::new(server).auth(auth),
                )));
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

/// A capability whose tools are optional: if it cannot produce them, the turn
/// runs without them instead of failing.
///
/// Attaching a server is a statement about configuration, not a promise the
/// server is up. A remote MCP endpoint can be down, unreachable, or refuse a
/// token that expired an hour ago — and none of that should cost an operator
/// the session they are in the middle of. A live run proved the point: a
/// server missing its token 401'd and took the whole run with it.
///
/// The failure is announced once per process rather than silently swallowed,
/// because tools that vanish without explanation are their own kind of bug.
struct BestEffort {
    name: String,
    inner: Arc<dyn Capability>,
    reported: AtomicBool,
}

impl BestEffort {
    fn new(name: String, inner: impl Capability + 'static) -> Self {
        Self {
            name,
            inner: Arc::new(inner),
            reported: AtomicBool::new(false),
        }
    }
}

#[async_trait]
impl Capability for BestEffort {
    fn id(&self) -> &str {
        self.inner.id()
    }

    fn name(&self) -> &str {
        self.inner.name()
    }

    fn description(&self) -> &str {
        self.inner.description()
    }

    async fn system_prompt_contribution(&self, context: &SystemPromptContext) -> Option<String> {
        self.inner.system_prompt_contribution(context).await
    }

    async fn tools(&self) -> AgentykResult<Vec<Arc<dyn Tool>>> {
        match self.inner.tools().await {
            Ok(tools) => Ok(tools),
            Err(error) => {
                // Once per process: a turn loop would otherwise repeat this
                // on every iteration of every turn.
                if !self.reported.swap(true, Ordering::SeqCst) {
                    eprintln!(
                        "yolop: mcp server `{}` unavailable, continuing without its tools: {error}",
                        self.name
                    );
                }
                Ok(Vec::new())
            }
        }
    }

    fn commands(&self) -> Vec<CommandDescriptor> {
        self.inner.commands()
    }

    async fn execute_command(
        &self,
        name: &str,
        args: &str,
        context: &CommandContext,
    ) -> Option<ToolOutput> {
        self.inner.execute_command(name, args, context).await
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

#[cfg(test)]
mod best_effort_tests {
    use super::*;
    use agentyk::{Agent, ModelSpec, SimDriver, SimTurn};

    /// A capability that cannot produce its tools — a server that is down.
    struct Unreachable;

    #[async_trait]
    impl Capability for Unreachable {
        fn id(&self) -> &str {
            "mcp:unreachable"
        }

        async fn tools(&self) -> AgentykResult<Vec<Arc<dyn Tool>>> {
            Err(agentyk::Error::Mcp("connection refused".into()))
        }
    }

    #[tokio::test]
    async fn an_unreachable_server_does_not_fail_the_turn() -> anyhow::Result<()> {
        let agent = Agent::builder()
            .model(ModelSpec::llmsim())
            .driver(SimDriver::new([SimTurn::text("worked anyway")]))
            .capability(BestEffort::new("unreachable".into(), Unreachable))
            .build()
            .map_err(|error| anyhow::anyhow!("{error}"))?;

        let turn = agent
            .session()
            .run("go")
            .await
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        assert_eq!(turn.response, "worked anyway");
        Ok(())
    }

    #[tokio::test]
    async fn an_unwrapped_failure_would_have_failed_the_turn() -> anyhow::Result<()> {
        // The behavior being guarded against, asserted so the wrapper's
        // purpose stays legible if agentyk ever changes this.
        let agent = Agent::builder()
            .model(ModelSpec::llmsim())
            .driver(SimDriver::new([SimTurn::text("never reached")]))
            .capability(Unreachable)
            .build()
            .map_err(|error| anyhow::anyhow!("{error}"))?;

        assert!(agent.session().run("go").await.is_err());
        Ok(())
    }
}

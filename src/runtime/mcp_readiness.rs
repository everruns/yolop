//! Bounded MCP discovery over the published Everruns 0.51 host contracts.
//!
//! TODO: Replace this compatibility adapter with HostServices' shared MCP
//! readiness cache and Session discovery statuses when those APIs are released
//! on crates.io. Do not change the published dependency family to git or paths.
//! Engine still owns admission, reasoning, execution and authority. This module
//! supplies definitions only; transports, authentication, pagination, deferred
//! reveal and tool execution retain their upstream implementations.

mod cache;

pub(crate) use cache::McpDiscoveryStatus;
use cache::{CacheKey, DiscoveryFailure, McpDiscoveryCache};
use everruns_contracts::{tool_types::ToolDefinition, typed_id::SessionId};
use everruns_core::mcp::{
    MCP_DISCOVERY_DEADLINE, MCP_SESSION_DISCOVERY_MAX_BYTES, MCP_SESSION_DISCOVERY_MAX_TOOLS,
    McpAuthProvider, McpCapability, McpClient, McpDiscoveryError, McpHttpStatusError,
    RelayUrlElicitations,
};
use everruns_core::session_services::SessionStorageStore;
use everruns_core::{Capability, ScopedMcpServer, ScopedMcpServers};
use futures::{StreamExt, TryStreamExt, future::BoxFuture, stream};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

const MAX_DISCOVERY_CONCURRENCY: usize = 16;

pub(crate) struct McpReadiness {
    budget: Duration,
    cache: Arc<McpDiscoveryCache>,
    client: Arc<McpClient>,
}

impl McpReadiness {
    pub(crate) fn new(budget: Duration, auth: Arc<dyn McpAuthProvider>) -> Self {
        Self {
            budget: budget.min(MCP_DISCOVERY_DEADLINE),
            cache: Arc::new(McpDiscoveryCache::new()),
            // Use exactly the facade's policy-aware runtime transport, including
            // its configured egress restrictions, rather than a new HTTP path.
            client: Arc::new(McpClient::with_url_elicitation(
                everruns::batteries::runtime_egress_service(),
                auth,
                Arc::new(RelayUrlElicitations),
            )),
        }
    }

    /// Deadline applies to the complete turn catalog, not separately per server.
    /// Expiry abandons only the caller's wait; queued background work stays
    /// bounded and publishes only complete catalogs for later model steps.
    pub(crate) async fn discover(
        self: &Arc<Self>,
        session: SessionId,
        servers: &ScopedMcpServers,
        storage: &dyn SessionStorageStore,
    ) -> anyhow::Result<Vec<ToolDefinition>> {
        if servers.is_empty() {
            return Ok(Vec::new());
        }
        let deadline = tokio::time::Instant::now() + self.budget;
        let revealed = if servers.values().any(|server| server.deferred) {
            everruns_core::revealed_mcp_servers(storage, session).await
        } else {
            Default::default()
        };
        let (listed, deferred) = everruns_core::partition_deferred_mcp_servers(servers, &revealed);
        let mut discoveries: Vec<BoxFuture<'static, Vec<ToolDefinition>>> = Vec::new();
        for (name, server) in listed {
            if !server.tool_discovery || !everruns_core::mcp_server::is_valid_mcp_server_name(&name)
            {
                continue;
            }
            let key = cache_key(session, &name, &server)?;
            let Some(mut connection) = super::mcp_connection_for(&name, &server) else {
                self.cache.record_invalid_configuration(key);
                continue;
            };
            connection.connect_in_chat = server.connect_in_chat;
            let client = self.client.clone();
            let cache = self.cache.clone();
            discoveries.push(Box::pin(async move {
                cache
                    .resolve_until(
                        key,
                        move || {
                            let client = client.clone();
                            let connection = connection.clone();
                            let name = name.clone();
                            async move {
                                match client.discover(&connection).await {
                                    Ok(tools) => {
                                        let id = uuid::Uuid::new_v5(&session.uuid(), name.as_bytes());
                                        Ok(McpCapability::new(id, name, None, tools).tool_definitions())
                                    }
                                    Err(error) => {
                                        if error.downcast_ref::<McpHttpStatusError>()
                                            .is_some_and(|error| matches!(error.status, 401 | 403))
                                        {
                                            tracing::warn!(server = %name,
                                                "MCP authentication rejected; reconnect and reload MCP to retry discovery");
                                            Err(DiscoveryFailure::Authentication)
                                        } else {
                                            tracing::warn!(server = %name, %error,
                                                "MCP discovery failed; retrying after a short cooldown");
                                            Err(DiscoveryFailure::Transient)
                                        }
                                    }
                                }
                            }
                        },
                        Some(deadline),
                    )
                    .await
            }));
        }
        // Preserve BTreeMap ordering for stable prompts, even when later servers
        // finish first. Each future shares the same deadline and bounded cache.
        let (mut definitions, _) = stream::iter(discoveries)
            .buffered(MAX_DISCOVERY_CONCURRENCY)
            .map(Ok::<_, anyhow::Error>)
            .try_fold(
                (Vec::new(), 0usize),
                |(mut catalog, bytes), tools| async move {
                    let bytes = append_catalog(&mut catalog, bytes, tools)?;
                    Ok((catalog, bytes))
                },
            )
            .await?;
        let placeholders = deferred
            .iter()
            .filter(|name| everruns_core::mcp_server::is_valid_mcp_server_name(name))
            .map(|name| {
                everruns_core::deferred_mcp_server_definition(
                    name,
                    servers
                        .get(name)
                        .and_then(|server| server.description.as_deref()),
                )
            })
            .collect();
        // Account for placeholders before applying metadata, then validate the
        // complete enriched catalog as well. Never expose a partial overflow.
        let bytes = catalog_bytes(&definitions, MCP_SESSION_DISCOVERY_MAX_BYTES)?;
        append_catalog(&mut definitions, bytes, placeholders)?;
        attach_server_metadata(&mut definitions, servers);
        catalog_bytes(&definitions, MCP_SESSION_DISCOVERY_MAX_BYTES)?;
        Ok(definitions)
    }

    pub(crate) fn invalidate_session(&self, session: SessionId) {
        self.cache.invalidate_session(session.uuid());
    }

    /// Observation only: never initiates a tools/list or connection check.
    pub(crate) fn statuses(
        &self,
        session: SessionId,
        servers: &ScopedMcpServers,
    ) -> Vec<(String, McpDiscoveryStatus)> {
        servers
            .iter()
            .filter(|(name, server)| {
                server.tool_discovery && everruns_core::mcp_server::is_valid_mcp_server_name(name)
            })
            .map(|(name, server)| {
                (
                    name.clone(),
                    cache_key(session, name, server)
                        .map(|key| self.cache.status(&key))
                        .unwrap_or(McpDiscoveryStatus::Failed),
                )
            })
            .collect()
    }
}

fn cache_key(session: SessionId, name: &str, server: &ScopedMcpServer) -> anyhow::Result<CacheKey> {
    Ok((
        session.uuid(),
        name.to_string(),
        configuration_digest(server)?,
    ))
}

fn configuration_digest(config: &impl serde::Serialize) -> anyhow::Result<[u8; 32]> {
    // Canonical object ordering makes credentials/config changes invalidate the
    // catalog without retaining raw configuration or secrets in the key. A
    // serializer error can contain credentials, so do not preserve its detail
    // or derive a fallback identity from an incomplete configuration.
    let serialization_error =
        |_| anyhow::anyhow!("MCP discovery configuration could not be serialized");
    let mut config = serde_json::to_value(config).map_err(serialization_error)?;
    config.sort_all_objects();
    let digest = Sha256::digest(serde_json::to_vec(&config).map_err(serialization_error)?);
    Ok(digest.into())
}

struct ByteCounter {
    written: usize,
    limit: usize,
}

impl std::io::Write for ByteCounter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.written = self.written.saturating_add(bytes.len());
        if self.written > self.limit {
            return Err(std::io::Error::other(
                "MCP session catalog exceeds byte limit",
            ));
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn catalog_bytes(tools: &[ToolDefinition], limit: usize) -> anyhow::Result<usize> {
    let mut writer = ByteCounter { written: 0, limit };
    for tool in tools {
        if serde_json::to_writer(&mut writer, tool).is_err() {
            return Err(McpDiscoveryError::LimitExceeded {
                resource: "session serialized bytes",
                limit: MCP_SESSION_DISCOVERY_MAX_BYTES,
            }
            .into());
        }
    }
    Ok(writer.written)
}

fn append_catalog(
    catalog: &mut Vec<ToolDefinition>,
    bytes: usize,
    tools: Vec<ToolDefinition>,
) -> anyhow::Result<usize> {
    if catalog.len().saturating_add(tools.len()) > MCP_SESSION_DISCOVERY_MAX_TOOLS {
        return Err(McpDiscoveryError::LimitExceeded {
            resource: "session tools",
            limit: MCP_SESSION_DISCOVERY_MAX_TOOLS,
        }
        .into());
    }
    let added = catalog_bytes(
        &tools,
        MCP_SESSION_DISCOVERY_MAX_BYTES.saturating_sub(bytes),
    )?;
    catalog.extend(tools);
    Ok(bytes.saturating_add(added))
}

fn attach_server_metadata(definitions: &mut [ToolDefinition], servers: &ScopedMcpServers) {
    let by_prefix: HashMap<_, _> = servers
        .iter()
        .map(|(name, server)| {
            (
                everruns_core::sanitize_mcp_server_name(name),
                (name, server),
            )
        })
        .collect();
    for definition in definitions {
        let prefix = everruns_core::deferred_mcp_server_prefix(definition.name())
            .map(str::to_string)
            .or_else(|| {
                everruns_core::parse_mcp_tool_name(definition.name()).map(|(prefix, _)| prefix)
            });
        let Some((name, server)) = prefix
            .as_deref()
            .and_then(|prefix| by_prefix.get(prefix).copied())
        else {
            continue;
        };
        let ToolDefinition::Builtin(tool) = definition else {
            continue;
        };
        if let Some(context) = &server.capability_context {
            tool.hints.capability_id = Some(context.id.clone());
            tool.hints.capability_name = Some(context.name.clone());
            tool.hints.capability_description = Some(context.description.clone());
        }
        let metadata = tool
            .hints
            .metadata
            .get_or_insert_with(|| serde_json::json!({}));
        if let Some(metadata) = metadata.as_object_mut() {
            metadata.insert(
                "mcp_server".into(),
                serde_json::json!({
                    "name": name,
                    "description": server.description.as_deref().unwrap_or_default(),
                }),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use everruns_core::mcp::NoAuthProvider;

    #[tokio::test]
    async fn deferred_servers_remain_uncontacted_and_keep_search_metadata() {
        let manager = Arc::new(McpReadiness::new(Duration::ZERO, Arc::new(NoAuthProvider)));
        let session = SessionId::new();
        let servers = [(
            "docs".to_string(),
            ScopedMcpServer {
                url: "https://unreachable.invalid/mcp".into(),
                deferred: true,
                description: Some("Reference documentation".into()),
                ..Default::default()
            },
        )]
        .into_iter()
        .collect();
        let storage = everruns_core::host::HostBackends::in_memory().storage_store;
        let tools = manager
            .discover(session, &servers, storage.as_ref())
            .await
            .unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(
            everruns_core::deferred_mcp_server_prefix(tools[0].name()),
            Some("docs")
        );
        assert_eq!(
            tools[0].hints().metadata.as_ref().unwrap()["mcp_server"]["description"],
            "Reference documentation"
        );
        assert_eq!(
            manager.statuses(session, &servers),
            vec![("docs".into(), McpDiscoveryStatus::Pending)]
        );
    }

    #[tokio::test]
    async fn disabled_discovery_has_no_placeholders_or_status() {
        let manager = Arc::new(McpReadiness::new(Duration::ZERO, Arc::new(NoAuthProvider)));
        let session = SessionId::new();
        let servers = [(
            "docs".into(),
            ScopedMcpServer {
                transport_type: everruns_core::McpServerTransportType::Stdio,
                tool_discovery: false,
                deferred: true,
                ..Default::default()
            },
        )]
        .into_iter()
        .collect();
        let storage = everruns_core::host::HostBackends::in_memory().storage_store;
        assert!(
            manager
                .discover(session, &servers, storage.as_ref())
                .await
                .unwrap()
                .is_empty()
        );
        assert!(manager.statuses(session, &servers).is_empty());
    }

    #[tokio::test]
    async fn missing_stdio_command_fails_discovery_until_configuration_changes() {
        let manager = Arc::new(McpReadiness::new(Duration::ZERO, Arc::new(NoAuthProvider)));
        let session = SessionId::new();
        let mut servers: ScopedMcpServers = [(
            "local".into(),
            ScopedMcpServer {
                transport_type: everruns_core::McpServerTransportType::Stdio,
                ..Default::default()
            },
        )]
        .into_iter()
        .collect();
        let storage = everruns_core::host::HostBackends::in_memory().storage_store;
        assert!(
            manager
                .discover(session, &servers, storage.as_ref())
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            manager.statuses(session, &servers),
            vec![("local".into(), McpDiscoveryStatus::Failed)]
        );

        servers.get_mut("local").unwrap().command = Some("python3".into());
        assert_eq!(
            manager.statuses(session, &servers),
            vec![("local".into(), McpDiscoveryStatus::Pending)]
        );
    }

    #[tokio::test]
    async fn invalid_deferred_server_stays_pending_until_revealed() {
        let manager = Arc::new(McpReadiness::new(Duration::ZERO, Arc::new(NoAuthProvider)));
        let session = SessionId::new();
        let servers = [(
            "local".into(),
            ScopedMcpServer {
                transport_type: everruns_core::McpServerTransportType::Stdio,
                deferred: true,
                ..Default::default()
            },
        )]
        .into_iter()
        .collect();
        let storage = everruns_core::host::HostBackends::in_memory().storage_store;
        let definitions = manager
            .discover(session, &servers, storage.as_ref())
            .await
            .unwrap();
        assert_eq!(definitions.len(), 1);
        assert_eq!(
            everruns_core::deferred_mcp_server_prefix(definitions[0].name()),
            Some("local")
        );
        assert_eq!(
            manager.statuses(session, &servers),
            vec![("local".into(), McpDiscoveryStatus::Pending)]
        );

        everruns_core::reveal_deferred_mcp_server(storage.as_ref(), session, "local")
            .await
            .unwrap();
        assert!(
            manager
                .discover(session, &servers, storage.as_ref())
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            manager.statuses(session, &servers),
            vec![("local".into(), McpDiscoveryStatus::Failed)]
        );
    }

    #[test]
    fn configuration_serialization_error_is_fallible_and_redacted() {
        struct InvalidConfiguration;
        impl serde::Serialize for InvalidConfiguration {
            fn serialize<S: serde::Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
                Err(serde::ser::Error::custom("secret authorization value"))
            }
        }
        let error = configuration_digest(&InvalidConfiguration).unwrap_err();
        assert_eq!(
            format!("{error:#}"),
            "MCP discovery configuration could not be serialized"
        );
    }

    #[test]
    fn configuration_digest_preserves_canonical_json_identity() {
        let first: serde_json::Value =
            serde_json::from_str(r#"{"b":{"z":1,"a":2},"a":3}"#).unwrap();
        let second: serde_json::Value =
            serde_json::from_str(r#"{"a":3,"b":{"a":2,"z":1}}"#).unwrap();
        assert_eq!(
            configuration_digest(&first).unwrap(),
            configuration_digest(&second).unwrap()
        );
        let expected: [u8; 32] = Sha256::digest(br#"{"a":3,"b":{"a":2,"z":1}}"#).into();
        assert_eq!(configuration_digest(&first).unwrap(), expected);
    }

    #[test]
    fn cache_identity_includes_session_name_and_full_configuration() {
        let session = SessionId::new();
        let mut server = ScopedMcpServer::default();
        let original = cache_key(session, "docs", &server).unwrap();
        assert_ne!(
            original,
            cache_key(SessionId::new(), "docs", &server).unwrap()
        );
        assert_ne!(original, cache_key(session, "other", &server).unwrap());
        server
            .headers
            .insert("Authorization".into(), "changed".into());
        assert_ne!(original, cache_key(session, "docs", &server).unwrap());
    }

    #[test]
    fn aggregate_limit_rejects_whole_addition_without_partial_catalog() {
        let tool = everruns_core::deferred_mcp_server_definition("docs", None);
        let mut catalog = vec![tool.clone(); MCP_SESSION_DISCOVERY_MAX_TOOLS];
        assert!(append_catalog(&mut catalog, 0, vec![tool]).is_err());
        assert_eq!(catalog.len(), MCP_SESSION_DISCOVERY_MAX_TOOLS);
    }

    #[test]
    fn serialized_limit_counts_final_metadata_without_buffering_the_catalog() {
        let tool = everruns_core::deferred_mcp_server_definition("docs", None);
        let length = catalog_bytes(std::slice::from_ref(&tool), usize::MAX).unwrap();
        assert_eq!(
            catalog_bytes(std::slice::from_ref(&tool), length).unwrap(),
            length
        );
        assert!(catalog_bytes(&[tool], length - 1).is_err());
    }
}

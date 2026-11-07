//! Execution-only MCP readiness projection for published Everruns 0.51.0.
//!
//! TODO: Replace this projection and augmentor with the shared HostServices
//! readiness API once a published Everruns release includes it. Keep the
//! Everruns dependency family on aligned crates.io versions in the meantime.
//!
//! Configuration and storage remain authoritative and unmodified. Only the
//! activation's read view disables native blocking discovery; the augmentor
//! reads the captured original backends. Native MCP execution is unchanged.
//! Explicit context queries use the original reader and keep full discovery.

mod storage;

use super::mcp_readiness::{McpDiscoveryStatus, McpReadiness};
use async_trait::async_trait;
use everruns_contracts::{
    AgentLoopError, CapabilityRef, Result, tool_types::ToolDefinition, typed_id::SessionId,
};
use everruns_core::execution_loading::SessionStore;
use everruns_core::host::{HostBackends, HostToolAugmentor, RuntimeSessionStore, SessionMutator};
use everruns_core::mcp::McpAuthProvider;
use everruns_core::session_files::SessionFileSystem;
use everruns_core::session_task::SessionTaskRegistry;
use everruns_core::tools::ToolRegistry;
use everruns_core::{
    Capability, CapabilityRegistry, ExecutionSession, ScopedMcpServers, apply_session_attachments,
    capabilities::collect_capability_mcp_servers, merge_scoped_mcp_servers,
    runtime_context::resolve_runtime_capabilities,
};
use std::collections::VecDeque;
use std::sync::{Arc, RwLock};
use std::time::Duration;

const MAX_ACTIVATION_VIEWS: usize = 64;

pub(crate) struct McpCompat {
    readiness: Arc<McpReadiness>,
    registry: RwLock<CapabilityRegistry>,
    activation_views: RwLock<VecDeque<(SessionId, HostBackends)>>,
}

impl McpCompat {
    pub(crate) fn new(
        budget: Duration,
        auth: Arc<dyn McpAuthProvider>,
        registry: CapabilityRegistry,
    ) -> Self {
        Self {
            readiness: Arc::new(McpReadiness::new(budget, auth)),
            registry: RwLock::new(registry),
            activation_views: RwLock::new(VecDeque::new()),
        }
    }

    pub(crate) fn register_capability(&self, capability: Arc<dyn Capability>) {
        self.registry
            .write()
            .expect("MCP capability registry lock")
            .register_arc(capability);
    }

    pub(crate) fn invalidate_session(&self, session: SessionId) {
        self.readiness.invalidate_session(session);
    }

    pub(crate) async fn statuses(
        &self,
        raw: &HostBackends,
        session: SessionId,
    ) -> anyhow::Result<Vec<(String, McpDiscoveryStatus)>> {
        // A fresh SqliteHosting reader has no transient harness/agent records.
        // Keep the last prepared activation's original stores, not resolved
        // server configs, so live capability and session edits remain visible.
        let retained = self.activation_view(session);
        let definitions = retained.as_ref().unwrap_or(raw);
        let Some(record) = raw.session_store.get_session(session).await? else {
            return Ok(Vec::new());
        };
        let Some(servers) = self.effective_servers(definitions, &record).await? else {
            // No activation has been prepared yet (or its view was evicted).
            // Observation must not create a runtime or trigger discovery.
            return Ok(Vec::new());
        };
        let revealed =
            everruns_core::revealed_mcp_servers(raw.storage_store.as_ref(), session).await;
        let (listed, _) = everruns_core::partition_deferred_mcp_servers(&servers, &revealed);
        Ok(self.readiness.statuses(session, &listed))
    }

    pub(crate) async fn servers(
        &self,
        raw: &HostBackends,
        session: SessionId,
    ) -> anyhow::Result<ScopedMcpServers> {
        let session = raw
            .session_store
            .get_session(session)
            .await?
            .ok_or_else(|| AgentLoopError::session_not_found(session))?;
        self.effective_servers(raw, &session)
            .await?
            .ok_or_else(|| anyhow::anyhow!("MCP activation definitions have not been prepared"))
    }

    async fn effective_servers(
        &self,
        raw: &HostBackends,
        session: &ExecutionSession,
    ) -> Result<Option<ScopedMcpServers>> {
        let Some(harness) = raw.harness_store.get_harness(session.harness_id).await? else {
            return Ok(None);
        };
        let agent = match session.agent_id {
            Some(agent) => {
                let Some(agent) = raw.agent_store.get_agent(agent).await? else {
                    return Ok(None);
                };
                Some(agent)
            }
            None => None,
        };
        let mut attached = session.clone();
        apply_session_attachments(raw.storage_store.as_ref(), &mut attached).await;
        // Dynamic extension registration cannot hold a synchronous guard over
        // asynchronous backend reads or discovery.
        let registry = self
            .registry
            .read()
            .expect("MCP capability registry lock")
            .clone();
        let resolved = resolve_runtime_capabilities(&harness, agent.as_ref(), &attached, &registry);
        let mut servers =
            collect_capability_mcp_servers(&resolved.resolved_capability_configs, &registry);
        servers = merge_scoped_mcp_servers(&servers, &harness.mcp_servers);
        if let Some(agent) = agent {
            servers = merge_scoped_mcp_servers(&servers, &agent.mcp_servers);
        }
        Ok(Some(merge_scoped_mcp_servers(
            &servers,
            &attached.mcp_servers,
        )))
    }

    fn remember_activation(&self, session: SessionId, raw: &HostBackends) {
        let mut views = self
            .activation_views
            .write()
            .expect("MCP activation views lock");
        if let Some(index) = views.iter().position(|(id, _)| *id == session) {
            views.remove(index);
        }
        views.push_back((session, raw.clone()));
        if views.len() > MAX_ACTIVATION_VIEWS {
            views.pop_front();
        }
    }

    fn activation_view(&self, session: SessionId) -> Option<HostBackends> {
        self.activation_views
            .read()
            .expect("MCP activation views lock")
            .iter()
            .find(|(id, _)| *id == session)
            .map(|(_, raw)| raw.clone())
    }

    /// Decorate an activation after host enrichment, never its query reader.
    pub(crate) fn decorate(self: &Arc<Self>, mut backends: HostBackends) -> HostBackends {
        let raw = backends.clone();
        backends.session_store = Arc::new(DiscoverySessionStore {
            compat: self.clone(),
            raw: raw.clone(),
        });
        backends.storage_store = Arc::new(storage::DiscoveryStorage(raw.storage_store.clone()));
        backends.tool_augmentor = Some(Arc::new(ReadinessAugmentor {
            compat: self.clone(),
            raw,
        }));
        backends
    }
}

struct DiscoverySessionStore {
    compat: Arc<McpCompat>,
    raw: HostBackends,
}

#[async_trait]
impl SessionStore for DiscoverySessionStore {
    async fn get_session(&self, session: SessionId) -> Result<Option<ExecutionSession>> {
        let Some(mut session) = self.raw.session_store.get_session(session).await? else {
            return Ok(None);
        };
        let Some(servers) = self.compat.effective_servers(&self.raw, &session).await? else {
            // RuntimeBuilder reconciles the authoritative session before it
            // seeds harnesses/agents. Returning an error here loses persisted
            // overlays. Ancestor reads also legitimately lack these records.
            return Ok(Some(session));
        };
        session.mcp_servers = servers;
        for server in session.mcp_servers.values_mut() {
            server.tool_discovery = false;
        }
        Ok(Some(session))
    }
}

#[async_trait]
impl RuntimeSessionStore for DiscoverySessionStore {
    async fn add_session(&self, session: ExecutionSession) -> Result<()> {
        let id = session.id;
        self.raw.session_store.add_session(session).await?;
        // Successful seeding identifies the activation's own session, unlike
        // get_session, which also serves reads of ancestor session records.
        self.compat.remember_activation(id, &self.raw);
        Ok(())
    }
}

#[async_trait]
impl SessionMutator for DiscoverySessionStore {
    async fn update_session_title(
        &self,
        session: SessionId,
        title: String,
    ) -> Result<ExecutionSession> {
        self.raw
            .session_store
            .update_session_title(session, title)
            .await
    }

    async fn upsert_session_capability(
        &self,
        session: SessionId,
        capability: CapabilityRef,
    ) -> Result<ExecutionSession> {
        self.raw
            .session_store
            .upsert_session_capability(session, capability)
            .await
    }

    async fn remove_session_capability(
        &self,
        session: SessionId,
        capability: &str,
    ) -> Result<ExecutionSession> {
        self.raw
            .session_store
            .remove_session_capability(session, capability)
            .await
    }
}

struct ReadinessAugmentor {
    compat: Arc<McpCompat>,
    raw: HostBackends,
}

#[async_trait]
impl HostToolAugmentor for ReadinessAugmentor {
    fn disabled_hook_contributions(
        &self,
        capability: &str,
        config: &serde_json::Value,
    ) -> Vec<String> {
        self.raw
            .tool_augmentor
            .as_ref()
            .map_or_else(Vec::new, |inner| {
                inner.disabled_hook_contributions(capability, config)
            })
    }

    async fn augment_reason_tools(
        &self,
        session: SessionId,
        session_store: Arc<dyn SessionStore>,
        task_registry: Option<Arc<dyn SessionTaskRegistry>>,
        definitions: &mut Vec<ToolDefinition>,
    ) -> Result<()> {
        let servers = self
            .compat
            .servers(&self.raw, session)
            .await
            .map_err(AgentLoopError::Internal)?;
        self.compat.remember_activation(session, &self.raw);
        let discovered = self
            .compat
            .readiness
            .discover(session, &servers, self.raw.storage_store.as_ref())
            .await;
        append_discovered_tools(definitions, discovered);
        if let Some(inner) = &self.raw.tool_augmentor {
            inner
                .augment_reason_tools(session, session_store, task_registry, definitions)
                .await?;
        }
        Ok(())
    }

    async fn augment_act_tools(
        &self,
        session: SessionId,
        session_store: Arc<dyn SessionStore>,
        task_registry: Option<Arc<dyn SessionTaskRegistry>>,
        file_store: Arc<dyn SessionFileSystem>,
        definitions: &[ToolDefinition],
        registry: &mut ToolRegistry,
    ) -> Result<()> {
        if let Some(inner) = &self.raw.tool_augmentor {
            inner
                .augment_act_tools(
                    session,
                    session_store,
                    task_registry,
                    file_store,
                    definitions,
                    registry,
                )
                .await?;
        }
        Ok(())
    }
}

fn append_discovered_tools(
    definitions: &mut Vec<ToolDefinition>,
    discovered: anyhow::Result<Vec<ToolDefinition>>,
) {
    match discovered {
        Ok(discovered) => definitions.extend(discovered),
        // Match native discovery: an invalid aggregate catalog contributes no
        // MCP tools, but ordinary tools and the rest of the turn stay usable.
        Err(error) => tracing::warn!(%error, "MCP discovery failed; exposing no MCP catalog"),
    }
}

#[cfg(test)]
mod tests;

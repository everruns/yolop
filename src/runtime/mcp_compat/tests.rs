use super::*;
use everruns_contracts::typed_id::{AgentId, HarnessId};
use everruns_core::host::InMemorySessionFileStore;
use everruns_core::mcp::NoAuthProvider;
use everruns_core::session_mcp_servers::{
    SessionMcpServer, SessionMcpServerSource, put_session_mcp_server, session_mcp_server_kv_key,
};
use everruns_core::{
    AgentDefinition, CapabilityMcpServer, CapabilityMcpServers, HarnessDefinition, ScopedMcpServer,
};
use serde_json::json;

struct ContributedServers(ScopedMcpServers);

#[async_trait]
impl Capability for ContributedServers {
    fn id(&self) -> &str {
        "test_mcp_contributor"
    }
    fn name(&self) -> &str {
        "Test MCP contributor"
    }
    fn description(&self) -> &str {
        "Contributes servers for precedence tests"
    }
    fn mcp_servers(&self) -> CapabilityMcpServers {
        self.0
            .iter()
            .map(|(name, server)| {
                (
                    name.clone(),
                    CapabilityMcpServer::new(server.clone(), server.acts_as),
                )
            })
            .collect()
    }
}

fn servers(names: &[&str], layer: &str) -> ScopedMcpServers {
    names
        .iter()
        .map(|name| {
            (
                (*name).to_owned(),
                ScopedMcpServer {
                    url: format!("https://{layer}.example.test/mcp"),
                    ..Default::default()
                },
            )
        })
        .collect()
}

fn compat(registry: CapabilityRegistry) -> Arc<McpCompat> {
    Arc::new(McpCompat::new(
        Duration::ZERO,
        Arc::new(NoAuthProvider),
        registry,
    ))
}

async fn seed(raw: &HostBackends, session: &ExecutionSession) {
    raw.harness_store
        .add_harness(session.harness_id, HarnessDefinition::default())
        .await
        .unwrap();
    raw.session_store
        .add_session(session.clone())
        .await
        .unwrap();
}

#[tokio::test]
async fn projection_preserves_authority_and_resolves_every_layer() {
    let raw = HostBackends::in_memory();
    let session_id = SessionId::new();
    let harness_id = HarnessId::new();
    let agent_id = AgentId::new();
    let mut registry = CapabilityRegistry::new();
    registry.register(ContributedServers(servers(
        &["capability", "harness", "agent", "session", "attached"],
        "capability",
    )));
    let mut harness = HarnessDefinition::default();
    harness
        .capabilities
        .push(CapabilityRef::new("test_mcp_contributor"));
    harness.mcp_servers = servers(&["harness", "agent", "session", "attached"], "harness");
    let mut agent = AgentDefinition::new(agent_id, "test", "test prompt");
    agent.mcp_servers = servers(&["agent", "session", "attached"], "agent");
    let mut session = ExecutionSession::with_own_workspace(session_id, harness_id);
    session.agent_id = Some(agent_id);
    session.mcp_servers = servers(&["session", "attached"], "session");
    raw.harness_store
        .add_harness(harness_id, harness.clone())
        .await
        .unwrap();
    raw.agent_store.add_agent(agent.clone()).await.unwrap();
    raw.session_store
        .add_session(session.clone())
        .await
        .unwrap();
    let record = SessionMcpServer {
        name: "attached".into(),
        server: servers(&["attached"], "attached")
            .remove("attached")
            .unwrap(),
        source: SessionMcpServerSource::UserMcp,
    };
    put_session_mcp_server(raw.storage_store.as_ref(), session_id, &record)
        .await
        .unwrap();
    let original_record = raw
        .storage_store
        .get_value(session_id, &session_mcp_server_kv_key("attached"))
        .await
        .unwrap();
    let compat = compat(registry);
    let effective = compat.servers(&raw, session_id).await.unwrap();
    assert_eq!(effective.len(), 5);
    for name in ["capability", "harness", "agent", "session", "attached"] {
        assert_eq!(
            effective[name].url,
            format!("https://{name}.example.test/mcp")
        );
        assert!(effective[name].tool_discovery);
    }
    let projected = compat.decorate(raw.clone());
    let mut view = projected
        .session_store
        .get_session(session_id)
        .await
        .unwrap()
        .unwrap();
    // Native context assembly folds storage attachments after loading the
    // session. A second fold must not turn blocking discovery back on.
    apply_session_attachments(projected.storage_store.as_ref(), &mut view).await;
    let mut expected = effective.clone();
    for server in expected.values_mut() {
        server.tool_discovery = false;
    }
    assert_eq!(view.mcp_servers, expected);
    assert_eq!(
        raw.session_store
            .get_session(session_id)
            .await
            .unwrap()
            .unwrap()
            .mcp_servers,
        session.mcp_servers
    );
    assert_eq!(
        raw.agent_store
            .get_agent(agent_id)
            .await
            .unwrap()
            .unwrap()
            .mcp_servers,
        agent.mcp_servers
    );
    assert_eq!(
        raw.harness_store
            .get_harness(harness_id)
            .await
            .unwrap()
            .unwrap()
            .mcp_servers,
        harness.mcp_servers
    );
    assert_eq!(
        raw.storage_store
            .get_value(session_id, &session_mcp_server_kv_key("attached"))
            .await
            .unwrap(),
        original_record
    );
    assert_eq!(compat.servers(&raw, session_id).await.unwrap(), effective);
}

#[tokio::test]
async fn already_decorated_backends_observe_dynamic_capabilities_and_config_changes() {
    let raw = HostBackends::in_memory();
    let mut session = ExecutionSession::with_own_workspace(SessionId::new(), HarnessId::new());
    session
        .capabilities
        .push(CapabilityRef::new("test_mcp_contributor"));
    seed(&raw, &session).await;
    let compat = compat(CapabilityRegistry::new());
    let projected = compat.decorate(raw.clone());
    assert!(compat.servers(&raw, session.id).await.unwrap().is_empty());
    compat.register_capability(Arc::new(ContributedServers(servers(
        &["late"],
        "extension",
    ))));
    let view = projected
        .session_store
        .get_session(session.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        view.mcp_servers["late"].url,
        "https://extension.example.test/mcp"
    );
    assert!(!view.mcp_servers["late"].tool_discovery);
    session.mcp_servers = servers(&["late"], "session");
    projected
        .session_store
        .add_session(session.clone())
        .await
        .unwrap();
    assert_eq!(
        compat.servers(&raw, session.id).await.unwrap()["late"],
        session.mcp_servers["late"]
    );
    assert_eq!(
        projected
            .session_store
            .get_session(session.id)
            .await
            .unwrap()
            .unwrap()
            .mcp_servers["late"]
            .url,
        "https://session.example.test/mcp"
    );
}

#[tokio::test]
async fn session_mutations_delegate_without_persisting_projection() {
    let raw = HostBackends::in_memory();
    let mut session = ExecutionSession::with_own_workspace(SessionId::new(), HarnessId::new());
    session.mcp_servers = servers(&["native"], "session");
    seed(&raw, &session).await;
    let projected = compat(CapabilityRegistry::new()).decorate(raw.clone());
    let updated = projected
        .session_store
        .update_session_title(session.id, "Updated".into())
        .await
        .unwrap();
    assert_eq!(updated.title.as_deref(), Some("Updated"));
    assert!(updated.mcp_servers["native"].tool_discovery);
    let updated = projected
        .session_store
        .upsert_session_capability(session.id, CapabilityRef::new("test"))
        .await
        .unwrap();
    assert!(
        updated
            .capabilities
            .iter()
            .any(|cap| cap.capability_id() == "test")
    );
    let updated = projected
        .session_store
        .remove_session_capability(session.id, "test")
        .await
        .unwrap();
    assert!(
        !updated
            .capabilities
            .iter()
            .any(|cap| cap.capability_id() == "test")
    );
    let original = raw
        .session_store
        .get_session(session.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(original.title.as_deref(), Some("Updated"));
    assert_eq!(original.mcp_servers, session.mcp_servers);
    assert!(
        projected
            .session_store
            .get_session(SessionId::new())
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn storage_projects_only_mcp_reads_and_preserves_writes_takes_and_secrets() {
    let raw = HostBackends::in_memory();
    let storage = compat(CapabilityRegistry::new())
        .decorate(raw.clone())
        .storage_store;
    let session = SessionId::new();
    let record = SessionMcpServer {
        name: "attached".into(),
        server: servers(&["attached"], "storage")
            .remove("attached")
            .unwrap(),
        source: SessionMcpServerSource::UserMcp,
    };
    let encoded = serde_json::to_string(&record).unwrap();
    let key = session_mcp_server_kv_key("attached");
    storage.set_value(session, &key, &encoded).await.unwrap();
    assert_eq!(
        raw.storage_store
            .get_value(session, &key)
            .await
            .unwrap()
            .as_deref(),
        Some(encoded.as_str())
    );
    let view: SessionMcpServer =
        serde_json::from_str(&storage.get_value(session, &key).await.unwrap().unwrap()).unwrap();
    assert!(!view.server.tool_discovery);
    assert_eq!(view.source, record.source);
    assert_eq!(view.server.url, record.server.url);
    assert_eq!(
        storage.take_value(session, &key).await.unwrap().as_deref(),
        Some(encoded.as_str())
    );
    assert!(
        raw.storage_store
            .get_value(session, &key)
            .await
            .unwrap()
            .is_none()
    );
    for (key, value) in [
        ("ordinary", encoded.as_str()),
        ("session_mcp:malformed", "not json"),
    ] {
        storage.set_value(session, key, value).await.unwrap();
        assert_eq!(
            storage.get_value(session, key).await.unwrap().as_deref(),
            Some(value)
        );
        assert!(
            storage
                .list_keys(session)
                .await
                .unwrap()
                .iter()
                .any(|entry| entry.key == key)
        );
        assert!(storage.delete_value(session, key).await.unwrap());
    }
    storage
        .set_secret(session, "fixture", "test value")
        .await
        .unwrap();
    assert_eq!(
        storage
            .get_secret(session, "fixture")
            .await
            .unwrap()
            .as_deref(),
        Some("test value")
    );
    assert_eq!(
        raw.storage_store
            .get_secret(session, "fixture")
            .await
            .unwrap()
            .as_deref(),
        Some("test value")
    );
    assert_eq!(
        storage.list_secrets(session).await.unwrap()[0].name,
        "fixture"
    );
    assert!(storage.delete_secret(session, "fixture").await.unwrap());
    assert!(
        raw.storage_store
            .get_secret(session, "fixture")
            .await
            .unwrap()
            .is_none()
    );
}

struct ExistingAugmentor;

#[async_trait]
impl HostToolAugmentor for ExistingAugmentor {
    fn disabled_hook_contributions(
        &self,
        capability: &str,
        config: &serde_json::Value,
    ) -> Vec<String> {
        vec![format!("{capability}:{}", config["hook"].as_str().unwrap())]
    }
    async fn augment_reason_tools(
        &self,
        _session: SessionId,
        _store: Arc<dyn SessionStore>,
        _tasks: Option<Arc<dyn SessionTaskRegistry>>,
        definitions: &mut Vec<ToolDefinition>,
    ) -> Result<()> {
        definitions.push(ToolDefinition::function(
            "existing",
            "Existing host tool",
            json!({"type": "object"}),
        ));
        Ok(())
    }
    async fn augment_act_tools(
        &self,
        _session: SessionId,
        _store: Arc<dyn SessionStore>,
        _tasks: Option<Arc<dyn SessionTaskRegistry>>,
        _files: Arc<dyn SessionFileSystem>,
        definitions: &[ToolDefinition],
        _registry: &mut ToolRegistry,
    ) -> Result<()> {
        // Returning the original failure proves act augmentation is neither
        // dropped nor swallowed by the readiness decorator.
        assert_eq!(definitions.last().unwrap().name(), "existing");
        Err(AgentLoopError::config("existing augmentor error"))
    }
}

#[tokio::test]
async fn existing_augmentor_retains_reason_act_and_disabled_hooks() {
    let mut raw = HostBackends::in_memory();
    let session = ExecutionSession::with_own_workspace(SessionId::new(), HarnessId::new());
    seed(&raw, &session).await;
    raw.tool_augmentor = Some(Arc::new(ExistingAugmentor));
    let projected = compat(CapabilityRegistry::new()).decorate(raw);
    let augmentor = projected.tool_augmentor.unwrap();
    assert_eq!(
        augmentor.disabled_hook_contributions("platform", &json!({"hook": "disabled"})),
        ["platform:disabled"]
    );
    let mut definitions = vec![ToolDefinition::function(
        "original",
        "Original tool",
        json!({}),
    )];
    augmentor
        .augment_reason_tools(
            session.id,
            projected.session_store.clone(),
            None,
            &mut definitions,
        )
        .await
        .unwrap();
    assert_eq!(
        definitions
            .iter()
            .map(ToolDefinition::name)
            .collect::<Vec<_>>(),
        ["original", "existing"]
    );
    let error = augmentor
        .augment_act_tools(
            session.id,
            projected.session_store,
            None,
            Arc::new(InMemorySessionFileStore::new()),
            &definitions,
            &mut ToolRegistry::new(),
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("existing augmentor error"));
}

#[tokio::test]
async fn deferred_servers_have_one_placeholder_and_only_report_status_after_reveal() {
    let raw = HostBackends::in_memory();
    let mut session = ExecutionSession::with_own_workspace(SessionId::new(), HarnessId::new());
    session.mcp_servers = servers(&["deferred"], "never-discovered");
    session.mcp_servers.get_mut("deferred").unwrap().deferred = true;
    seed(&raw, &session).await;
    let compat = compat(CapabilityRegistry::new());
    let projected = compat.decorate(raw.clone());
    let view = projected
        .session_store
        .get_session(session.id)
        .await
        .unwrap()
        .unwrap();
    let (listed, deferred) =
        everruns_core::partition_deferred_mcp_servers(&view.mcp_servers, &Default::default());
    assert!(
        deferred.is_empty(),
        "native discovery must not also emit placeholders"
    );
    assert!(listed.values().all(|server| !server.tool_discovery));
    let mut definitions = Vec::new();
    projected
        .tool_augmentor
        .unwrap()
        .augment_reason_tools(session.id, projected.session_store, None, &mut definitions)
        .await
        .unwrap();
    assert_eq!(definitions.len(), 1);
    assert_eq!(definitions[0].name(), "mcp_deferred");
    assert!(compat.statuses(&raw, session.id).await.unwrap().is_empty());
    everruns_core::reveal_deferred_mcp_server(raw.storage_store.as_ref(), session.id, "deferred")
        .await
        .unwrap();
    assert_eq!(
        compat.statuses(&raw, session.id).await.unwrap(),
        vec![("deferred".into(), McpDiscoveryStatus::Pending)]
    );
}

#[test]
fn invalid_aggregate_catalog_keeps_non_mcp_tools_available() {
    let mut definitions = vec![ToolDefinition::function("native", "Native tool", json!({}))];
    append_discovered_tools(
        &mut definitions,
        Err(everruns_core::mcp::McpDiscoveryError::LimitExceeded {
            resource: "session tools",
            limit: everruns_core::mcp::MCP_SESSION_DISCOVERY_MAX_TOOLS,
        }
        .into()),
    );
    assert_eq!(definitions.len(), 1);
    assert_eq!(definitions[0].name(), "native");
}

#[tokio::test]
async fn preseed_and_ancestor_reads_preserve_complete_authoritative_session() {
    let raw = HostBackends::in_memory();
    let mut session = ExecutionSession::with_own_workspace(SessionId::new(), HarnessId::new());
    let agent = AgentId::new();
    session.agent_id = Some(agent);
    session.title = Some("Retained title".into());
    session.goal = Some("Retained goal".into());
    session.locale = Some("uk".into());
    session.tags = vec!["retained".into()];
    session.capabilities = vec![CapabilityRef::with_config(
        "retained",
        json!({"setting": true}),
    )];
    session.max_iterations = Some(23);
    session.parent_session_id = Some(SessionId::new());
    session.blueprint_config = Some(json!({"retained": true}));
    session.unattended = true;
    session.mcp_servers = servers(&["retained"], "session");
    raw.session_store
        .add_session(session.clone())
        .await
        .unwrap();
    let expected = serde_json::to_value(&session).unwrap();
    let compat = compat(CapabilityRegistry::new());
    let projected = compat.decorate(raw.clone());
    // This is the read RuntimeBuilder performs before seeding definitions.
    let view = projected
        .session_store
        .get_session(session.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(serde_json::to_value(view).unwrap(), expected);
    assert!(compat.activation_view(session.id).is_none());
    raw.harness_store
        .add_harness(session.harness_id, HarnessDefinition::default())
        .await
        .unwrap();
    // A referenced agent may also be seeded after the reconciliation read.
    let view = projected
        .session_store
        .get_session(session.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(serde_json::to_value(view).unwrap(), expected);
    raw.agent_store
        .add_agent(AgentDefinition::new(agent, "agent", "prompt"))
        .await
        .unwrap();
    let mut view = projected
        .session_store
        .get_session(session.id)
        .await
        .unwrap()
        .unwrap();
    assert!(!view.mcp_servers["retained"].tool_discovery);
    view.mcp_servers = session.mcp_servers.clone();
    assert_eq!(serde_json::to_value(view).unwrap(), expected);
    // Ancestor records are readable through native activation stores, but
    // their definitions need not be in this activation's transient stores.
    let mut ancestor = session.clone();
    ancestor.id = SessionId::new();
    ancestor.harness_id = HarnessId::new();
    raw.session_store
        .add_session(ancestor.clone())
        .await
        .unwrap();
    let view = projected
        .session_store
        .get_session(ancestor.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::to_value(view).unwrap(),
        serde_json::to_value(ancestor.clone()).unwrap()
    );
    assert!(compat.activation_view(ancestor.id).is_none());
    assert_eq!(
        serde_json::to_value(
            raw.session_store
                .get_session(session.id)
                .await
                .unwrap()
                .unwrap()
        )
        .unwrap(),
        expected
    );
}

#[tokio::test]
async fn fresh_native_reader_uses_prepared_definitions_and_current_session_for_status() {
    let raw = HostBackends::in_memory();
    let mut session = ExecutionSession::with_own_workspace(SessionId::new(), HarnessId::new());
    session.mcp_servers = servers(&["original"], "session");
    raw.session_store
        .add_session(session.clone())
        .await
        .unwrap();
    // SqliteHosting.reader creates fresh empty transient definition stores.
    let reader = HostBackends::in_memory()
        .with_session_store(raw.session_store.clone())
        .with_storage_store(raw.storage_store.clone());
    let compat = compat(CapabilityRegistry::new());
    assert!(
        compat
            .statuses(&reader, session.id)
            .await
            .unwrap()
            .is_empty()
    );
    let projected = compat.decorate(raw.clone());
    projected
        .harness_store
        .add_harness(session.harness_id, HarnessDefinition::default())
        .await
        .unwrap();
    projected
        .session_store
        .add_session(session.clone())
        .await
        .unwrap();
    assert_eq!(
        compat.statuses(&reader, session.id).await.unwrap(),
        vec![("original".into(), McpDiscoveryStatus::Pending)]
    );
    session.mcp_servers = servers(&["changed"], "edited");
    raw.session_store
        .add_session(session.clone())
        .await
        .unwrap();
    assert_eq!(
        compat.statuses(&reader, session.id).await.unwrap(),
        vec![("changed".into(), McpDiscoveryStatus::Pending)]
    );
    assert!(
        reader
            .harness_store
            .get_harness(session.harness_id)
            .await
            .unwrap()
            .is_none()
    );
}

#[test]
fn retained_activation_views_are_bounded_and_do_not_retain_compat() {
    let raw = HostBackends::in_memory();
    let compat = compat(CapabilityRegistry::new());
    let first = SessionId::new();
    compat.remember_activation(first, &raw);
    let mut last = first;
    for _ in 0..MAX_ACTIVATION_VIEWS {
        last = SessionId::new();
        compat.remember_activation(last, &raw);
    }
    assert!(compat.activation_view(first).is_none());
    assert!(compat.activation_view(last).is_some());
    assert_eq!(
        compat.activation_views.read().unwrap().len(),
        MAX_ACTIVATION_VIEWS
    );
    let weak = Arc::downgrade(&compat);
    let projected = compat.decorate(raw);
    drop(projected);
    drop(compat);
    assert!(weak.upgrade().is_none());
}

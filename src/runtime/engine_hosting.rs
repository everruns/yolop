//! Host services and process routing around the native SQLite authority.
//!
//! All consistency and execution ports remain those of SqliteHosting. A Yolop
//! process claims only the build whose workspace and UI services it opened;
//! another process may serve a different build in the same authority database.
use async_trait::async_trait;
use everruns::hosting::spi::{HostBackends, session_commands::SessionCommand};
use everruns::hosting::*;
use everruns_contracts::{
    Result,
    typed_id::{MessageId, SessionId},
};
use everruns_core::connection_services::UserConnectionResolver;
use everruns_core::{ObservingTaskRegistry, TaskTransitionObserver};
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;

#[derive(Clone)]
pub struct YolopHosting {
    pub native: SqliteHosting,
    pub build: AgentBuild,
    pub root: SessionId,
    pub restore_gate: Arc<tokio::sync::RwLock<()>>,
    pub coordination: crate::capabilities::CoordinationStore,
    pub connections: Arc<dyn UserConnectionResolver>,
    pub observer: Arc<dyn TaskTransitionObserver>,
    pub schedules: Arc<dyn everruns_core::session_services::SessionScheduleStore>,
    pub platform: Arc<super::engine_platform::PlatformServices>,
}
impl YolopHosting {
    fn enrich(&self, mut backends: HostBackends) -> HostBackends {
        backends = backends.with_connection_resolver(self.connections.clone());
        if backends.schedule_store_factory.is_none() {
            let schedules = self.schedules.clone();
            backends = backends.with_schedule_store_factory(Arc::new(move |_| schedules.clone()));
        }
        if let Some(registry) = backends.session_task_registry.take() {
            backends = backends.with_session_task_registry(Arc::new(
                ObservingTaskRegistry::new(registry).with_observer(self.observer.clone()),
            ));
        }
        if let (Some(factory), Some(tasks)) = (
            backends.tool_context_extensions_factory.clone(),
            backends.session_task_registry.clone(),
        ) {
            use everruns_capabilities::PlatformHostBackendsExt;
            let services = self.platform.clone();
            backends = backends.with_platform_store_factory(Arc::new(move |org, session| {
                let extensions = factory(org, session);
                let native = extensions
                    .get::<everruns_capabilities::PlatformStoreExt>()
                    .expect("native hosting installs the platform store");
                Arc::new(super::engine_platform::PlatformAdapter {
                    inner: native.0.clone(),
                    services: services.clone(),
                    tasks: tasks.clone(),
                })
            }));
        }
        backends
    }
}
#[async_trait]
impl SessionHosting for YolopHosting {
    async fn command(&self, session: SessionId, command: SessionCommand) -> Result<Value> {
        self.native.command(session, command).await
    }
    async fn query_snapshot(&self, session: SessionId) -> Result<SessionQuerySnapshot> {
        self.native.query_snapshot(session).await
    }
    async fn history_lineage(&self, session: SessionId) -> Result<HistoryLineage> {
        self.native.history_lineage(session).await
    }
    async fn status(&self, session: SessionId) -> Result<HostingStatus> {
        self.native.status(session).await
    }
    async fn receipt(
        &self,
        session: SessionId,
        message: MessageId,
    ) -> Result<Option<AdmissionReceipt>> {
        self.native.receipt(session, message).await
    }
    fn reader(&self, session: SessionId) -> HostBackends {
        self.enrich(self.native.reader(session))
    }
    async fn record(&self, session: SessionId, key: &str) -> Result<Option<Value>> {
        self.native.record(session, key).await
    }
    async fn claim(
        &self,
        session: SessionId,
        holder: &str,
        ttl: Duration,
    ) -> Result<Option<SessionActivation>> {
        let intent = self
            .platform
            .session_dir
            .join(crate::session_state::checkpoint::RESTORE_INTENT_FILE);
        if intent.exists() {
            return Ok(None);
        }
        let mut cursor = session;
        loop {
            let binding = self.native.status(cursor).await?.binding;
            if binding.behavior != self.build {
                return Ok(None);
            }
            if cursor == self.root {
                break;
            }
            if self
                .coordination
                .thread(cursor)
                .map_err(|error| everruns_contracts::AgentLoopError::store(error.to_string()))?
                .is_some()
            {
                return Ok(None);
            }
            let Some(parent) = binding
                .metadata
                .get("parent_session_id")
                .and_then(Value::as_str)
                .and_then(|value| value.parse().ok())
            else {
                return Ok(None);
            };
            cursor = parent;
        }
        let Ok(workspace) = self.restore_gate.clone().try_read_owned() else {
            return Ok(None);
        };
        let Some(mut activation) = self.native.claim(session, holder, ttl).await? else {
            return Ok(None);
        };
        activation.epoch = Arc::new(WorkspaceEpoch {
            inner: activation.epoch,
            _workspace: workspace,
        });
        Ok(Some(activation))
    }
    fn process_scheduler(&self) -> bool {
        false
    }
    fn changes(&self, session: SessionId) -> Option<Arc<tokio::sync::Notify>> {
        self.native.changes(session)
    }
    async fn prepare_activation(
        &self,
        engine: &everruns::Engine,
        session: SessionId,
        environment: &everruns::Environment,
        backends: HostBackends,
    ) -> Result<HostBackends> {
        Ok(self.enrich(
            self.native
                .prepare_activation(engine, session, environment, backends)
                .await?,
        ))
    }
    async fn import_legacy(&self, session: SessionId, image: LegacySessionImage) -> Result<bool> {
        self.native.import_legacy(session, image).await
    }
}

struct WorkspaceEpoch {
    inner: Arc<dyn SessionEpoch>,
    _workspace: tokio::sync::OwnedRwLockReadGuard<()>,
}
#[async_trait]
impl SessionEpoch for WorkspaceEpoch {
    fn renew_poke(&self) -> Option<Arc<tokio::sync::Notify>> {
        self.inner.renew_poke()
    }
    async fn schedule(&self, at: Option<i64>) -> Result<()> {
        self.inner.schedule(at).await
    }
    fn backends(&self) -> HostBackends {
        self.inner.backends()
    }
    fn actor_store(&self) -> Arc<dyn spi::actor::SessionActorStore> {
        self.inner.actor_store()
    }
    fn observe_fence(&self, fence: &spi::actor::ActorFence) {
        self.inner.observe_fence(fence);
    }
    async fn record(&self, key: &str) -> Result<Option<Value>> {
        self.inner.record(key).await
    }
    async fn records(&self, prefix: &str) -> Result<Vec<(String, Value)>> {
        self.inner.records(prefix).await
    }
    async fn commit(&self, key: &str, value: Value, absent: bool) -> Result<bool> {
        self.inner.commit(key, value, absent).await
    }
    async fn suspend(&self, reason: &str) -> Result<()> {
        self.inner.suspend(reason).await
    }
}

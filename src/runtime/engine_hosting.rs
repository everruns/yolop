//! Host services and process routing around the native SQLite authority.
//!
//! All consistency and execution ports remain those of SqliteHosting. A Yolop
//! process claims only the build whose workspace and UI services it opened;
//! each session, including a linked child, has its own authority database.
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
    pub mcp_readiness: Option<Arc<super::mcp_compat::McpCompat>>,
    pub native: SqliteHosting,
    pub build: AgentBuild,
    pub root: SessionId,
    pub restore_gate: Arc<tokio::sync::RwLock<()>>,
    pub coordination: crate::capabilities::CoordinationStore,
    pub connections: Arc<dyn UserConnectionResolver>,
    pub observer: Arc<dyn TaskTransitionObserver>,
    pub platform: Arc<super::engine_platform::PlatformServices>,
}
impl YolopHosting {
    /// Upstream gap (everruns 0.52.0, unchanged on everruns main): the local
    /// schedule runner admits a fired schedule as a plain user message
    /// (`HostedSessionRunner::deliver_occurrence`), without the `Schedule`
    /// origin the server's scheduler writes (`fire_origin`). The engine's
    /// credit-shortfall wait recognizes its check only by that origin, so
    /// without it the first check runs as an ordinary turn, finds itself still
    /// queued, schedules nothing, and the wait silently ends after one check.
    /// Write the origin here, only on the admission whose id is the
    /// occurrence id the runner derives for one of this session's schedules;
    /// user input never carries such an id. Drop once everruns writes it.
    async fn stamp_schedule_origin(
        &self,
        session: SessionId,
        message: &mut everruns_core::RuntimeMessage,
    ) {
        if message.role != everruns_core::RuntimeMessageRole::User || message.origin().is_some() {
            return;
        }
        let Some(text) = message.text().map(str::to_owned) else {
            return;
        };
        let store = self.native.schedule_store(
            session,
            everruns_contracts::runtime::organization::DEFAULT_ORG_ID,
        );
        let Ok(schedules) = store.list_schedules(session).await else {
            return;
        };
        if let Some(schedule) = schedules
            .iter()
            .find(|s| s.description == text && occurrence_message_id(s) == message.id)
        {
            everruns_contracts::message_origin::MessageOrigin::schedule(
                schedule.id.to_string(),
                &schedule.description,
            )
            .write(&mut message.metadata);
        }
    }

    fn enrich(&self, mut backends: HostBackends) -> HostBackends {
        backends = backends.with_connection_resolver(self.connections.clone());
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
    async fn command(&self, session: SessionId, mut command: SessionCommand) -> Result<Value> {
        if let SessionCommand::Admit { message, .. } = &mut command {
            self.stamp_schedule_origin(session, message).await;
        }
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
    fn definitions(&self) -> Option<&dyn everruns::hosting::AgentDefinitions> {
        self.native.definitions()
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
        let backends = self.enrich(
            self.native
                .prepare_activation(engine, session, environment, backends)
                .await?,
        );
        Ok(match &self.mcp_readiness {
            Some(readiness) => readiness.decorate(backends),
            None => backends,
        })
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

/// The admission id `everruns::local::LocalScheduleRunner` gives one schedule
/// occurrence (`schedule_runner::occurrence_message_id`, crate-private there).
/// Kept byte-identical; `engine_credit_wait_check_is_recognized_when_it_fires`
/// fails if it drifts.
fn occurrence_message_id(schedule: &everruns_core::session_schedule::SessionSchedule) -> MessageId {
    const NAMESPACE: uuid::Uuid = uuid::Uuid::from_u128(0x6f1d_2c4e_8a3b_5d17_9e0f_4b2a_7c61_d385);
    let occurrence = format!(
        "{}:{}:{}",
        schedule.id,
        schedule.trigger_count,
        schedule
            .next_trigger_at
            .map_or(0, |time| time.timestamp_millis())
    );
    MessageId::from_uuid(uuid::Uuid::new_v5(&NAMESPACE, occurrence.as_bytes()))
}

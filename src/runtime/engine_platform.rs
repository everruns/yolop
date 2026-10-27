//! Product routing and notification policy around native delegation services.
use super::{
    background_wake::{WakeRunner, WakeSender},
    engine::SessionHost,
};
use async_trait::async_trait;
use everruns_capabilities::{PlatformCreateSessionRequest, PlatformMessage, PlatformStore};
use everruns_contracts::{
    Result,
    typed_id::{AgentId, HarnessId, MessageId, SessionId, SessionParticipantId},
};
use everruns_core::{AgentDefinition, ExecutionSession, HarnessDefinition, SessionTaskRegistry};
use std::sync::{Arc, OnceLock, Weak};

pub struct PlatformServices {
    pub instance: MessageId,
    pub session_dir: std::path::PathBuf,
    pub host: Arc<OnceLock<Weak<SessionHost>>>,
    pub router: Arc<everruns::local::HostRoutedRunner<WakeRunner>>,
    pub settings: Arc<crate::config::SettingsStore>,
    pub wakes: WakeSender,
    pub notified: std::sync::Mutex<std::collections::HashSet<MessageId>>,
}

impl PlatformServices {
    pub fn observe_input(&self, input: &everruns_core::RuntimeMessage) {
        let mut notified = self.notified.lock().expect("admission notification lock");
        if notified.contains(&input.id) {
            return;
        }
        if input
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.get("yolop.frontend_host"))
            .and_then(serde_json::Value::as_str)
            .is_some_and(|host| host == self.instance.to_string())
        {
            notified.insert(input.id);
            return;
        }
        let wake = super::background_wake::WakeMessage::admitted(
            input.id,
            input.text().unwrap_or_default().to_string(),
        )
        .with_handoff(
            input
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get(super::background_wake::HANDOFF_METADATA_KEY))
                .and_then(|value| serde_json::from_value(value.clone()).ok()),
        );
        if self.wakes.send(wake).is_ok() {
            notified.insert(input.id);
        }
    }
}

pub struct PlatformAdapter {
    pub inner: Arc<dyn PlatformStore>,
    pub services: Arc<PlatformServices>,
    pub tasks: Arc<dyn SessionTaskRegistry>,
}

impl PlatformAdapter {
    fn host(&self) -> Result<Arc<SessionHost>> {
        self.services
            .host
            .get()
            .and_then(Weak::upgrade)
            .ok_or_else(|| {
                everruns_contracts::AgentLoopError::config("Yolop Engine host unavailable")
            })
    }
}

#[async_trait]
impl PlatformStore for PlatformAdapter {
    fn for_execution(&self, id: uuid::Uuid) -> Option<Arc<dyn PlatformStore>> {
        Some(Arc::new(Self {
            inner: self
                .inner
                .for_execution(id)
                .unwrap_or_else(|| self.inner.clone()),
            services: self.services.clone(),
            tasks: self.tasks.clone(),
        }))
    }
    async fn get_harness(&self, id: HarnessId) -> Result<Option<HarnessDefinition>> {
        self.inner.get_harness(id).await
    }
    async fn get_agent_by_id(&self, id: AgentId) -> Result<Option<AgentDefinition>> {
        self.inner.get_agent_by_id(id).await
    }
    async fn create_session_with_options(
        &self,
        mut request: PlatformCreateSessionRequest,
    ) -> Result<ExecutionSession> {
        // Linked subagents always start fresh. The shared tool exposes seed
        // for detached sessions too; Yolop has never applied it to linked work.
        if request.parent_session_id.is_some() {
            request.seed = everruns_core::session::SessionSeedMode::Fresh;
        }
        let parent = request.parent_session_id;
        let child = self.inner.create_session_with_options(request).await?;
        if let Some(parent) = parent {
            let runtime = self.host()?;
            use everruns::hosting::SessionHosting;
            let configuration = runtime
                .profile
                .native
                .query_snapshot(parent)
                .await?
                .configuration;
            if let Some(model) = configuration.model.flatten() {
                runtime
                    .engine
                    .resume(child.id)
                    .await
                    .map_err(host_error)?
                    .set_model(everruns::Model::on(model.provider.as_str(), model.model))
                    .await
                    .map_err(host_error)?;
            }
            if let Some(effort) = configuration.reasoning_effort {
                runtime
                    .engine
                    .resume(child.id)
                    .await
                    .map_err(host_error)?
                    .set_reasoning_effort(effort)
                    .await
                    .map_err(host_error)?;
            }
        }
        Ok(child)
    }
    async fn get_session_by_id(&self, id: SessionId) -> Result<Option<ExecutionSession>> {
        self.inner.get_session_by_id(id).await
    }
    async fn add_agent_session_participant(
        &self,
        session: SessionId,
        agent: AgentId,
        reason: &str,
    ) -> Result<SessionParticipantId> {
        self.inner
            .add_agent_session_participant(session, agent, reason)
            .await
    }
    async fn send_message(&self, id: SessionId, content: &str) -> Result<()> {
        self.send_message_with_causal_context(id, content, &Default::default())
            .await
    }
    async fn send_message_with_causal_context(
        &self,
        id: SessionId,
        content: &str,
        causes: &everruns_contracts::runtime::causal::CausalContext,
    ) -> Result<()> {
        if let Some((routing, thread)) = self.services.router.inner().thread(id).await? {
            return routing.deliver(&thread, content).map_err(host_error);
        }
        let wake = self.services.router.inner().wake_message(id, content).await;
        let task_ids = wake.task_ids();
        if let Some(task_id) = task_ids.first() {
            let runtime = self.host()?;
            if super::background_wake::completion_already_observed(
                &wake,
                &runtime.session_events(id).await.map_err(host_error)?,
            ) {
                let _ = self.services.wakes.send(wake.observed());
                return Ok(());
            }
            if !self.services.settings.snapshot().proactive_wake_enabled() {
                let _ = self.services.wakes.send(wake);
                return Ok(());
            }
            let task = self
                .tasks
                .get(id, task_id)
                .await?
                .ok_or_else(|| host_error("background task disappeared"))?;
            let identity = format!("{}:{}:{}:{}", id, task.id, task.attempt, task.state);
            let namespace = uuid::Uuid::from_u128(0x796f6c6f_702d_7461_736b_2d77616b6531);
            let input_id =
                MessageId::from_uuid(uuid::Uuid::new_v5(&namespace, identity.as_bytes()));
            let wake = wake.resolve_artifacts(runtime.files.clone().as_ref());
            let input = super::background_wake::input_for_wake(&wake).with_causal_context(causes);
            runtime
                .engine
                .resume(id)
                .await
                .map_err(host_error)?
                .send_with_id(input_id, input)
                .await
                .map_err(host_error)?;
            return Ok(());
        }
        self.inner
            .send_message_with_causal_context(id, content, causes)
            .await
    }
    async fn get_messages(
        &self,
        id: SessionId,
        limit: Option<usize>,
    ) -> Result<Vec<PlatformMessage>> {
        self.inner.get_messages(id, limit).await
    }
    async fn wait_for_idle(&self, id: SessionId, timeout: Option<u64>) -> Result<String> {
        self.inner.wait_for_idle(id, timeout).await
    }
    async fn get_agent_harness_id(&self, id: AgentId) -> Result<Option<HarnessId>> {
        self.inner.get_agent_harness_id(id).await
    }
    async fn set_session_archived(
        &self,
        id: SessionId,
        archived: bool,
        reason: &str,
    ) -> Result<()> {
        self.inner.set_session_archived(id, archived, reason).await
    }
}
fn host_error(error: impl std::fmt::Display) -> everruns_contracts::AgentLoopError {
    everruns_contracts::AgentLoopError::config(error.to_string())
}

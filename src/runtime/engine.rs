//! Yolop's application adapter over registered Engine sessions.
//!
//! No turn runner or writable conversation store is owned here. All input,
//! execution, cancellation, controls and history selection go through Engine.

use anyhow::{Context, Result};
use everruns::Engine;
use everruns::Turn;
#[cfg(test)]
use everruns::hosting::AgentBuild;
use everruns::hosting::{SessionHosting, SqliteHosting};
use everruns_contracts::{CapabilityRef, typed_id::SessionId};
#[cfg(test)]
use everruns_core::InputMessage;
use everruns_core::{Event, RuntimeMessage, SessionFileSystem};
use std::path::PathBuf;
use std::sync::Arc;

use super::session_log::{JsonlEventEmitter, messages_from_events};

#[derive(serde::Deserialize)]
pub(super) struct RetainedBehavior {
    pub instructions: String,
    pub model: everruns_contracts::model_spec::ModelSpec,
    pub capabilities: Vec<CapabilityRef>,
}

pub struct SessionHost {
    pub engine: Engine,
    pub profile: super::engine_hosting::YolopHosting,
    pub(crate) files: Arc<dyn SessionFileSystem>,
    selection: Arc<std::sync::RwLock<super::ProviderChoice>>,
    default_model: everruns_contracts::model_spec::ModelSpec,
    events: Arc<JsonlEventEmitter>,
    export_path: PathBuf,
    refresh: tokio::sync::Mutex<()>,
    registered: std::sync::RwLock<std::collections::HashSet<String>>,
    shutdown: everruns::CancellationToken,
    supervisor: std::sync::Mutex<Option<tokio::task::AbortHandle>>,
}

impl Drop for SessionHost {
    fn drop(&mut self) {
        self.shutdown.cancel();
        if let Some(supervisor) = self.supervisor.lock().expect("supervisor lock").take() {
            supervisor.abort();
        }
        self.events.release_session_lease();
    }
}

impl SessionHost {
    pub fn new(
        engine: Engine,
        profile: super::engine_hosting::YolopHosting,
        files: Arc<dyn SessionFileSystem>,
        selection: Arc<std::sync::RwLock<super::ProviderChoice>>,
        default_model: everruns_contracts::model_spec::ModelSpec,
        events: Arc<JsonlEventEmitter>,
        registered: std::collections::HashSet<String>,
    ) -> Arc<Self> {
        Arc::new(Self {
            engine,
            files,
            selection,
            default_model,
            events,
            export_path: super::session_log::session_log_path(&profile.platform.session_dir),
            profile,
            refresh: tokio::sync::Mutex::new(()),
            registered: std::sync::RwLock::new(registered),
            shutdown: everruns::CancellationToken::new(),
            supervisor: std::sync::Mutex::new(None),
        })
    }

    /// Lifecycle and startup reads recover UI observation when a lossy listener
    /// misses admission. Execution is already owned by the native supervisor.
    pub async fn observe_pending_admissions(&self) -> Result<()> {
        let imported = self.imported_boundary().await?;
        let session = self.engine.resume(self.profile.root).await?;
        for event in self.session_events(self.profile.root).await? {
            if event.sequence.is_some_and(|sequence| sequence <= imported) {
                continue;
            }
            let everruns_core::EventData::InputMessage(input) = event.data else {
                continue;
            };
            let Some(admission) = session.admission(input.message.id).await? else {
                continue;
            };
            if matches!(
                admission.state().await?,
                everruns::hosting::AdmissionState::Complete { .. }
            ) {
                continue;
            }
            self.profile.platform.observe_input(&input.message);
        }
        Ok(())
    }

    async fn imported_boundary(&self) -> Result<i32> {
        Ok(self
            .profile
            .native
            .record(self.profile.root, "cutover")
            .await?
            .and_then(|record| {
                record
                    .pointer("/source/boundary/last_sequence")
                    .and_then(serde_json::Value::as_i64)
            })
            .map(i32::try_from)
            .transpose()?
            .unwrap_or(0))
    }

    pub fn start_supervisor(self: &Arc<Self>) {
        let hosting = self.profile.native.clone();
        let engine = self.engine.clone();
        let shutdown = self.shutdown.clone();
        let supervisor = tokio::spawn(async move {
            if let Err(error) = hosting.supervise(engine, shutdown).await {
                tracing::error!(%error, "Engine supervisor stopped");
            }
        });
        *self.supervisor.lock().expect("supervisor lock") = Some(supervisor.abort_handle());
    }

    pub(super) async fn turn_result(
        &self,
        id: SessionId,
        mut turn: everruns::Turn,
    ) -> Result<Turn> {
        if !turn.success
            && let Some(error) = self
                .session_events(id)
                .await?
                .iter()
                .rev()
                .find_map(|event| {
                    if event
                        .context
                        .turn_id
                        .is_some_and(|turn_id| turn_id.to_string() == turn.turn_id)
                    {
                        match &event.data {
                            everruns_core::EventData::ReasonCompleted(reason) => {
                                reason.error.clone()
                            }
                            _ => None,
                        }
                    } else {
                        None
                    }
                })
        {
            turn.error = Some(error);
        }
        Ok(turn)
    }

    pub async fn events(&self) -> Result<Vec<Event>> {
        let mut events = self.session_events(self.profile.root).await?;
        let turns: std::collections::HashSet<_> = events
            .iter()
            .filter_map(|event| event.context.turn_id)
            .collect();
        events.extend(
            self.events
                .collected_events()
                .await
                .into_iter()
                .filter(|event| {
                    event.sequence.is_none()
                        && event
                            .context
                            .turn_id
                            .is_none_or(|turn| turns.contains(&turn))
                }),
        );
        Ok(events)
    }

    pub async fn session_events(&self, id: SessionId) -> Result<Vec<Event>> {
        read_events(&self.profile.native, id).await
    }

    pub async fn messages(&self, id: SessionId) -> Result<Vec<RuntimeMessage>> {
        Ok(messages_from_events(&self.session_events(id).await?))
    }

    /// Refresh the frontend cache from authority reads. The observer is lossy;
    /// neither it nor a JSONL file determines what the next turn can see.
    pub async fn synchronize(&self) -> Result<()> {
        let _guard = self.refresh.lock().await;
        let events = self.session_events(self.profile.root).await?;
        if !events.is_empty() {
            self.events.materializer().ensure()?;
        }
        self.events.replace_collected_events(events.clone()).await;
        if let Some(title) = super::latest_session_title(&events) {
            super::session_log::update_session_workspace_title(
                self.export_path.parent().expect("session directory"),
                &title,
            )?;
        }
        self.events.observe_sequence(
            events
                .iter()
                .filter_map(|event| event.sequence)
                .max()
                .unwrap_or(0),
        );
        if !events.is_empty() || self.export_path.exists() {
            self.profile
                .native
                .rebuild_jsonl_export(self.profile.root, &self.export_path)?;
        }
        Ok(())
    }

    pub(super) async fn sync_model(&self, id: SessionId) -> Result<()> {
        if id != self.profile.root {
            return Ok(());
        }
        let selection = self
            .selection
            .read()
            .expect("provider lock poisoned")
            .clone();
        let desired = selection.model_without_stored_key().model_spec();
        let effort = selection
            .reasoning_effort()
            .map(|value| {
                everruns_contracts::ReasoningEffort::parse(value)
                    .context("invalid selected reasoning effort")
            })
            .transpose()?;
        let configuration = self.profile.native.query_snapshot(id).await?.configuration;
        let effective = configuration
            .model
            .flatten()
            .unwrap_or_else(|| self.default_model.clone());
        if effective != desired {
            self.engine
                .resume(id)
                .await?
                .set_model(everruns::Model::on(
                    desired.provider.as_str(),
                    desired.model,
                ))
                .await?;
        }
        if configuration.reasoning_effort.flatten() != effort {
            self.engine
                .resume(id)
                .await?
                .set_reasoning_effort(effort)
                .await?;
        }
        Ok(())
    }

    pub fn is_capability_registered(&self, id: &str) -> bool {
        self.registered
            .read()
            .expect("capability registry lock")
            .contains(id)
    }

    pub fn register_capability(
        &self,
        capability: Arc<dyn everruns_core::Capability>,
    ) -> Result<()> {
        let id = capability.id().to_string();
        self.engine
            .register_shared_capability(capability)
            .context("register Engine capability")?;
        self.registered
            .write()
            .expect("capability registry lock")
            .insert(id);
        Ok(())
    }

    #[cfg(test)]
    pub fn session_task_registry(
        &self,
        _org: i64,
    ) -> Option<Arc<dyn everruns_core::SessionTaskRegistry>> {
        self.profile.reader(self.profile.root).session_task_registry
    }

    #[cfg(test)]
    pub fn schedule_store(
        &self,
        org: i64,
    ) -> Option<Arc<dyn everruns_core::session_services::SessionScheduleStore>> {
        self.profile
            .reader(self.profile.root)
            .schedule_store_factory
            .map(|factory| factory(org))
    }

    #[cfg(test)]
    pub fn connection_resolver(
        &self,
        _org: i64,
    ) -> Option<Arc<dyn everruns_core::connection_services::UserConnectionResolver>> {
        self.profile.reader(self.profile.root).connection_resolver
    }
}

/// Read active canonical history before an Engine session has been reconstructed.
pub(super) async fn read_events(
    hosting: &impl SessionHosting,
    id: SessionId,
) -> Result<Vec<Event>> {
    use everruns_core::host::{EventReadLimit, EventReadRequest};
    let log = hosting.reader(id).event_log;
    let mut events = Vec::new();
    let mut cursor = None;
    loop {
        let mut request = EventReadRequest::new(id, EventReadLimit::new(256)?);
        if let Some(next) = cursor.take() {
            request = request.with_cursor(next);
        }
        let page = log.read_page(request).await?;
        events.extend(page.events);
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => return Ok(events),
        }
    }
}

pub(super) fn host_error(error: impl std::fmt::Display) -> everruns_contracts::AgentLoopError {
    everruns_contracts::AgentLoopError::config(error.to_string())
}

pub(super) struct CancelOnDrop(pub Option<everruns::CancellationToken>);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if let Some(cancellation) = &self.0 {
            cancellation.cancel();
        }
    }
}

pub struct FrontendObserver {
    pub root: SessionId,
    pub platform: Arc<super::engine_platform::PlatformServices>,
    pub events: Arc<JsonlEventEmitter>,
}
#[async_trait::async_trait]
impl everruns_core::EventListener for FrontendObserver {
    async fn on_event(&self, event: &Event) {
        if event.session_id == self.root {
            if let everruns_core::EventData::InputMessage(input) = &event.data {
                self.platform.observe_input(&input.message);
            }
            if let Some(sequence) = event.sequence {
                self.events.observe_sequence(sequence);
            }
        }
        let _ = everruns_core::host::EventSink::try_send(self.events.as_ref(), event.clone());
    }
}

impl SessionHost {
    pub async fn begin_checkpoint(
        &self,
        checkpoints: &crate::session_state::checkpoint::CheckpointManager,
    ) -> Result<()> {
        self.observe_pending_admissions().await?;
        self.synchronize().await?;
        let events = self.events().await?;
        let imported = self.imported_boundary().await?;
        let session = self.engine.resume(self.profile.root).await?;
        for event in events.iter().rev() {
            let everruns_core::EventData::InputMessage(input) = &event.data else {
                continue;
            };
            let Some(admission) = session.admission(input.message.id).await? else {
                continue;
            };
            let Some(assignment) = admission.assignment().await? else {
                continue;
            };
            if assignment.steered
                || matches!(
                    admission.state().await?,
                    everruns::hosting::AdmissionState::Complete { .. }
                )
            {
                continue;
            }
            let turn = assignment.turn_id;
            let mut before = Vec::new();
            for prior in &events {
                let include = if prior.sequence.is_some_and(|sequence| sequence <= imported) {
                    true
                } else if let everruns_core::EventData::InputMessage(input) = &prior.data {
                    let assignment = match session.admission(input.message.id).await? {
                        Some(admission) => admission.assignment().await?,
                        None => None,
                    };
                    assignment.is_some_and(|assigned| assigned.turn_id != turn)
                } else {
                    prior.context.turn_id.is_none_or(|id| id != turn)
                };
                if include && let Some(sequence) = prior.sequence {
                    match before.last_mut() {
                        Some(everruns::hosting::SequenceRange { last, .. })
                            if *last + 1 == sequence =>
                        {
                            *last = sequence
                        }
                        _ => before.push(everruns::hosting::SequenceRange::new(sequence, sequence)),
                    }
                }
            }
            checkpoints.begin_native_turn(
                input.message.text().unwrap_or_default(),
                event.sequence.context("admission has no sequence")?,
                Some(before),
            )?;
            return Ok(());
        }
        anyhow::bail!("Engine turn has no retained input assignment")
    }
}

/// Frontend catalog reads choose their own session-scoped authority ports.
/// Canonical mutations remain available only to Engine's claimed epochs.
pub struct AuthorityCatalog {
    pub hosting: SqliteHosting,
    pub template: std::sync::OnceLock<everruns_core::ExecutionSession>,
}
#[async_trait::async_trait]
impl everruns_core::SessionStore for AuthorityCatalog {
    async fn get_session(
        &self,
        id: SessionId,
    ) -> everruns_contracts::Result<Option<everruns_core::ExecutionSession>> {
        let status = match self.hosting.status(id).await {
            Ok(status) => status,
            Err(everruns_contracts::AgentLoopError::SessionNotFound(_)) => return Ok(None),
            Err(error) => return Err(error),
        };
        if let Some(record) = self
            .hosting
            .reader(id)
            .session_store
            .get_session(id)
            .await?
        {
            return Ok(Some(record));
        }
        // Creation can precede the first activation that materializes the
        // runtime catalog. This projection is for host presentation only.
        Ok(self.template.get().cloned().map(|mut record| {
            record.id = id;
            record.parent_session_id = status
                .binding
                .metadata
                .get("parent_session_id")
                .and_then(serde_json::Value::as_str)
                .and_then(|value| value.parse().ok());
            record
        }))
    }
}

impl SessionHost {
    /// Completion review must not race an independently admitted Engine turn
    /// or spend the simulator/provider's next call before a just-finished task
    /// has delivered its result. Task facts and receipts remain authoritative.
    pub async fn has_owed_work(
        &self,
        completed: everruns_contracts::typed_id::TurnId,
    ) -> Result<bool> {
        let events = self.session_events(self.profile.root).await?;
        let imported = self.imported_boundary().await?;
        let session = self.engine.resume(self.profile.root).await?;
        for event in events.iter().rev() {
            if event.sequence.is_some_and(|sequence| sequence <= imported) {
                continue;
            }
            if let everruns_core::EventData::InputMessage(input) = &event.data {
                // Imported historical inputs have no outstanding native receipt.
                let Some(admission) = session.admission(input.message.id).await? else {
                    continue;
                };
                let Some(assignment) = admission.assignment().await? else {
                    return Ok(true);
                };
                if assignment.turn_id != completed
                    && !matches!(
                        admission.state().await?,
                        everruns::hosting::AdmissionState::Complete { .. }
                    )
                {
                    return Ok(true);
                }
            }
        }
        let started = events
            .iter()
            .find(|event| {
                event.context.turn_id == Some(completed)
                    && matches!(event.data, everruns_core::EventData::TurnStarted(_))
            })
            .map(|event| event.ts);
        if let (Some(started), Some(tasks)) = (
            started,
            self.profile.reader(self.profile.root).session_task_registry,
        ) {
            for task in tasks.list(self.profile.root, None).await? {
                if task.kind == "background_tool"
                    && task.state.is_terminal()
                    && task.created_at >= started
                    && let Some(handoff) = super::background_wake::task_handoff(task)
                {
                    let wake = super::background_wake::WakeMessage::handoff_message(
                        String::new(),
                        super::background_wake::WakeHandoff {
                            version: 1,
                            tasks: vec![handoff],
                            omitted_tasks: 0,
                        },
                    );
                    if !super::background_wake::completion_already_observed(&wake, &events) {
                        return Ok(true);
                    }
                }
            }
        }
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::super::{BuildOptions, ProviderChoice, build_with_options};
    use super::*;
    use crate::config::SettingsStore;
    use crate::session_state::checkpoint::RestoreMode;

    #[tokio::test]
    async fn native_resume_bootstraps_transcript_when_jsonl_export_is_stale() {
        let workspace = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let settings = Arc::new(SettingsStore::open(data.path().join("settings.toml")));
        let build = |session| {
            build_with_options(
                workspace.path().into(),
                ProviderChoice::Sim,
                session,
                data.path().into(),
                settings.clone(),
                BuildOptions::default(),
            )
        };
        let built = build(None).await.unwrap();
        let id = built.handles.session_id;
        let turn = built
            .handles
            .run_checkpointed_turn(built.model.input_message("retained question"))
            .await
            .unwrap();
        assert!(turn.success, "{turn:?}");
        let export = built.handles.host.export_path.clone();
        drop(built);
        // A crash can leave the presentation export behind the committed turn.
        std::fs::write(export, b"").unwrap();
        let resumed = build(Some(id)).await.unwrap();
        assert!(resumed.startup.replayed_events > 0);
        let session =
            super::super::session::Session::new(resumed.handles.clone(), resumed.model.clone());
        let lines = session
            .replayed_lines(resumed.startup.replayed_events)
            .await
            .unwrap();
        assert!(
            lines
                .iter()
                .any(|line| line.text.contains("retained question")),
            "{lines:?}"
        );
        assert!(
            lines.iter().any(|line| line.text.contains(&turn.response)),
            "{lines:?}"
        );
    }

    #[tokio::test]
    async fn unknown_or_missing_retained_behavior_refuses_resume_without_admission() {
        let workspace = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let key = super::super::engine_keys::open_or_create(data.path()).unwrap();
        let hosting = SqliteHosting::open(data.path().join("engine.sqlite"))
            .unwrap()
            .with_secret_keys(&key, &[])
            .unwrap();
        for version in ["v99", "v1"] {
            let id = SessionId::new();
            let build = AgentBuild::new("yolop", format!("{version}:{id}"));
            let agent = everruns::Agent::builder()
                .instructions("retained behavior")
                .model(everruns::Model::simulated("reply"))
                .build()
                .unwrap();
            let engine = Engine::builder()
                .hosting(hosting.clone())
                .register(build.clone(), agent)
                .build();
            engine.create_registered(id, &build).await.unwrap();
            let settings = Arc::new(SettingsStore::open(data.path().join("settings.toml")));
            let result = build_with_options(
                workspace.path().into(),
                ProviderChoice::Sim,
                Some(id),
                data.path().into(),
                settings,
                BuildOptions::default(),
            )
            .await;
            let error = match result {
                Err(error) => error,
                Ok(_) => panic!("unavailable behavior was silently replaced"),
            };
            assert!(
                error.to_string().contains(if version == "v99" {
                    "unsupported retained"
                } else {
                    "descriptor is unavailable"
                }),
                "{error:#}"
            );
            let status = hosting.status(id).await.unwrap();
            assert_eq!(status.binding.behavior, build);
            assert!(!status.runnable);
            let mut history = Vec::new();
            hosting.export_jsonl(id, &mut history).unwrap();
            assert!(history.is_empty());
        }
    }

    #[tokio::test]
    async fn an_observed_failed_admission_can_recover_in_a_new_scoped_engine_turn() {
        let workspace = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let settings = Arc::new(SettingsStore::open(data.path().join("settings.toml")));
        let built = build_with_options(
            workspace.path().into(),
            ProviderChoice::Sim,
            None,
            data.path().into(),
            settings,
            BuildOptions {
                llmsim_override: Some(everruns_llmsim::LlmSimConfig::scripted(vec![
                    everruns_llmsim::SimTurn::Error(everruns_llmsim::SimError::Other(
                        "previous_response_id not found".into(),
                    )),
                    everruns_llmsim::SimTurn::Assistant("recovered automatic input".into()),
                ])),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let runtime = &built.handles.host;
        runtime.events.materializer().ensure().unwrap();
        let admission = runtime
            .engine
            .resume(built.handles.session_id)
            .await
            .unwrap()
            .send("automatic recovery ask")
            .await
            .unwrap();
        let mut input = super::super::background_wake::input_for_wake(
            &super::super::background_wake::WakeMessage::admitted(
                admission.message_id.parse().unwrap(),
                "automatic recovery ask".into(),
            ),
        );
        let mut scope = super::super::turn_scope::TurnScope::new(&mut input);
        let result = built
            .handles
            .run_turn_with_reasoning_recovery(
                &built.model,
                input,
                everruns::CancellationToken::new(),
                &|_| {},
            )
            .await
            .unwrap();
        assert!(result.success, "{result:?}");
        assert_eq!(result.response, "recovered automatic input");
        let events = runtime.events().await.unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event.data, everruns_core::EventData::InputMessage(_)))
                .count(),
            2
        );
        let mut recovered_output = false;
        for event in &events {
            let accepted = scope.accepts(&built.handles.session, event).await;
            if event.context.turn_id == Some(result.turn_id.parse().unwrap()) {
                assert!(accepted, "recovery output escaped its frontend scope");
                recovered_output = true;
            }
        }
        assert!(recovered_output);
    }

    #[tokio::test]
    async fn automatic_and_child_sessions_retain_the_selected_reasoning_effort() {
        use everruns_contracts::ReasoningEffort;
        let workspace = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let settings = Arc::new(SettingsStore::open(data.path().join("settings.toml")));
        let built = build_with_options(
            workspace.path().into(),
            ProviderChoice::OpenRouter {
                model: "meta/muse-spark-1.3-contributor".into(),
                base_url: "https://openrouter.ai/api/v1".into(),
                reasoning_effort: Some("medium".into()),
            },
            None,
            data.path().into(),
            settings,
            BuildOptions::default(),
        )
        .await
        .unwrap();
        let runtime = &built.handles.host;
        let id = built.handles.session_id;
        assert_eq!(
            runtime
                .profile
                .native
                .query_snapshot(id)
                .await
                .unwrap()
                .configuration
                .reasoning_effort,
            Some(Some(ReasoningEffort::Medium))
        );
        let record = built
            .handles
            .session_store
            .get_session(id)
            .await
            .unwrap()
            .unwrap();
        let supervisor = runtime.supervisor.lock().unwrap().take().unwrap();
        supervisor.abort();
        while !supervisor.is_finished() {
            tokio::task::yield_now().await;
        }
        runtime
            .engine
            .resume(id)
            .await
            .unwrap()
            .send("prepare delegation")
            .await
            .unwrap();
        let activation = runtime
            .profile
            .claim(id, "delegation-test", std::time::Duration::from_secs(30))
            .await
            .unwrap()
            .unwrap();
        use everruns::hosting::EnvironmentFactory;
        let environment =
            super::super::engine_workspace::YolopEnvironment::new(id, runtime.files.clone())
                .unwrap()
                .open(id)
                .await
                .unwrap();
        let backends = runtime
            .profile
            .prepare_activation(
                &runtime.engine,
                id,
                &environment,
                activation.epoch.backends(),
            )
            .await
            .unwrap();
        let extensions = backends.tool_context_extensions_factory.unwrap()(
            everruns_core::host::in_process_internal_org_id(everruns_core::DEFAULT_ORG_PUBLIC_ID),
            id,
        );
        let platform = &extensions
            .get::<everruns_capabilities::PlatformStoreExt>()
            .unwrap()
            .0;
        let child = platform
            .create_session_with_options(everruns_capabilities::PlatformCreateSessionRequest {
                harness_id: record.harness_id,
                agent_id: record.agent_id,
                title: Some("reasoning child".into()),
                goal: None,
                locale: None,
                blueprint_id: None,
                blueprint_config: None,
                parent_session_id: Some(id),
                forked_from_session_id: None,
                budget_root_session_id: None,
                seed: everruns_core::session::SessionSeedMode::Fresh,
                reason: "delegated test".into(),
            })
            .await
            .unwrap();
        assert_eq!(
            runtime
                .profile
                .native
                .query_snapshot(child.id)
                .await
                .unwrap()
                .configuration
                .reasoning_effort,
            Some(Some(ReasoningEffort::Medium))
        );
        if let ProviderChoice::OpenRouter {
            reasoning_effort, ..
        } = &mut *runtime.selection.write().unwrap()
        {
            *reasoning_effort = Some("high".into());
        }
        runtime.sync_model(id).await.unwrap();
        built.handles.session.command_context().await.unwrap();
        runtime.sync_model(child.id).await.unwrap();
        runtime
            .engine
            .resume(child.id)
            .await
            .unwrap()
            .command_context()
            .await
            .unwrap();
        assert_eq!(
            runtime
                .profile
                .native
                .query_snapshot(id)
                .await
                .unwrap()
                .configuration
                .reasoning_effort,
            Some(Some(ReasoningEffort::High))
        );
        assert_eq!(
            runtime
                .profile
                .native
                .query_snapshot(child.id)
                .await
                .unwrap()
                .configuration
                .reasoning_effort,
            Some(Some(ReasoningEffort::Medium))
        );
    }

    #[test]
    fn application_sources_cannot_reintroduce_direct_core_execution() {
        let forbidden = [
            ["InProcess", "Runtime"].concat(),
            ["InProcess", "Backend"].concat(),
        ];
        let mut directories = vec![PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src")];
        while let Some(directory) = directories.pop() {
            for entry in std::fs::read_dir(directory).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    directories.push(path);
                } else if path.extension().is_some_and(|extension| extension == "rs") {
                    let source = std::fs::read_to_string(&path).unwrap();
                    for name in &forbidden {
                        assert!(
                            !source.contains(name),
                            "{} must execute through registered Engine sessions, found {name}",
                            path.display()
                        );
                    }
                }
            }
        }
    }

    #[tokio::test]
    async fn cancel_acknowledges_its_turn_and_does_not_cancel_or_stream_the_next() {
        let workspace = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let settings = Arc::new(SettingsStore::open(data.path().join("settings.toml")));
        let built = build_with_options(
            workspace.path().into(),
            ProviderChoice::Sim,
            None,
            data.path().into(),
            settings,
            BuildOptions {
                llmsim_override: Some(everruns_llmsim::LlmSimConfig::scripted(vec![
                    everruns_llmsim::SimTurn::StreamStall,
                    everruns_llmsim::SimTurn::Assistant("next answer".into()),
                ])),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let runtime = built.handles.host.clone();
        let mut input = built.model.input_message("cancel this");
        let mut first = super::super::turn_scope::TurnScope::new(&mut input);
        let worker = tokio::spawn({
            let handles = built.handles.clone();
            let cancellation = first.cancellation();
            async move { handles.execute_input(input, cancellation).await }
        });
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                if runtime
                    .events()
                    .await
                    .unwrap()
                    .iter()
                    .any(|event| matches!(event.data, everruns_core::EventData::ReasonStarted(_)))
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        first.cancel(&built.handles.session).await.unwrap();
        let cancelled = worker.await.unwrap().unwrap();
        assert_eq!(
            cancelled.stop_reason,
            everruns_contracts::turn::TurnStopReason::Cancelled
        );
        for event in runtime.events().await.unwrap() {
            first.accepts(&built.handles.session, &event).await;
        }
        let next = built
            .handles
            .run_checkpointed_turn(built.model.input_message("next"))
            .await
            .unwrap();
        assert!(next.success, "{next:?}");
        assert_eq!(next.response, "next answer");
        let mut saw_next = false;
        for event in runtime.events().await.unwrap() {
            if event.context.turn_id == Some(next.turn_id.parse().unwrap()) {
                saw_next = true;
                assert!(
                    !first.accepts(&built.handles.session, &event).await,
                    "cancelled frontend consumed another turn's event"
                );
            }
        }
        assert!(saw_next);
    }

    #[tokio::test]
    async fn native_admission_process_fixture() {
        let Some(directory) = std::env::var_os("YOLOP_TEST_ENGINE_PROCESS_SOURCE") else {
            return;
        };
        let directory = PathBuf::from(directory);
        let lost_background = std::env::var_os("YOLOP_TEST_LOST_BACKGROUND").is_some();
        let config = if lost_background {
            everruns_llmsim::LlmSimConfig::scripted(vec![
                everruns_llmsim::SimTurn::ToolCalls(vec![everruns_llmsim::SimToolCall {
                    name: "spawn_background".into(),
                    arguments: serde_json::json!({
                        "tool":"bash", "args":{"command":"printf started >> launches; printf retained; sleep 5"},
                        "title":"restart evidence", "signal_on_completion":false,
                    }),
                    id: None,
                }]),
                everruns_llmsim::SimTurn::Assistant("Detached work is running.".into()),
            ])
        } else {
            everruns_llmsim::LlmSimConfig::fixed("recovered reply")
        };
        let settings = Arc::new(SettingsStore::open(directory.join("settings.toml")));
        let built = build_with_options(
            directory.join("workspace"),
            ProviderChoice::Sim,
            None,
            directory.join("sessions"),
            settings,
            BuildOptions {
                llmsim_override: Some(config),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        if lost_background {
            let turn = built
                .handles
                .run_checkpointed_turn(built.model.input_message("Start detached work."))
                .await
                .unwrap();
            assert!(turn.success, "{turn:?}");
            tokio::time::timeout(std::time::Duration::from_secs(10), async {
                while !directory.join("workspace/launches").exists() {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
            let task = built
                .task_registry
                .list(built.handles.session_id, None)
                .await
                .unwrap()
                .into_iter()
                .find(|task| task.kind == "background_tool")
                .unwrap();
            assert_eq!(task.state, everruns_core::SessionTaskState::Running);
            std::fs::write(
                directory.join("background.json.tmp"),
                serde_json::to_vec(&serde_json::json!({
                    "session":built.handles.session_id,"task":task.id,"attempt":task.attempt,
                }))
                .unwrap(),
            )
            .unwrap();
            std::fs::rename(
                directory.join("background.json.tmp"),
                directory.join("background.json"),
            )
            .unwrap();
            std::future::pending::<()>().await;
        }
        let supervisor = built
            .handles
            .host
            .supervisor
            .lock()
            .unwrap()
            .take()
            .unwrap();
        supervisor.abort();
        while !supervisor.is_finished() {
            tokio::task::yield_now().await;
        }
        let handles = built.handles.clone();
        let _worker = tokio::spawn(async move {
            handles
                .execute_input(
                    InputMessage::user("recover this admitted ask"),
                    everruns::CancellationToken::new(),
                )
                .await
        });
        let input = loop {
            if let Some(input) =
                built
                    .handles
                    .host
                    .events()
                    .await
                    .unwrap()
                    .iter()
                    .find_map(|event| {
                        if let everruns_core::EventData::InputMessage(input) = &event.data {
                            Some(input.message.id)
                        } else {
                            None
                        }
                    })
            {
                break input;
            }
            tokio::task::yield_now().await;
        };
        std::fs::write(
            directory.join("admitted.json.tmp"),
            serde_json::to_vec(&serde_json::json!({
                "session":built.handles.session_id,"input":input,
            }))
            .unwrap(),
        )
        .unwrap();
        // File existence signals the parent to kill this process. Publish only
        // after the complete admission record has been written.
        std::fs::rename(
            directory.join("admitted.json.tmp"),
            directory.join("admitted.json"),
        )
        .unwrap();
        std::future::pending::<()>().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn process_death_settles_background_without_replaying_or_claiming_cancellation() {
        struct Process(std::process::Child);
        impl Drop for Process {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir(directory.path().join("workspace")).unwrap();
        let log = std::fs::File::create(directory.path().join("child.log")).unwrap();
        let mut child = Process(
            std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "runtime::engine::tests::native_admission_process_fixture",
                    "--nocapture",
                ])
                .env("YOLOP_TEST_ENGINE_PROCESS_SOURCE", directory.path())
                .env("YOLOP_TEST_LOST_BACKGROUND", "1")
                .stdout(log.try_clone().unwrap())
                .stderr(log)
                .spawn()
                .unwrap(),
        );
        tokio::time::timeout(std::time::Duration::from_secs(20), async {
            while !directory.path().join("background.json").exists() {
                assert!(
                    child.0.try_wait().unwrap().is_none(),
                    "fixture exited: {}",
                    std::fs::read_to_string(directory.path().join("child.log")).unwrap()
                );
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        child.0.kill().unwrap();
        child.0.wait().unwrap();
        let saved: serde_json::Value = serde_json::from_slice(
            &std::fs::read(directory.path().join("background.json")).unwrap(),
        )
        .unwrap();
        let id: SessionId = serde_json::from_value(saved["session"].clone()).unwrap();
        let built = build_with_options(
            directory.path().join("workspace"),
            ProviderChoice::Sim,
            Some(id),
            directory.path().join("sessions"),
            Arc::new(SettingsStore::open(directory.path().join("settings.toml"))),
            BuildOptions {
                llmsim_override: Some(everruns_llmsim::LlmSimConfig::fixed("recovered reply")),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let task = built
            .task_registry
            .get(id, saved["task"].as_str().unwrap())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            task.state,
            everruns_core::SessionTaskState::Failed,
            "{task:?}"
        );
        assert!(task.attempt > saved["attempt"].as_i64().unwrap() as i32);
        assert_eq!(task.error.unwrap().kind, "execution_status_unknown");
        assert_eq!(
            std::fs::read_to_string(directory.path().join("workspace/launches")).unwrap(),
            "started"
        );
    }

    #[tokio::test]
    async fn process_death_after_admission_recovers_without_another_request() {
        struct Process(std::process::Child);
        impl Drop for Process {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir(directory.path().join("workspace")).unwrap();
        let log = std::fs::File::create(directory.path().join("child.log")).unwrap();
        let mut child = Process(
            std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "runtime::engine::tests::native_admission_process_fixture",
                    "--nocapture",
                ])
                .env("YOLOP_TEST_ENGINE_PROCESS_SOURCE", directory.path())
                .stdout(log.try_clone().unwrap())
                .stderr(log)
                .spawn()
                .unwrap(),
        );
        tokio::time::timeout(std::time::Duration::from_secs(20), async {
            while !directory.path().join("admitted.json").exists() {
                assert!(
                    child.0.try_wait().unwrap().is_none(),
                    "fixture exited: {}",
                    std::fs::read_to_string(directory.path().join("child.log")).unwrap()
                );
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        child.0.kill().unwrap();
        child.0.wait().unwrap();
        let saved: serde_json::Value =
            serde_json::from_slice(&std::fs::read(directory.path().join("admitted.json")).unwrap())
                .unwrap();
        let id: SessionId = serde_json::from_value(saved["session"].clone()).unwrap();
        let input = serde_json::from_value(saved["input"].clone()).unwrap();
        std::fs::create_dir(directory.path().join("other-workspace")).unwrap();
        let settings = Arc::new(SettingsStore::open(directory.path().join("settings.toml")));
        let built = build_with_options(
            directory.path().join("other-workspace"),
            ProviderChoice::Sim,
            Some(id),
            directory.path().join("sessions"),
            settings,
            BuildOptions {
                llmsim_override: Some(everruns_llmsim::LlmSimConfig::fixed("recovered reply")),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(
            std::fs::canonicalize(&built.startup.workspace_root).unwrap(),
            std::fs::canonicalize(directory.path().join("workspace")).unwrap(),
            "accepted input lost its original workspace"
        );
        let session = built.handles.host.engine.resume(id).await.unwrap();
        let admission = session.admission(input).await.unwrap().unwrap();
        let result = tokio::time::timeout(std::time::Duration::from_secs(20), admission.wait())
            .await
            .unwrap()
            .unwrap();
        assert!(result.success);
        let mut replay_input = InputMessage::user("recover this admitted ask");
        replay_input.metadata =
            built
                .handles
                .host
                .events()
                .await
                .unwrap()
                .iter()
                .find_map(|event| {
                    if let everruns_core::EventData::InputMessage(message) = &event.data
                        && message.message.id == input
                    {
                        message.message.metadata.clone()
                    } else {
                        None
                    }
                });
        let replay = session.send_with_id(input, replay_input).await.unwrap();
        assert_eq!(replay.wait().await.unwrap().turn_id, result.turn_id);
        let events = built.handles.host.events().await.unwrap();
        assert_eq!(events.iter().filter(|event| matches!(&event.data, everruns_core::EventData::InputMessage(message) if message.message.id == input)).count(), 1);
        assert_eq!(
            events
                .iter()
                .filter(|event| event.event_type == "turn.started")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn expired_native_epoch_cannot_append_after_a_new_owner_claims() {
        let workspace = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let settings = Arc::new(SettingsStore::open(data.path().join("settings.toml")));
        let built = build_with_options(
            workspace.path().into(),
            ProviderChoice::Sim,
            None,
            data.path().into(),
            settings,
            BuildOptions::default(),
        )
        .await
        .unwrap();
        let runtime = &built.handles.host;
        let id = built.handles.session_id;
        let supervisor = runtime.supervisor.lock().unwrap().take().unwrap();
        supervisor.abort();
        while !supervisor.is_finished() {
            tokio::task::yield_now().await;
        }
        runtime
            .engine
            .resume(id)
            .await
            .unwrap()
            .send("pending")
            .await
            .unwrap();
        let old = runtime
            .profile
            .claim(id, "old-owner", std::time::Duration::from_millis(5))
            .await
            .unwrap()
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let replacement = runtime
            .profile
            .claim(id, "new-owner", std::time::Duration::from_secs(1))
            .await
            .unwrap()
            .unwrap();
        let append = old
            .epoch
            .backends()
            .event_log
            .append(everruns_core::EventRequest::new(
                id,
                everruns_core::EventContext::empty(),
                everruns_core::SessionTitleUpdatedData {
                    previous_title: None,
                    title: "stale".into(),
                },
            ))
            .await;
        assert!(append.is_err());
        assert!(
            !runtime
                .events()
                .await
                .unwrap()
                .iter()
                .any(|event| event.event_type == "session.title.updated")
        );
        drop(replacement);
    }

    #[tokio::test]
    async fn legacy_runtime_resume_continues_without_waiting_on_historical_inputs() {
        let workspace = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let id = SessionId::new();
        let directory = super::super::session_log::session_dir_path(data.path(), id);
        std::fs::create_dir_all(&directory).unwrap();
        let original = everruns_core::EventRequest::new(
            id,
            everruns_core::EventContext::empty(),
            everruns_core::InputMessageData::new(RuntimeMessage::user("saved legacy ask")),
        )
        .into_event(everruns_contracts::typed_id::EventId::new(), 1);
        std::fs::write(
            directory.join("events.jsonl"),
            format!("{}\n", serde_json::to_string(&original).unwrap()),
        )
        .unwrap();
        let settings = Arc::new(SettingsStore::open(data.path().join("settings.toml")));
        let built = build_with_options(
            workspace.path().into(),
            ProviderChoice::Sim,
            Some(id),
            data.path().into(),
            settings,
            BuildOptions {
                llmsim_override: Some(everruns_llmsim::LlmSimConfig::fixed("reply")),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let result = built
            .handles
            .run_checkpointed_turn(built.model.input_message("next"))
            .await
            .unwrap();
        assert!(result.success);
        assert!(
            !built
                .handles
                .host
                .has_owed_work(result.turn_id.parse().unwrap())
                .await
                .unwrap()
        );
        let history = built.handles.host.events().await.unwrap();
        assert!(
            history
                .iter()
                .any(|event| event.id == original.id && event.sequence == original.sequence)
        );
        let asks: Vec<_> = messages_from_events(&history)
            .into_iter()
            .filter(|message| message.role == everruns_core::RuntimeMessageRole::User)
            .map(|message| message.text().unwrap_or_default().to_string())
            .collect();
        assert_eq!(asks, ["saved legacy ask", "next"]);
        let undo = built
            .handles
            .checkpoints
            .prepare_undo(RestoreMode::Conversation)
            .unwrap();
        built
            .handles
            .checkpoints
            .confirm(&undo.token)
            .await
            .unwrap();
        let history = built.handles.host.events().await.unwrap();
        assert!(history.iter().any(|event| event.id == original.id));
        assert_eq!(
            messages_from_events(&history)
                .iter()
                .filter(|message| message.role == everruns_core::RuntimeMessageRole::User)
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn native_turns_rewind_redo_fork_and_reopen_the_authority() {
        let workspace = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let settings = Arc::new(SettingsStore::open(data.path().join("settings.toml")));
        let build = || BuildOptions {
            llmsim_override: Some(everruns_llmsim::LlmSimConfig::fixed("reply")),
            ..Default::default()
        };
        let built = build_with_options(
            workspace.path().into(),
            ProviderChoice::Sim,
            None,
            data.path().into(),
            settings.clone(),
            build(),
        )
        .await
        .unwrap();
        let id = built.handles.session_id;
        for prompt in ["alpha", "beta"] {
            assert!(
                built
                    .handles
                    .run_checkpointed_turn(built.model.input_message(prompt))
                    .await
                    .unwrap()
                    .success
            );
        }
        let beta = built.handles.checkpoints.list()[0].id.clone();
        let undo = built
            .handles
            .checkpoints
            .prepare_undo(RestoreMode::Conversation)
            .unwrap();
        built
            .handles
            .checkpoints
            .confirm(&undo.token)
            .await
            .unwrap();
        let texts = |events: Vec<Event>| {
            messages_from_events(&events)
                .iter()
                .filter(|message| message.role == everruns_core::RuntimeMessageRole::User)
                .filter_map(|message| message.text().map(str::to_string))
                .collect::<Vec<_>>()
        };
        assert_eq!(texts(built.handles.host.events().await.unwrap()), ["alpha"]);
        let redo = built
            .handles
            .checkpoints
            .prepare_redo(RestoreMode::Conversation)
            .unwrap();
        built
            .handles
            .checkpoints
            .confirm(&redo.token)
            .await
            .unwrap();
        assert_eq!(
            texts(built.handles.host.events().await.unwrap()),
            ["alpha", "beta"]
        );
        let rewind = built
            .handles
            .checkpoints
            .prepare_rewind(&beta, RestoreMode::Conversation)
            .unwrap();
        built
            .handles
            .checkpoints
            .confirm(&rewind.token)
            .await
            .unwrap();
        assert!(
            built
                .handles
                .run_checkpointed_turn(built.model.input_message("gamma"))
                .await
                .unwrap()
                .success
        );
        assert_eq!(
            texts(built.handles.host.events().await.unwrap()),
            ["alpha", "gamma"]
        );
        let raw = std::fs::read_to_string(&built.startup.session_log_path).unwrap();
        assert!(raw.contains("beta"), "raw audit keeps the abandoned branch");
        drop(built);
        let reopened = build_with_options(
            workspace.path().into(),
            ProviderChoice::Sim,
            Some(id),
            data.path().into(),
            settings,
            build(),
        )
        .await
        .unwrap();
        assert_eq!(
            texts(reopened.handles.host.events().await.unwrap()),
            ["alpha", "gamma"]
        );
        assert_eq!(reopened.handles.checkpoints.list().len(), 2);
    }
}

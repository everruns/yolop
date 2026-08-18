//! Yolop-owned session-task cancellation semantics.
//!
//! Everruns 0.17.8 returns the snapshot captured before the task executor runs,
//! so synchronous monitor cancellation is reported as merely requested even
//! after its schedule is disabled and its task is terminal. Keep the upstream
//! task surface, replacing only `cancel_task` until the runtime result contract
//! exposes the post-cancellation state.

use crate::capabilities::narration::narrate_session_task_tool;
use async_trait::async_trait;
use everruns_core::ToolContext;
use everruns_core::capabilities::CapabilityLocalization;
use everruns_core::session_schedule::SessionSchedule;
use everruns_core::session_services::SessionScheduleStore;
use everruns_core::tool_narration::ToolNarrationPhase;
use everruns_core::{Capability, CapabilityStatus, SystemPromptContext};
use everruns_core::{SessionTask, SessionTaskRegistry, SessionTaskState, SessionTaskUpdate};
use everruns_core::{Tool, ToolExecutionResult};
use everruns_platform::capabilities::SessionTasksCapability;
use everruns_provider::AgentLoopError;
use everruns_provider::typed_id::ScheduleId;
use everruns_provider::{ToolCall, ToolDefinition, ToolHints, ToolPolicy};
use serde_json::{Value, json};

const CANCEL_TASK: &str = "cancel_task";

pub(crate) struct TruthfulSessionTasksCapability {
    inner: SessionTasksCapability,
}

impl TruthfulSessionTasksCapability {
    pub(crate) fn new() -> Self {
        Self {
            inner: SessionTasksCapability,
        }
    }
}

#[async_trait]
impl Capability for TruthfulSessionTasksCapability {
    fn id(&self) -> &str {
        self.inner.id()
    }

    fn name(&self) -> &str {
        self.inner.name()
    }

    fn description(&self) -> &str {
        self.inner.description()
    }

    fn localizations(&self) -> Vec<CapabilityLocalization> {
        self.inner.localizations()
    }

    fn status(&self) -> CapabilityStatus {
        self.inner.status()
    }

    fn icon(&self) -> Option<&str> {
        self.inner.icon()
    }

    fn category(&self) -> Option<&str> {
        self.inner.category()
    }

    fn features(&self) -> Vec<&'static str> {
        self.inner.features()
    }

    fn system_prompt_preview(&self) -> Option<String> {
        self.inner.system_prompt_preview()
    }

    async fn system_prompt_contribution(&self, ctx: &SystemPromptContext) -> Option<String> {
        self.inner.system_prompt_contribution(ctx).await
    }

    fn tools(&self) -> Vec<Box<dyn Tool>> {
        self.inner
            .tools()
            .into_iter()
            .map(|tool| {
                if tool.name() == CANCEL_TASK {
                    Box::new(TruthfulCancelTaskTool { inner: tool }) as Box<dyn Tool>
                } else {
                    tool
                }
            })
            .collect()
    }

    fn narrate(
        &self,
        _tool_def: Option<&ToolDefinition>,
        tool_call: &ToolCall,
        phase: ToolNarrationPhase,
        _locale: Option<&str>,
        _ctx: everruns_core::tool_narration::ToolNarrationContext<'_>,
    ) -> Option<String> {
        narrate_session_task_tool(tool_call, phase)
    }
}

struct TruthfulCancelTaskTool {
    inner: Box<dyn Tool>,
}

#[async_trait]
impl Tool for TruthfulCancelTaskTool {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn display_name(&self) -> Option<&str> {
        self.inner.display_name()
    }

    fn description(&self) -> &str {
        "Cancel a task. Monitor schedules are disarmed synchronously; other task kinds may report cancellation_pending while they wind down."
    }

    fn parameters_schema(&self) -> Value {
        self.inner.parameters_schema()
    }

    fn requires_context(&self) -> bool {
        self.inner.requires_context()
    }

    fn policy(&self) -> ToolPolicy {
        self.inner.policy()
    }

    fn hints(&self) -> ToolHints {
        self.inner.hints()
    }

    fn narrate(
        &self,
        tool_call: &ToolCall,
        phase: ToolNarrationPhase,
        _locale: Option<&str>,
        _ctx: everruns_core::tool_narration::ToolNarrationContext<'_>,
    ) -> Option<String> {
        narrate_session_task_tool(tool_call, phase)
    }

    async fn execute(&self, arguments: Value) -> ToolExecutionResult {
        self.inner.execute(arguments).await
    }

    async fn execute_with_context(
        &self,
        arguments: Value,
        context: &ToolContext,
    ) -> ToolExecutionResult {
        let Some(task_id) = arguments
            .get("task_id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|task_id| !task_id.is_empty())
            .map(str::to_string)
        else {
            return ToolExecutionResult::tool_error("cancel_task requires a non-empty task_id.");
        };
        let Some(registry) = context.session_task_registry.as_ref() else {
            return ToolExecutionResult::tool_error(
                "Session task tools require session_task_registry context (not available in this environment)",
            );
        };
        let task = match registry.get(context.session_id, &task_id).await {
            Ok(Some(task)) => task,
            Ok(None) => {
                return ToolExecutionResult::tool_error(format!(
                    "No task found with id: {task_id}"
                ));
            }
            Err(error) => return ToolExecutionResult::internal_error(error),
        };

        if task.kind == everruns_core::session_task::TASK_KIND_MONITOR {
            let Some(schedule_store) = context.schedule_store.as_ref() else {
                return ToolExecutionResult::tool_error(format!(
                    "Monitor {task_id} cannot be disarmed because no schedule store is available; cancellation remains pending."
                ));
            };
            return match cancel_monitor_task(&task, registry.as_ref(), schedule_store.as_ref())
                .await
            {
                Ok(canceled) => ToolExecutionResult::success(json!({
                    "task_id": task_id,
                    "state": canceled.task.state,
                    "terminal": canceled.task.state.is_terminal(),
                    "disarmed": true,
                    "cancellation_pending": false,
                    "cancel_requested_at": canceled.task.cancel_requested_at,
                    "schedule_id": canceled.schedule.id,
                    "schedule_enabled": canceled.schedule.enabled,
                })),
                Err(AgentLoopError::ToolExecution(message)) => {
                    ToolExecutionResult::tool_error(message)
                }
                Err(error) => ToolExecutionResult::internal_error(error),
            };
        }

        let result = self.inner.execute_with_context(arguments, context).await;
        if !matches!(result, ToolExecutionResult::Success(_)) {
            return result;
        }
        let refreshed = match registry.get(context.session_id, &task_id).await {
            Ok(Some(task)) => task,
            Ok(None) => {
                return ToolExecutionResult::tool_error(format!(
                    "Task disappeared after cancellation: {task_id}"
                ));
            }
            Err(error) => return ToolExecutionResult::internal_error(error),
        };
        let terminal = refreshed.state.is_terminal();
        ToolExecutionResult::success(json!({
            "task_id": task_id,
            "state": refreshed.state,
            "terminal": terminal,
            "cancellation_pending": !terminal && refreshed.cancel_requested_at.is_some(),
            "cancel_requested_at": refreshed.cancel_requested_at,
        }))
    }
}

pub(crate) struct CanceledMonitor {
    pub(crate) task: SessionTask,
    pub(crate) schedule: SessionSchedule,
}

/// Keep monitor cancellation identical whether it starts from the task tool or
/// the interactive TUI: first record intent, then disarm, then settle terminal.
pub(crate) async fn cancel_monitor_task(
    task: &SessionTask,
    registry: &dyn SessionTaskRegistry,
    schedule_store: &dyn SessionScheduleStore,
) -> everruns_provider::error::Result<CanceledMonitor> {
    let task_id = &task.id;
    let task = registry
        .request_cancel(task.session_id, task_id)
        .await?
        .ok_or_else(|| AgentLoopError::tool(format!("No task found with id: {task_id}")))?;
    let Some(schedule_id) = task
        .spec
        .get("schedule_id")
        .and_then(Value::as_str)
        .and_then(|raw| ScheduleId::parse(raw).ok())
    else {
        return Err(AgentLoopError::tool(format!(
            "Monitor {task_id} has no valid linked schedule_id; cancellation remains pending."
        )));
    };
    let schedule = schedule_store
        .cancel_schedule(task.session_id, schedule_id)
        .await
        .map_err(|_| {
            AgentLoopError::tool(format!(
                "Monitor {task_id} could not be disarmed; cancellation remains pending."
            ))
        })?;
    if schedule.enabled {
        return Err(AgentLoopError::tool(format!(
            "Monitor {task_id} cancellation did not disable its schedule; cancellation remains pending."
        )));
    }

    let canceled = registry
        .update(
            task.session_id,
            task_id,
            SessionTaskUpdate {
                state: Some(SessionTaskState::Canceled),
                summary: Some("Monitor canceled".into()),
                ..Default::default()
            },
        )
        .await?
        .ok_or_else(|| {
            AgentLoopError::tool(format!(
                "Monitor schedule was disarmed, but task {task_id} disappeared before its state was updated."
            ))
        })?;
    if canceled.state != SessionTaskState::Canceled {
        return Err(AgentLoopError::tool(format!(
            "Monitor {task_id} schedule was disarmed, but its task did not reach canceled state."
        )));
    }

    Ok(CanceledMonitor {
        task: canceled,
        schedule,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use everruns::local::{LocalScheduleStore, LocalSessionTaskRegistry, SqliteDb};
    use everruns_core::session_schedule::SessionSchedule;
    use everruns_core::session_services::SessionScheduleStore;
    use everruns_core::{
        CreateSessionTask, SessionTaskRegistry, SessionTaskState, TASK_KIND_MONITOR, TaskWakePolicy,
    };
    use everruns_provider::typed_id::{PrincipalId, ScheduleId, SessionId};
    use serde_json::json;
    use std::sync::Arc;

    struct FailingScheduleStore;

    #[async_trait]
    impl SessionScheduleStore for FailingScheduleStore {
        async fn create_schedule(
            &self,
            _session_id: SessionId,
            _description: String,
            _cron_expression: Option<String>,
            _scheduled_at: Option<chrono::DateTime<chrono::Utc>>,
            _timezone: String,
        ) -> everruns_provider::error::Result<SessionSchedule> {
            unimplemented!()
        }

        async fn cancel_schedule(
            &self,
            _session_id: SessionId,
            _schedule_id: ScheduleId,
        ) -> everruns_provider::error::Result<SessionSchedule> {
            Err(everruns_provider::error::AgentLoopError::tool(
                "injected schedule cancellation failure",
            ))
        }

        async fn list_schedules(
            &self,
            _session_id: SessionId,
        ) -> everruns_provider::error::Result<Vec<SessionSchedule>> {
            Ok(vec![])
        }

        async fn count_active_schedules(
            &self,
            _session_id: SessionId,
        ) -> everruns_provider::error::Result<u32> {
            Ok(0)
        }

        async fn count_active_org_schedules(&self) -> everruns_provider::error::Result<u32> {
            Ok(0)
        }
    }

    async fn monitor_fixture() -> (
        SessionId,
        ScheduleId,
        Arc<LocalSessionTaskRegistry>,
        Arc<LocalScheduleStore>,
    ) {
        let session_id = SessionId::from_seed(42);
        let db = SqliteDb::open_in_memory().expect("in-memory database");
        let registry = Arc::new(LocalSessionTaskRegistry::new(db.clone()).expect("task registry"));
        let schedules = Arc::new(
            LocalScheduleStore::new(db, 1, PrincipalId::from_seed(1)).expect("schedule store"),
        );
        let schedule = schedules
            .create_schedule(
                session_id,
                "scheduled check".into(),
                None,
                Some(chrono::Utc::now() + chrono::Duration::minutes(10)),
                "UTC".into(),
            )
            .await
            .expect("create schedule");
        registry
            .create(CreateSessionTask {
                session_id,
                id: Some("task_monitor".into()),
                kind: TASK_KIND_MONITOR.into(),
                display_name: "scheduled check".into(),
                spec: json!({"schedule_id": schedule.id.to_string()}),
                state: SessionTaskState::Running,
                links: Default::default(),
                wake_policy: TaskWakePolicy::Silent,
            })
            .await
            .expect("create monitor task");
        (session_id, schedule.id, registry, schedules)
    }

    #[test]
    fn session_tasks_capability_narrates_wait_task() {
        let capability = TruthfulSessionTasksCapability::new();
        let call = ToolCall {
            id: "call-1".to_owned(),
            name: "wait_task".to_owned(),
            arguments: json!({ "task_id": "task_ci_watch" }),
        };
        let narration = capability.narrate(
            None,
            &call,
            ToolNarrationPhase::Started,
            None,
            everruns_core::tool_narration::ToolNarrationContext::default(),
        );
        assert_eq!(narration.as_deref(), Some("Wait for task: task_ci_watch"));
    }

    #[tokio::test]
    async fn cancel_monitor_reports_terminal_disarmed_state() {
        let (session_id, _, registry, schedules) = monitor_fixture().await;
        let tool = TruthfulSessionTasksCapability::new()
            .tools()
            .into_iter()
            .find(|tool| tool.name() == CANCEL_TASK)
            .expect("cancel_task tool");
        let context = ToolContext::new(session_id)
            .with_session_task_registry(registry.clone())
            .with_schedule_store(schedules.clone());

        let result = tool
            .execute_with_context(json!({"task_id": "task_monitor"}), &context)
            .await;
        let ToolExecutionResult::Success(value) = result else {
            panic!("expected success: {result:?}");
        };

        assert_eq!(value["state"], "canceled");
        assert_eq!(value["terminal"], true);
        assert_eq!(value["disarmed"], true);
        assert_eq!(value["cancellation_pending"], false);
        assert_eq!(
            registry
                .get(session_id, "task_monitor")
                .await
                .expect("load task")
                .expect("monitor task")
                .state,
            SessionTaskState::Canceled
        );
        assert_eq!(
            schedules
                .count_active_schedules(session_id)
                .await
                .expect("count schedules"),
            0
        );
    }

    #[tokio::test]
    async fn cancel_monitor_does_not_claim_disarmed_when_schedule_cancel_fails() {
        let (session_id, _, registry, schedules) = monitor_fixture().await;
        let failing_schedules = Arc::new(FailingScheduleStore);
        let tool = TruthfulSessionTasksCapability::new()
            .tools()
            .into_iter()
            .find(|tool| tool.name() == CANCEL_TASK)
            .expect("cancel_task tool");
        let context = ToolContext::new(session_id)
            .with_session_task_registry(registry.clone())
            .with_schedule_store(failing_schedules.clone());

        let result = tool
            .execute_with_context(json!({"task_id": "task_monitor"}), &context)
            .await;

        let ToolExecutionResult::ToolError(message) = result else {
            panic!("expected tool error: {result:?}");
        };
        assert!(message.contains("could not be disarmed"));
        assert_eq!(
            registry
                .get(session_id, "task_monitor")
                .await
                .expect("load task")
                .expect("monitor task")
                .state,
            SessionTaskState::Running
        );
        assert_eq!(
            schedules
                .count_active_schedules(session_id)
                .await
                .expect("count schedules"),
            1
        );
    }

    #[tokio::test]
    async fn cancel_non_monitor_reports_cooperative_cancellation_as_pending() {
        let session_id = SessionId::from_seed(42);
        let registry = Arc::new(
            LocalSessionTaskRegistry::new(SqliteDb::open_in_memory().expect("in-memory database"))
                .expect("task registry"),
        );
        registry
            .create(CreateSessionTask {
                session_id,
                id: Some("task_external".into()),
                kind: "external_agent".into(),
                display_name: "remote work".into(),
                spec: json!({}),
                state: SessionTaskState::Running,
                links: Default::default(),
                wake_policy: TaskWakePolicy::Silent,
            })
            .await
            .expect("create external task");
        let tool = TruthfulSessionTasksCapability::new()
            .tools()
            .into_iter()
            .find(|tool| tool.name() == CANCEL_TASK)
            .expect("cancel_task tool");
        let context = ToolContext::new(session_id).with_session_task_registry(registry);

        let result = tool
            .execute_with_context(json!({"task_id": "task_external"}), &context)
            .await;
        let ToolExecutionResult::Success(value) = result else {
            panic!("expected success: {result:?}");
        };

        assert_eq!(value["state"], "running");
        assert_eq!(value["terminal"], false);
        assert_eq!(value["cancellation_pending"], true);
        assert!(value.get("disarmed").is_none());
    }
}

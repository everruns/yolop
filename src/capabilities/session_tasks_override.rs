//! Yolop-owned session-task cancellation semantics.
//!
//! Everruns 0.17.8 returns the snapshot captured before the task executor runs,
//! so synchronous monitor cancellation is reported as merely requested even
//! after its schedule is disabled and its task is terminal. Keep the upstream
//! task surface, replacing only `cancel_task` until the runtime result contract
//! exposes the post-cancellation state.

use async_trait::async_trait;
use everruns_core::capabilities::{
    Capability, CapabilityLocalization, CapabilityStatus, SessionTasksCapability,
    SystemPromptContext,
};
use everruns_core::tool_narration::ToolNarrationPhase;
use everruns_core::tool_types::{ToolCall, ToolHints, ToolPolicy};
use everruns_core::tools::{Tool, ToolExecutionResult};
use everruns_core::traits::ToolContext;
use everruns_core::{ScheduleId, SessionTaskState, SessionTaskUpdate};
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
        locale: Option<&str>,
        ctx: everruns_core::tool_narration::ToolNarrationContext<'_>,
    ) -> Option<String> {
        self.inner.narrate(tool_call, phase, locale, ctx)
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
            return cancel_monitor(&task_id, context).await;
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

async fn cancel_monitor(task_id: &str, context: &ToolContext) -> ToolExecutionResult {
    let registry = context
        .session_task_registry
        .as_ref()
        .expect("cancel monitor is called only with a task registry");
    let task = match registry.request_cancel(context.session_id, task_id).await {
        Ok(Some(task)) => task,
        Ok(None) => {
            return ToolExecutionResult::tool_error(format!("No task found with id: {task_id}"));
        }
        Err(error) => return ToolExecutionResult::internal_error(error),
    };
    let Some(schedule_id) = task
        .spec
        .get("schedule_id")
        .and_then(Value::as_str)
        .and_then(|raw| ScheduleId::parse(raw).ok())
    else {
        return ToolExecutionResult::tool_error(format!(
            "Monitor {task_id} has no valid linked schedule_id; cancellation remains pending."
        ));
    };
    let Some(schedule_store) = context.schedule_store.as_ref() else {
        return ToolExecutionResult::tool_error(format!(
            "Monitor {task_id} cannot be disarmed because no schedule store is available; cancellation remains pending."
        ));
    };
    let schedule = match schedule_store
        .cancel_schedule(context.session_id, schedule_id)
        .await
    {
        Ok(schedule) => schedule,
        Err(_) => {
            return ToolExecutionResult::tool_error(format!(
                "Monitor {task_id} could not be disarmed; cancellation remains pending."
            ));
        }
    };
    if schedule.enabled {
        return ToolExecutionResult::tool_error(format!(
            "Monitor {task_id} cancellation did not disable its schedule; cancellation remains pending."
        ));
    }

    let canceled = match registry
        .update(
            context.session_id,
            task_id,
            SessionTaskUpdate {
                state: Some(SessionTaskState::Canceled),
                summary: Some("Monitor canceled".into()),
                ..Default::default()
            },
        )
        .await
    {
        Ok(Some(task)) => task,
        Ok(None) => {
            return ToolExecutionResult::tool_error(format!(
                "Monitor schedule was disarmed, but task {task_id} disappeared before its state was updated."
            ));
        }
        Err(error) => return ToolExecutionResult::internal_error(error),
    };
    if canceled.state != SessionTaskState::Canceled {
        return ToolExecutionResult::tool_error(format!(
            "Monitor {task_id} schedule was disarmed, but its task did not reach canceled state."
        ));
    }

    ToolExecutionResult::success(json!({
        "task_id": task_id,
        "state": canceled.state,
        "terminal": canceled.state.is_terminal(),
        "disarmed": true,
        "cancellation_pending": false,
        "cancel_requested_at": canceled.cancel_requested_at,
        "schedule_id": schedule.id,
        "schedule_enabled": schedule.enabled,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use everruns_core::session_schedule::SessionSchedule;
    use everruns_core::session_task::{
        CreateSessionTask, SessionTaskRegistry, SessionTaskState, TASK_KIND_MONITOR, TaskWakePolicy,
    };
    use everruns_core::traits::SessionScheduleStore;
    use everruns_core::{PrincipalId, ScheduleId, SessionId};
    use everruns_local::{LocalScheduleStore, LocalSessionTaskRegistry, SqliteDb};
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
        ) -> everruns_core::Result<SessionSchedule> {
            unimplemented!()
        }

        async fn cancel_schedule(
            &self,
            _session_id: SessionId,
            _schedule_id: ScheduleId,
        ) -> everruns_core::Result<SessionSchedule> {
            Err(everruns_core::AgentLoopError::tool(
                "injected schedule cancellation failure",
            ))
        }

        async fn list_schedules(
            &self,
            _session_id: SessionId,
        ) -> everruns_core::Result<Vec<SessionSchedule>> {
            Ok(vec![])
        }

        async fn count_active_schedules(
            &self,
            _session_id: SessionId,
        ) -> everruns_core::Result<u32> {
            Ok(0)
        }

        async fn count_active_org_schedules(&self) -> everruns_core::Result<u32> {
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

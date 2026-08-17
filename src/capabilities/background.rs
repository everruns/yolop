// The `background` capability — a thin surface over everruns session tasks.
//
// Detached background work (e.g. `gh pr checks --watch` waiting on CI) runs
// through everruns' `spawn_background`, which wraps the background-capable
// `bash` tool: it streams to a session-file log, tracks a `background_tool`
// session task, and on completion signals the session. yolop delivers that
// signal to the host as a proactive wake turn via the platform-store wake seam
// (see `crate::runtime::background_wake`).
//
// This capability adds only the `/background` command, which lists the
// session's everruns tasks. The model inspects and controls them with the
// everruns `list_tasks` / `get_task` / `cancel_task` tools (the `session_tasks`
// capability). See knowledge/specs/background.md.
//
// `NarratedBackgroundExecutionCapability` wraps upstream
// `BackgroundExecutionCapability` so `spawn_background` gets human narration
// instead of the generic "Running Spawn Background" fallback.

use crate::capabilities::narration::narrate_spawn_background;
use crate::tui::session_tasks_view::{load_task_tree, render_task_tree};
use async_trait::async_trait;
use everruns_core::SessionStore;
use everruns_core::SessionTaskRegistry;
use everruns_core::TASK_KIND_MONITOR;
use everruns_core::Tool;
use everruns_core::capabilities::CapabilityLocalization;
use everruns_core::command::{
    CommandDescriptor, CommandExecutionContext, CommandResult, CommandSource, ExecuteCommandRequest,
};
use everruns_core::tool_narration::ToolNarrationPhase;
use everruns_core::{Capability, CapabilityStatus, SystemPromptContext};
use everruns_platform::capabilities::BackgroundExecutionCapability;
use everruns_provider::typed_id::SessionId;
use everruns_provider::{ToolCall, ToolDefinition};
use std::sync::Arc;

pub(crate) const BACKGROUND_CAPABILITY_ID: &str = "background";

// Prompt-side half of the poll-proofing seam (the runtime-enforced half is
// `progress_guard`'s Waiting class): waiting on an external event must cost
// zero turns, so steer the model to detach the wait and rely on the
// completion wake instead of foreground watches or poll-sleep turns.
pub(crate) const BACKGROUND_SYSTEM_PROMPT: &str = "<capability id=\"background\">\n\
    Waiting on an external event — a CI run, a PR review window, a deploy, a long \
    build — must not consume turns. Do not run watch commands in the foreground and \
    do not poll status across turns. Start one blocking watch detached via \
    `spawn_background` (e.g. `gh pr checks --watch`, `gh run watch --exit-status`, \
    or `until <check>; do sleep 30; done`), say what you are waiting for, and end \
    the turn: completion wakes you with the result. You can keep working on other \
    steps while it runs. In one-shot (`-p`) runs there is no wake — block on the \
    spawned task with `wait_task` instead of ending the turn. To inspect background \
    state, call `list_tasks` once without kind or state filters; scheduled work is a \
    `monitor`, not a `background_tool`. Scheduled monitors are obligations you own: \
    before finishing work, cancel any monitor whose purpose is satisfied, superseded, \
    or no longer needed; keep it armed only when its future wake is still required. \
    Treat `disarmed: true` or a terminal task state from `cancel_task` as completed \
    cancellation; `cancellation_pending: true` means cooperative shutdown is still in \
    progress.\n\
    </capability>";

pub(crate) struct BackgroundCapability {
    pub(crate) session_id: SessionId,
    pub(crate) task_registry: Arc<dyn SessionTaskRegistry>,
    pub(crate) session_store: Arc<dyn SessionStore>,
}

#[async_trait]
impl Capability for BackgroundCapability {
    fn id(&self) -> &str {
        BACKGROUND_CAPABILITY_ID
    }
    fn name(&self) -> &str {
        "Background execution"
    }
    fn description(&self) -> &str {
        "List detached background tasks (started with `spawn_background`, e.g. waiting for CI) and \
         their status. Completions wake the agent automatically; inspect results with \
         `get_task`/`list_tasks`."
    }
    fn status(&self) -> CapabilityStatus {
        CapabilityStatus::Available
    }
    fn category(&self) -> Option<&str> {
        Some("Execution")
    }

    async fn system_prompt_contribution(&self, _ctx: &SystemPromptContext) -> Option<String> {
        let active_monitors = self
            .task_registry
            .list(self.session_id, None)
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|task| task.kind == TASK_KIND_MONITOR && !task.state.is_terminal())
            .map(|task| task.id)
            .collect::<Vec<_>>();
        let mut prompt = BACKGROUND_SYSTEM_PROMPT
            .strip_suffix("</capability>")
            .unwrap_or(BACKGROUND_SYSTEM_PROMPT)
            .to_string();
        if !active_monitors.is_empty() {
            prompt.push_str(&format!(
                "Active scheduled monitor obligations in this session: {}. Reconcile each before reporting its parent work complete.\n",
                active_monitors.join(", ")
            ));
        }
        prompt.push_str("</capability>");
        Some(prompt)
    }

    fn system_prompt_preview(&self) -> Option<String> {
        Some(
            "<capability id=\"background\">\nDetach waits on external events via `spawn_background`; completion wakes the agent.\n</capability>"
                .to_string(),
        )
    }

    fn commands(&self) -> Vec<CommandDescriptor> {
        vec![CommandDescriptor {
            name: "background".to_string(),
            description: "show the session task tree and branch usage".to_string(),
            source: CommandSource::System,
            args: Vec::new(),
        }]
    }

    async fn execute_command(
        &self,
        request: &ExecuteCommandRequest,
        _ctx: &CommandExecutionContext,
    ) -> everruns_provider::error::Result<CommandResult> {
        if request.name != "background" {
            return Err(everruns_provider::error::AgentLoopError::config(format!(
                "{} cannot execute /{}",
                self.id(),
                request.name
            )));
        }
        let tree = load_task_tree(
            self.session_id,
            self.task_registry.as_ref(),
            self.session_store.as_ref(),
        )
        .await;
        Ok(CommandResult {
            success: true,
            message: render_task_tree(&tree, None),
            error_code: None,
            error_fields: None,
        })
    }
}

/// Upstream `spawn_background` without argument-aware narration. Yolop wraps it
/// so transcript / ACP titles read "Spawn background: …" instead of
/// "Running Spawn Background".
pub(crate) struct NarratedBackgroundExecutionCapability {
    inner: BackgroundExecutionCapability,
}

impl NarratedBackgroundExecutionCapability {
    pub(crate) fn new() -> Self {
        Self {
            inner: BackgroundExecutionCapability,
        }
    }
}

#[async_trait]
impl Capability for NarratedBackgroundExecutionCapability {
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

    /// Delegate activation too, not just presentation. The capability is
    /// auto-activating — it turns on when some tool declares
    /// `supports_background` — so a wrapper that inherits the default
    /// `false` silently withholds `spawn_background` from the model.
    fn auto_activates_for(&self, tools: &[everruns_provider::tool_types::ToolDefinition]) -> bool {
        self.inner.auto_activates_for(tools)
    }

    fn tools(&self) -> Vec<Box<dyn Tool>> {
        self.inner.tools()
    }

    fn narrate(
        &self,
        _tool_def: Option<&ToolDefinition>,
        tool_call: &ToolCall,
        phase: ToolNarrationPhase,
        _locale: Option<&str>,
        _ctx: everruns_core::tool_narration::ToolNarrationContext<'_>,
    ) -> Option<String> {
        if tool_call.name == "spawn_background" {
            Some(narrate_spawn_background(tool_call, phase))
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use everruns_core::{
        CreateSessionTask, NewTaskMessage, SessionTask, SessionTaskFilter, SessionTaskUpdate,
        TaskMessage,
    };
    use serde_json::json;

    /// The prompt contribution tolerates an empty registry.
    struct StubRegistry {
        tasks: Vec<SessionTask>,
    }

    #[async_trait]
    impl SessionTaskRegistry for StubRegistry {
        async fn create(
            &self,
            _input: CreateSessionTask,
        ) -> everruns_provider::error::Result<SessionTask> {
            unimplemented!("stub")
        }
        async fn update(
            &self,
            _session_id: SessionId,
            _task_id: &str,
            _update: SessionTaskUpdate,
        ) -> everruns_provider::error::Result<Option<SessionTask>> {
            unimplemented!("stub")
        }
        async fn get(
            &self,
            _session_id: SessionId,
            _task_id: &str,
        ) -> everruns_provider::error::Result<Option<SessionTask>> {
            unimplemented!("stub")
        }
        async fn list(
            &self,
            _session_id: SessionId,
            _filter: Option<&SessionTaskFilter>,
        ) -> everruns_provider::error::Result<Vec<SessionTask>> {
            Ok(self.tasks.clone())
        }
        async fn request_cancel(
            &self,
            _session_id: SessionId,
            _task_id: &str,
        ) -> everruns_provider::error::Result<Option<SessionTask>> {
            unimplemented!("stub")
        }
        async fn record_message(
            &self,
            _session_id: SessionId,
            _task_id: &str,
            _message: NewTaskMessage,
        ) -> everruns_provider::error::Result<TaskMessage> {
            unimplemented!("stub")
        }
        async fn list_messages(
            &self,
            _session_id: SessionId,
            _task_id: &str,
            _limit: Option<u32>,
            _after_id: Option<&str>,
        ) -> everruns_provider::error::Result<Vec<TaskMessage>> {
            unimplemented!("stub")
        }
    }

    #[async_trait]
    impl everruns_core::execution_loading::SessionStore for StubRegistry {
        async fn get_session(
            &self,
            _session_id: SessionId,
        ) -> everruns_provider::error::Result<Option<everruns_core::ExecutionSession>> {
            Ok(None)
        }
    }

    #[tokio::test]
    async fn system_prompt_teaches_detached_waits_per_host() {
        let store = Arc::new(StubRegistry { tasks: vec![] });
        let capability = BackgroundCapability {
            session_id: SessionId::new(),
            task_registry: store.clone(),
            session_store: store,
        };
        let ctx = SystemPromptContext::without_file_store(SessionId::new());

        let prompt = capability
            .system_prompt_contribution(&ctx)
            .await
            .expect("background capability contributes a prompt");
        // Interactive hosts detach and get woken; one-shot runs must block
        // instead because `-p` exits before any wake can be delivered.
        assert!(prompt.contains("spawn_background"));
        assert!(prompt.contains("end the turn"));
        assert!(prompt.contains("wait_task"));
        assert!(prompt.contains("cancel any monitor whose purpose is satisfied"));
        assert!(prompt.contains("disarmed: true"));
    }

    #[tokio::test]
    async fn system_prompt_surfaces_active_monitor_obligations() {
        let session_id = SessionId::from_seed(42);
        let task = everruns_core::session_task::new_session_task(
            CreateSessionTask {
                session_id,
                id: Some("task_scheduled_check".into()),
                kind: TASK_KIND_MONITOR.into(),
                display_name: "scheduled check".into(),
                spec: serde_json::json!({}),
                state: everruns_core::session_task::SessionTaskState::Running,
                links: Default::default(),
                wake_policy: everruns_core::session_task::TaskWakePolicy::Silent,
            },
            chrono::Utc::now(),
        );
        let store = Arc::new(StubRegistry { tasks: vec![task] });
        let capability = BackgroundCapability {
            session_id,
            task_registry: store.clone(),
            session_store: store,
        };
        let ctx = SystemPromptContext::without_file_store(session_id);

        let prompt = capability
            .system_prompt_contribution(&ctx)
            .await
            .expect("background capability contributes a prompt");

        assert!(prompt.contains("Active scheduled monitor obligations"));
        assert!(prompt.contains("task_scheduled_check"));
        assert!(prompt.contains("Reconcile each"));
    }

    #[tokio::test]
    async fn background_command_renders_the_session_task_tree() {
        let session_id = SessionId::from_seed(43);
        let task = everruns_core::session_task::new_session_task(
            CreateSessionTask {
                session_id,
                id: Some("task_command".into()),
                kind: everruns_core::session_task::TASK_KIND_BACKGROUND_TOOL.into(),
                display_name: "compile workspace".into(),
                spec: serde_json::json!({}),
                state: everruns_core::session_task::SessionTaskState::Running,
                links: Default::default(),
                wake_policy: everruns_core::session_task::TaskWakePolicy::Silent,
            },
            chrono::Utc::now(),
        );
        let store = Arc::new(StubRegistry { tasks: vec![task] });
        let capability = BackgroundCapability {
            session_id,
            task_registry: store.clone(),
            session_store: store,
        };

        let result = capability
            .execute_command(
                &ExecuteCommandRequest {
                    name: "background".into(),
                    arguments: None,
                    controls: None,
                },
                &CommandExecutionContext::without_host(session_id),
            )
            .await
            .expect("background command");

        assert!(result.success);
        assert!(
            result
                .message
                .contains("[task_command] background_tool running: compile workspace")
        );
    }

    #[test]
    fn spawn_background_capability_narrates_with_title() {
        let capability = NarratedBackgroundExecutionCapability::new();
        let call = ToolCall {
            id: "call-1".to_owned(),
            name: "spawn_background".to_owned(),
            arguments: json!({
                "tool": "bash",
                "title": "Wait for CI",
                "args": { "command": "gh pr checks --watch" }
            }),
        };
        let narration = capability.narrate(
            None,
            &call,
            ToolNarrationPhase::Started,
            None,
            everruns_core::tool_narration::ToolNarrationContext::default(),
        );
        assert_eq!(narration.as_deref(), Some("Spawn background: Wait for CI"));
    }
}

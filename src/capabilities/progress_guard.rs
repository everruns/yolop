// Progress guard for coding-agent efficiency.
//
// This is intentionally runtime-enforced rather than prompt-only: it observes
// tool traffic and injects a warning into the next tool result when the turn is
// spending many tools on investigation without edits or validation.

use async_trait::async_trait;
use everruns_core::atoms::{PostToolExecHook, PostToolExecHookPriority};
use everruns_core::capabilities::{Capability, CapabilityStatus, SystemPromptContext};
use everruns_core::tool_types::{ToolCall, ToolDefinition, ToolResult};
use everruns_core::traits::ToolContext;
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

pub(crate) const PROGRESS_GUARD_CAPABILITY_ID: &str = "progress_guard";

const EXPLORATION_WITHOUT_PROGRESS_THRESHOLD: usize = 24;
const CHECKPOINT_WITHOUT_PROGRESS_THRESHOLD: usize = 48;
const REPEATED_EXPLORATION_THRESHOLD: usize = 5;
const REPEATED_STATUS_THRESHOLD: usize = 3;
const REPEATED_WAITING_THRESHOLD: usize = 3;

pub(crate) struct ProgressGuardCapability {
    state: Arc<Mutex<ProgressGuardState>>,
}

impl ProgressGuardCapability {
    pub(crate) fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(ProgressGuardState::default())),
        }
    }
}

#[async_trait]
impl Capability for ProgressGuardCapability {
    fn id(&self) -> &str {
        PROGRESS_GUARD_CAPABILITY_ID
    }

    fn name(&self) -> &str {
        "Progress Guard"
    }

    fn description(&self) -> &str {
        "Warns the coding agent when tool usage suggests investigation without progress."
    }

    fn status(&self) -> CapabilityStatus {
        CapabilityStatus::Available
    }

    fn category(&self) -> Option<&str> {
        Some("Guardrails")
    }

    fn is_guardrail(&self) -> bool {
        true
    }

    async fn system_prompt_contribution(&self, _ctx: &SystemPromptContext) -> Option<String> {
        Some(
            "<capability id=\"progress_guard\">\n\
             The runtime tracks investigation, mutation, and validation tool usage. If a \
             progress_guard warning appears in a tool result, stop broad exploration, state \
             the current hypothesis, inspect only the missing evidence, make/verify the \
             smallest relevant change, or end with a no-change diagnosis before continuing. \
             Escalated warnings require a checkpoint: facts, hypothesis, and next decisive \
             action.\n\
             </capability>"
                .to_string(),
        )
    }

    fn system_prompt_preview(&self) -> Option<String> {
        Some(
            "<capability id=\"progress_guard\">\nWarns on investigation without progress.\n</capability>"
                .to_string(),
        )
    }

    fn post_tool_exec_hooks(&self) -> Vec<Arc<dyn PostToolExecHook>> {
        vec![Arc::new(ProgressGuardHook {
            state: self.state.clone(),
        })]
    }
}

#[derive(Default)]
struct ProgressGuardState {
    sessions: HashMap<String, SessionProgress>,
}

#[derive(Default)]
struct SessionProgress {
    tool_count: usize,
    exploration_since_progress: usize,
    mutation_count: usize,
    validation_count: usize,
    repeated_status_count: usize,
    last_status_command: Option<String>,
    repeated_waiting_count: usize,
    repeated_exploration_count: usize,
    last_exploration_signature: Option<String>,
    warning_count: usize,
}

impl SessionProgress {
    fn observe(&mut self, tool_call: &ToolCall) -> Option<String> {
        self.tool_count += 1;
        let class = classify_tool_call(tool_call);

        match class {
            ToolClass::Mutation => {
                self.mutation_count += 1;
                self.exploration_since_progress = 0;
                self.repeated_status_count = 0;
                self.last_status_command = None;
                self.repeated_waiting_count = 0;
                self.repeated_exploration_count = 0;
                self.last_exploration_signature = None;
                None
            }
            ToolClass::Validation => {
                self.validation_count += 1;
                self.exploration_since_progress = 0;
                self.repeated_status_count = 0;
                self.last_status_command = None;
                self.repeated_waiting_count = 0;
                self.repeated_exploration_count = 0;
                self.last_exploration_signature = None;
                None
            }
            ToolClass::Waiting => {
                self.exploration_since_progress += 1;
                self.repeated_waiting_count += 1;
                if self.repeated_waiting_count >= REPEATED_WAITING_THRESHOLD {
                    self.warning_count += 1;
                    self.repeated_waiting_count = 0;
                    return Some(
                        "progress_guard: repeated checks of an external event (CI run, PR checks/reviews) without other progress. Do not poll across turns: run one blocking watch detached via spawn_background (e.g. `gh pr checks --watch` or an `until <check>; do sleep 30; done` loop) and end the turn — completion wakes the agent. In one-shot mode, block on the spawned task with wait_task instead."
                            .to_string(),
                    );
                }
                self.exploration_warning()
            }
            ToolClass::Status(command) => {
                self.exploration_since_progress += 1;
                self.repeated_waiting_count = 0;
                if self.last_status_command.as_deref() == Some(command.as_str()) {
                    self.repeated_status_count += 1;
                } else {
                    self.repeated_status_count = 1;
                    self.last_status_command = Some(command);
                }
                if self.repeated_status_count >= REPEATED_STATUS_THRESHOLD {
                    self.warning_count += 1;
                    self.repeated_status_count = 0;
                    return Some(
                        "progress_guard: repeated git status/diff checks without an intervening edit or validation. Use the latest result, make a targeted change, run a decisive check, or explain why no change is needed."
                            .to_string(),
                    );
                }
                self.exploration_warning()
            }
            ToolClass::Exploration => {
                self.exploration_since_progress += 1;
                self.repeated_waiting_count = 0;
                if let Some(warning) = self.repetition_warning(tool_call) {
                    return Some(warning);
                }
                self.exploration_warning()
            }
            ToolClass::Other => {
                // The waiting counter is strictly consecutive (any other tool
                // resets it) so legitimate one-off PR/CI checks between real
                // work never accumulate into a false poll warning.
                self.repeated_waiting_count = 0;
                None
            }
        }
    }

    fn exploration_warning(&mut self) -> Option<String> {
        if self.exploration_since_progress == EXPLORATION_WITHOUT_PROGRESS_THRESHOLD {
            self.warning_count += 1;
            return Some(format!(
                "progress_guard: {EXPLORATION_WITHOUT_PROGRESS_THRESHOLD} investigation tools have run without an edit or validation. Narrow the hypothesis now: identify the exact missing evidence, make the smallest relevant change, or run one decisive verification command."
            ));
        }
        if self.exploration_since_progress >= CHECKPOINT_WITHOUT_PROGRESS_THRESHOLD {
            self.warning_count += 1;
            return Some(format!(
                "progress_guard: checkpoint required after {count} investigation tools without an edit or validation. Stop reading broadly and produce a checkpoint before more exploration: facts learned, current hypothesis, and the next decisive action (edit, validation, or no-change diagnosis).",
                count = self.exploration_since_progress
            ));
        }
        None
    }

    fn repetition_warning(&mut self, tool_call: &ToolCall) -> Option<String> {
        let Some(signature) = exploration_signature(tool_call) else {
            self.repeated_exploration_count = 0;
            self.last_exploration_signature = None;
            return None;
        };
        if self.last_exploration_signature.as_deref() == Some(signature.as_str()) {
            self.repeated_exploration_count += 1;
        } else {
            self.repeated_exploration_count = 1;
            self.last_exploration_signature = Some(signature);
        }
        if self.repeated_exploration_count >= REPEATED_EXPLORATION_THRESHOLD {
            self.warning_count += 1;
            self.repeated_exploration_count = 0;
            return Some(
                "progress_guard: repeated the same investigation target without an intervening edit or validation. Use the evidence already gathered, state the hypothesis, or switch to a decisive test/change."
                    .to_string(),
            );
        }
        None
    }
}

struct ProgressGuardHook {
    state: Arc<Mutex<ProgressGuardState>>,
}

#[async_trait]
impl PostToolExecHook for ProgressGuardHook {
    fn priority(&self) -> PostToolExecHookPriority {
        PostToolExecHookPriority::Normal
    }

    async fn after_exec(
        &self,
        tool_call: &ToolCall,
        _tool_def: &ToolDefinition,
        result: &mut ToolResult,
        context: &ToolContext,
    ) {
        let warning = {
            let mut state = self.state.lock().expect("progress guard state poisoned");
            let progress = state
                .sessions
                .entry(context.session_id.to_string())
                .or_default();
            progress.observe(tool_call)
        };

        if let Some(warning) = warning {
            inject_warning(result, warning);
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum ToolClass {
    Exploration,
    Mutation,
    Validation,
    Status(String),
    /// Checking on an external event (CI run, PR checks/reviews) or plain
    /// sleeping — the poll-loop shape that should instead be one detached
    /// `spawn_background` watch.
    Waiting,
    Other,
}

fn classify_tool_call(tool_call: &ToolCall) -> ToolClass {
    match tool_call.name.as_str() {
        "read_file" | "grep_files" | "repo_map" | "ast_grep" | "list_directory" | "stat_file" => {
            ToolClass::Exploration
        }
        "write_file" | "edit_file" | "delete_file" | "ast_edit" => ToolClass::Mutation,
        "bash" => classify_bash_command(
            tool_call
                .arguments
                .get("command")
                .and_then(Value::as_str)
                .unwrap_or_default(),
        ),
        _ => ToolClass::Other,
    }
}

fn classify_bash_command(command: &str) -> ToolClass {
    let normalized = normalize_command(command);
    if normalized.is_empty() {
        return ToolClass::Other;
    }
    // A leading `sleep N && …` is a poll delay: classify the probed command
    // (`sleep 5 && cargo test` is still validation). A bare `sleep` is pure
    // waiting.
    if let Some(tail) = poll_delay_tail(&normalized) {
        if tail.is_empty() {
            return ToolClass::Waiting;
        }
        return classify_bash_command(tail);
    }
    if is_waiting_command(&normalized) {
        return ToolClass::Waiting;
    }
    if is_status_command(&normalized) {
        return ToolClass::Status(normalized);
    }
    if is_validation_command(&normalized) {
        return ToolClass::Validation;
    }
    if is_mutating_command(&normalized) {
        return ToolClass::Mutation;
    }
    if is_exploration_command(&normalized) {
        return ToolClass::Exploration;
    }
    ToolClass::Other
}

/// For a command starting with `sleep`, everything after the first `&&`/`;`
/// (empty when the sleep is the whole command). `None` when the command does
/// not start with a sleep.
fn poll_delay_tail(command: &str) -> Option<&str> {
    let rest = command.strip_prefix("sleep")?;
    if !rest.is_empty() && !rest.starts_with(' ') {
        return None;
    }
    let tail = rest
        .find("&&")
        .map(|at| &rest[at + 2..])
        .or_else(|| rest.find(';').map(|at| &rest[at + 1..]))
        .unwrap_or("");
    Some(tail.trim())
}

fn normalize_command(command: &str) -> String {
    command.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn exploration_signature(tool_call: &ToolCall) -> Option<String> {
    match tool_call.name.as_str() {
        "read_file" => {
            let path = tool_call.arguments.get("path").and_then(Value::as_str)?;
            let offset = tool_call
                .arguments
                .get("offset")
                .and_then(Value::as_i64)
                .unwrap_or(0);
            Some(format!("read_file:{path}:{offset}"))
        }
        "grep_files" => {
            let pattern = tool_call
                .arguments
                .get("pattern")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let path_pattern = tool_call
                .arguments
                .get("path_pattern")
                .and_then(Value::as_str)
                .unwrap_or_default();
            Some(format!("grep_files:{path_pattern}:{pattern}"))
        }
        "repo_map" | "ast_grep" | "list_directory" | "stat_file" => Some(format!(
            "{}:{}",
            tool_call.name,
            normalize_value(&tool_call.arguments)
        )),
        "bash" => {
            let command = tool_call
                .arguments
                .get("command")
                .and_then(Value::as_str)
                .map(normalize_command)
                .unwrap_or_default();
            is_exploration_command(&command).then(|| format!("bash:{command}"))
        }
        _ => None,
    }
}

fn normalize_value(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| value.to_string())
}

fn is_waiting_command(command: &str) -> bool {
    let prefixes = [
        "gh pr checks",
        "gh pr status",
        "gh pr view",
        "gh run list",
        "gh run view",
        "gh run watch",
        "gh workflow view",
    ];
    prefixes.iter().any(|prefix| command.starts_with(prefix))
}

fn is_status_command(command: &str) -> bool {
    matches!(
        command,
        "git status" | "git status --short" | "git status --short --branch" | "git diff"
    ) || command.starts_with("git diff ")
        || command.starts_with("git status ")
}

fn is_validation_command(command: &str) -> bool {
    let prefixes = [
        "cargo test",
        "cargo clippy",
        "cargo fmt --check",
        "npm test",
        "npm run test",
        "pnpm test",
        "pnpm run test",
        "yarn test",
        "pytest",
        "uv run",
        "go test",
        "python -m unittest",
    ];
    prefixes.iter().any(|prefix| command.starts_with(prefix))
}

fn is_mutating_command(command: &str) -> bool {
    let tokens = [
        "apply_patch",
        "cargo fmt",
        "npm run format",
        "pnpm run format",
        "git apply",
        "git commit",
        "git add",
        "mv ",
        "cp ",
        "rm ",
        "mkdir ",
    ];
    tokens.iter().any(|token| command.contains(token))
}

fn is_exploration_command(command: &str) -> bool {
    let prefixes = [
        "rg ",
        "grep ",
        "find ",
        "sed ",
        "cat ",
        "ls",
        "git show",
        "git log",
        "git blame",
        "git grep",
        "git ls-files",
    ];
    prefixes.iter().any(|prefix| command.starts_with(prefix))
}

fn inject_warning(result: &mut ToolResult, warning: String) {
    let mut object = match result.result.take() {
        Some(Value::Object(object)) => object,
        Some(value) => {
            let mut object = Map::new();
            object.insert("result".to_string(), value);
            object
        }
        None => Map::new(),
    };
    object.insert("progress_guard_warning".to_string(), json!(warning));
    result.result = Some(Value::Object(object));
}

#[cfg(test)]
mod tests {
    use super::*;
    use everruns_core::tool_types::{
        BuiltinTool, DeferrablePolicy, ToolHints, ToolPolicy, ToolResult,
    };
    use everruns_core::typed_id::SessionId;

    fn call(name: &str, arguments: Value) -> ToolCall {
        ToolCall {
            id: format!("call-{name}"),
            name: name.to_string(),
            arguments,
        }
    }

    fn tool_def(name: &str) -> ToolDefinition {
        ToolDefinition::Builtin(BuiltinTool {
            name: name.to_string(),
            display_name: None,
            description: "test".to_string(),
            parameters: json!({ "type": "object" }),
            policy: ToolPolicy::Auto,
            category: None,
            deferrable: DeferrablePolicy::Never,
            hints: ToolHints::default(),
            full_parameters: None,
        })
    }

    fn result() -> ToolResult {
        ToolResult {
            tool_call_id: "call".to_string(),
            result: Some(json!({ "ok": true })),
            images: None,
            error: None,
            connection_required: None,
            raw_output: None,
        }
    }

    #[test]
    fn classify_bash_command_distinguishes_status_and_validation() {
        assert_eq!(
            classify_bash_command("git status --short --branch"),
            ToolClass::Status("git status --short --branch".to_string())
        );
        assert_eq!(
            classify_bash_command("cargo test --all-features"),
            ToolClass::Validation
        );
        assert_eq!(
            classify_bash_command("rg progress_guard"),
            ToolClass::Exploration
        );
    }

    #[test]
    fn classify_bash_command_detects_external_event_waits() {
        assert_eq!(classify_bash_command("gh pr checks 42"), ToolClass::Waiting);
        assert_eq!(
            classify_bash_command("gh run list --branch main --limit 5"),
            ToolClass::Waiting
        );
        assert_eq!(classify_bash_command("sleep 120"), ToolClass::Waiting);
        assert_eq!(
            classify_bash_command("sleep 30 && gh pr checks 42"),
            ToolClass::Waiting
        );
        // A sleep in front of real work classifies as the work itself.
        assert_eq!(
            classify_bash_command("sleep 5 && cargo test --all-features"),
            ToolClass::Validation
        );
        // `sleepwalk` must not parse as a sleep prefix.
        assert_eq!(classify_bash_command("sleepwalk"), ToolClass::Other);
    }

    #[tokio::test]
    async fn hook_warns_after_long_exploration_without_progress() {
        let state = Arc::new(Mutex::new(ProgressGuardState::default()));
        let hook = ProgressGuardHook { state };
        let context = ToolContext::new(SessionId::new());
        let mut last = result();

        for _ in 0..EXPLORATION_WITHOUT_PROGRESS_THRESHOLD {
            last = result();
            hook.after_exec(
                &call("read_file", json!({ "path": "/src/lib.rs" })),
                &tool_def("read_file"),
                &mut last,
                &context,
            )
            .await;
        }

        assert!(
            last.result
                .as_ref()
                .and_then(|value| value.get("progress_guard_warning"))
                .and_then(Value::as_str)
                .is_some_and(|warning| warning.contains("investigation tools"))
        );
    }

    #[tokio::test]
    async fn hook_escalates_to_checkpoint_after_more_exploration() {
        let state = Arc::new(Mutex::new(ProgressGuardState::default()));
        let hook = ProgressGuardHook { state };
        let context = ToolContext::new(SessionId::new());
        let mut last = result();

        for i in 0..CHECKPOINT_WITHOUT_PROGRESS_THRESHOLD {
            last = result();
            hook.after_exec(
                &call("read_file", json!({ "path": format!("/src/{i}.rs") })),
                &tool_def("read_file"),
                &mut last,
                &context,
            )
            .await;
        }

        assert!(
            last.result
                .as_ref()
                .and_then(|value| value.get("progress_guard_warning"))
                .and_then(Value::as_str)
                .is_some_and(|warning| warning.contains("checkpoint required"))
        );
    }

    #[tokio::test]
    async fn hook_keeps_warning_after_checkpoint_until_progress() {
        let state = Arc::new(Mutex::new(ProgressGuardState::default()));
        let hook = ProgressGuardHook { state };
        let context = ToolContext::new(SessionId::new());
        let mut last = result();

        for i in 0..=CHECKPOINT_WITHOUT_PROGRESS_THRESHOLD {
            last = result();
            hook.after_exec(
                &call("grep_files", json!({ "pattern": format!("needle{i}") })),
                &tool_def("grep_files"),
                &mut last,
                &context,
            )
            .await;
        }

        assert!(
            last.result
                .as_ref()
                .and_then(|value| value.get("progress_guard_warning"))
                .and_then(Value::as_str)
                .is_some_and(|warning| warning.contains("checkpoint required"))
        );
    }

    #[tokio::test]
    async fn mutation_resets_exploration_warning_counter() {
        let state = Arc::new(Mutex::new(ProgressGuardState::default()));
        let hook = ProgressGuardHook { state };
        let context = ToolContext::new(SessionId::new());
        let mut last = result();

        for _ in 0..(EXPLORATION_WITHOUT_PROGRESS_THRESHOLD - 1) {
            hook.after_exec(
                &call("read_file", json!({ "path": "/src/lib.rs" })),
                &tool_def("read_file"),
                &mut result(),
                &context,
            )
            .await;
        }
        hook.after_exec(
            &call("edit_file", json!({ "path": "/src/lib.rs" })),
            &tool_def("edit_file"),
            &mut result(),
            &context,
        )
        .await;
        for _ in 0..(EXPLORATION_WITHOUT_PROGRESS_THRESHOLD - 1) {
            last = result();
            hook.after_exec(
                &call("read_file", json!({ "path": "/src/lib.rs" })),
                &tool_def("read_file"),
                &mut last,
                &context,
            )
            .await;
        }

        assert!(
            last.result
                .as_ref()
                .and_then(|value| value.get("progress_guard_warning"))
                .is_none()
        );
    }

    #[tokio::test]
    async fn validation_resets_checkpoint_warning_counter() {
        let state = Arc::new(Mutex::new(ProgressGuardState::default()));
        let hook = ProgressGuardHook { state };
        let context = ToolContext::new(SessionId::new());
        let mut last = result();

        for i in 0..CHECKPOINT_WITHOUT_PROGRESS_THRESHOLD {
            hook.after_exec(
                &call("read_file", json!({ "path": format!("/src/{i}.rs") })),
                &tool_def("read_file"),
                &mut result(),
                &context,
            )
            .await;
        }
        hook.after_exec(
            &call("bash", json!({ "command": "cargo test --all-features" })),
            &tool_def("bash"),
            &mut result(),
            &context,
        )
        .await;
        for i in 0..(EXPLORATION_WITHOUT_PROGRESS_THRESHOLD - 1) {
            last = result();
            hook.after_exec(
                &call("read_file", json!({ "path": format!("/src/after/{i}.rs") })),
                &tool_def("read_file"),
                &mut last,
                &context,
            )
            .await;
        }

        assert!(
            last.result
                .as_ref()
                .and_then(|value| value.get("progress_guard_warning"))
                .is_none()
        );
    }

    #[tokio::test]
    async fn repeated_exploration_target_warns() {
        let state = Arc::new(Mutex::new(ProgressGuardState::default()));
        let hook = ProgressGuardHook { state };
        let context = ToolContext::new(SessionId::new());
        let mut last = result();

        for _ in 0..REPEATED_EXPLORATION_THRESHOLD {
            last = result();
            hook.after_exec(
                &call("read_file", json!({ "path": "/src/lib.rs", "offset": 10 })),
                &tool_def("read_file"),
                &mut last,
                &context,
            )
            .await;
        }

        assert!(
            last.result
                .as_ref()
                .and_then(|value| value.get("progress_guard_warning"))
                .and_then(Value::as_str)
                .is_some_and(|warning| warning.contains("same investigation target"))
        );
    }

    #[tokio::test]
    async fn original_session_pattern_gets_interrupted_before_runaway_reads() {
        let state = Arc::new(Mutex::new(ProgressGuardState::default()));
        let hook = ProgressGuardHook { state };
        let context = ToolContext::new(SessionId::new());
        let mut warnings = Vec::new();

        let observe = |tool_call: ToolCall| {
            let hook = &hook;
            let context = &context;
            async move {
                let mut out = result();
                hook.after_exec(&tool_call, &tool_def(&tool_call.name), &mut out, context)
                    .await;
                out.result
                    .as_ref()
                    .and_then(|value| value.get("progress_guard_warning"))
                    .and_then(Value::as_str)
                    .map(str::to_string)
            }
        };

        // Same shape as the failed investigation: broad searches, then many
        // repeated reads of the callback-adjacent files, with no edit/test.
        let broad_searches = [
            "scheduled|schedule|signal_on_completion|callback|background",
            "spawn_background|signal_on_completion|scheduled_at",
            "cron|completion|task|background_run|Background|Task",
            "drain_finished_for_wake|wake_prompt|BackgroundRegistry",
            "SessionScheduleStore|schedule_store|create_schedule",
            "maybe_wake_for_background|TaskRegistryEvent",
        ];
        for pattern in broad_searches {
            if let Some(warning) = observe(call(
                "grep_files",
                json!({ "pattern": pattern, "path_pattern": "src/**/*.rs" }),
            ))
            .await
            {
                warnings.push(warning);
            }
        }
        for offset in [180, 388, 633, 940, 1370, 1540, 180, 388, 633] {
            if let Some(warning) = observe(call(
                "read_file",
                json!({ "path": "/workspace/src/capabilities/background.rs", "offset": offset }),
            ))
            .await
            {
                warnings.push(warning);
            }
        }
        for offset in [1000, 1020, 1010, 1028, 1000, 1020, 1010, 1028, 1000] {
            if let Some(warning) = observe(call(
                "read_file",
                json!({ "path": "/workspace/src/app/mod.rs", "offset": offset }),
            ))
            .await
            {
                warnings.push(warning);
            }
        }
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("investigation tools")),
            "first threshold should warn before the session keeps circling: {warnings:?}"
        );

        for i in 0..24 {
            if let Some(warning) = observe(call(
                "read_file",
                json!({ "path": format!("/workspace/src/runtime.rs"), "offset": 2100 + i }),
            ))
            .await
            {
                warnings.push(warning);
            }
        }
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("checkpoint required")),
            "checkpoint escalation should trigger by 48 read/search calls: {warnings:?}"
        );

        for _ in 0..REPEATED_EXPLORATION_THRESHOLD {
            if let Some(warning) = observe(call(
                "read_file",
                json!({ "path": "/workspace/src/app/mod.rs", "offset": 1000 }),
            ))
            .await
            {
                warnings.push(warning);
            }
        }
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("same investigation target")),
            "semantic repetition should catch rereading the same range: {warnings:?}"
        );
    }

    #[tokio::test]
    async fn repeated_ci_polling_warns_toward_spawn_background() {
        let state = Arc::new(Mutex::new(ProgressGuardState::default()));
        let hook = ProgressGuardHook { state };
        let context = ToolContext::new(SessionId::new());
        let mut last = result();

        // The classic poll loop: delay-then-check, over and over. Different
        // probe commands still count — polling rarely repeats verbatim.
        let polls = [
            "gh pr checks 42",
            "sleep 30 && gh pr checks 42",
            "gh run list --limit 1",
        ];
        for command in polls {
            last = result();
            hook.after_exec(
                &call("bash", json!({ "command": command })),
                &tool_def("bash"),
                &mut last,
                &context,
            )
            .await;
        }

        assert!(
            last.result
                .as_ref()
                .and_then(|value| value.get("progress_guard_warning"))
                .and_then(Value::as_str)
                .is_some_and(|warning| warning.contains("spawn_background"))
        );
    }

    #[tokio::test]
    async fn interleaved_work_resets_waiting_counter() {
        let state = Arc::new(Mutex::new(ProgressGuardState::default()));
        let hook = ProgressGuardHook { state };
        let context = ToolContext::new(SessionId::new());

        // Check CI, fix something, check again — legitimate, never a poll
        // warning because the counter is strictly consecutive.
        for _ in 0..(REPEATED_WAITING_THRESHOLD * 2) {
            let mut check = result();
            hook.after_exec(
                &call("bash", json!({ "command": "gh pr checks 42" })),
                &tool_def("bash"),
                &mut check,
                &context,
            )
            .await;
            assert!(
                check
                    .result
                    .as_ref()
                    .and_then(|value| value.get("progress_guard_warning"))
                    .is_none()
            );
            hook.after_exec(
                &call("edit_file", json!({ "path": "/src/lib.rs" })),
                &tool_def("edit_file"),
                &mut result(),
                &context,
            )
            .await;
        }
    }

    #[tokio::test]
    async fn repeated_git_status_warns() {
        let state = Arc::new(Mutex::new(ProgressGuardState::default()));
        let hook = ProgressGuardHook { state };
        let context = ToolContext::new(SessionId::new());
        let mut last = result();

        for _ in 0..REPEATED_STATUS_THRESHOLD {
            last = result();
            hook.after_exec(
                &call("bash", json!({ "command": "git status --short" })),
                &tool_def("bash"),
                &mut last,
                &context,
            )
            .await;
        }

        assert!(
            last.result
                .as_ref()
                .and_then(|value| value.get("progress_guard_warning"))
                .and_then(Value::as_str)
                .is_some_and(|warning| warning.contains("repeated git status"))
        );
    }
}

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
const REPEATED_STATUS_THRESHOLD: usize = 3;

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
             the current hypothesis, inspect only the missing evidence, or make/verify the \
             smallest relevant change before continuing.\n\
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
                None
            }
            ToolClass::Validation => {
                self.validation_count += 1;
                self.exploration_since_progress = 0;
                self.repeated_status_count = 0;
                self.last_status_command = None;
                None
            }
            ToolClass::Status(command) => {
                self.exploration_since_progress += 1;
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
                self.exploration_warning()
            }
            ToolClass::Other => None,
        }
    }

    fn exploration_warning(&mut self) -> Option<String> {
        if self.exploration_since_progress == EXPLORATION_WITHOUT_PROGRESS_THRESHOLD {
            self.warning_count += 1;
            return Some(format!(
                "progress_guard: {EXPLORATION_WITHOUT_PROGRESS_THRESHOLD} investigation tools have run without an edit or validation. Narrow the hypothesis now: identify the exact missing evidence, make the smallest relevant change, or run one decisive verification command."
            ));
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
    Other,
}

fn classify_tool_call(tool_call: &ToolCall) -> ToolClass {
    match tool_call.name.as_str() {
        "read_file" | "grep_files" | "repo_map" | "ast_grep" | "list_directory" | "stat_file" => {
            ToolClass::Exploration
        }
        "write_file" | "edit_file" | "delete_file" => ToolClass::Mutation,
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

fn normalize_command(command: &str) -> String {
    command.split_whitespace().collect::<Vec<_>>().join(" ")
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

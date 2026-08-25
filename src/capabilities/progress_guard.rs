// Progress guard for coding-agent efficiency.
//
// This is intentionally runtime-enforced rather than prompt-only: it observes
// tool traffic and injects a warning into the next tool result when the turn is
// spending many tools on investigation without edits or validation.

use async_trait::async_trait;
use everruns_core::ToolContext;
use everruns_core::tool_hooks::{
    PostToolExecHook, PostToolExecHookPriority, PreToolUseDecision, PreToolUseHook,
};
use everruns_core::{Capability, CapabilityStatus, SystemPromptContext, ToolDefinitionHook};
use everruns_core::{Tool, ToolExecutionResult};
use everruns_provider::typed_id::SessionId;
use everruns_provider::{DeferrablePolicy, ToolCall, ToolDefinition, ToolResult};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet, VecDeque, hash_map::DefaultHasher};
use std::fs::OpenOptions;
use std::hash::{Hash, Hasher};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub(crate) const PROGRESS_GUARD_CAPABILITY_ID: &str = "progress_guard";
const PROGRESS_CHECKPOINT_TOOL: &str = "progress_checkpoint";

const EXPLORATION_WITHOUT_PROGRESS_THRESHOLD: usize = 24;
const CHECKPOINT_WITHOUT_PROGRESS_THRESHOLD: usize = 48;
const REPEATED_EXPLORATION_THRESHOLD: usize = 5;
const ZERO_EVIDENCE_SEARCH_THRESHOLD: usize = 3;
const TRUNCATED_EXPLORATION_THRESHOLD: usize = 2;
const REPEATED_STATUS_THRESHOLD: usize = 3;
const WAITING_WINDOW_SIZE: usize = 8;
const WAITING_WINDOW_THRESHOLD: usize = 4;
const SEMANTIC_HISTORY_LIMIT: usize = 512;
const TRACKED_PATH_LIMIT: usize = 1024;
const MIN_REUSABLE_RESULT_BYTES: usize = 512;
const FAILURE_DIAGNOSTIC_LIMIT: usize = 4096;
const SESSION_STATE_LIMIT: usize = 32;
const STORED_STATE_VERSION: u8 = 1;

pub(crate) struct ProgressGuardCapability {
    state: Arc<Mutex<ProgressGuardState>>,
}

impl ProgressGuardCapability {
    pub(crate) fn open(
        session_dir: &Path,
        session_id: SessionId,
        active_tool_count: usize,
    ) -> Self {
        let store = Arc::new(ProgressGuardStore::new(
            session_dir.join("progress-guard.json"),
        ));
        let mut state = ProgressGuardState {
            store: store.clone(),
            ..ProgressGuardState::default()
        };
        if let Some(progress) = store.load(session_id, active_tool_count) {
            state.sessions.insert(session_id.to_string(), progress);
            state.recent_sessions.push_back(session_id.to_string());
        }
        Self {
            state: Arc::new(Mutex::new(state)),
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
        "Compacts unchanged evidence and forces a different action or checkpoint when investigation or equivalent failures stop making progress."
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

    // No system-prompt contribution. Every warning this capability emits already
    // names the situation and the required next action, and it arrives in the
    // tool result at the moment it applies. Pre-announcing the mechanism on every
    // turn paid for a warning that usually never fires.

    fn system_prompt_preview(&self) -> Option<String> {
        Some(
            "<capability id=\"progress_guard\">\nWarns and gates investigation or equivalent failures without progress.\n</capability>"
                .to_string(),
        )
    }

    fn post_tool_exec_hooks(&self) -> Vec<Arc<dyn PostToolExecHook>> {
        vec![Arc::new(ProgressGuardHook {
            state: self.state.clone(),
        })]
    }

    fn pre_tool_use_hooks(&self) -> Vec<Arc<dyn PreToolUseHook>> {
        vec![Arc::new(ProgressGuardGate {
            state: self.state.clone(),
        })]
    }

    fn tool_definition_hooks_with_context(
        &self,
        context: &SystemPromptContext,
        _config: &Value,
    ) -> Vec<Arc<dyn ToolDefinitionHook>> {
        vec![Arc::new(ProgressGuardToolGate {
            state: self.state.clone(),
            session_id: context.session_id.to_string(),
        })]
    }

    fn tools(&self) -> Vec<Box<dyn Tool>> {
        vec![Box::new(ProgressCheckpointTool)]
    }
}

#[derive(Default)]
struct ProgressGuardState {
    sessions: HashMap<String, SessionProgress>,
    recent_sessions: VecDeque<String>,
    store: Arc<ProgressGuardStore>,
}

impl ProgressGuardState {
    fn session_mut(&mut self, session_id: &str) -> &mut SessionProgress {
        self.recent_sessions
            .retain(|candidate| candidate != session_id);
        self.recent_sessions.push_back(session_id.to_string());
        while self.recent_sessions.len() > SESSION_STATE_LIMIT {
            if let Some(expired) = self.recent_sessions.pop_front() {
                self.sessions.remove(&expired);
            }
        }
        self.sessions.entry(session_id.to_string()).or_default()
    }
}

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(default)]
struct SessionProgress {
    tool_count: usize,
    exploration_since_progress: usize,
    mutation_count: usize,
    validation_count: usize,
    repeated_status_count: usize,
    last_status_command: Option<String>,
    recent_waiting: VecDeque<bool>,
    repeated_exploration_count: usize,
    last_exploration_signature: Option<String>,
    consecutive_zero_evidence_searches: usize,
    consecutive_truncated_exploration: usize,
    warning_count: usize,
    checkpoint_required: bool,
    checkpoint_count: usize,
    workspace_epoch: u64,
    workspace_hashes: HashMap<String, String>,
    recent_workspace_states: VecDeque<u64>,
    seen_workspace_states: HashSet<u64>,
    recent_validations: VecDeque<(u64, String)>,
    seen_validations: HashSet<(u64, String)>,
    recent_warned_validations: VecDeque<(u64, String)>,
    warned_validations: HashSet<(u64, String)>,
    recent_observations: VecDeque<String>,
    observation_hashes: HashMap<String, [u8; 32]>,
    warned_observation_hashes: HashMap<String, [u8; 32]>,
    warned_repetition_signatures: HashSet<String>,
    recent_warned_workspace_states: VecDeque<u64>,
    warned_workspace_states: HashSet<u64>,
    warned_zero_evidence: bool,
    warned_truncated_exploration: bool,
    waiting_warning_emitted: bool,
    recent_warned_status_commands: VecDeque<String>,
    warned_status_commands: HashSet<String>,
    recent_checkpoints: VecDeque<String>,
    seen_checkpoints: HashSet<String>,
    last_failure: Option<FailureFingerprint>,
    consecutive_equivalent_failures: usize,
    failure_gate: Option<FailureGate>,
    recent_warned_failures: VecDeque<FailureFingerprint>,
    warned_failures: HashSet<FailureFingerprint>,
}

impl SessionProgress {
    fn observe(&mut self, tool_call: &ToolCall, result: &mut ToolResult) -> Option<String> {
        self.tool_count += 1;
        if tool_call.name == PROGRESS_CHECKPOINT_TOOL {
            self.observe_checkpoint(tool_call, result);
            return None;
        }
        if let Some(class) = classify_semantic_failure(result) {
            return self.observe_failure(tool_call, class);
        }
        self.reset_failure_streak();
        let class = classify_tool_call(tool_call);

        match class {
            ToolClass::Mutation => self.observe_mutation(tool_call, result),
            ToolClass::Validation(command) => {
                self.validation_count += 1;
                let validation = (self.validation_state_signature(), command);
                if self.seen_validations.contains(&validation) {
                    if !self.warned_validations.contains(&validation) {
                        self.warning_count += 1;
                        remember_bounded(
                            &mut self.warned_validations,
                            &mut self.recent_warned_validations,
                            validation,
                        );
                        return Some(
                            "progress_guard: repeated the same validation command on an unchanged workspace state. The result adds no new code evidence; use the existing result, change the relevant state, or explain why an external retry is necessary before running it again."
                                .to_string(),
                        );
                    }
                    return None;
                }
                self.reset_activity_streaks();
                self.seen_checkpoints.clear();
                self.recent_checkpoints.clear();
                remember_bounded(
                    &mut self.seen_validations,
                    &mut self.recent_validations,
                    validation,
                );
                None
            }
            ToolClass::Waiting => {
                self.exploration_since_progress += 1;
                self.reset_result_streaks();
                if self.observe_waiting_signal(true) && !self.waiting_warning_emitted {
                    self.waiting_warning_emitted = true;
                    self.warning_count += 1;
                    return Some(
                        "progress_guard: repeated checks of an external event (CI run, PR checks/reviews) without other progress. Do not poll across turns: run one blocking watch detached via spawn_background (e.g. `gh pr checks --watch` or an `until <check>; do sleep 30; done` loop) and end the turn — completion wakes the agent. In one-shot mode, block on the spawned task with wait_task instead."
                            .to_string(),
                    );
                }
                self.exploration_warning()
            }
            ToolClass::Status(command) => {
                self.exploration_since_progress += 1;
                self.reset_result_streaks();
                self.observe_waiting_signal(false);
                if self.last_status_command.as_deref() == Some(command.as_str()) {
                    self.repeated_status_count += 1;
                } else {
                    self.repeated_status_count = 1;
                    self.last_status_command = Some(command);
                }
                if self.repeated_status_count >= REPEATED_STATUS_THRESHOLD {
                    self.repeated_status_count = 0;
                    let command = self.last_status_command.clone().unwrap_or_default();
                    if !self.warned_status_commands.contains(&command) {
                        remember_bounded(
                            &mut self.warned_status_commands,
                            &mut self.recent_warned_status_commands,
                            command,
                        );
                        self.warning_count += 1;
                        return Some(
                            "progress_guard: repeated git status/diff checks without an intervening edit or validation. Use the latest result, make a targeted change, run a decisive check, or explain why no change is needed."
                                .to_string(),
                        );
                    }
                }
                self.exploration_warning()
            }
            ToolClass::Exploration => {
                self.exploration_since_progress += 1;
                self.observe_waiting_signal(false);
                if let Some(warning) = self.result_warning(tool_call, result) {
                    return Some(warning);
                }
                if let Some(warning) = self.reuse_unchanged_observation(tool_call, result) {
                    return Some(warning);
                }
                if let Some(warning) = self.repetition_warning(tool_call) {
                    return Some(warning);
                }
                self.exploration_warning()
            }
            ToolClass::Other => {
                self.observe_waiting_signal(false);
                self.reset_result_streaks();
                None
            }
        }
    }

    fn observe_checkpoint(&mut self, tool_call: &ToolCall, result: &mut ToolResult) {
        if result.error.is_some() {
            return;
        }
        let failure_recovery = self.failure_gate.is_some();
        if !self.checkpoint_required && !failure_recovery {
            reject_checkpoint(result, "no progress checkpoint is currently required");
            return;
        }
        let fingerprint = checkpoint_fingerprint(&tool_call.arguments);
        if self.seen_checkpoints.contains(&fingerprint) {
            reject_checkpoint(
                result,
                "this checkpoint is unchanged; add new evidence or take the stated decisive action",
            );
            return;
        }
        remember_bounded(
            &mut self.seen_checkpoints,
            &mut self.recent_checkpoints,
            fingerprint,
        );
        self.checkpoint_count += 1;
        self.checkpoint_required = false;
        self.failure_gate = None;
        self.reset_failure_streak();
        self.reset_activity_streaks();
        result.result = Some(json!({
            "accepted": true,
            "exploration_resumed": true,
            "message": if failure_recovery {
                "failure-recovery checkpoint accepted; execute a materially different action that addresses the classified root failure"
            } else {
                "checkpoint accepted; execute the stated decisive action or gather only evidence that tests it"
            }
        }));
    }

    fn observe_failure(
        &mut self,
        tool_call: &ToolCall,
        class: SemanticFailureClass,
    ) -> Option<String> {
        let fingerprint = FailureFingerprint {
            workspace_state: self.validation_state_signature(),
            class,
        };
        if self.last_failure.as_ref() == Some(&fingerprint) {
            self.consecutive_equivalent_failures += 1;
        } else {
            self.last_failure = Some(fingerprint.clone());
            self.consecutive_equivalent_failures = 1;
        }
        if self.consecutive_equivalent_failures < 2 {
            return None;
        }

        self.failure_gate = Some(FailureGate {
            source_tool: tool_call.name.clone(),
            fingerprint: fingerprint.clone(),
        });
        if self.warned_failures.contains(&fingerprint) {
            return None;
        }
        remember_bounded(
            &mut self.warned_failures,
            &mut self.recent_warned_failures,
            fingerprint,
        );
        self.warning_count += 1;
        Some(format!(
            "progress_guard: repeated an equivalent {} failure on an unchanged workspace state. The full failure evidence remains above. Do not rewrite and retry the same invocation path: use a materially different tool/action that addresses the root cause, or call progress_checkpoint before another attempt.",
            class.label()
        ))
    }

    fn reset_failure_streak(&mut self) {
        self.last_failure = None;
        self.consecutive_equivalent_failures = 0;
    }

    fn observe_mutation(&mut self, tool_call: &ToolCall, result: &ToolResult) -> Option<String> {
        if result.error.is_some() || !mutation_was_applied(tool_call, result) {
            return None;
        }

        self.mutation_count += 1;
        self.reset_activity_streaks();
        // A mutation changes the meaning of later repository evidence even when
        // a file happens to return to the same bytes. Clear reuse state instead
        // of serving a compact marker across an edit boundary.
        self.recent_observations.clear();
        self.observation_hashes.clear();
        self.warned_observation_hashes.clear();
        self.seen_checkpoints.clear();
        self.recent_checkpoints.clear();
        self.failure_gate = None;
        self.reset_failure_streak();
        self.warned_failures.clear();
        self.recent_warned_failures.clear();

        let Some(transition) = mutation_hash_transition(tool_call, result) else {
            // Shell, delete, and structural edits can touch an unknown set of
            // files. They make validation fresh, but retaining structured file
            // hashes lets us still recognize a manifest cycle around lockfile
            // updates and other adjacent mutations.
            self.advance_opaque_mutation();
            return None;
        };

        if self.workspace_hashes.len() >= TRACKED_PATH_LIMIT
            && !self.workspace_hashes.contains_key(&transition.path)
        {
            self.reset_tracked_history();
        }

        if let Some(previous_hash) = transition.previous_hash {
            if let Some(known_hash) = self.workspace_hashes.get(&transition.path)
                && known_hash != &previous_hash
            {
                // An unobserved writer changed the file. Preserve safety by
                // abandoning comparisons with the now-incomplete trajectory.
                self.reset_tracked_history();
            }
            if !self.workspace_hashes.contains_key(&transition.path) {
                self.workspace_hashes
                    .insert(transition.path.clone(), previous_hash);
                let previous_state = self.tracked_state_signature();
                self.remember_workspace_state(previous_state);
            }
        }

        self.workspace_hashes
            .insert(transition.path, transition.content_hash);
        let current_state = self.tracked_state_signature();
        if self.remember_workspace_state(current_state)
            && !self.warned_workspace_states.contains(&current_state)
        {
            remember_bounded(
                &mut self.warned_workspace_states,
                &mut self.recent_warned_workspace_states,
                current_state,
            );
            self.warning_count += 1;
            return Some(
                "progress_guard: this mutation returned to a recently seen workspace state (the same tracked content hashes). Confirm the revert is intentional; if this is an edit/validate cycle, keep the coherent state, report the blocker, and stop repeating the cycle."
                    .to_string(),
            );
        }
        None
    }

    fn reset_activity_streaks(&mut self) {
        self.exploration_since_progress = 0;
        self.repeated_status_count = 0;
        self.last_status_command = None;
        self.recent_waiting.clear();
        self.repeated_exploration_count = 0;
        self.last_exploration_signature = None;
        self.checkpoint_required = false;
        self.warned_repetition_signatures.clear();
        self.warned_status_commands.clear();
        self.recent_warned_status_commands.clear();
        self.waiting_warning_emitted = false;
        self.reset_result_streaks();
    }

    fn observe_waiting_signal(&mut self, waiting: bool) -> bool {
        // A bounded semantic window catches heterogeneous check/read/task-probe
        // cycles while mutations and validations still clear it. Transparently
        // moving an already-started shell process would risk replaying side
        // effects and weakening one-shot cancellation, so the guard steers the
        // next wait onto the existing durable background-task path instead.
        self.recent_waiting.push_back(waiting);
        if self.recent_waiting.len() > WAITING_WINDOW_SIZE {
            self.recent_waiting.pop_front();
        }
        if self
            .recent_waiting
            .iter()
            .filter(|waiting| **waiting)
            .count()
            >= WAITING_WINDOW_THRESHOLD
        {
            self.recent_waiting.clear();
            return true;
        }
        false
    }

    fn advance_opaque_mutation(&mut self) {
        self.workspace_epoch = self.workspace_epoch.wrapping_add(1);
        self.recent_validations.clear();
        self.seen_validations.clear();
        self.recent_warned_validations.clear();
        self.warned_validations.clear();
    }

    fn reset_tracked_history(&mut self) {
        self.workspace_hashes.clear();
        self.recent_workspace_states.clear();
        self.seen_workspace_states.clear();
        self.warned_workspace_states.clear();
        self.recent_warned_workspace_states.clear();
        self.advance_opaque_mutation();
    }

    fn tracked_state_signature(&self) -> u64 {
        let mut entries = self.workspace_hashes.iter().collect::<Vec<_>>();
        entries.sort_unstable_by(|left, right| left.0.cmp(right.0));
        let mut hasher = DefaultHasher::new();
        entries.hash(&mut hasher);
        hasher.finish()
    }

    fn validation_state_signature(&self) -> u64 {
        let mut hasher = DefaultHasher::new();
        self.workspace_epoch.hash(&mut hasher);
        self.tracked_state_signature().hash(&mut hasher);
        hasher.finish()
    }

    fn remember_workspace_state(&mut self, state: u64) -> bool {
        if self.seen_workspace_states.contains(&state) {
            return true;
        }
        remember_bounded(
            &mut self.seen_workspace_states,
            &mut self.recent_workspace_states,
            state,
        );
        false
    }

    fn result_warning(&mut self, tool_call: &ToolCall, result: &ToolResult) -> Option<String> {
        let Some(evidence) = exploration_evidence(tool_call, result) else {
            self.reset_result_streaks();
            return None;
        };

        if evidence.zero_matches {
            self.consecutive_zero_evidence_searches += 1;
        } else {
            self.consecutive_zero_evidence_searches = 0;
            self.warned_zero_evidence = false;
        }
        if evidence.truncated {
            self.consecutive_truncated_exploration += 1;
        } else {
            self.consecutive_truncated_exploration = 0;
            self.warned_truncated_exploration = false;
        }

        if self.consecutive_zero_evidence_searches >= ZERO_EVIDENCE_SEARCH_THRESHOLD
            && !self.warned_zero_evidence
        {
            self.warning_count += 1;
            self.warned_zero_evidence = true;
            return Some(
                "progress_guard: three consecutive searches returned zero matches. Stop varying broad terms: verify the path/scope and search contract, then use one targeted alternative or state that no evidence was found."
                    .to_string(),
            );
        }
        if self.consecutive_truncated_exploration >= TRUNCATED_EXPLORATION_THRESHOLD
            && !self.warned_truncated_exploration
        {
            self.warning_count += 1;
            self.warned_truncated_exploration = true;
            return Some(
                "progress_guard: repeated exploration results were truncated. Narrow the query or path before requesting more output, then inspect the owning module and a small number of call sites."
                    .to_string(),
            );
        }
        None
    }

    fn reset_result_streaks(&mut self) {
        self.consecutive_zero_evidence_searches = 0;
        self.consecutive_truncated_exploration = 0;
        self.warned_zero_evidence = false;
        self.warned_truncated_exploration = false;
    }

    fn reuse_unchanged_observation(
        &mut self,
        tool_call: &ToolCall,
        result: &mut ToolResult,
    ) -> Option<String> {
        if result.error.is_some() {
            return None;
        }
        let signature = exploration_signature(tool_call)?;
        let value = result.result.as_ref()?;
        let encoded = serde_json::to_vec(value).ok()?;
        let result_hash: [u8; 32] = Sha256::digest(&encoded).into();

        let previous_hash = self.observation_hashes.get(&signature).copied();
        let unchanged = previous_hash == Some(result_hash);
        if previous_hash.is_some() && !unchanged {
            // The same read contract returning different bytes is direct
            // evidence of an external writer. Keep exploration accounting, but
            // make workspace-keyed validation evidence fresh.
            self.advance_opaque_mutation();
            self.seen_checkpoints.clear();
            self.recent_checkpoints.clear();
        }
        if !unchanged {
            self.warned_observation_hashes.remove(&signature);
            self.warned_repetition_signatures.remove(&signature);
        }
        self.remember_observation(signature.clone(), result_hash);
        if !unchanged || encoded.len() < MIN_REUSABLE_RESULT_BYTES {
            return None;
        }

        // This won against generic oversized-result summarization in the focused
        // discovery study: the first full result stays available, while an exact
        // repeat becomes a small freshness proof instead of another lossy summary.
        result.result = Some(json!({
            "unchanged_since_last_read": true,
            "tool": tool_call.name,
        }));
        if self.warned_observation_hashes.get(&signature) == Some(&result_hash) {
            return None;
        }
        self.warned_observation_hashes
            .insert(signature, result_hash);
        self.warning_count += 1;
        Some(
            "progress_guard: this read/search result is unchanged since the same target was last inspected. Reuse the earlier full result; continue only if a different question or scope needs evidence."
                .to_string(),
        )
    }

    fn remember_observation(&mut self, signature: String, result_hash: [u8; 32]) {
        if self.observation_hashes.contains_key(&signature) {
            self.recent_observations
                .retain(|candidate| candidate != &signature);
        }
        self.observation_hashes
            .insert(signature.clone(), result_hash);
        self.recent_observations.push_back(signature);
        if self.recent_observations.len() > SEMANTIC_HISTORY_LIMIT
            && let Some(expired) = self.recent_observations.pop_front()
        {
            self.observation_hashes.remove(&expired);
            self.warned_observation_hashes.remove(&expired);
            self.warned_repetition_signatures.remove(&expired);
        }
    }

    fn exploration_warning(&mut self) -> Option<String> {
        if self.exploration_since_progress == EXPLORATION_WITHOUT_PROGRESS_THRESHOLD {
            self.warning_count += 1;
            return Some(format!(
                "progress_guard: {EXPLORATION_WITHOUT_PROGRESS_THRESHOLD} investigation tools have run without an edit or validation. Narrow the hypothesis now: identify the exact missing evidence, make the smallest relevant change, or run one decisive verification command."
            ));
        }
        if self.exploration_since_progress == CHECKPOINT_WITHOUT_PROGRESS_THRESHOLD {
            self.checkpoint_required = true;
            self.warning_count += 1;
            return Some(format!(
                "progress_guard: checkpoint required after {count} investigation tools without an edit or validation. Further exploration is host-blocked until you call progress_checkpoint with bounded facts, hypothesis, missing evidence, and one next decisive action (mutation, validation, or no-change diagnosis).",
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
            self.last_exploration_signature = Some(signature.clone());
        }
        if self.repeated_exploration_count >= REPEATED_EXPLORATION_THRESHOLD {
            self.repeated_exploration_count = 0;
            if self.warned_repetition_signatures.insert(signature) {
                self.warning_count += 1;
                return Some(
                    "progress_guard: repeated the same investigation target without an intervening edit or validation. Use the evidence already gathered, state the hypothesis, or switch to a decisive test/change."
                        .to_string(),
                );
            }
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
        let (warning, store, progress) = {
            let mut state = self.state.lock().expect("progress guard state poisoned");
            let session_id = context.session_id.to_string();
            let store = state.store.clone();
            let progress = state.session_mut(&session_id);
            let warning = progress.observe(tool_call, result);
            (warning, store, progress.clone())
        };

        store.save(context.session_id, &progress);

        if let Some(warning) = warning {
            inject_warning(result, warning);
        }
    }
}

struct ProgressGuardGate {
    state: Arc<Mutex<ProgressGuardState>>,
}

struct ProgressGuardToolGate {
    state: Arc<Mutex<ProgressGuardState>>,
    session_id: String,
}

impl ToolDefinitionHook for ProgressGuardToolGate {
    fn transform(&self, tools: Vec<ToolDefinition>) -> Vec<ToolDefinition> {
        let (checkpoint_required, failed_tool) = self
            .state
            .lock()
            .expect("progress guard state poisoned")
            .sessions
            .get(&self.session_id)
            .map(|progress| {
                (
                    progress.checkpoint_required,
                    progress
                        .failure_gate
                        .as_ref()
                        .map(|gate| gate.source_tool.clone()),
                )
            })
            .unwrap_or_default();
        if !checkpoint_required && failed_tool.is_none() {
            return tools;
        }
        tools
            .into_iter()
            .filter(|tool| {
                failed_tool.as_deref() != Some(tool.name())
                    && (!checkpoint_required
                        || !matches!(
                            static_tool_class(tool.name()),
                            Some(ToolClass::Exploration | ToolClass::Waiting)
                        ))
            })
            .collect()
    }
}

#[async_trait]
impl PreToolUseHook for ProgressGuardGate {
    async fn before_exec(
        &self,
        tool_call: ToolCall,
        _tool_def: &ToolDefinition,
        context: &ToolContext,
    ) -> PreToolUseDecision {
        if tool_call.name == PROGRESS_CHECKPOINT_TOOL {
            return PreToolUseDecision::Continue(tool_call);
        }
        let mut state = self.state.lock().expect("progress guard state poisoned");
        let Some(progress) = state.sessions.get_mut(&context.session_id.to_string()) else {
            return PreToolUseDecision::Continue(tool_call);
        };
        if let Some(failure_gate) = &progress.failure_gate {
            if failure_gate.source_tool == tool_call.name {
                return PreToolUseDecision::Block {
                    reason: format!(
                        "equivalent {} failure repeated on unchanged state: use a different tool/action or call progress_checkpoint before retrying {}",
                        failure_gate.fingerprint.class.label(),
                        tool_call.name
                    ),
                    user_message: None,
                    tool_call,
                };
            }
            progress.failure_gate = None;
        }
        let blocked = progress.checkpoint_required
            && matches!(
                classify_tool_call(&tool_call),
                ToolClass::Exploration | ToolClass::Status(_) | ToolClass::Waiting
            );
        if blocked {
            PreToolUseDecision::Block {
                reason: "progress checkpoint required: call progress_checkpoint with facts, hypothesis, missing_evidence, and next_decisive_action before more exploration"
                    .to_string(),
                user_message: None,
                tool_call,
            }
        } else {
            PreToolUseDecision::Continue(tool_call)
        }
    }
}

struct ProgressCheckpointTool;

#[async_trait]
impl Tool for ProgressCheckpointTool {
    fn name(&self) -> &str {
        PROGRESS_CHECKPOINT_TOOL
    }

    fn description(&self) -> &str {
        "Submit the bounded trajectory checkpoint required by progress_guard before further exploration. Pass facts and missing_evidence as JSON arrays of strings, hypothesis as one string, and next_decisive_action as an object with kind and description. Use only when progress_guard requires it."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "facts": {
                    "type": "array",
                    "description": "Established facts as a JSON array of 1 to 8 concise strings; do not pass one combined string.",
                    "minItems": 1,
                    "maxItems": 8,
                    "items": { "type": "string", "minLength": 1, "maxLength": 300 }
                },
                "hypothesis": {
                    "type": "string",
                    "description": "The single current hypothesis that best explains the evidence.",
                    "minLength": 1,
                    "maxLength": 600
                },
                "missing_evidence": {
                    "type": "array",
                    "description": "Evidence still needed as a JSON array of up to 6 strings; use [] when none remains, never a single string.",
                    "maxItems": 6,
                    "items": { "type": "string", "minLength": 1, "maxLength": 300 }
                },
                "next_decisive_action": {
                    "type": "object",
                    "description": "Exactly one next transition: a mutation, validation, or explicit no-change decision.",
                    "properties": {
                        "kind": {
                            "type": "string",
                            "enum": ["mutation", "validation", "no_change"]
                        },
                        "description": { "type": "string", "minLength": 1, "maxLength": 500 }
                    },
                    "required": ["kind", "description"],
                    "additionalProperties": false
                }
            },
            "required": ["facts", "hypothesis", "missing_evidence", "next_decisive_action"],
            "additionalProperties": false
        })
    }

    fn deferrable_policy(&self) -> DeferrablePolicy {
        // The progress gate can require this tool without allowing tool_search;
        // its authoritative schema must therefore remain callable at all times.
        DeferrablePolicy::Never
    }

    async fn execute(&self, arguments: Value) -> ToolExecutionResult {
        match validate_checkpoint_arguments(&arguments) {
            Ok(()) => ToolExecutionResult::success(json!({ "submitted": true })),
            Err(error) => ToolExecutionResult::tool_error(error),
        }
    }
}

fn validate_checkpoint_arguments(arguments: &Value) -> Result<(), String> {
    let facts = bounded_string_array(arguments, "facts", 1, 8, 300)?;
    let _missing = bounded_string_array(arguments, "missing_evidence", 0, 6, 300)?;
    if facts.is_empty() {
        return Err("facts must contain at least one established fact".to_string());
    }
    bounded_string(arguments, "hypothesis", 600)?;
    let action = arguments
        .get("next_decisive_action")
        .and_then(Value::as_object)
        .ok_or_else(|| "next_decisive_action must be an object".to_string())?;
    let kind = action
        .get("kind")
        .and_then(Value::as_str)
        .ok_or_else(|| "next_decisive_action.kind is required".to_string())?;
    if !matches!(kind, "mutation" | "validation" | "no_change") {
        return Err(
            "next_decisive_action.kind must be mutation, validation, or no_change".to_string(),
        );
    }
    bounded_string(&Value::Object(action.clone()), "description", 500)?;
    Ok(())
}

fn bounded_string<'a>(value: &'a Value, field: &str, max_chars: usize) -> Result<&'a str, String> {
    let text = value
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{field} must be a string"))?;
    let chars = text.chars().count();
    if chars == 0 || chars > max_chars {
        return Err(format!("{field} must contain 1..={max_chars} characters"));
    }
    Ok(text)
}

fn bounded_string_array<'a>(
    value: &'a Value,
    field: &str,
    min_items: usize,
    max_items: usize,
    max_chars: usize,
) -> Result<Vec<&'a str>, String> {
    let items = value
        .get(field)
        .and_then(Value::as_array)
        .ok_or_else(|| format!("{field} must be an array"))?;
    if items.len() < min_items || items.len() > max_items {
        return Err(format!(
            "{field} must contain {min_items}..={max_items} items"
        ));
    }
    items
        .iter()
        .map(|item| {
            let text = item
                .as_str()
                .ok_or_else(|| format!("{field} entries must be strings"))?;
            let chars = text.chars().count();
            if chars == 0 || chars > max_chars {
                return Err(format!(
                    "{field} entries must contain 1..={max_chars} characters"
                ));
            }
            Ok(text)
        })
        .collect()
}

fn checkpoint_fingerprint(arguments: &Value) -> String {
    Sha256::digest(normalize_value(arguments).as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn reject_checkpoint(result: &mut ToolResult, reason: &str) {
    result.result = None;
    result.error = Some(format!("progress_checkpoint rejected: {reason}"));
}

#[derive(Default)]
struct ProgressGuardStore {
    path: Option<PathBuf>,
    last_saved_tool_count: Mutex<usize>,
}

impl ProgressGuardStore {
    fn new(path: PathBuf) -> Self {
        Self {
            path: Some(path),
            last_saved_tool_count: Mutex::new(0),
        }
    }

    fn load(&self, session_id: SessionId, active_tool_count: usize) -> Option<SessionProgress> {
        let path = self.path.as_ref()?;
        let encoded = std::fs::read(path).ok()?;
        let stored: StoredProgress = serde_json::from_slice(&encoded).ok()?;
        let progress = (stored.version == STORED_STATE_VERSION
            && stored.session_id == session_id.to_string()
            && stored.progress.tool_count <= active_tool_count)
            .then_some(stored.progress)?;
        *self.last_saved_tool_count.lock().unwrap() = progress.tool_count;
        Some(progress)
    }

    fn save(&self, session_id: SessionId, progress: &SessionProgress) {
        let Some(path) = self.path.as_ref() else {
            return;
        };
        let mut last_saved = self.last_saved_tool_count.lock().unwrap();
        if progress.tool_count < *last_saved {
            return;
        }
        let stored = StoredProgress {
            version: STORED_STATE_VERSION,
            session_id: session_id.to_string(),
            progress: progress.clone(),
        };
        let Ok(encoded) = serde_json::to_vec(&stored) else {
            return;
        };
        let Some(parent) = path.parent() else {
            return;
        };
        let temporary_path = parent.join(format!(
            ".progress-guard-{}-{:016x}.tmp",
            std::process::id(),
            rand::random::<u64>()
        ));
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let persisted = options.open(&temporary_path).and_then(|mut file| {
            file.write_all(&encoded)?;
            file.flush()?;
            replace_state_file(&temporary_path, path)
        });
        match persisted {
            Ok(()) => *last_saved = progress.tool_count,
            Err(error) => {
                let _ = std::fs::remove_file(&temporary_path);
                tracing::warn!(%error, path = %path.display(), "persist progress guard state")
            }
        }
    }
}

fn replace_state_file(temporary_path: &Path, destination: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    if std::fs::symlink_metadata(destination).is_ok() {
        std::fs::remove_file(destination)?;
    }
    std::fs::rename(temporary_path, destination)
}

#[derive(Deserialize, Serialize)]
struct StoredProgress {
    version: u8,
    session_id: String,
    progress: SessionProgress,
}

#[derive(Clone, Copy)]
struct ExplorationEvidence {
    zero_matches: bool,
    truncated: bool,
}

fn exploration_evidence(tool_call: &ToolCall, result: &ToolResult) -> Option<ExplorationEvidence> {
    if result.error.is_some() {
        return None;
    }
    if !matches!(
        tool_call.name.as_str(),
        "grep_files" | "repo_map" | "ast_grep"
    ) {
        return None;
    }
    let value = result.result.as_ref()?;
    let count = value
        .get("count")
        .or_else(|| value.get("match_count"))
        .and_then(Value::as_u64)?;
    Some(ExplorationEvidence {
        zero_matches: count == 0,
        truncated: value
            .get("truncated")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum SemanticFailureClass {
    Invocation,
    MissingCommand,
    WrongPath,
    Usage,
}

impl SemanticFailureClass {
    fn label(self) -> &'static str {
        match self {
            Self::Invocation => "invocation",
            Self::MissingCommand => "missing-command",
            Self::WrongPath => "wrong-path",
            Self::Usage => "usage",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
struct FailureFingerprint {
    workspace_state: u64,
    class: SemanticFailureClass,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct FailureGate {
    source_tool: String,
    fingerprint: FailureFingerprint,
}

fn classify_semantic_failure(result: &ToolResult) -> Option<SemanticFailureClass> {
    let value = result.result.as_ref();
    let structured_failure = value
        .and_then(|value| value.get("success"))
        .and_then(Value::as_bool)
        == Some(false);
    if result.error.is_none() && !structured_failure {
        return None;
    }

    let diagnostic = bounded_failure_diagnostic(result).to_ascii_lowercase();
    let exit_code = value
        .and_then(|value| value.get("exit_code"))
        .and_then(Value::as_i64);
    if diagnostic.contains("command not found")
        || diagnostic.contains("not recognized as an internal or external command")
        || (exit_code == Some(127) && diagnostic.contains("not found"))
    {
        return Some(SemanticFailureClass::MissingCommand);
    }
    if [
        "no such file or directory",
        "cannot find the path specified",
        "could not find a part of the path",
        "can't cd to",
        "cannot cd to",
    ]
    .iter()
    .any(|pattern| diagnostic.contains(pattern))
    {
        return Some(SemanticFailureClass::WrongPath);
    }
    if [
        "usage:",
        "unknown option",
        "unrecognized option",
        "unexpected argument",
        "invalid option",
        "requires an argument",
        "missing required argument",
        "the following required arguments were not provided",
    ]
    .iter()
    .any(|pattern| diagnostic.contains(pattern))
    {
        return Some(SemanticFailureClass::Usage);
    }
    if exit_code == Some(126)
        || [
            "spawn failed",
            "sandbox setup failed",
            "failed to execute",
            "could not execute",
            "exec format error",
        ]
        .iter()
        .any(|pattern| diagnostic.contains(pattern))
    {
        return Some(SemanticFailureClass::Invocation);
    }
    None
}

fn bounded_failure_diagnostic(result: &ToolResult) -> String {
    let mut diagnostic = String::new();
    if let Some(error) = &result.error {
        append_bounded_diagnostic(&mut diagnostic, error);
    }
    for field in ["stderr", "stdout", "message"] {
        let Some(text) = result
            .result
            .as_ref()
            .and_then(|value| value.get(field))
            .and_then(Value::as_str)
        else {
            continue;
        };
        if !diagnostic.is_empty() {
            append_bounded_diagnostic(&mut diagnostic, "\n");
        }
        append_bounded_diagnostic(&mut diagnostic, text);
    }
    diagnostic
}

fn append_bounded_diagnostic(diagnostic: &mut String, text: &str) {
    let remaining = FAILURE_DIAGNOSTIC_LIMIT.saturating_sub(diagnostic.len());
    if remaining == 0 {
        return;
    }
    let mut end = remaining.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    diagnostic.push_str(&text[..end]);
}

#[derive(Debug, PartialEq, Eq)]
enum ToolClass {
    Exploration,
    Mutation,
    Validation(String),
    Status(String),
    /// Checking on an external event (CI run, PR checks/reviews) or plain
    /// sleeping — the poll-loop shape that should instead be one detached
    /// `spawn_background` watch.
    Waiting,
    Other,
}

fn classify_tool_call(tool_call: &ToolCall) -> ToolClass {
    if let Some(class) = static_tool_class(&tool_call.name) {
        return class;
    }
    match tool_call.name.as_str() {
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

fn static_tool_class(name: &str) -> Option<ToolClass> {
    match name {
        "read_file" | "read_many_files" | "grep_files" | "repo_map" | "search_sessions"
        | "ast_grep" | "list_directory" | "stat_file" | "tool_search" => {
            Some(ToolClass::Exploration)
        }
        "write_file" | "edit_file" | "delete_file" | "ast_edit" => Some(ToolClass::Mutation),
        "get_task" | "list_tasks" => Some(ToolClass::Waiting),
        _ => None,
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
        return ToolClass::Validation(normalized);
    }
    if is_mutating_command(&normalized) {
        return ToolClass::Mutation;
    }
    if is_exploration_command(&normalized) {
        return ToolClass::Exploration;
    }
    ToolClass::Other
}

struct MutationHashTransition {
    path: String,
    previous_hash: Option<String>,
    content_hash: String,
}

fn mutation_hash_transition(
    tool_call: &ToolCall,
    result: &ToolResult,
) -> Option<MutationHashTransition> {
    if !matches!(tool_call.name.as_str(), "edit_file" | "write_file") {
        return None;
    }
    let value = result.result.as_ref()?;
    Some(MutationHashTransition {
        path: value.get("path")?.as_str()?.to_string(),
        previous_hash: value
            .get("previous_content_hash")
            .and_then(Value::as_str)
            .map(str::to_string),
        content_hash: value.get("content_hash")?.as_str()?.to_string(),
    })
}

fn mutation_was_applied(tool_call: &ToolCall, result: &ToolResult) -> bool {
    if result
        .result
        .as_ref()
        .and_then(|value| value.get("success"))
        .and_then(Value::as_bool)
        == Some(false)
    {
        return false;
    }
    if tool_call.name == "ast_edit" {
        return result
            .result
            .as_ref()
            .and_then(|value| value.get("applied"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
    }
    true
}

fn remember_bounded<T: Clone + Eq + Hash>(
    seen: &mut HashSet<T>,
    recent: &mut VecDeque<T>,
    value: T,
) {
    seen.insert(value.clone());
    recent.push_back(value);
    if recent.len() > SEMANTIC_HISTORY_LIMIT
        && let Some(expired) = recent.pop_front()
    {
        seen.remove(&expired);
    }
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
        "read_many_files" | "repo_map" | "search_sessions" | "ast_grep" | "list_directory"
        | "stat_file" => Some(format!(
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
        "cargo check",
        "cargo build",
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
        "cargo update",
        "cargo generate-lockfile",
        "cargo add",
        "cargo remove",
        "cargo fix",
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
    use everruns_provider::typed_id::SessionId;
    use everruns_provider::{BuiltinTool, DeferrablePolicy, ToolHints, ToolPolicy, ToolResult};

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

    fn result_value(value: Value) -> ToolResult {
        ToolResult {
            result: Some(value),
            ..result()
        }
    }

    fn failed_bash_result(exit_code: i32, stderr: &str) -> ToolResult {
        result_value(json!({
            "exit_code": exit_code,
            "success": false,
            "stdout": "",
            "stderr": stderr,
        }))
    }

    fn tool_error_result(error: &str) -> ToolResult {
        ToolResult {
            result: None,
            error: Some(error.to_string()),
            ..result()
        }
    }

    fn checkpoint_arguments(label: &str) -> Value {
        json!({
            "facts": [format!("fact {label}")],
            "hypothesis": format!("hypothesis {label}"),
            "missing_evidence": [format!("missing {label}")],
            "next_decisive_action": {
                "kind": "validation",
                "description": format!("validate {label}")
            }
        })
    }

    #[test]
    fn checkpoint_schema_is_always_loaded_and_self_describing() {
        let tool = ProgressCheckpointTool;
        let schema = tool.parameters_schema();

        assert_eq!(tool.deferrable_policy(), DeferrablePolicy::Never);
        for field in [
            "facts",
            "hypothesis",
            "missing_evidence",
            "next_decisive_action",
        ] {
            assert!(
                schema["properties"][field]["description"].is_string(),
                "{field} needs a model-facing shape description"
            );
        }
        assert_eq!(schema["properties"]["missing_evidence"]["type"], "array");
        assert_eq!(
            schema["properties"]["missing_evidence"]["items"]["type"],
            "string"
        );
    }

    #[test]
    fn batch_reads_are_exploration_with_order_sensitive_signatures() {
        let first = call(
            "read_many_files",
            json!({ "paths": ["a.rs", "b.rs"], "offset": 4 }),
        );
        let same = call(
            "read_many_files",
            json!({ "paths": ["a.rs", "b.rs"], "offset": 4 }),
        );
        let reordered = call(
            "read_many_files",
            json!({ "paths": ["b.rs", "a.rs"], "offset": 4 }),
        );

        assert_eq!(classify_tool_call(&first), ToolClass::Exploration);
        assert_eq!(exploration_signature(&first), exploration_signature(&same));
        assert_ne!(
            exploration_signature(&first),
            exploration_signature(&reordered),
            "the batch result follows request order, so signatures must too"
        );
    }

    #[tokio::test]
    async fn unchanged_large_read_returns_compact_marker_and_mutation_invalidates_it() {
        let state = Arc::new(Mutex::new(ProgressGuardState::default()));
        let hook = ProgressGuardHook { state };
        let context = ToolContext::new(SessionId::new());
        let read = call("read_file", json!({ "path": "/src/lib.rs" }));
        let payload = json!({ "content": "discovery evidence\n".repeat(400) });

        let mut first = result_value(payload.clone());
        hook.after_exec(&read, &tool_def("read_file"), &mut first, &context)
            .await;
        let first_bytes = serde_json::to_vec(first.result.as_ref().unwrap())
            .unwrap()
            .len();

        let mut repeated = result_value(payload.clone());
        hook.after_exec(&read, &tool_def("read_file"), &mut repeated, &context)
            .await;
        let repeated_value = repeated.result.as_ref().unwrap();
        let repeated_bytes = serde_json::to_vec(repeated_value).unwrap().len();
        assert_eq!(repeated_value["unchanged_since_last_read"], true);
        assert!(
            repeated_value["progress_guard_warning"]
                .as_str()
                .is_some_and(|warning| warning.contains("earlier full result"))
        );
        assert!(
            repeated_bytes * 10 < first_bytes,
            "compact unchanged marker should materially cut context bytes: {repeated_bytes} vs {first_bytes}"
        );

        let mut write = result();
        hook.after_exec(
            &call(
                "write_file",
                json!({ "path": "/src/lib.rs", "content": "changed" }),
            ),
            &tool_def("write_file"),
            &mut write,
            &context,
        )
        .await;
        let mut after_mutation = result_value(payload);
        hook.after_exec(&read, &tool_def("read_file"), &mut after_mutation, &context)
            .await;
        assert!(after_mutation.result.unwrap().get("content").is_some());
    }

    #[tokio::test]
    async fn unchanged_large_read_warns_once_but_every_repeat_stays_compact() {
        let state = Arc::new(Mutex::new(ProgressGuardState::default()));
        let hook = ProgressGuardHook { state };
        let context = ToolContext::new(SessionId::new());
        let read = call("read_file", json!({ "path": "/src/lib.rs" }));
        let payload = json!({ "content": "stable evidence\n".repeat(400) });
        let mut warning_count = 0;

        for _ in 0..4 {
            let mut out = result_value(payload.clone());
            hook.after_exec(&read, &tool_def("read_file"), &mut out, &context)
                .await;
            let value = out.result.as_ref().unwrap();
            warning_count += usize::from(value.get("progress_guard_warning").is_some());
            if value.get("content").is_none() {
                assert_eq!(value["unchanged_since_last_read"], true);
            }
        }

        assert_eq!(warning_count, 1);
    }

    #[tokio::test]
    async fn checkpoint_gate_blocks_exploration_then_accepts_a_bounded_transition() {
        let state = Arc::new(Mutex::new(ProgressGuardState::default()));
        let capability = ProgressGuardCapability {
            state: state.clone(),
        };
        let hook = ProgressGuardHook {
            state: state.clone(),
        };
        let gate = ProgressGuardGate {
            state: state.clone(),
        };
        let context = ToolContext::new(SessionId::new());

        for i in 0..CHECKPOINT_WITHOUT_PROGRESS_THRESHOLD {
            hook.after_exec(
                &call("read_file", json!({ "path": format!("/src/{i}.rs") })),
                &tool_def("read_file"),
                &mut result(),
                &context,
            )
            .await;
        }

        let blocked = gate
            .before_exec(
                call("read_file", json!({ "path": "/src/blocked.rs" })),
                &tool_def("read_file"),
                &context,
            )
            .await;
        assert!(matches!(blocked, PreToolUseDecision::Block { .. }));

        let arguments = checkpoint_arguments("owner");
        let executed = ProgressCheckpointTool.execute(arguments.clone()).await;
        assert!(executed.is_success());
        let mut checkpoint_result = result_value(json!({ "submitted": true }));
        hook.after_exec(
            &call(PROGRESS_CHECKPOINT_TOOL, arguments),
            &tool_def(PROGRESS_CHECKPOINT_TOOL),
            &mut checkpoint_result,
            &context,
        )
        .await;
        assert_eq!(checkpoint_result.result.unwrap()["accepted"], true);

        let resumed = gate
            .before_exec(
                call("read_file", json!({ "path": "/src/decisive.rs" })),
                &tool_def("read_file"),
                &context,
            )
            .await;
        assert!(matches!(resumed, PreToolUseDecision::Continue(_)));

        let prompt_context = SystemPromptContext::without_file_store(context.session_id);
        let visible = capability.tool_definition_hooks_with_context(&prompt_context, &json!({}))[0]
            .transform(vec![
                tool_def("read_file"),
                tool_def(PROGRESS_CHECKPOINT_TOOL),
            ]);
        assert_eq!(
            visible.iter().map(|tool| tool.name()).collect::<Vec<_>>(),
            vec!["read_file", PROGRESS_CHECKPOINT_TOOL],
            "an accepted checkpoint should restore ordinary exploration"
        );
    }

    #[test]
    fn checkpoint_gate_leaves_ordinary_tool_surface_unchanged() {
        let capability = ProgressGuardCapability {
            state: Arc::new(Mutex::new(ProgressGuardState::default())),
        };
        let session_id = SessionId::new();
        let prompt_context = SystemPromptContext::without_file_store(session_id);
        let tools = vec![
            tool_def("read_file"),
            tool_def("tool_search"),
            tool_def("edit_file"),
            tool_def(PROGRESS_CHECKPOINT_TOOL),
        ];

        let visible = capability.tool_definition_hooks_with_context(&prompt_context, &json!({}))[0]
            .transform(tools.clone());

        assert_eq!(
            visible.iter().map(|tool| tool.name()).collect::<Vec<_>>(),
            tools.iter().map(|tool| tool.name()).collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn checkpoint_gate_removes_blocked_paths_from_the_next_model_round() {
        let state = Arc::new(Mutex::new(ProgressGuardState::default()));
        let capability = ProgressGuardCapability {
            state: state.clone(),
        };
        let hook = ProgressGuardHook { state };
        let context = ToolContext::new(SessionId::new());

        for i in 0..CHECKPOINT_WITHOUT_PROGRESS_THRESHOLD {
            hook.after_exec(
                &call("read_file", json!({ "path": format!("/src/{i}.rs") })),
                &tool_def("read_file"),
                &mut result(),
                &context,
            )
            .await;
        }

        let tools = [
            "read_file",
            "grep_files",
            "tool_search",
            "get_task",
            "bash",
            "edit_file",
            PROGRESS_CHECKPOINT_TOOL,
            "write_todos",
        ]
        .into_iter()
        .map(tool_def)
        .collect::<Vec<_>>();
        let prompt_context = SystemPromptContext::without_file_store(context.session_id);
        let visible = capability
            .tool_definition_hooks_with_context(&prompt_context, &json!({}))
            .into_iter()
            .fold(tools, |tools, hook| hook.transform(tools))
            .into_iter()
            .map(|tool| tool.name().to_string())
            .collect::<Vec<_>>();

        assert_eq!(
            visible,
            vec!["bash", "edit_file", PROGRESS_CHECKPOINT_TOOL, "write_todos"],
            "the gated model round should not advertise paths the pre-tool hook will reject"
        );

        let other_session = SystemPromptContext::without_file_store(SessionId::new());
        let other_visible =
            capability.tool_definition_hooks_with_context(&other_session, &json!({}))[0].transform(
                vec![tool_def("read_file"), tool_def(PROGRESS_CHECKPOINT_TOOL)],
            );
        assert_eq!(
            other_visible
                .iter()
                .map(|tool| tool.name())
                .collect::<Vec<_>>(),
            vec!["read_file", PROGRESS_CHECKPOINT_TOOL],
            "one session's checkpoint gate must not alter another session's tools"
        );
    }

    #[tokio::test]
    async fn resumed_session_preserves_the_gate_and_accepted_checkpoint() {
        let dir = tempfile::tempdir().unwrap();
        let session_id = SessionId::new();
        let context = ToolContext::new(session_id);
        let first = ProgressGuardCapability::open(dir.path(), session_id, 0);
        let post = first.post_tool_exec_hooks().pop().unwrap();

        for i in 0..CHECKPOINT_WITHOUT_PROGRESS_THRESHOLD {
            post.after_exec(
                &call("read_file", json!({ "path": format!("/src/{i}.rs") })),
                &tool_def("read_file"),
                &mut result(),
                &context,
            )
            .await;
        }
        drop(first);

        let resumed = ProgressGuardCapability::open(
            dir.path(),
            session_id,
            CHECKPOINT_WITHOUT_PROGRESS_THRESHOLD,
        );
        let gate = resumed.pre_tool_use_hooks().pop().unwrap();
        assert!(matches!(
            gate.before_exec(
                call("read_file", json!({ "path": "/src/blocked.rs" })),
                &tool_def("read_file"),
                &context,
            )
            .await,
            PreToolUseDecision::Block { .. }
        ));

        let arguments = checkpoint_arguments("resume");
        let mut checkpoint_result = result_value(json!({ "submitted": true }));
        resumed.post_tool_exec_hooks()[0]
            .after_exec(
                &call(PROGRESS_CHECKPOINT_TOOL, arguments),
                &tool_def(PROGRESS_CHECKPOINT_TOOL),
                &mut checkpoint_result,
                &context,
            )
            .await;
        drop(resumed);

        let after_checkpoint = ProgressGuardCapability::open(
            dir.path(),
            session_id,
            CHECKPOINT_WITHOUT_PROGRESS_THRESHOLD + 1,
        );
        let gate = after_checkpoint.pre_tool_use_hooks().pop().unwrap();
        assert!(matches!(
            gate.before_exec(
                call("read_file", json!({ "path": "/src/resumed.rs" })),
                &tool_def("read_file"),
                &context,
            )
            .await,
            PreToolUseDecision::Continue(_)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn persisted_state_replaces_a_symlink_without_overwriting_its_target() {
        use std::os::unix::fs::symlink;

        let session_dir = tempfile::tempdir().unwrap();
        let outside_dir = tempfile::tempdir().unwrap();
        let state_path = session_dir.path().join("progress-guard.json");
        let outside_path = outside_dir.path().join("outside.txt");
        std::fs::write(&outside_path, "do not overwrite").unwrap();
        symlink(&outside_path, &state_path).unwrap();

        let store = ProgressGuardStore::new(state_path.clone());
        store.save(SessionId::new(), &SessionProgress::default());

        assert_eq!(
            std::fs::read_to_string(outside_path).unwrap(),
            "do not overwrite"
        );
        assert!(std::fs::symlink_metadata(state_path).unwrap().is_file());
    }

    #[tokio::test]
    async fn deterministic_baseline_candidate_trajectory_study() {
        #[derive(Default)]
        struct Metrics {
            calls: usize,
            result_bytes: usize,
            complete: bool,
        }

        async fn run(advisory_only: bool) -> Metrics {
            let state = Arc::new(Mutex::new(ProgressGuardState::default()));
            let hook = ProgressGuardHook {
                state: state.clone(),
            };
            let gate = ProgressGuardGate { state };
            let context = ToolContext::new(SessionId::new());
            let mut metrics = Metrics::default();

            for index in 0..160 {
                let read = call(
                    "read_file",
                    json!({ "path": format!("/src/{}.rs", index.min(40)) }),
                );
                if !advisory_only
                    && matches!(
                        gate.before_exec(read.clone(), &tool_def("read_file"), &context)
                            .await,
                        PreToolUseDecision::Block { .. }
                    )
                {
                    let arguments = checkpoint_arguments("no-change diagnosis");
                    let mut checkpoint = result_value(json!({ "submitted": true }));
                    hook.after_exec(
                        &call(PROGRESS_CHECKPOINT_TOOL, arguments),
                        &tool_def(PROGRESS_CHECKPOINT_TOOL),
                        &mut checkpoint,
                        &context,
                    )
                    .await;
                    metrics.calls += 1;
                    metrics.result_bytes += serde_json::to_vec(&checkpoint.result).unwrap().len();
                    break;
                }

                let payload = if index < 40 {
                    json!({ "content": format!("scope {index}") })
                } else {
                    json!({ "content": "root-cause evidence\n".repeat(35) })
                };
                let mut out = result_value(payload);
                hook.after_exec(&read, &tool_def("read_file"), &mut out, &context)
                    .await;
                metrics.calls += 1;
                metrics.result_bytes += serde_json::to_vec(&out.result).unwrap().len();
                metrics.complete |= index >= 40;
            }
            metrics
        }

        // The baseline is deliberately conservative: it has the same compact
        // cache as the candidate, but checkpoint prose remains advisory and is
        // ignored. The candidate follows the host-enforced transition and ends
        // with the already-complete no-change diagnosis.
        let baseline = run(true).await;
        let candidate = run(false).await;
        assert!(baseline.complete && candidate.complete);
        assert!(
            candidate.calls * 2 <= baseline.calls,
            "success criterion: at least 50% fewer calls ({} -> {})",
            baseline.calls,
            candidate.calls
        );
        assert!(
            candidate.result_bytes * 2 <= baseline.result_bytes,
            "success criterion: at least 50% fewer result bytes ({} -> {})",
            baseline.result_bytes,
            candidate.result_bytes
        );
    }

    #[test]
    fn checkpoint_payload_is_strictly_bounded() {
        assert!(validate_checkpoint_arguments(&checkpoint_arguments("ok")).is_ok());
        let mut oversized = checkpoint_arguments("large");
        oversized["hypothesis"] = json!("x".repeat(601));
        assert!(validate_checkpoint_arguments(&oversized).is_err());
    }

    #[test]
    fn session_and_evidence_state_are_bounded() {
        let mut state = ProgressGuardState::default();
        for index in 0..(SESSION_STATE_LIMIT * 2) {
            state.session_mut(&format!("session-{index}"));
        }
        assert_eq!(state.sessions.len(), SESSION_STATE_LIMIT);

        let progress = state.session_mut("active");
        for index in 0..(SEMANTIC_HISTORY_LIMIT * 2) {
            progress.remember_observation(format!("read:{index}"), [index as u8; 32]);
        }
        assert_eq!(progress.observation_hashes.len(), SEMANTIC_HISTORY_LIMIT);
        assert_eq!(progress.recent_observations.len(), SEMANTIC_HISTORY_LIMIT);

        for index in 0..(SEMANTIC_HISTORY_LIMIT * 2) {
            remember_bounded(
                &mut progress.warned_failures,
                &mut progress.recent_warned_failures,
                FailureFingerprint {
                    workspace_state: index as u64,
                    class: SemanticFailureClass::WrongPath,
                },
            );
        }
        assert_eq!(progress.warned_failures.len(), SEMANTIC_HISTORY_LIMIT);
        assert_eq!(
            progress.recent_warned_failures.len(),
            SEMANTIC_HISTORY_LIMIT
        );
    }

    #[tokio::test]
    async fn reuse_requires_the_same_target_and_same_result() {
        let state = Arc::new(Mutex::new(ProgressGuardState::default()));
        let hook = ProgressGuardHook { state };
        let context = ToolContext::new(SessionId::new());
        let large = "x".repeat(2_000);

        let cases = [
            ("/a.rs", large.clone()),
            ("/b.rs", large.clone()),
            ("/a.rs", format!("{large} changed")),
        ];
        for (path, content) in cases {
            let mut observed = result_value(json!({ "content": content }));
            hook.after_exec(
                &call("read_file", json!({ "path": path })),
                &tool_def("read_file"),
                &mut observed,
                &context,
            )
            .await;
            assert!(
                observed
                    .result
                    .as_ref()
                    .and_then(|value| value.get("unchanged_since_last_read"))
                    .is_none(),
                "different targets or changed bytes are not reusable"
            );
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
            ToolClass::Validation("cargo test --all-features".to_string())
        );
        assert_eq!(
            classify_bash_command("cargo check --all-features"),
            ToolClass::Validation("cargo check --all-features".to_string())
        );
        assert_eq!(
            classify_bash_command("cargo update -p everruns-core --precise 0.17.7"),
            ToolClass::Mutation
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
            ToolClass::Validation("cargo test --all-features".to_string())
        );
        // `sleepwalk` must not parse as a sleep prefix.
        assert_eq!(classify_bash_command("sleepwalk"), ToolClass::Other);
    }

    #[tokio::test]
    async fn repeated_equivalent_missing_commands_force_a_different_action() {
        let state = Arc::new(Mutex::new(ProgressGuardState::default()));
        let hook = ProgressGuardHook {
            state: state.clone(),
        };
        let gate = ProgressGuardGate {
            state: state.clone(),
        };
        let context = ToolContext::new(SessionId::new());
        let tool_gate = ProgressGuardToolGate {
            state,
            session_id: context.session_id.to_string(),
        };

        for (command, executable) in [("rg needle .", "rg"), ("ag needle .", "ag")] {
            let mut failed = result_value(json!({
                "command": command,
                "exit_code": 127,
                "success": false,
                "stdout": "",
                "stderr": format!("bash: {executable}: command not found"),
            }));
            hook.after_exec(
                &call("bash", json!({ "command": command })),
                &tool_def("bash"),
                &mut failed,
                &context,
            )
            .await;

            if executable == "ag" {
                assert!(
                    failed
                        .result
                        .as_ref()
                        .and_then(|value| value.get("progress_guard_warning"))
                        .and_then(Value::as_str)
                        .is_some_and(|warning| {
                            warning.contains("equivalent missing-command failure")
                        }),
                    "the rewritten command should be recognized as the same root failure: {failed:?}"
                );
            }
        }

        let visible_tools = tool_gate.transform(vec![
            tool_def("bash"),
            tool_def("grep_files"),
            tool_def(PROGRESS_CHECKPOINT_TOOL),
        ]);
        assert_eq!(
            visible_tools
                .iter()
                .map(|tool| tool.name())
                .collect::<Vec<_>>(),
            ["grep_files", PROGRESS_CHECKPOINT_TOOL],
            "the next model round should expose recovery paths but not the failed tool"
        );
        assert!(matches!(
            gate.before_exec(
                call("bash", json!({ "command": "grep -R needle ." })),
                &tool_def("bash"),
                &context,
            )
            .await,
            PreToolUseDecision::Block { .. }
        ));
        assert!(matches!(
            gate.before_exec(
                call("grep_files", json!({ "pattern": "needle" })),
                &tool_def("grep_files"),
                &context,
            )
            .await,
            PreToolUseDecision::Continue(_)
        ));
    }

    #[test]
    fn semantic_failure_classification_is_structured_and_conservative() {
        let cases = [
            (
                failed_bash_result(127, "bash: rg: command not found"),
                Some(SemanticFailureClass::MissingCommand),
            ),
            (
                failed_bash_result(1, "cat: src/missing.rs: No such file or directory"),
                Some(SemanticFailureClass::WrongPath),
            ),
            (
                failed_bash_result(2, "error: unexpected argument '--bogus'\nUsage: cargo test"),
                Some(SemanticFailureClass::Usage),
            ),
            (
                tool_error_result("spawn failed: exec format error"),
                Some(SemanticFailureClass::Invocation),
            ),
            (
                failed_bash_result(101, "test result: FAILED. 1 failed; 3 passed"),
                None,
            ),
            (
                failed_bash_result(1, "assertion failed: expected 2, received 3"),
                None,
            ),
        ];

        for (result, expected) in cases {
            assert_eq!(classify_semantic_failure(&result), expected, "{result:?}");
        }
    }

    #[test]
    fn failure_diagnostics_are_bounded_without_splitting_utf8() {
        let oversized = "é".repeat(FAILURE_DIAGNOSTIC_LIMIT);
        let diagnostic = bounded_failure_diagnostic(&ToolResult {
            result: Some(json!({ "stderr": oversized })),
            error: Some("x".repeat(FAILURE_DIAGNOSTIC_LIMIT)),
            ..result()
        });
        assert_eq!(diagnostic.len(), FAILURE_DIAGNOSTIC_LIMIT);
        assert!(diagnostic.chars().all(|character| character == 'x'));

        let unicode_only = bounded_failure_diagnostic(&failed_bash_result(2, &oversized));
        assert!(unicode_only.len() <= FAILURE_DIAGNOSTIC_LIMIT);
        assert!(unicode_only.is_char_boundary(unicode_only.len()));
    }

    #[test]
    fn every_supported_failure_class_fingerprints_equivalent_rewrites() {
        let cases = [
            (
                failed_bash_result(126, "failed to execute probe-one"),
                failed_bash_result(126, "failed to execute probe-two"),
                "invocation",
            ),
            (
                failed_bash_result(127, "bash: rg: command not found"),
                failed_bash_result(127, "bash: ag: command not found"),
                "missing-command",
            ),
            (
                tool_error_result("read failed for old/a: No such file or directory"),
                tool_error_result("read failed for old/b: No such file or directory"),
                "wrong-path",
            ),
            (
                failed_bash_result(2, "usage: probe-one ARG"),
                failed_bash_result(2, "usage: probe-two ARG"),
                "usage",
            ),
        ];

        for (mut first, mut second, label) in cases {
            let mut progress = SessionProgress::default();
            let first_warning = progress.observe(
                &call("bash", json!({ "command": format!("{label}-one") })),
                &mut first,
            );
            let second_warning = progress.observe(
                &call("bash", json!({ "command": format!("{label}-two") })),
                &mut second,
            );
            assert!(first_warning.is_none());
            assert!(
                second_warning.is_some_and(|warning| warning.contains(label)),
                "{label} rewrites should share one semantic fingerprint"
            );
        }
    }

    #[tokio::test]
    async fn expected_diagnostic_test_failures_do_not_warn_or_gate_validation() {
        let state = Arc::new(Mutex::new(ProgressGuardState::default()));
        let hook = ProgressGuardHook {
            state: state.clone(),
        };
        let gate = ProgressGuardGate { state };
        let context = ToolContext::new(SessionId::new());
        let test = call("bash", json!({ "command": "cargo test" }));

        let mut failed = failed_bash_result(
            101,
            "failures:\n    tests::reproduces_bug\ntest result: FAILED. 1 failed",
        );
        hook.after_exec(&test, &tool_def("bash"), &mut failed, &context)
            .await;
        assert!(
            failed
                .result
                .as_ref()
                .and_then(|value| value.get("progress_guard_warning"))
                .is_none(),
            "an initially failing test is code evidence, not command misuse"
        );
        assert!(matches!(
            gate.before_exec(test.clone(), &tool_def("bash"), &context)
                .await,
            PreToolUseDecision::Continue(_)
        ));

        let mut edit = result_value(json!({
            "path": "/workspace/src/lib.rs",
            "previous_content_hash": "before",
            "content_hash": "after",
        }));
        hook.after_exec(
            &call("edit_file", json!({ "path": "/workspace/src/lib.rs" })),
            &tool_def("edit_file"),
            &mut edit,
            &context,
        )
        .await;
        let mut diagnostic_after_edit = failed_bash_result(
            101,
            "failures:\n    tests::another_bug\ntest result: FAILED. 1 failed",
        );
        hook.after_exec(
            &test,
            &tool_def("bash"),
            &mut diagnostic_after_edit,
            &context,
        )
        .await;
        assert!(
            diagnostic_after_edit
                .result
                .as_ref()
                .and_then(|value| value.get("progress_guard_warning"))
                .is_none(),
            "a new failing test after mutation is fresh diagnostic evidence"
        );
    }

    #[tokio::test]
    async fn distinct_failure_classes_and_success_break_the_equivalence_streak() {
        let state = Arc::new(Mutex::new(ProgressGuardState::default()));
        let hook = ProgressGuardHook { state };
        let context = ToolContext::new(SessionId::new());
        let observations = [
            failed_bash_result(127, "bash: rg: command not found"),
            failed_bash_result(1, "cat: absent: No such file or directory"),
            result(),
            failed_bash_result(127, "bash: ag: command not found"),
        ];

        for (index, mut observed) in observations.into_iter().enumerate() {
            hook.after_exec(
                &call("bash", json!({ "command": format!("probe-{index}") })),
                &tool_def("bash"),
                &mut observed,
                &context,
            )
            .await;
            assert!(
                observed
                    .result
                    .as_ref()
                    .and_then(|value| value.get("progress_guard_warning"))
                    .is_none(),
                "different evidence must not create an equivalent-failure warning"
            );
        }
    }

    #[tokio::test]
    async fn repeated_wrong_path_can_recover_through_a_checkpoint() {
        let state = Arc::new(Mutex::new(ProgressGuardState::default()));
        let hook = ProgressGuardHook {
            state: state.clone(),
        };
        let gate = ProgressGuardGate { state };
        let context = ToolContext::new(SessionId::new());

        for path in ["src/old.rs", "lib/old.rs"] {
            let mut failed = tool_error_result(&format!(
                "read failed for {path}: No such file or directory"
            ));
            hook.after_exec(
                &call("read_file", json!({ "path": path })),
                &tool_def("read_file"),
                &mut failed,
                &context,
            )
            .await;
        }
        assert!(matches!(
            gate.before_exec(
                call("read_file", json!({ "path": "src/third.rs" })),
                &tool_def("read_file"),
                &context,
            )
            .await,
            PreToolUseDecision::Block { .. }
        ));

        let arguments = checkpoint_arguments("wrong path recovery");
        let mut checkpoint = result_value(json!({ "submitted": true }));
        hook.after_exec(
            &call(PROGRESS_CHECKPOINT_TOOL, arguments),
            &tool_def(PROGRESS_CHECKPOINT_TOOL),
            &mut checkpoint,
            &context,
        )
        .await;
        assert_eq!(checkpoint.result.unwrap()["accepted"], true);
        assert!(matches!(
            gate.before_exec(
                call("read_file", json!({ "path": "src/verified.rs" })),
                &tool_def("read_file"),
                &context,
            )
            .await,
            PreToolUseDecision::Continue(_)
        ));
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
    async fn checkpoint_warning_fires_once_for_the_unchanged_state() {
        let state = Arc::new(Mutex::new(ProgressGuardState::default()));
        let hook = ProgressGuardHook {
            state: state.clone(),
        };
        let context = ToolContext::new(SessionId::new());
        let mut warning_count = 0;
        let mut checkpoint_warning = String::new();

        for i in 0..(CHECKPOINT_WITHOUT_PROGRESS_THRESHOLD + 12) {
            let mut out = result();
            hook.after_exec(
                &call("grep_files", json!({ "pattern": format!("needle{i}") })),
                &tool_def("grep_files"),
                &mut out,
                &context,
            )
            .await;
            if let Some(warning) = out
                .result
                .as_ref()
                .and_then(|value| value.get("progress_guard_warning"))
                .and_then(Value::as_str)
                .filter(|warning| warning.contains("checkpoint required"))
            {
                warning_count += 1;
                checkpoint_warning = warning.to_string();
            }
        }

        assert_eq!(warning_count, 1);
        assert!(
            !checkpoint_warning.contains("tool_search"),
            "progress_checkpoint is always eager, so the warning must not prescribe a reveal round"
        );
        assert!(
            state
                .lock()
                .unwrap()
                .sessions
                .get(&context.session_id.to_string())
                .is_some_and(|progress| progress.checkpoint_required)
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
    async fn repeated_unchanged_validation_does_not_unlock_checkpoint_gate() {
        let state = Arc::new(Mutex::new(ProgressGuardState::default()));
        let hook = ProgressGuardHook {
            state: state.clone(),
        };
        let context = ToolContext::new(SessionId::new());
        let validation = call("bash", json!({ "command": "cargo test" }));

        hook.after_exec(&validation, &tool_def("bash"), &mut result(), &context)
            .await;
        for i in 0..CHECKPOINT_WITHOUT_PROGRESS_THRESHOLD {
            hook.after_exec(
                &call("read_file", json!({ "path": format!("/src/{i}.rs") })),
                &tool_def("read_file"),
                &mut result(),
                &context,
            )
            .await;
        }
        hook.after_exec(&validation, &tool_def("bash"), &mut result(), &context)
            .await;

        assert!(
            state
                .lock()
                .unwrap()
                .sessions
                .get(&context.session_id.to_string())
                .is_some_and(|progress| progress.checkpoint_required),
            "an unchanged validation is not decisive progress"
        );
    }

    #[tokio::test]
    async fn externally_changed_read_makes_validation_fresh() {
        let state = Arc::new(Mutex::new(ProgressGuardState::default()));
        let hook = ProgressGuardHook { state };
        let context = ToolContext::new(SessionId::new());
        let validation = call("bash", json!({ "command": "cargo test" }));
        let read = call("read_file", json!({ "path": "/src/lib.rs" }));

        hook.after_exec(&validation, &tool_def("bash"), &mut result(), &context)
            .await;
        hook.after_exec(
            &read,
            &tool_def("read_file"),
            &mut result_value(json!({ "content": "before".repeat(200) })),
            &context,
        )
        .await;
        hook.after_exec(
            &read,
            &tool_def("read_file"),
            &mut result_value(json!({ "content": "after".repeat(200) })),
            &context,
        )
        .await;
        let mut after_external_change = result();
        hook.after_exec(
            &validation,
            &tool_def("bash"),
            &mut after_external_change,
            &context,
        )
        .await;

        assert!(
            after_external_change
                .result
                .as_ref()
                .and_then(|value| value.get("progress_guard_warning"))
                .is_none()
        );
    }

    #[tokio::test]
    async fn workspace_state_revisit_warns_on_a_mutation_cycle() {
        let state = Arc::new(Mutex::new(ProgressGuardState::default()));
        let hook = ProgressGuardHook { state };
        let context = ToolContext::new(SessionId::new());
        let transitions = [("A", "B"), ("B", "C"), ("C", "A")];

        for (index, (previous, current)) in transitions.into_iter().enumerate() {
            let mut out = result_value(json!({
                "path": "/workspace/Cargo.toml",
                "previous_content_hash": previous,
                "content_hash": current,
            }));
            hook.after_exec(
                &call("edit_file", json!({ "path": "/workspace/Cargo.toml" })),
                &tool_def("edit_file"),
                &mut out,
                &context,
            )
            .await;

            let warning = out
                .result
                .as_ref()
                .and_then(|value| value.get("progress_guard_warning"))
                .and_then(Value::as_str);
            if index < 2 {
                assert!(warning.is_none(), "new states are progress");
            } else {
                assert!(
                    warning.is_some_and(|text| text.contains("workspace state")),
                    "returning to A should expose the mutation cycle"
                );
            }
        }
    }

    #[tokio::test]
    async fn lockfile_updates_do_not_hide_a_manifest_state_cycle() {
        let state = Arc::new(Mutex::new(ProgressGuardState::default()));
        let hook = ProgressGuardHook { state };
        let context = ToolContext::new(SessionId::new());
        let transitions = [("A", "B"), ("B", "C"), ("C", "A")];

        for (index, (previous, current)) in transitions.into_iter().enumerate() {
            let mut edit = result_value(json!({
                "path": "/workspace/Cargo.toml",
                "previous_content_hash": previous,
                "content_hash": current,
            }));
            hook.after_exec(
                &call("edit_file", json!({ "path": "/workspace/Cargo.toml" })),
                &tool_def("edit_file"),
                &mut edit,
                &context,
            )
            .await;

            let warning = edit
                .result
                .as_ref()
                .and_then(|value| value.get("progress_guard_warning"))
                .and_then(Value::as_str);
            if index < 2 {
                assert!(warning.is_none());
            } else {
                assert!(warning.is_some_and(|text| text.contains("workspace state")));
            }

            let mut update = result();
            hook.after_exec(
                &call(
                    "bash",
                    json!({ "command": "cargo update -p everruns-core --precise 0.17.7" }),
                ),
                &tool_def("bash"),
                &mut update,
                &context,
            )
            .await;
        }
    }

    #[tokio::test]
    async fn lockfile_update_makes_repeated_validation_fresh() {
        let state = Arc::new(Mutex::new(ProgressGuardState::default()));
        let hook = ProgressGuardHook { state };
        let context = ToolContext::new(SessionId::new());
        let validation = call("bash", json!({ "command": "cargo check" }));

        hook.after_exec(&validation, &tool_def("bash"), &mut result(), &context)
            .await;
        hook.after_exec(
            &call(
                "bash",
                json!({ "command": "cargo update -p everruns-core --precise 0.17.7" }),
            ),
            &tool_def("bash"),
            &mut result(),
            &context,
        )
        .await;

        let mut after_update = result();
        hook.after_exec(&validation, &tool_def("bash"), &mut after_update, &context)
            .await;
        assert!(
            after_update
                .result
                .as_ref()
                .and_then(|value| value.get("progress_guard_warning"))
                .is_none(),
            "validation after a lockfile mutation has new workspace evidence"
        );

        let mut unchanged_again = result();
        hook.after_exec(
            &validation,
            &tool_def("bash"),
            &mut unchanged_again,
            &context,
        )
        .await;
        assert!(
            unchanged_again
                .result
                .as_ref()
                .and_then(|value| value.get("progress_guard_warning"))
                .is_some(),
            "a second validation without another mutation is redundant"
        );
    }

    #[tokio::test]
    async fn repeated_validation_on_unchanged_state_warns() {
        let state = Arc::new(Mutex::new(ProgressGuardState::default()));
        let hook = ProgressGuardHook { state };
        let context = ToolContext::new(SessionId::new());
        let validation = call("bash", json!({ "command": "cargo test" }));

        let mut first = result();
        hook.after_exec(&validation, &tool_def("bash"), &mut first, &context)
            .await;
        assert!(
            first
                .result
                .as_ref()
                .and_then(|value| value.get("progress_guard_warning"))
                .is_none()
        );

        let mut repeated = result();
        hook.after_exec(&validation, &tool_def("bash"), &mut repeated, &context)
            .await;
        assert!(
            repeated
                .result
                .as_ref()
                .and_then(|value| value.get("progress_guard_warning"))
                .and_then(Value::as_str)
                .is_some_and(|text| text.contains("unchanged workspace state"))
        );

        let mut edit = result_value(json!({
            "path": "/workspace/src/lib.rs",
            "previous_content_hash": "A",
            "content_hash": "B",
        }));
        hook.after_exec(
            &call("edit_file", json!({ "path": "/workspace/src/lib.rs" })),
            &tool_def("edit_file"),
            &mut edit,
            &context,
        )
        .await;

        let mut after_progress = result();
        hook.after_exec(
            &validation,
            &tool_def("bash"),
            &mut after_progress,
            &context,
        )
        .await;
        assert!(
            after_progress
                .result
                .as_ref()
                .and_then(|value| value.get("progress_guard_warning"))
                .is_none(),
            "the same validation is useful after the workspace changes"
        );
    }

    #[tokio::test]
    async fn semantic_progress_has_no_fixed_session_iteration_limit() {
        let state = Arc::new(Mutex::new(ProgressGuardState::default()));
        let hook = ProgressGuardHook { state };
        let context = ToolContext::new(SessionId::new());

        for index in 0..256 {
            let mut out = result_value(json!({
                "path": "/workspace/src/lib.rs",
                "previous_content_hash": format!("state-{index}"),
                "content_hash": format!("state-{}", index + 1),
            }));
            hook.after_exec(
                &call("edit_file", json!({ "path": "/workspace/src/lib.rs" })),
                &tool_def("edit_file"),
                &mut out,
                &context,
            )
            .await;
            assert!(
                out.result
                    .as_ref()
                    .and_then(|value| value.get("progress_guard_warning"))
                    .is_none(),
                "each new state remains progress after iteration {index}"
            );
        }
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
    async fn three_zero_evidence_searches_warn_even_when_queries_differ() {
        let state = Arc::new(Mutex::new(ProgressGuardState::default()));
        let hook = ProgressGuardHook { state };
        let context = ToolContext::new(SessionId::new());
        let mut last = result();

        for pattern in ["history", "ground", "session"] {
            last = result_value(json!({ "ok": true, "count": 0, "matches": [] }));
            hook.after_exec(
                &call("grep_files", json!({ "pattern": pattern })),
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
                .is_some_and(|warning| warning.contains("zero matches"))
        );
    }

    #[tokio::test]
    async fn positive_search_evidence_resets_zero_result_streak() {
        let state = Arc::new(Mutex::new(ProgressGuardState::default()));
        let hook = ProgressGuardHook { state };
        let context = ToolContext::new(SessionId::new());

        for count in [0, 0, 1, 0, 0] {
            let mut out = result_value(json!({ "ok": true, "count": count }));
            hook.after_exec(
                &call("repo_map", json!({ "query": format!("query-{count}") })),
                &tool_def("repo_map"),
                &mut out,
                &context,
            )
            .await;
            assert!(
                out.result
                    .as_ref()
                    .and_then(|value| value.get("progress_guard_warning"))
                    .is_none(),
                "a positive result should break the zero-evidence streak"
            );
        }
    }

    #[tokio::test]
    async fn repeated_truncated_exploration_warns_to_narrow_scope() {
        let state = Arc::new(Mutex::new(ProgressGuardState::default()));
        let hook = ProgressGuardHook { state };
        let context = ToolContext::new(SessionId::new());
        let mut last = result();

        for query in ["runtime", "capability"] {
            last = result_value(json!({ "ok": true, "count": 50, "truncated": true }));
            hook.after_exec(
                &call("repo_map", json!({ "query": query })),
                &tool_def("repo_map"),
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
                .is_some_and(|warning| warning.contains("truncated"))
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
                json!({ "path": "/repo/src/capabilities/background.rs", "offset": offset }),
            ))
            .await
            {
                warnings.push(warning);
            }
        }
        for offset in [1000, 1020, 1010, 1028, 1000, 1020, 1010, 1028, 1000] {
            if let Some(warning) = observe(call(
                "read_file",
                json!({ "path": "/repo/src/app/mod.rs", "offset": offset }),
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
                json!({ "path": "/repo/src/runtime.rs", "offset": 2100 + i }),
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
                json!({ "path": "/repo/src/app/mod.rs", "offset": 1000 }),
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
            "gh pr view 42",
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
    async fn semantic_polling_cycle_warns_across_heterogeneous_tools() {
        let state = Arc::new(Mutex::new(ProgressGuardState::default()));
        let hook = ProgressGuardHook { state };
        let context = ToolContext::new(SessionId::new());
        let cycle = [
            call("bash", json!({ "command": "gh pr checks 42" })),
            call("read_file", json!({ "path": "/tmp/synthetic-ci-note" })),
            call("get_task", json!({ "task_id": "task_ci" })),
            call("bash", json!({ "command": "gh run view 123" })),
        ];
        let mut warnings = Vec::new();

        for tool_call in cycle.into_iter().cycle().take(8) {
            let mut output = result();
            hook.after_exec(
                &tool_call,
                &tool_def(&tool_call.name),
                &mut output,
                &context,
            )
            .await;
            if let Some(warning) = output
                .result
                .as_ref()
                .and_then(|value| value.get("progress_guard_warning"))
                .and_then(Value::as_str)
            {
                warnings.push(warning.to_string());
            }
        }

        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("spawn_background")),
            "a semantic polling cycle must be caught even when unrelated reads and task probes separate external checks: {warnings:?}"
        );
    }

    #[tokio::test]
    async fn one_off_task_and_ci_status_checks_do_not_warn() {
        let state = Arc::new(Mutex::new(ProgressGuardState::default()));
        let hook = ProgressGuardHook { state };
        let context = ToolContext::new(SessionId::new());

        for tool_call in [
            call("list_tasks", json!({})),
            call("get_task", json!({ "task_id": "task_once" })),
            call("bash", json!({ "command": "gh pr checks 42" })),
        ] {
            let mut output = result();
            hook.after_exec(
                &tool_call,
                &tool_def(&tool_call.name),
                &mut output,
                &context,
            )
            .await;
            assert!(
                output
                    .result
                    .as_ref()
                    .and_then(|value| value.get("progress_guard_warning"))
                    .is_none(),
                "a one-off status check is legitimate"
            );
        }
    }

    #[tokio::test]
    async fn interleaved_work_resets_waiting_window() {
        let state = Arc::new(Mutex::new(ProgressGuardState::default()));
        let hook = ProgressGuardHook { state };
        let context = ToolContext::new(SessionId::new());

        // Check CI, fix something, check again — legitimate, because real
        // progress clears the semantic window.
        for _ in 0..(WAITING_WINDOW_THRESHOLD * 2) {
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

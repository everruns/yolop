//! Session-scoped user-ask tracking and end-of-turn validation.
//!
//! Records what the user is asking for, allows updates when they change direction,
//! and evaluates after each turn whether the ask was achieved, blocked, or still
//! in progress. The default host completion gate may selectively continue it.

use anyhow::{Context, Result, bail};
use everruns_core::Message;
use everruns_core::SessionCompletionRequest;
use everruns_core::command::{CommandExecutionContext, ExecuteCommandRequest};
use everruns_provider::typed_id::SessionId;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::RwLock;

pub(crate) const USER_ASK_CAPABILITY_ID: &str = "yolop_user_ask";
pub(crate) const USER_ASK_COMMAND_NAME: &str = "ask";
/// Internal `execute_command` argument — not user-facing.
pub(crate) const USER_ASK_EVALUATE_ARG: &str = "\x00evaluate";

pub(crate) const MAX_USER_ASK_LEN: usize = 4_000;
pub(crate) const MAX_USER_ASK_REVISIONS: usize = 8;
const MAX_EVALUATION_REASON_CHARS: usize = 1_000;

const USER_ASK_FILE: &str = "user_ask.json";

const EVALUATOR_SYSTEM_PROMPT: &str = "\
You evaluate whether the user's request was addressed using only the conversation \
transcript below. You cannot run commands or read files independently — judge \
only what the agent has already surfaced in the conversation.\n\
\n\
Respond with exactly one JSON object and no other text:\n\
{\"outcome\": \"achieved\"|\"blocked\"|\"failed\"|\"waiting_on_background\"|\"in_progress\", \"reason\": \"short explanation\"}\n\
\n\
- `achieved`: the user's request is clearly satisfied from the transcript.\n\
- `blocked`: the agent hit a blocker that needs user input, permission, or an \
external dependency before work can continue.\n\
- `failed`: a permanent error ended the work; retrying will not help.\n\
- `waiting_on_background`: detached work is running and its completion will wake the agent.\n\
- `in_progress`: work is underway but not finished and no hard blocker is evident.";

const CLEAR_ALIASES: &[&str] = &["clear", "stop", "off", "reset", "none", "cancel"];

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AskOutcome {
    Achieved,
    Blocked,
    Failed,
    WaitingOnBackground,
    InProgress,
}

impl AskOutcome {
    fn parse_str(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "achieved" | "done" | "complete" | "completed" => Some(Self::Achieved),
            "blocked" | "blocker" | "stuck" => Some(Self::Blocked),
            "failed" | "failure" | "permanent_error" => Some(Self::Failed),
            "waiting_on_background" | "waiting" | "background" => Some(Self::WaitingOnBackground),
            "in_progress" | "in-progress" | "progress" | "ongoing" | "pending" => {
                Some(Self::InProgress)
            }
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Achieved => "achieved",
            Self::Blocked => "blocked",
            Self::Failed => "failed",
            Self::WaitingOnBackground => "waiting_on_background",
            Self::InProgress => "in_progress",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct AskRevision {
    pub text: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct PersistedUserAsk {
    pub text: String,
    pub active: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub revisions: Vec<AskRevision>,
    pub evaluated_turns: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_outcome: Option<AskOutcome>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_reason: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct UserAskStatus {
    pub active: Option<PersistedUserAsk>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum UserAskCommandOutcome {
    Set { text: String },
    Cleared,
    Status(UserAskStatus),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct UserAskEvaluation {
    pub outcome: AskOutcome,
    pub reason: String,
}

struct SessionUserAskRuntime {
    persisted: PersistedUserAsk,
}

pub(crate) struct UserAskStore {
    session_dir: PathBuf,
    sessions: RwLock<HashMap<SessionId, SessionUserAskRuntime>>,
}

impl UserAskStore {
    pub(crate) fn open(session_dir: PathBuf) -> Self {
        Self {
            session_dir,
            sessions: RwLock::new(HashMap::new()),
        }
    }

    pub(crate) fn load_session(&self, session_id: SessionId) -> Result<()> {
        let path = self.ask_path(session_id);
        if !path.is_file() {
            return Ok(());
        }
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("read user ask state: {}", path.display()))?;
        let persisted: PersistedUserAsk = serde_json::from_str(&raw)
            .with_context(|| format!("parse user ask state: {}", path.display()))?;
        if persisted.active {
            let mut sessions = self.sessions.write().expect("user ask store lock poisoned");
            sessions.insert(session_id, SessionUserAskRuntime { persisted });
        }
        Ok(())
    }

    pub(crate) fn parse_user_args(arguments: Option<&str>) -> Result<UserAskCommandOutcome> {
        let trimmed = arguments.map(str::trim).unwrap_or_default();
        if trimmed.is_empty() {
            return Ok(UserAskCommandOutcome::Status(UserAskStatus {
                active: None,
            }));
        }
        if CLEAR_ALIASES
            .iter()
            .any(|alias| trimmed.eq_ignore_ascii_case(alias))
        {
            return Ok(UserAskCommandOutcome::Cleared);
        }
        if trimmed.len() > MAX_USER_ASK_LEN {
            bail!("user ask is too long (max {MAX_USER_ASK_LEN} characters)");
        }
        Ok(UserAskCommandOutcome::Set {
            text: trimmed.to_string(),
        })
    }

    pub(crate) fn apply_outcome(
        &self,
        session_id: SessionId,
        outcome: UserAskCommandOutcome,
    ) -> Result<String> {
        match outcome {
            UserAskCommandOutcome::Set { text } => {
                self.set_ask(session_id, text.clone())?;
                Ok(format!("user ask recorded: {text}"))
            }
            UserAskCommandOutcome::Cleared => {
                self.clear_active(session_id);
                Ok("user ask cleared".into())
            }
            UserAskCommandOutcome::Status(_) => Ok(String::new()),
        }
    }

    pub(crate) fn status(&self, session_id: SessionId) -> UserAskStatus {
        let sessions = self.sessions.read().expect("user ask store lock poisoned");
        if let Some(runtime) = sessions.get(&session_id)
            && runtime.persisted.active
        {
            return UserAskStatus {
                active: Some(runtime.persisted.clone()),
            };
        }
        UserAskStatus { active: None }
    }

    pub(crate) fn record_user_prompt(&self, session_id: SessionId, text: &str) -> Result<()> {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return Ok(());
        }
        if trimmed.len() > MAX_USER_ASK_LEN {
            bail!("user ask is too long (max {MAX_USER_ASK_LEN} characters)");
        }
        self.set_ask(session_id, trimmed.to_string())
    }

    pub(crate) fn set_ask(&self, session_id: SessionId, text: String) -> Result<()> {
        if text.len() > MAX_USER_ASK_LEN {
            bail!("user ask is too long (max {MAX_USER_ASK_LEN} characters)");
        }
        let mut sessions = self.sessions.write().expect("user ask store lock poisoned");
        match sessions.get_mut(&session_id) {
            Some(runtime) if runtime.persisted.active => {
                if runtime.persisted.text != text {
                    let previous = runtime.persisted.text.clone();
                    push_revision(&mut runtime.persisted, previous);
                    runtime.persisted.text = text;
                    runtime.persisted.evaluated_turns = 0;
                    runtime.persisted.last_outcome = None;
                    runtime.persisted.last_reason = None;
                }
            }
            _ => {
                sessions.insert(
                    session_id,
                    SessionUserAskRuntime {
                        persisted: PersistedUserAsk {
                            text: text.clone(),
                            active: true,
                            revisions: Vec::new(),
                            evaluated_turns: 0,
                            last_outcome: None,
                            last_reason: None,
                        },
                    },
                );
            }
        }
        drop(sessions);
        self.persist(session_id)
    }

    pub(crate) fn clear_active(&self, session_id: SessionId) {
        let mut sessions = self.sessions.write().expect("user ask store lock poisoned");
        sessions.remove(&session_id);
        drop(sessions);
        let path = self.ask_path(session_id);
        if path.is_file() {
            let _ = std::fs::remove_file(path);
        }
    }

    pub(crate) fn is_active(&self, session_id: SessionId) -> bool {
        self.sessions
            .read()
            .expect("user ask store lock poisoned")
            .get(&session_id)
            .is_some_and(|runtime| runtime.persisted.active)
    }

    pub(crate) fn active_text(&self, session_id: SessionId) -> Option<String> {
        self.sessions
            .read()
            .expect("user ask store lock poisoned")
            .get(&session_id)
            .filter(|runtime| runtime.persisted.active)
            .map(|runtime| runtime.persisted.text.clone())
    }

    pub(crate) fn record_evaluation(
        &self,
        session_id: SessionId,
        evaluation: &UserAskEvaluation,
    ) -> Result<()> {
        let mut sessions = self.sessions.write().expect("user ask store lock poisoned");
        let Some(runtime) = sessions.get_mut(&session_id) else {
            return Ok(());
        };
        runtime.persisted.evaluated_turns = runtime.persisted.evaluated_turns.saturating_add(1);
        runtime.persisted.last_outcome = Some(evaluation.outcome);
        runtime.persisted.last_reason = Some(evaluation.reason.clone());
        if matches!(
            evaluation.outcome,
            AskOutcome::Achieved | AskOutcome::Blocked | AskOutcome::Failed
        ) {
            runtime.persisted.active = false;
        }
        drop(sessions);
        self.persist(session_id)
    }

    fn ask_path(&self, _session_id: SessionId) -> PathBuf {
        self.session_dir.join(USER_ASK_FILE)
    }

    fn persist(&self, session_id: SessionId) -> Result<()> {
        let sessions = self.sessions.read().expect("user ask store lock poisoned");
        let Some(runtime) = sessions.get(&session_id) else {
            return Ok(());
        };
        if !runtime.persisted.active && runtime.persisted.last_outcome.is_none() {
            return Ok(());
        }
        let path = self.ask_path(session_id);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create user ask parent dir: {}", parent.display()))?;
        }
        let encoded = serde_json::to_string_pretty(&runtime.persisted)?;
        std::fs::write(&path, encoded)
            .with_context(|| format!("write user ask state: {}", path.display()))?;
        Ok(())
    }
}

fn push_revision(persisted: &mut PersistedUserAsk, previous: String) {
    if previous.trim().is_empty() {
        return;
    }
    persisted.revisions.push(AskRevision { text: previous });
    if persisted.revisions.len() > MAX_USER_ASK_REVISIONS {
        let overflow = persisted.revisions.len() - MAX_USER_ASK_REVISIONS;
        persisted.revisions.drain(0..overflow);
    }
}

pub(crate) fn format_status(status: &UserAskStatus) -> String {
    let Some(active) = &status.active else {
        return "no active user ask".into();
    };
    let reason = active
        .last_reason
        .as_deref()
        .filter(|value| !value.is_empty())
        .map(|value| format!("\nlast evaluation: {value}"))
        .unwrap_or_default();
    let outcome = active
        .last_outcome
        .map(|value| format!("\nlast outcome: {}", value.as_str()))
        .unwrap_or_default();
    let revisions = if active.revisions.is_empty() {
        String::new()
    } else {
        let lines: Vec<String> = active
            .revisions
            .iter()
            .map(|revision| format!("- {}", revision.text))
            .collect();
        format!("\nprevious asks:\n{}", lines.join("\n"))
    };
    format!(
        "user ask active\nask: {}\nevaluated turns: {}{}{}{}",
        active.text, active.evaluated_turns, outcome, reason, revisions
    )
}

pub(crate) fn system_prompt_block(status: &UserAskStatus) -> String {
    let Some(active) = &status.active else {
        return String::new();
    };
    let evaluation = active
        .last_reason
        .as_deref()
        .filter(|value| !value.is_empty())
        .map(|reason| {
            let outcome = active
                .last_outcome
                .map(|value| value.as_str())
                .unwrap_or("in_progress");
            format!("\nLast evaluation ({outcome}): {reason}\n")
        })
        .unwrap_or_default();
    format!(
        "<capability id=\"yolop_user_ask\">\n\
Track and satisfy the user's request. Call `set_user_ask` when they change direction; \
`clear_user_ask` only when they abandon it.\n\
Current user ask: {}\n\
{evaluation}\
</capability>",
        active.text
    )
}

pub(crate) async fn evaluate_active_user_ask(
    ctx: &CommandExecutionContext,
    ask: &str,
) -> everruns_provider::error::Result<UserAskEvaluation> {
    let turn = ctx.host.turn_context().await?;
    let transcript = format_transcript(&turn.messages);
    let user_prompt = format!("User request:\n{ask}\n\nConversation transcript:\n{transcript}");
    let completion_request = SessionCompletionRequest {
        system_prompts: vec![EVALUATOR_SYSTEM_PROMPT.to_string()],
        messages: vec![Message::user(user_prompt)],
        controls: None,
        metadata: std::collections::HashMap::from([(
            "command".to_string(),
            "user_ask_evaluate".to_string(),
        )]),
    };
    let completion = ctx
        .host
        .completion(completion_request)
        .await
        .map_err(map_completion_error)?;
    parse_evaluation_response(&completion.text)
}

fn map_completion_error(
    error: everruns_core::command_host::SessionCompletionError,
) -> everruns_provider::error::AgentLoopError {
    match error {
        everruns_core::command_host::SessionCompletionError::InvalidRequest(err) => err,
        everruns_core::command_host::SessionCompletionError::StreamingUnsupported => {
            everruns_provider::error::AgentLoopError::config(
                "user ask evaluator does not support streaming",
            )
        }
        everruns_core::command_host::SessionCompletionError::Completion { error, .. } => {
            everruns_provider::error::AgentLoopError::config(error)
        }
    }
}

fn format_transcript(messages: &[Message]) -> String {
    if messages.is_empty() {
        return "(empty)".into();
    }
    messages
        .iter()
        .filter_map(|message| {
            let role = match message.role {
                everruns_core::message::MessageRole::User => "user",
                everruns_core::message::MessageRole::Agent => "assistant",
                everruns_core::message::MessageRole::System => "system",
                everruns_core::message::MessageRole::ToolResult => "tool",
            };
            message
                .text()
                .map(|text| format!("{role}: {}", text.trim()))
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

pub(crate) fn parse_evaluation_response(
    text: &str,
) -> everruns_provider::error::Result<UserAskEvaluation> {
    let trimmed = text.trim();
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) {
        return parse_evaluation_value(&value);
    }
    if let Some(start) = trimmed.find('{')
        && let Some(end) = trimmed.rfind('}')
    {
        let slice = &trimmed[start..=end];
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(slice) {
            return parse_evaluation_value(&value);
        }
    }
    Ok(UserAskEvaluation {
        outcome: AskOutcome::InProgress,
        reason: bounded_reason(trimmed),
    })
}

fn parse_evaluation_value(
    value: &serde_json::Value,
) -> everruns_provider::error::Result<UserAskEvaluation> {
    let outcome_raw = value
        .get("outcome")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            everruns_provider::error::AgentLoopError::config(
                "user ask evaluator returned JSON without `outcome`",
            )
        })?;
    let outcome = AskOutcome::parse_str(outcome_raw).ok_or_else(|| {
        everruns_provider::error::AgentLoopError::config(format!(
            "user ask evaluator returned unknown outcome: {outcome_raw}"
        ))
    })?;
    let reason = value
        .get("reason")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .trim();
    let reason = bounded_reason(reason);
    Ok(UserAskEvaluation { outcome, reason })
}

fn bounded_reason(reason: &str) -> String {
    reason.chars().take(MAX_EVALUATION_REASON_CHARS).collect()
}

pub(crate) fn evaluation_result_message(evaluation: &UserAskEvaluation) -> String {
    serde_json::json!({
        "outcome": evaluation.outcome.as_str(),
        "reason": evaluation.reason,
    })
    .to_string()
}

pub(crate) fn evaluation_status_message(evaluation: &UserAskEvaluation) -> Option<String> {
    match evaluation.outcome {
        AskOutcome::Achieved => Some(format!("user ask achieved: {}", evaluation.reason)),
        // The assistant's clarification is already the visible handoff to the user.
        // Repeating blocked state as host-authored transcript copy only adds noise.
        AskOutcome::Blocked => None,
        AskOutcome::Failed => Some(format!("user ask failed: {}", evaluation.reason)),
        AskOutcome::WaitingOnBackground => Some(format!(
            "user ask waiting on background: {}",
            evaluation.reason
        )),
        AskOutcome::InProgress => Some(format!("user ask in progress: {}", evaluation.reason)),
    }
}

pub(crate) fn is_user_ask_evaluate_request(request: &ExecuteCommandRequest) -> bool {
    request.name == USER_ASK_COMMAND_NAME
        && request
            .arguments
            .as_deref()
            .is_some_and(|args| args == USER_ASK_EVALUATE_ARG)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_clear_aliases() {
        for alias in CLEAR_ALIASES {
            let outcome = UserAskStore::parse_user_args(Some(alias)).expect("parse");
            assert_eq!(outcome, UserAskCommandOutcome::Cleared);
        }
    }

    #[test]
    fn set_ask_records_revision_on_change() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = UserAskStore::open(dir.path().to_path_buf());
        let session_id = SessionId::new();
        store.set_ask(session_id, "add tests".into()).expect("set");
        store
            .set_ask(session_id, "ship the fix".into())
            .expect("set");
        let status = store.status(session_id);
        let active = status.active.expect("active");
        assert_eq!(active.text, "ship the fix");
        assert_eq!(active.revisions.len(), 1);
        assert_eq!(active.revisions[0].text, "add tests");
    }

    #[test]
    fn record_user_prompt_updates_active_ask() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = UserAskStore::open(dir.path().to_path_buf());
        let session_id = SessionId::new();
        store
            .record_user_prompt(session_id, "fix the login bug")
            .expect("record");
        assert_eq!(
            store.active_text(session_id).as_deref(),
            Some("fix the login bug")
        );
    }

    #[test]
    fn resumed_store_restores_actual_ask_and_pivot_history() {
        let dir = tempfile::tempdir().expect("tempdir");
        let session_id = SessionId::new();
        let store = UserAskStore::open(dir.path().to_path_buf());
        store
            .record_user_prompt(session_id, "wait for CI then merge")
            .expect("record original ask");

        let resumed = UserAskStore::open(dir.path().to_path_buf());
        resumed.load_session(session_id).expect("resume ask");
        assert_eq!(
            resumed.active_text(session_id).as_deref(),
            Some("wait for CI then merge")
        );
        resumed
            .record_user_prompt(session_id, "do not merge; only report CI")
            .expect("record pivot");
        let active = resumed.status(session_id).active.expect("active pivot");
        assert_eq!(active.text, "do not merge; only report CI");
        assert_eq!(active.revisions[0].text, "wait for CI then merge");
    }

    #[test]
    fn inactive_tracking_adds_no_system_prompt_text() {
        assert!(system_prompt_block(&UserAskStatus { active: None }).is_empty());
    }

    #[test]
    fn evaluation_marks_achieved_inactive() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = UserAskStore::open(dir.path().to_path_buf());
        let session_id = SessionId::new();
        store.set_ask(session_id, "run tests".into()).expect("set");
        let evaluation = UserAskEvaluation {
            outcome: AskOutcome::Achieved,
            reason: "tests passed".into(),
        };
        store
            .record_evaluation(session_id, &evaluation)
            .expect("record");
        assert!(!store.is_active(session_id));
    }

    #[test]
    fn parse_evaluation_json() {
        let evaluation = parse_evaluation_response(
            r#"{"outcome": "blocked", "reason": "needs API key from user"}"#,
        )
        .expect("parse");
        assert_eq!(evaluation.outcome, AskOutcome::Blocked);
        assert!(evaluation.reason.contains("API key"));
    }

    #[test]
    fn evaluation_status_message_suppresses_only_blocked() {
        for (outcome, expected) in [
            (AskOutcome::Achieved, Some("user ask achieved: reason")),
            (AskOutcome::Blocked, None),
            (AskOutcome::Failed, Some("user ask failed: reason")),
            (
                AskOutcome::WaitingOnBackground,
                Some("user ask waiting on background: reason"),
            ),
            (AskOutcome::InProgress, Some("user ask in progress: reason")),
        ] {
            let evaluation = UserAskEvaluation {
                outcome,
                reason: "reason".into(),
            };
            assert_eq!(evaluation_status_message(&evaluation).as_deref(), expected);
        }
    }

    #[test]
    fn system_prompt_includes_current_ask() {
        let block = system_prompt_block(&UserAskStatus {
            active: Some(PersistedUserAsk {
                text: "upgrade dependencies".into(),
                active: true,
                revisions: vec![AskRevision {
                    text: "obsolete request must stay out of the prompt".into(),
                }],
                evaluated_turns: 0,
                last_outcome: None,
                last_reason: None,
            }),
        });
        assert!(block.contains("upgrade dependencies"));
        assert!(block.contains("set_user_ask"));
        assert!(!block.contains("obsolete request"));
    }
}

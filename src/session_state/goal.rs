//! Session-scoped `/goal` state and evaluator helpers.
//!
//! `/goal` keeps the agent working across turns until a model-evaluated
//! completion condition holds. The evaluator reads the conversation transcript
//! (no tools) after each turn, matching the Claude Code pattern.

use anyhow::{Context, Result, bail};
use everruns_core::Message;
use everruns_core::SessionCompletionRequest;
use everruns_core::command::{CommandExecutionContext, ExecuteCommandRequest};
use everruns_provider::typed_id::SessionId;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::RwLock;
use std::time::{Duration, Instant};

pub(crate) const GOAL_CAPABILITY_ID: &str = "yolop_goal";
pub(crate) const GOAL_COMMAND_NAME: &str = "goal";
/// Internal `execute_command` argument — not user-facing.
pub(crate) const GOAL_EVALUATE_ARG: &str = "\x00evaluate";

pub(crate) const MAX_GOAL_CONDITION_LEN: usize = 4_000;

const GOAL_FILE: &str = "goal.json";

const EVALUATOR_SYSTEM_PROMPT: &str = "\
You evaluate whether a completion condition is met using only the conversation \
transcript below. You cannot run commands or read files independently — judge \
only what the agent has already surfaced in the conversation.\n\
\n\
Respond with exactly one JSON object and no other text:\n\
{\"met\": true|false, \"reason\": \"short explanation\"}\n\
\n\
Set `met` to true only when the condition is clearly satisfied from the \
transcript. Otherwise set `met` to false and explain what is still missing or \
what the agent should do next.";

const CLEAR_ALIASES: &[&str] = &["clear", "stop", "off", "reset", "none", "cancel"];
const PAUSE_ALIASES: &[&str] = &["pause", "paused"];
const RESUME_ALIASES: &[&str] = &["resume", "continue"];

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct GoalAchieved {
    pub condition: String,
    pub evaluated_turns: u32,
    pub reason: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct PersistedGoal {
    pub condition: String,
    pub active: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub paused: bool,
    pub evaluated_turns: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub achieved: Option<GoalAchieved>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct GoalStatus {
    pub active: Option<PersistedGoal>,
    pub achieved: Option<GoalAchieved>,
    pub elapsed: Option<Duration>,
    pub session_tokens: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum GoalCommandOutcome {
    Set { condition: String },
    Paused,
    Resumed,
    Cleared,
    Status(GoalStatus),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct GoalEvaluation {
    pub met: bool,
    pub reason: String,
}

struct SessionGoalRuntime {
    persisted: PersistedGoal,
    started_at: Instant,
    pending_turn: bool,
}

pub(crate) struct GoalStore {
    session_dir: PathBuf,
    sessions: RwLock<HashMap<SessionId, SessionGoalRuntime>>,
}

impl GoalStore {
    pub(crate) fn open(session_dir: PathBuf) -> Self {
        Self {
            session_dir,
            sessions: RwLock::new(HashMap::new()),
        }
    }

    pub(crate) fn load_session(&self, session_id: SessionId) -> Result<()> {
        let path = self.goal_path(session_id);
        if !path.is_file() {
            return Ok(());
        }
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("read goal state: {}", path.display()))?;
        let persisted: PersistedGoal = serde_json::from_str(&raw)
            .with_context(|| format!("parse goal state: {}", path.display()))?;
        if persisted.active {
            let pending_turn = !persisted.paused;
            let mut sessions = self.sessions.write().expect("goal store lock poisoned");
            sessions.insert(
                session_id,
                SessionGoalRuntime {
                    persisted,
                    started_at: Instant::now(),
                    pending_turn,
                },
            );
        }
        Ok(())
    }

    pub(crate) fn parse_user_args(arguments: Option<&str>) -> Result<GoalCommandOutcome> {
        let trimmed = arguments.map(str::trim).unwrap_or_default();
        if trimmed.is_empty() {
            return Ok(GoalCommandOutcome::Status(GoalStatus {
                active: None,
                achieved: None,
                elapsed: None,
                session_tokens: None,
            }));
        }
        if CLEAR_ALIASES
            .iter()
            .any(|alias| trimmed.eq_ignore_ascii_case(alias))
        {
            return Ok(GoalCommandOutcome::Cleared);
        }
        if PAUSE_ALIASES
            .iter()
            .any(|alias| trimmed.eq_ignore_ascii_case(alias))
        {
            return Ok(GoalCommandOutcome::Paused);
        }
        if RESUME_ALIASES
            .iter()
            .any(|alias| trimmed.eq_ignore_ascii_case(alias))
        {
            return Ok(GoalCommandOutcome::Resumed);
        }
        if trimmed.len() > MAX_GOAL_CONDITION_LEN {
            bail!("goal condition is too long (max {MAX_GOAL_CONDITION_LEN} characters)");
        }
        Ok(GoalCommandOutcome::Set {
            condition: trimmed.to_string(),
        })
    }

    pub(crate) fn apply_outcome(
        &self,
        session_id: SessionId,
        outcome: GoalCommandOutcome,
    ) -> Result<String> {
        match outcome {
            GoalCommandOutcome::Set { condition } => {
                self.set_active(session_id, condition.clone())?;
                Ok(format!("goal active: {condition}"))
            }
            GoalCommandOutcome::Cleared => {
                self.clear_active(session_id);
                Ok("goal cleared".into())
            }
            GoalCommandOutcome::Paused => self.pause_active(session_id),
            GoalCommandOutcome::Resumed => self.resume_active(session_id),
            GoalCommandOutcome::Status(_) => Ok(String::new()),
        }
    }

    pub(crate) fn status(&self, session_id: SessionId, session_tokens: Option<u64>) -> GoalStatus {
        let sessions = self.sessions.read().expect("goal store lock poisoned");
        if let Some(runtime) = sessions.get(&session_id) {
            if runtime.persisted.active {
                return GoalStatus {
                    active: Some(runtime.persisted.clone()),
                    achieved: None,
                    elapsed: Some(runtime.started_at.elapsed()),
                    session_tokens,
                };
            }
            if let Some(achieved) = runtime.persisted.achieved.clone() {
                return GoalStatus {
                    active: None,
                    achieved: Some(achieved),
                    elapsed: None,
                    session_tokens,
                };
            }
        }
        GoalStatus {
            active: None,
            achieved: None,
            elapsed: None,
            session_tokens,
        }
    }

    pub(crate) fn set_active(&self, session_id: SessionId, condition: String) -> Result<()> {
        let mut sessions = self.sessions.write().expect("goal store lock poisoned");
        sessions.insert(
            session_id,
            SessionGoalRuntime {
                persisted: PersistedGoal {
                    condition: condition.clone(),
                    active: true,
                    paused: false,
                    evaluated_turns: 0,
                    last_reason: None,
                    achieved: None,
                },
                started_at: Instant::now(),
                pending_turn: true,
            },
        );
        drop(sessions);
        self.persist(session_id)
    }

    pub(crate) fn clear_active(&self, session_id: SessionId) {
        let mut sessions = self.sessions.write().expect("goal store lock poisoned");
        if let Some(runtime) = sessions.get_mut(&session_id) {
            runtime.persisted.active = false;
            runtime.pending_turn = false;
        } else {
            sessions.remove(&session_id);
        }
        drop(sessions);
        let _ = self.persist(session_id);
        let path = self.goal_path(session_id);
        if path.is_file() {
            let _ = std::fs::remove_file(path);
        }
    }

    pub(crate) fn pause_active(&self, session_id: SessionId) -> Result<String> {
        let mut sessions = self.sessions.write().expect("goal store lock poisoned");
        let Some(runtime) = sessions.get_mut(&session_id) else {
            return Ok("no active goal".into());
        };
        if !runtime.persisted.active {
            return Ok("no active goal".into());
        }
        runtime.persisted.paused = true;
        runtime.pending_turn = false;
        drop(sessions);
        self.persist(session_id)?;
        Ok("goal paused; run /goal resume to continue".into())
    }

    pub(crate) fn resume_active(&self, session_id: SessionId) -> Result<String> {
        let mut sessions = self.sessions.write().expect("goal store lock poisoned");
        let Some(runtime) = sessions.get_mut(&session_id) else {
            return Ok("no active goal".into());
        };
        if !runtime.persisted.active {
            return Ok("no active goal".into());
        }
        runtime.persisted.paused = false;
        runtime.pending_turn = true;
        let condition = runtime.persisted.condition.clone();
        drop(sessions);
        self.persist(session_id)?;
        Ok(format!("goal resumed: {condition}"))
    }

    pub(crate) fn is_active(&self, session_id: SessionId) -> bool {
        self.sessions
            .read()
            .expect("goal store lock poisoned")
            .get(&session_id)
            .is_some_and(|runtime| runtime.persisted.active)
    }

    pub(crate) fn is_paused(&self, session_id: SessionId) -> bool {
        self.sessions
            .read()
            .expect("goal store lock poisoned")
            .get(&session_id)
            .is_some_and(|runtime| runtime.persisted.active && runtime.persisted.paused)
    }

    pub(crate) fn active_condition(&self, session_id: SessionId) -> Option<String> {
        self.sessions
            .read()
            .expect("goal store lock poisoned")
            .get(&session_id)
            .filter(|runtime| runtime.persisted.active)
            .map(|runtime| runtime.persisted.condition.clone())
    }

    pub(crate) fn take_pending_turn(&self, session_id: SessionId) -> bool {
        self.sessions
            .write()
            .expect("goal store lock poisoned")
            .get_mut(&session_id)
            .is_some_and(|runtime| {
                if runtime.persisted.paused {
                    return false;
                }
                std::mem::take(&mut runtime.pending_turn)
            })
    }

    pub(crate) fn record_evaluation(
        &self,
        session_id: SessionId,
        evaluation: &GoalEvaluation,
    ) -> Result<()> {
        let mut sessions = self.sessions.write().expect("goal store lock poisoned");
        let Some(runtime) = sessions.get_mut(&session_id) else {
            return Ok(());
        };
        runtime.persisted.evaluated_turns = runtime.persisted.evaluated_turns.saturating_add(1);
        runtime.persisted.last_reason = Some(evaluation.reason.clone());
        if evaluation.met {
            let achieved = GoalAchieved {
                condition: runtime.persisted.condition.clone(),
                evaluated_turns: runtime.persisted.evaluated_turns,
                reason: evaluation.reason.clone(),
            };
            runtime.persisted.active = false;
            runtime.persisted.achieved = Some(achieved);
            runtime.persisted.paused = false;
            runtime.pending_turn = false;
        } else {
            runtime.pending_turn = !runtime.persisted.paused;
        }
        drop(sessions);
        self.persist(session_id)
    }

    pub(crate) fn continuation_prompt(&self, session_id: SessionId) -> Option<String> {
        let sessions = self.sessions.read().expect("goal store lock poisoned");
        let runtime = sessions.get(&session_id)?;
        if !runtime.persisted.active || runtime.persisted.paused {
            return None;
        }
        let reason = runtime
            .persisted
            .last_reason
            .as_deref()
            .filter(|value| !value.is_empty())
            .unwrap_or("keep working toward the goal");
        Some(format!(
            "Continue working toward the active goal:\n{}\n\nEvaluator: {}",
            runtime.persisted.condition, reason
        ))
    }

    fn goal_path(&self, _session_id: SessionId) -> PathBuf {
        self.session_dir.join(GOAL_FILE)
    }

    fn persist(&self, session_id: SessionId) -> Result<()> {
        let sessions = self.sessions.read().expect("goal store lock poisoned");
        let Some(runtime) = sessions.get(&session_id) else {
            return Ok(());
        };
        if !runtime.persisted.active && runtime.persisted.achieved.is_none() {
            return Ok(());
        }
        let path = self.goal_path(session_id);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create goal parent dir: {}", parent.display()))?;
        }
        let encoded = serde_json::to_string_pretty(&runtime.persisted)?;
        std::fs::write(&path, encoded)
            .with_context(|| format!("write goal state: {}", path.display()))?;
        Ok(())
    }
}

pub(crate) fn format_status(status: &GoalStatus) -> String {
    if let Some(active) = &status.active {
        let elapsed = status
            .elapsed
            .map(format_duration)
            .unwrap_or_else(|| "0s".into());
        let tokens = status
            .session_tokens
            .map(|value| value.to_string())
            .unwrap_or_else(|| "—".into());
        let reason = active
            .last_reason
            .as_deref()
            .filter(|value| !value.is_empty())
            .map(|value| format!("\nlast evaluation: {value}"))
            .unwrap_or_default();
        let state = if active.paused {
            "goal paused"
        } else {
            "goal active"
        };
        let resume = if active.paused {
            "\nresume: /goal resume"
        } else {
            ""
        };
        return format!(
            "{state}\ncondition: {}\nrunning: {elapsed}\nevaluated turns: {}\nsession tokens: {tokens}{reason}{resume}",
            active.condition, active.evaluated_turns
        );
    }
    if let Some(achieved) = &status.achieved {
        return format!(
            "goal achieved\ncondition: {}\nevaluated turns: {}\nreason: {}",
            achieved.condition, achieved.evaluated_turns, achieved.reason
        );
    }
    "no active goal".into()
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn format_duration(duration: Duration) -> String {
    let secs = duration.as_secs();
    if secs < 60 {
        return format!("{secs}s");
    }
    let mins = secs / 60;
    let rem = secs % 60;
    if mins < 60 {
        return format!("{mins}m {rem}s");
    }
    let hours = mins / 60;
    let rem_mins = mins % 60;
    format!("{hours}h {rem_mins}m")
}

pub(crate) async fn evaluate_active_goal(
    ctx: &CommandExecutionContext,
    condition: &str,
) -> everruns_provider::error::Result<GoalEvaluation> {
    let turn = ctx.host.turn_context().await?;
    let transcript = format_transcript(&turn.messages);
    let user_prompt =
        format!("Completion condition:\n{condition}\n\nConversation transcript:\n{transcript}");
    let completion_request = SessionCompletionRequest {
        system_prompts: vec![EVALUATOR_SYSTEM_PROMPT.to_string()],
        messages: vec![Message::user(user_prompt)],
        controls: None,
        metadata: std::collections::HashMap::from([(
            "command".to_string(),
            "goal_evaluate".to_string(),
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
                "goal evaluator does not support streaming",
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
) -> everruns_provider::error::Result<GoalEvaluation> {
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
    let met = trimmed.eq_ignore_ascii_case("yes")
        || trimmed.starts_with("YES")
        || trimmed.contains("\"met\": true")
        || trimmed.contains("\"met\":true");
    Ok(GoalEvaluation {
        met,
        reason: trimmed.to_string(),
    })
}

fn parse_evaluation_value(
    value: &serde_json::Value,
) -> everruns_provider::error::Result<GoalEvaluation> {
    let met = value
        .get("met")
        .and_then(serde_json::Value::as_bool)
        .ok_or_else(|| {
            everruns_provider::error::AgentLoopError::config(
                "goal evaluator returned JSON without `met`",
            )
        })?;
    let reason = value
        .get("reason")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    Ok(GoalEvaluation { met, reason })
}

pub(crate) fn evaluation_result_message(evaluation: &GoalEvaluation) -> String {
    serde_json::json!({
        "met": evaluation.met,
        "reason": evaluation.reason,
    })
    .to_string()
}

pub(crate) fn is_goal_evaluate_request(request: &ExecuteCommandRequest) -> bool {
    request.name == GOAL_COMMAND_NAME
        && request
            .arguments
            .as_deref()
            .is_some_and(|args| args == GOAL_EVALUATE_ARG)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_clear_aliases() {
        for alias in CLEAR_ALIASES {
            let outcome = GoalStore::parse_user_args(Some(alias)).expect("parse");
            assert_eq!(outcome, GoalCommandOutcome::Cleared);
        }
    }

    #[test]
    fn parse_pause_and_resume_aliases() {
        for alias in PAUSE_ALIASES {
            let outcome = GoalStore::parse_user_args(Some(alias)).expect("parse");
            assert_eq!(outcome, GoalCommandOutcome::Paused);
        }
        for alias in RESUME_ALIASES {
            let outcome = GoalStore::parse_user_args(Some(alias)).expect("parse");
            assert_eq!(outcome, GoalCommandOutcome::Resumed);
        }
    }

    #[test]
    fn pause_blocks_continuation_until_resume() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = GoalStore::open(dir.path().to_path_buf());
        let session_id = SessionId::new();
        store
            .set_active(session_id, "ship the fix".into())
            .expect("set");
        assert!(store.take_pending_turn(session_id));

        let message = store.pause_active(session_id).expect("pause");
        assert!(message.contains("goal paused"));
        assert!(store.is_active(session_id));
        assert!(store.is_paused(session_id));
        assert!(!store.take_pending_turn(session_id));
        assert!(store.continuation_prompt(session_id).is_none());

        let evaluation = GoalEvaluation {
            met: false,
            reason: "not done".into(),
        };
        store
            .record_evaluation(session_id, &evaluation)
            .expect("record");
        assert!(!store.take_pending_turn(session_id));

        let message = store.resume_active(session_id).expect("resume");
        assert!(message.contains("ship the fix"));
        assert!(!store.is_paused(session_id));
        assert!(store.take_pending_turn(session_id));
    }

    #[test]
    fn paused_goal_restores_without_pending_turn() {
        let dir = tempfile::tempdir().expect("tempdir");
        let session_id = SessionId::new();
        let store = GoalStore::open(dir.path().to_path_buf());
        store
            .set_active(session_id, "ship the fix".into())
            .expect("set");
        store.pause_active(session_id).expect("pause");

        let restored = GoalStore::open(dir.path().to_path_buf());
        restored.load_session(session_id).expect("load");

        assert!(restored.is_active(session_id));
        assert!(restored.is_paused(session_id));
        assert!(!restored.take_pending_turn(session_id));
    }

    #[test]
    fn parse_set_condition() {
        let outcome = GoalStore::parse_user_args(Some("all tests pass")).expect("parse");
        assert_eq!(
            outcome,
            GoalCommandOutcome::Set {
                condition: "all tests pass".into()
            }
        );
    }

    #[test]
    fn parse_evaluation_json() {
        let evaluation = parse_evaluation_response(
            r#"{"met": true, "reason": "cargo test exits 0 in the transcript"}"#,
        )
        .expect("parse");
        assert!(evaluation.met);
        assert!(evaluation.reason.contains("cargo test"));
    }

    #[test]
    fn format_status_active_and_achieved() {
        let active = format_status(&GoalStatus {
            active: Some(PersistedGoal {
                condition: "tests pass".into(),
                active: true,
                paused: false,
                evaluated_turns: 2,
                last_reason: Some("still failing".into()),
                achieved: None,
            }),
            achieved: None,
            elapsed: Some(Duration::from_secs(95)),
            session_tokens: Some(42),
        });
        assert!(active.contains("goal active"));
        assert!(active.contains("tests pass"));
        assert!(active.contains("1m 35s"));

        let achieved = format_status(&GoalStatus {
            active: None,
            achieved: Some(GoalAchieved {
                condition: "tests pass".into(),
                evaluated_turns: 3,
                reason: "all green".into(),
            }),
            elapsed: None,
            session_tokens: None,
        });
        assert!(achieved.contains("goal achieved"));
        assert!(achieved.contains("all green"));
    }
}

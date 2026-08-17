//! ATIF (Agent Trajectory Interchange Format) export.
//!
//! Spec: harbor-framework/harbor `rfcs/0001-trajectory-format.md`, pinned to
//! `ATIF-v1.7`. `--trajectory-out <path>` writes the whole session as one
//! ATIF JSON document at end of run (see `knowledge/specs/trajectory.md`).
//!
//! The trajectory is folded from the session's runtime event log rather than
//! the message store because events carry timestamps, per-generation token
//! usage, and tool results in one ordered stream:
//!
//! * `input.message`             → one `user` (or `system`) step
//! * `reason.item`               → reasoning summaries buffered for the next
//!   agent step (OpenAI Responses-style providers)
//! * `output.message.completed`  → one `agent` step per reason/act iteration,
//!   carrying text, `reasoning_content` (message thinking, else buffered
//!   summaries), `tool_calls`, and per-call `metrics`
//! * `tool.completed`            → an observation result attached to the agent
//!   step that issued the call, keyed by `source_call_id`
//!
//! Only text content is exported; image parts are dropped (ATIF image parts
//! reference on-disk paths, which yolop does not materialize). Empty and
//! `None` fields are skipped when serializing, per the RFC's convention.

use everruns_core::{ContentPart, Message, MessageRole};
use everruns_core::{Event, EventData, TokenUsage, ToolCompletedData};
use everruns_provider::typed_id::SessionId;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

pub const SCHEMA_VERSION: &str = "ATIF-v1.7";

/// Identity block for the `agent` object. The caller supplies the resolved
/// model id; name/version are the yolop crate's.
pub struct AgentInfo {
    pub name: String,
    pub version: String,
    pub model_name: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Trajectory {
    pub schema_version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub agent: Agent,
    pub steps: Vec<Step>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub final_metrics: Option<FinalMetrics>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Agent {
    pub name: String,
    pub version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_name: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StepSource {
    System,
    User,
    Agent,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Step {
    /// 1-based sequential id.
    pub step_id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,
    pub source: StepSource,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observation: Option<Observation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metrics: Option<StepMetrics>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ToolCall {
    pub tool_call_id: String,
    pub function_name: String,
    pub arguments: serde_json::Value,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Observation {
    pub results: Vec<ObservationResult>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ObservationResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extra: Option<serde_json::Value>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct StepMetrics {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completion_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cached_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct FinalMetrics {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_prompt_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_completion_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_cached_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_cost_usd: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_steps: Option<u64>,
}

/// Fold the ordered session event log into one ATIF trajectory.
pub fn trajectory_from_events(
    agent: AgentInfo,
    session_id: SessionId,
    events: &[Event],
) -> Trajectory {
    let mut steps: Vec<Step> = Vec::new();
    // tool_call_id → index of the agent step that issued the call.
    let mut call_step: HashMap<String, usize> = HashMap::new();
    // reason.item summaries buffered until the next agent step.
    let mut pending_reasoning: Vec<String> = Vec::new();

    for event in events {
        match &event.data {
            EventData::InputMessage(data) => {
                let source = match data.message.role {
                    MessageRole::System => StepSource::System,
                    MessageRole::User => StepSource::User,
                    // Agent/tool-result inputs are runtime plumbing, not
                    // conversation steps.
                    _ => continue,
                };
                steps.push(Step {
                    step_id: steps.len() as u64 + 1,
                    timestamp: Some(timestamp(event)),
                    source,
                    message: message_text(&data.message),
                    model_name: None,
                    reasoning_content: None,
                    tool_calls: Vec::new(),
                    observation: None,
                    metrics: None,
                });
            }
            EventData::ReasonItem(data) => {
                pending_reasoning.extend(
                    data.summary
                        .iter()
                        .map(|s| s.trim())
                        .filter(|s| !s.is_empty())
                        .map(str::to_string),
                );
            }
            EventData::OutputMessageCompleted(data) => {
                if data.message.role != MessageRole::Agent {
                    continue;
                }
                let reasoning_content = data
                    .message
                    .thinking
                    .clone()
                    .filter(|t| !t.trim().is_empty())
                    .or_else(|| {
                        (!pending_reasoning.is_empty()).then(|| pending_reasoning.join("\n\n"))
                    });
                pending_reasoning.clear();
                let tool_calls: Vec<ToolCall> = data
                    .message
                    .tool_calls()
                    .into_iter()
                    .map(|tc| ToolCall {
                        tool_call_id: tc.id.clone(),
                        function_name: tc.name.clone(),
                        arguments: tc.arguments.clone(),
                    })
                    .collect();
                let index = steps.len();
                for call in &tool_calls {
                    call_step.insert(call.tool_call_id.clone(), index);
                }
                steps.push(Step {
                    step_id: index as u64 + 1,
                    timestamp: Some(timestamp(event)),
                    source: StepSource::Agent,
                    message: message_text(&data.message),
                    model_name: data.metadata.as_ref().map(|m| m.model.clone()),
                    reasoning_content,
                    tool_calls,
                    observation: None,
                    metrics: data.usage.as_ref().map(step_metrics),
                });
            }
            EventData::ToolCompleted(data) => {
                // Fall back to the most recent agent step for results whose
                // call id was never seen (e.g. repaired/replayed logs).
                let index = call_step.get(&data.tool_call_id).copied().or_else(|| {
                    steps
                        .iter()
                        .rposition(|s| s.source == StepSource::Agent && !s.tool_calls.is_empty())
                });
                let Some(index) = index else { continue };
                steps[index]
                    .observation
                    .get_or_insert_default()
                    .results
                    .push(observation_result(data));
            }
            _ => {}
        }
    }

    let final_metrics = fold_final_metrics(&steps);
    Trajectory {
        schema_version: SCHEMA_VERSION.to_string(),
        session_id: Some(session_id.to_string()),
        agent: Agent {
            name: agent.name,
            version: agent.version,
            model_name: agent.model_name,
        },
        steps,
        final_metrics,
    }
}

/// Serialize `trajectory` as pretty-printed JSON at `path`, creating parent
/// directories as needed.
pub fn write_trajectory_file(path: &Path, trajectory: &Trajectory) -> anyhow::Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    let mut json = serde_json::to_string_pretty(trajectory)?;
    json.push('\n');
    std::fs::write(path, json)?;
    Ok(())
}

fn timestamp(event: &Event) -> String {
    event
        .ts
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// All text parts joined with newlines. Image and tool-call parts are
/// intentionally dropped (tool calls are exported via `tool_calls`).
fn message_text(message: &Message) -> String {
    message
        .content
        .iter()
        .filter_map(ContentPart::as_text)
        .collect::<Vec<_>>()
        .join("\n")
}

fn step_metrics(usage: &TokenUsage) -> StepMetrics {
    // ATIF's prompt_tokens is the full prompt; everruns buckets are disjoint
    // (input + cache_read + cache_creation = total prompt).
    let prompt = u64::from(usage.input_tokens)
        + u64::from(usage.cache_read_tokens.unwrap_or(0))
        + u64::from(usage.cache_creation_tokens.unwrap_or(0));
    StepMetrics {
        prompt_tokens: Some(prompt),
        completion_tokens: Some(u64::from(usage.output_tokens)),
        cached_tokens: usage.cache_read_tokens.map(u64::from),
        cost_usd: usage.actual_cost_usd.or(usage.estimated_cost_usd),
    }
}

fn observation_result(data: &ToolCompletedData) -> ObservationResult {
    let text = data
        .result
        .as_ref()
        .map(|parts| {
            parts
                .iter()
                .filter_map(ContentPart::as_text)
                .collect::<Vec<_>>()
                .join("\n")
        })
        .filter(|t| !t.is_empty())
        .or_else(|| data.error.clone());
    let extra = (!data.success).then(|| serde_json::json!({ "status": data.status.clone() }));
    ObservationResult {
        source_call_id: Some(data.tool_call_id.clone()),
        content: text,
        extra,
    }
}

fn fold_final_metrics(steps: &[Step]) -> Option<FinalMetrics> {
    if steps.is_empty() {
        return None;
    }
    let mut totals = FinalMetrics {
        total_steps: Some(steps.len() as u64),
        ..FinalMetrics::default()
    };
    for metrics in steps.iter().filter_map(|s| s.metrics.as_ref()) {
        add(&mut totals.total_prompt_tokens, metrics.prompt_tokens);
        add(
            &mut totals.total_completion_tokens,
            metrics.completion_tokens,
        );
        add(&mut totals.total_cached_tokens, metrics.cached_tokens);
        add_f64(&mut totals.total_cost_usd, metrics.cost_usd);
    }
    Some(totals)
}

fn add(total: &mut Option<u64>, value: Option<u64>) {
    if let Some(value) = value {
        *total = Some(total.unwrap_or(0) + value);
    }
}

fn add_f64(total: &mut Option<f64>, value: Option<f64>) {
    if let Some(value) = value {
        *total = Some(total.unwrap_or(0.0) + value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use everruns_core::Message;
    use everruns_core::{
        EventContext, InputMessageData, OutputMessageCompletedData, ReasonItemData,
        ToolCompletedData,
    };
    use everruns_provider::tool_types::ToolCall as RuntimeToolCall;

    fn session() -> SessionId {
        SessionId::from_seed(7)
    }

    fn event(data: impl Into<EventData>) -> Event {
        Event::new(session(), EventContext::empty(), data)
    }

    fn agent_info() -> AgentInfo {
        AgentInfo {
            name: "yolop".to_string(),
            version: "0.0.0-test".to_string(),
            model_name: Some("sim-model".to_string()),
        }
    }

    /// A full synthetic turn: user input, a reasoning+act iteration with one
    /// tool call and result, then the final assistant message, with usage on
    /// both generations.
    fn synthetic_events() -> Vec<Event> {
        let mut with_tools = Message::assistant_with_tools(
            "Let me check the file.",
            vec![RuntimeToolCall {
                id: "call_1".to_string(),
                name: "read_file".to_string(),
                arguments: serde_json::json!({ "path": "src/lib.rs" }),
            }],
        );
        with_tools.thinking = Some("The user wants the file contents.".to_string());
        vec![
            event(EventData::InputMessage(InputMessageData::new(
                Message::user("fix the bug"),
            ))),
            event(EventData::ReasonItem(ReasonItemData {
                turn_id: everruns_provider::typed_id::TurnId::from_seed(1),
                provider: "sim".to_string(),
                model: None,
                item_id: "ri_1".to_string(),
                encrypted_content: None,
                summary: vec!["Looking at the bug report.".to_string()],
                token_count: Some(5),
            })),
            event(EventData::OutputMessageCompleted(
                OutputMessageCompletedData::new(with_tools).with_usage(TokenUsage::with_cache(
                    100,
                    20,
                    Some(50),
                    None,
                )),
            )),
            event(EventData::ToolCompleted(ToolCompletedData::success(
                "call_1".to_string(),
                "read_file".to_string(),
                vec![ContentPart::text("fn main() {}")],
                Some(12),
            ))),
            event(EventData::OutputMessageCompleted(
                OutputMessageCompletedData::new(Message::assistant("Fixed."))
                    .with_usage(TokenUsage::new(30, 10)),
            )),
        ]
    }

    #[test]
    fn folds_synthetic_turn_into_steps() {
        let trajectory = trajectory_from_events(agent_info(), session(), &synthetic_events());

        assert_eq!(trajectory.schema_version, SCHEMA_VERSION);
        assert_eq!(trajectory.session_id, Some(session().to_string()));
        assert_eq!(trajectory.agent.name, "yolop");
        assert_eq!(trajectory.agent.model_name.as_deref(), Some("sim-model"));

        let steps = &trajectory.steps;
        assert_eq!(steps.len(), 3);
        assert_eq!(
            steps.iter().map(|s| s.step_id).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );

        assert_eq!(steps[0].source, StepSource::User);
        assert_eq!(steps[0].message, "fix the bug");
        assert!(steps[0].timestamp.is_some());

        let act = &steps[1];
        assert_eq!(act.source, StepSource::Agent);
        assert_eq!(act.message, "Let me check the file.");
        // Message-level thinking wins over buffered reason.item summaries.
        assert_eq!(
            act.reasoning_content.as_deref(),
            Some("The user wants the file contents.")
        );
        assert_eq!(act.tool_calls.len(), 1);
        assert_eq!(act.tool_calls[0].tool_call_id, "call_1");
        assert_eq!(act.tool_calls[0].function_name, "read_file");
        assert_eq!(
            act.tool_calls[0].arguments,
            serde_json::json!({ "path": "src/lib.rs" })
        );
        let results = &act.observation.as_ref().expect("observation").results;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].source_call_id.as_deref(), Some("call_1"));
        assert_eq!(results[0].content.as_deref(), Some("fn main() {}"));
        assert!(results[0].extra.is_none());
        let metrics = act.metrics.as_ref().expect("act metrics");
        assert_eq!(metrics.prompt_tokens, Some(150)); // input 100 + cache_read 50
        assert_eq!(metrics.completion_tokens, Some(20));
        assert_eq!(metrics.cached_tokens, Some(50));

        let last = &steps[2];
        assert_eq!(last.source, StepSource::Agent);
        assert_eq!(last.message, "Fixed.");
        assert!(last.tool_calls.is_empty());
        assert!(last.observation.is_none());
        // No thinking and no pending summaries left → no reasoning_content.
        assert!(last.reasoning_content.is_none());

        let totals = trajectory.final_metrics.expect("final metrics");
        assert_eq!(totals.total_prompt_tokens, Some(180));
        assert_eq!(totals.total_completion_tokens, Some(30));
        assert_eq!(totals.total_cached_tokens, Some(50));
        assert_eq!(totals.total_steps, Some(3));
    }

    #[test]
    fn reason_item_summaries_feed_next_agent_step() {
        let events = vec![
            event(EventData::InputMessage(InputMessageData::new(
                Message::user("hi"),
            ))),
            event(EventData::ReasonItem(ReasonItemData {
                turn_id: everruns_provider::typed_id::TurnId::from_seed(2),
                provider: "sim".to_string(),
                model: None,
                item_id: "ri_2".to_string(),
                encrypted_content: None,
                summary: vec!["First thought.".to_string(), "Second thought.".to_string()],
                token_count: None,
            })),
            event(EventData::OutputMessageCompleted(
                OutputMessageCompletedData::new(Message::assistant("Hello!")),
            )),
        ];
        let trajectory = trajectory_from_events(agent_info(), session(), &events);
        assert_eq!(
            trajectory.steps[1].reasoning_content.as_deref(),
            Some("First thought.\n\nSecond thought.")
        );
        // No usage anywhere → totals carry only the step count.
        let totals = trajectory.final_metrics.expect("final metrics");
        assert!(totals.total_prompt_tokens.is_none());
        assert_eq!(totals.total_steps, Some(2));
    }

    #[test]
    fn failed_tool_result_exports_error_and_status() {
        let events = vec![
            event(EventData::OutputMessageCompleted(
                OutputMessageCompletedData::new(Message::assistant_with_tools(
                    "",
                    vec![RuntimeToolCall {
                        id: "call_9".to_string(),
                        name: "bash".to_string(),
                        arguments: serde_json::json!({ "command": "false" }),
                    }],
                )),
            )),
            event(EventData::ToolCompleted(ToolCompletedData::failure(
                "call_9".to_string(),
                "bash".to_string(),
                "error".to_string(),
                "exit status 1".to_string(),
                None,
            ))),
        ];
        let trajectory = trajectory_from_events(agent_info(), session(), &events);
        let results = &trajectory.steps[0]
            .observation
            .as_ref()
            .expect("observation")
            .results;
        assert_eq!(results[0].content.as_deref(), Some("exit status 1"));
        assert_eq!(
            results[0].extra,
            Some(serde_json::json!({ "status": "error" }))
        );
    }

    #[test]
    fn serialization_skips_empty_fields() {
        let trajectory = trajectory_from_events(agent_info(), session(), &synthetic_events());
        let json = serde_json::to_value(&trajectory).expect("serialize");

        assert_eq!(json["schema_version"], SCHEMA_VERSION);
        let user_step = &json["steps"][0];
        assert!(user_step.get("tool_calls").is_none());
        assert!(user_step.get("observation").is_none());
        assert!(user_step.get("metrics").is_none());
        assert!(user_step.get("reasoning_content").is_none());
        let final_step = &json["steps"][2];
        assert!(final_step.get("tool_calls").is_none());
        // Round-trips through the serde types.
        let parsed: Trajectory = serde_json::from_value(json).expect("deserialize");
        assert_eq!(parsed.steps.len(), 3);
    }
}
